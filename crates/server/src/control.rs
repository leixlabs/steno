//! Service lifecycle control for the Portal's control centre (`[control]`).
//!
//! ## What this is allowed to touch
//!
//! Nothing but `systemctl <verb> <unit>`. There is no D-Bus client, no shell,
//! and no path built from a request body: a request names a *target*
//! (`server` / `node`) and an *action* (`start` / `stop` / `restart`), and both
//! are closed enums. Unit names come from the configuration file, never from
//! the wire, so a request cannot address an arbitrary unit.
//!
//! ## Why it is off by default
//!
//! `Config` refuses to validate `control.enabled = true` unless
//! `server.bind_address` is a loopback address. The API has no authentication
//! and no TLS, and this is the one endpoint that changes machine state instead
//! of reporting it — reachable from the LAN it would be an unauthenticated kill
//! switch for a device that records private audio.
//!
//! ## Nothing here invents an answer
//!
//! When systemd is not the init system (a container, a developer's Mac) every
//! query reports "unavailable" with the reason, and a start/stop attempt fails
//! with that reason instead of a success the operator cannot verify.

use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

use steno_lib::config::Config;

/// Which unit a control request addresses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Server,
    Node,
}

impl Target {
    pub fn as_str(self) -> &'static str {
        match self {
            Target::Server => "server",
            Target::Node => "node",
        }
    }

    pub fn parse(value: &str) -> Option<Target> {
        match value.trim() {
            "server" => Some(Target::Server),
            "node" => Some(Target::Node),
            _ => None,
        }
    }

    /// The unit name as configured. Never taken from a request body.
    pub fn unit(self, config: &Config) -> &str {
        match self {
            Target::Server => &config.control.server_unit,
            Target::Node => &config.control.node_unit,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Start,
    Stop,
    Restart,
}

impl Action {
    pub fn as_str(self) -> &'static str {
        match self {
            Action::Start => "start",
            Action::Stop => "stop",
            Action::Restart => "restart",
        }
    }

    pub fn parse(value: &str) -> Option<Action> {
        match value.trim() {
            "start" => Some(Action::Start),
            "stop" => Some(Action::Stop),
            "restart" => Some(Action::Restart),
            _ => None,
        }
    }

    fn verb(self) -> &'static str {
        self.as_str()
    }
}

/// Whether control can work at all on this host, and why not when it cannot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Availability {
    /// `[control] enabled = false`: the Portal hides the buttons.
    Disabled,
    /// systemd is not the init system, so there is no unit to ask about.
    NoSystemd,
    /// `systemctl` is configured and systemd is running.
    Ready,
}

impl Availability {
    pub fn is_ready(&self) -> bool {
        matches!(self, Availability::Ready)
    }

    /// A short operator-facing reason, or `None` when control is ready.
    pub fn reason(&self) -> Option<&'static str> {
        match self {
            Availability::Disabled => Some("控制已关闭（[control] enabled = false）"),
            Availability::NoSystemd => {
                Some("这台机器没有运行 systemd，无法通过 systemctl 启停单元")
            }
            Availability::Ready => None,
        }
    }
}

/// Probes the host. Only ever called when `control.enabled` is true.
///
/// `/run/systemd/system` is the same signal `sd_booted(3)` uses. Checking the
/// path first means a developer machine without systemd gets "unavailable"
/// without spawning a process that is guaranteed to fail.
pub fn availability(config: &Config) -> Availability {
    if !config.control.enabled {
        return Availability::Disabled;
    }
    if !Path::new("/run/systemd/system").is_dir() {
        return Availability::NoSystemd;
    }
    Availability::Ready
}

/// `systemctl is-active` for one unit.
///
/// Returns the word systemctl printed, limited to the values documented for
/// `is-active`; anything else is reported as `"unknown"` rather than being
/// passed through, because the Portal renders this verbatim.
pub fn unit_state(config: &Config, target: Target) -> &'static str {
    if !availability(config).is_ready() {
        return "unknown";
    }
    let unit = target.unit(config);
    let output = match run(
        &config.control.systemctl_command,
        &["is-active", unit],
        Duration::from_secs(config.control.timeout_seconds.min(10)),
    ) {
        Ok(output) => output,
        Err(_) => return "unknown",
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    match stdout.trim() {
        "active" => "active",
        "activating" => "activating",
        "deactivating" => "deactivating",
        "inactive" => "inactive",
        "failed" => "failed",
        "reloading" => "reloading",
        "unknown" => "unknown",
        _ => "unknown",
    }
}

/// Runs `<systemctl> <action> <unit>`.
///
/// Errors carry systemctl's own stderr, so a refused operation (no permission,
/// no such unit, polkit denial) reaches the operator as the real message.
pub fn perform(config: &Config, target: Target, action: Action) -> Result<(), ControlError> {
    match availability(config) {
        Availability::Ready => {}
        other => {
            return Err(ControlError::Unavailable(
                other.reason().unwrap_or("控制不可用").to_string(),
            ));
        }
    }
    let unit = target.unit(config);
    let timeout = Duration::from_secs(config.control.timeout_seconds);
    let output = run(
        &config.control.systemctl_command,
        &[action.verb(), unit],
        timeout,
    )?;
    if output.status.success() {
        return Ok(());
    }
    Err(ControlError::Refused {
        unit: unit.to_string(),
        action: action.as_str(),
        message: describe_failure(&output),
    })
}

#[derive(Debug, thiserror::Error)]
pub enum ControlError {
    #[error("{0}")]
    Unavailable(String),
    #[error("cannot run the configured control command: {0}")]
    Spawn(#[from] std::io::Error),
    #[error("the control command did not finish within the timeout and was killed")]
    TimedOut,
    #[error("systemctl {action} {unit} failed: {message}")]
    Refused {
        unit: String,
        action: &'static str,
        message: String,
    },
}

/// Joins stderr (and stdout when stderr is empty) into one line.
fn describe_failure(output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if !stderr.is_empty() {
        return stderr;
    }
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if !stdout.is_empty() {
        return stdout;
    }
    match output.status.code() {
        Some(code) => format!("exit status {code}"),
        None => "terminated by a signal".to_string(),
    }
}

/// Spawns the command, polls for completion and kills it after `timeout`.
///
/// `Command::output()` has no timeout, and `systemctl start` on a unit whose
/// start job hangs would otherwise pin an HTTP worker thread for as long as the
/// job runs.
fn run(program: &str, args: &[&str], timeout: Duration) -> Result<Output, ControlError> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait()? {
            Some(_) => break,
            None => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(ControlError::TimedOut);
                }
                sleep(Duration::from_millis(20));
            }
        }
    }
    Ok(child.wait_with_output()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(enabled: bool) -> Config {
        let mut config = Config::from_toml_str(
            "[storage]\nrecording_dir = \"/tmp/steno-control\"\ndatabase_path = \"/tmp/steno-control/app.db\"\n",
        )
        .expect("base config");
        config.control.enabled = enabled;
        config
    }

    #[test]
    fn targets_parse_only_from_the_closed_set() {
        assert_eq!(Target::parse("server"), Some(Target::Server));
        assert_eq!(Target::parse(" node "), Some(Target::Node));
        assert_eq!(Target::parse("steno-node"), None);
        assert_eq!(Target::parse("../etc/passwd"), None);
    }

    #[test]
    fn actions_parse_only_from_the_closed_set() {
        assert_eq!(Action::parse("start"), Some(Action::Start));
        assert_eq!(Action::parse("stop"), Some(Action::Stop));
        assert_eq!(Action::parse("restart"), Some(Action::Restart));
        assert_eq!(Action::parse("rm -rf /"), None);
    }

    #[test]
    fn unit_names_come_from_the_config_not_the_request() {
        let mut config = config(true);
        config.control.node_unit = "bedside-node".to_string();
        assert_eq!(Target::Node.unit(&config), "bedside-node");
        assert_eq!(Target::Server.unit(&config), "steno-monitor");
    }

    #[test]
    fn control_reports_disabled_before_it_touches_the_host() {
        let config = config(false);
        assert_eq!(availability(&config), Availability::Disabled);
        assert_eq!(unit_state(&config, Target::Server), "unknown");
        let error = perform(&config, Target::Server, Action::Start).expect_err("disabled");
        assert!(matches!(error, ControlError::Unavailable(_)));
    }

    #[test]
    fn a_missing_systemctl_is_an_error_not_a_silent_success() {
        let mut config = config(true);
        config.control.systemctl_command = "definitely-not-a-command-steno".to_string();
        if !availability(&config).is_ready() {
            // No systemd on this host: the command is never reached, which is
            // itself the correct answer, so only assert the honest refusal.
            let error = perform(&config, Target::Server, Action::Stop).expect_err("no systemd");
            assert!(matches!(error, ControlError::Unavailable(_)));
            return;
        }
        let error = perform(&config, Target::Server, Action::Stop).expect_err("spawn fails");
        assert!(matches!(error, ControlError::Spawn(_)), "got {error:?}");
    }
}
