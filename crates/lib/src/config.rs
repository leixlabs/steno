//! Configuration loading and validation (TECH_SPEC §11, task T1.2).
//!
//! Validation never falls back to a different value: a bad field is reported by
//! name and startup fails. The only documented fallbacks live in the capture
//! device negotiation (TECH_SPEC §4.1), not here.

use serde::Deserialize;
use std::path::{Path, PathBuf};

/// Default configuration path (TECH_SPEC §11).
pub const DEFAULT_CONFIG_PATH: &str = "/etc/steno-monitor/config.toml";
/// Environment variable that overrides the configuration path.
pub const CONFIG_PATH_ENV: &str = "STENO_MONITOR_CONFIG";

/// Capture rates a device may be opened at.
///
/// These are device rates, not wire rates: every one of them is converted onto
/// [`WIRE_RATE_HZ`] by [`crate::audio_capture::WireConverter`] before anything
/// records, detects or transcribes. The list is closed because the resampler
/// expresses each ratio exactly (`Resampler::new` refuses the rest).
pub const SUPPORTED_CAPTURE_RATES: [u32; 5] = [16_000, 32_000, 44_100, 48_000, 96_000];
/// The MVP detection rate. Any other configured value is a validation error.
pub const DETECTION_RATE_HZ: u32 = 16_000;

/// The one format everything downstream speaks: what travels on the wire, what
/// the recorder writes to disk, what the detector analyses and what the
/// recogniser is handed.
///
/// This is a constant, not a setting, because every one of those consumers was
/// already written against it and only the *device* was allowed to differ. It
/// used to be expressed as `[audio] preferred_rate_hz` + `channels` on both
/// sides of the split deployment, which meant two files had to agree by hand;
/// a mismatch did not resample, it refused the audio, and a node that kept
/// sending HELLO while its PCM was refused looked online all night. Making the
/// wire format a constant removes that failure mode: the only thing left to
/// configure is the device, and the conversion to this format happens once,
/// next to the microphone.
pub const WIRE_RATE_HZ: u32 = DETECTION_RATE_HZ;
/// Mono. Stereo capture is downmixed (one chosen channel) before anything else
/// sees it, so no consumer has to think about interleaving.
pub const WIRE_CHANNELS: u16 = 1;
/// Classic RIFF/WAVE size ceiling; segments must stay below it (TECH_SPEC §4.2).
pub const RIFF_MAX_BYTES: u64 = 4 * 1024 * 1024 * 1024;
/// Bytes per stored sample for the only supported capture format.
pub const BYTES_PER_SAMPLE: u64 = 2;

/// Field-level validation failures.
#[derive(Debug, thiserror::Error)]
#[error("invalid configuration:\n  {}", .0.join("\n  "))]
pub struct ConfigError(pub Vec<String>);

/// Why a configuration file could not be turned into a usable [`Config`].
#[derive(Debug, thiserror::Error)]
pub enum ConfigLoadError {
    #[error("cannot read configuration file {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("cannot parse configuration file {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error(transparent)]
    Invalid(#[from] ConfigError),
}

/// Sample format of the recorded stream. MVP supports `S16_LE` only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub enum SampleFormat {
    #[serde(rename = "S16_LE")]
    S16Le,
}

impl SampleFormat {
    pub fn as_str(self) -> &'static str {
        match self {
            SampleFormat::S16Le => "S16_LE",
        }
    }

    pub fn bytes_per_sample(self) -> u64 {
        match self {
            SampleFormat::S16Le => BYTES_PER_SAMPLE,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub audio: AudioConfig,
    /// Where the audio comes from. Defaults to the local ALSA device.
    #[serde(default)]
    pub source: SourceConfig,
    #[serde(default)]
    pub detector: DetectorConfig,
    #[serde(default)]
    pub stt: SttConfig,
    /// Service lifecycle control, used by the Portal's control centre.
    #[serde(default)]
    pub control: ControlConfig,
    pub storage: StorageConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    /// T0.1 decision: the shipped default binds to loopback. Deployments that
    /// need LAN access set this explicitly (see `deployment/config.example.toml`).
    #[serde(default = "default_bind_address")]
    pub bind_address: String,
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default = "default_http_threads")]
    pub http_threads: usize,
    /// IANA timezone used to turn `?date=YYYY-MM-DD` into a UTC range.
    /// Empty means "use the host's local timezone" (T0.1 decision).
    #[serde(default)]
    pub timezone: String,
    /// Directory holding the built UI (`web/dist`). Relative paths resolve
    /// against the working directory. Empty disables the UI entirely, which is
    /// what a headless or API-only deployment wants.
    #[serde(default = "default_web_dir")]
    pub web_dir: String,
}

/// Speech-to-text settings.
///
/// Transcription is off by default: it is the only feature that sends audio
/// through a recogniser, and on Apple platforms the first run needs a user
/// authorisation prompt. Turning it on is an explicit decision.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SttConfig {
    #[serde(default)]
    pub enabled: bool,
    /// `"stream"` keeps one recogniser alive and feeds it audio continuously.
    /// This is the only supported mode: `"segment"` (shelling out per finished
    /// WAV) was removed, and validation refuses it by name rather than silently
    /// transcribing nothing.
    #[serde(default = "default_stt_mode")]
    pub mode: SttMode,
    /// Launcher for the streaming sidecar. It is started once at boot and talks
    /// over `socket_path` (see `stt_stream`).
    #[serde(default = "default_stt_stream_command")]
    pub stream_command: String,
    /// Unix socket the streaming sidecar connects back to. Empty means
    /// "derive it from the recording directory".
    #[serde(default)]
    pub socket_path: String,
    /// Human-readable backend name stored with each transcript.
    #[serde(default = "default_stt_engine")]
    pub engine: String,
    /// Language the recogniser listens for, e.g. `zh-CN`. Sent to the sidecar in
    /// the `config` frame that precedes the first audio block: the language is
    /// the orchestrator's decision, and a sidecar guessing it from a built-in
    /// default would silently transcribe the night in the wrong language.
    #[serde(default = "default_stt_locale")]
    pub locale: String,
    /// Additional recogniser processes, each with its own launcher and lanes.
    /// The `[stt]` keys above describe the first one (named `primary`, lanes
    /// from `stt.lanes`); an entry here adds another, so one machine can run Qwen3-ASR for
    /// text and the Apple sidecar for snores at the same time.
    #[serde(default)]
    pub recognizer: Vec<RecognizerConfig>,
    /// Lanes the first recogniser is asked for. The default asks for both, which
    /// is what the Apple sidecar offers; a text-only engine (audio.cpp) must say
    /// `["text"]` here, and until it does the status page lists `snore` as a
    /// lane that was asked for and is not offered.
    #[serde(default = "default_primary_lanes")]
    pub lanes: Vec<String>,
}

fn default_primary_lanes() -> Vec<String> {
    vec!["text".to_string(), "snore".to_string()]
}

/// One recogniser process. The server decides which lanes it wants from it; the
/// process declares which it has on `ready`, and a mismatch is reported.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecognizerConfig {
    pub name: String,
    pub stream_command: String,
    /// Empty derives `$TMPDIR/steno-stt-<pid>-<name>.sock`.
    #[serde(default)]
    pub socket_path: String,
    /// Fallback label for rows this recogniser stores.
    #[serde(default)]
    pub engine: String,
    /// Subset of `text` / `snore`. Must be non-empty.
    pub lanes: Vec<String>,
}

pub const KNOWN_LANES: [&str; 2] = ["text", "snore"];

impl SttConfig {
    /// Every recogniser the server runs, the legacy `[stt]` keys first.
    pub fn recognizers(&self) -> Vec<RecognizerConfig> {
        let mut all = vec![RecognizerConfig {
            name: "primary".to_string(),
            stream_command: self.stream_command.clone(),
            socket_path: self.socket_path.clone(),
            engine: self.engine.clone(),
            lanes: self.lanes.clone(),
        }];
        all.extend(self.recognizer.iter().cloned());
        all
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum SttMode {
    #[default]
    Stream,
    /// Removed. Kept in the enum so an old config file is refused with an
    /// explanation instead of an "unknown variant" parse error.
    Segment,
}

impl SttMode {
    pub fn as_str(self) -> &'static str {
        match self {
            SttMode::Stream => "stream",
            SttMode::Segment => "segment",
        }
    }
}

/// Service lifecycle control: the one place where the HTTP API may change
/// machine state instead of only reporting it.
///
/// Off by default, and it refuses to be combined with a non-loopback bind
/// address (see `ControlConfig::validate`). Both rules exist for the same
/// reason: the API has no authentication and no TLS, so a start/stop endpoint
/// reachable from the LAN would be an unauthenticated remote kill switch for a
/// device that is recording medical-adjacent audio.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ControlConfig {
    #[serde(default)]
    pub enabled: bool,
    /// The systemctl binary. Only the `systemctl` CLI is used; nothing talks to
    /// D-Bus directly, so this also works inside a minimal container image that
    /// ships the CLI.
    #[serde(default = "default_systemctl_command")]
    pub systemctl_command: String,
    /// Unit that runs this server (`deployment/systemd/steno-monitor.service`).
    #[serde(default = "default_server_unit")]
    pub server_unit: String,
    /// Unit that runs the capture node (`deployment/systemd/steno-node.service`). It only
    /// resolves when the node runs on the same host as the server; on a split
    /// deployment `systemctl` reports the unit as unknown and the Portal says so
    /// instead of pretending the bedroom device is controllable from here.
    #[serde(default = "default_node_unit")]
    pub node_unit: String,
    /// How long a single systemctl invocation may take before it is killed.
    #[serde(default = "default_control_timeout_seconds")]
    pub timeout_seconds: u64,
}

impl Default for ControlConfig {
    fn default() -> Self {
        ControlConfig {
            enabled: false,
            systemctl_command: default_systemctl_command(),
            server_unit: default_server_unit(),
            node_unit: default_node_unit(),
            timeout_seconds: default_control_timeout_seconds(),
        }
    }
}

impl ControlConfig {
    fn validate(&self, bind_address: &str, errors: &mut Vec<String>) {
        if !self.enabled {
            return;
        }
        // Loopback-only is a precondition for enabling this at all. Loopback is
        // checked on the configured address rather than per request so a
        // misconfiguration fails at startup with the offending key named,
        // instead of silently exposing a kill switch to the LAN.
        match bind_address.parse::<std::net::IpAddr>() {
            Ok(address) if address.is_loopback() => {}
            Ok(_) => errors.push(format!(
                "control.enabled: requires server.bind_address to be a loopback address, got {:?}",
                bind_address
            )),
            Err(_) => errors.push(format!(
                "control.enabled: server.bind_address must be an IP address, got {:?}",
                bind_address
            )),
        }
        if self.systemctl_command.trim().is_empty() {
            errors.push("control.systemctl_command: must not be empty".to_string());
        }
        if self.server_unit.trim().is_empty() {
            errors.push("control.server_unit: must not be empty".to_string());
        }
        if self.node_unit.trim().is_empty() {
            errors.push("control.node_unit: must not be empty".to_string());
        }
        if !(1..=120).contains(&self.timeout_seconds) {
            errors.push(format!(
                "control.timeout_seconds: must be within 1..=120, got {}",
                self.timeout_seconds
            ));
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AudioConfig {
    /// ALSA card name. Empty means "pick the only USB capture device"; with
    /// more than one candidate present, startup fails instead of guessing.
    #[serde(default)]
    pub alsa_device: String,
    /// Rate to *ask the device* for. Not the rate anything is recorded or
    /// analysed at: that is [`WIRE_RATE_HZ`], fixed. The device's own default
    /// configuration is tried first regardless of this value, because it is the
    /// only rate ALSA guarantees; this is the fallback hint.
    #[serde(default = "default_device_rate_hz")]
    pub device_rate_hz: u32,
    /// Channels to *ask the device* for. The wire is always [`WIRE_CHANNELS`];
    /// a device that only offers stereo is captured as stereo and one channel of
    /// it is taken (see `mono_channel_index`).
    #[serde(default = "default_device_channels")]
    pub device_channels: u16,
    #[serde(default = "default_format")]
    pub format: SampleFormat,
    #[serde(default = "default_period_ms")]
    pub period_ms: u32,
    #[serde(default = "default_buffer_ms")]
    pub buffer_ms: u32,
    #[serde(default = "default_segment_duration_seconds")]
    pub segment_duration_seconds: u64,
    /// Which of the device's channels becomes the mono wire stream.
    ///
    /// This is the one place "stereo in, mono out" is a decision rather than an
    /// average: a bedside microphone often has one capsule pointed at the room
    /// and one at a wall, and averaging them mixes in a reflection instead of
    /// removing noise. Must be `< device_channels`; it is checked against the
    /// channels the device actually opened at before the first block is
    /// converted, and a device that delivered fewer channels is an error rather
    /// than a silent clamp.
    #[serde(default)]
    pub mono_channel_index: u16,
    /// Rumble filter for the capture path, in Hz; 0 disables it.
    ///
    /// A condenser microphone at high gain produces a floor that is almost
    /// entirely sub-250 Hz airflow and preamp noise. Left in, it spends the
    /// recogniser's dynamic range on sound that carries no words: measured on the
    /// bedside device the floor was -33 dBFS and only a few dB below speech.
    /// [`crate::dsp::HighPass`] implements the filter.
    #[serde(default = "default_rumble_highpass_hz")]
    pub rumble_highpass_hz: u32,
    #[serde(default = "default_capture_ring_ms")]
    pub capture_ring_ms: u32,
    #[serde(default = "default_recording_queue_ms")]
    pub recording_queue_ms: u32,
    #[serde(default = "default_detection_queue_ms")]
    pub detection_queue_ms: u32,
    #[serde(default = "default_gap_tolerance_ms")]
    pub gap_tolerance_ms: u32,
    #[serde(default = "default_gap_rotate_seconds")]
    pub gap_rotate_seconds: u64,
    #[serde(default = "default_header_flush_interval_seconds")]
    pub header_flush_interval_seconds: u64,
    #[serde(default = "default_shutdown_timeout_seconds")]
    pub shutdown_timeout_seconds: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DetectorConfig {
    #[serde(default = "default_detection_rate_hz")]
    pub detection_rate_hz: u32,
    #[serde(default = "default_frame_ms")]
    pub frame_ms: u32,
    #[serde(default = "default_band_low_hz")]
    pub band_low_hz: f32,
    #[serde(default = "default_band_high_hz")]
    pub band_high_hz: f32,
    #[serde(default = "default_absolute_floor_dbfs")]
    pub absolute_floor_dbfs: f32,
    #[serde(default = "default_noise_margin_db")]
    pub noise_margin_db: f32,
    #[serde(default = "default_band_margin_db")]
    pub band_margin_db: f32,
    #[serde(default = "default_band_ratio_min")]
    pub band_ratio_min: f32,
    #[serde(default = "default_noise_floor_window_seconds")]
    pub noise_floor_window_seconds: u32,
    #[serde(default = "default_noise_floor_percentile")]
    pub noise_floor_percentile: u32,
    #[serde(default = "default_noise_floor_min_seconds")]
    pub noise_floor_min_seconds: u32,
    #[serde(default = "default_noise_floor_reset_gap_seconds")]
    pub noise_floor_reset_gap_seconds: u64,
    #[serde(default = "default_noise_freeze_max_seconds")]
    pub noise_freeze_max_seconds: u32,
    #[serde(default = "default_filter_warmup_ms")]
    pub filter_warmup_ms: u32,
    #[serde(default = "default_attack_window_frames")]
    pub attack_window_frames: u32,
    #[serde(default = "default_attack_min_hits")]
    pub attack_min_hits: u32,
    #[serde(default = "default_hangover_frames")]
    pub hangover_frames: u32,
    #[serde(default = "default_min_event_ms")]
    pub min_event_ms: u32,
    #[serde(default = "default_merge_gap_ms")]
    pub merge_gap_ms: u32,
    #[serde(default = "default_max_event_seconds")]
    pub max_event_seconds: u32,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StorageConfig {
    pub recording_dir: PathBuf,
    pub database_path: PathBuf,
    #[serde(default = "default_recording_max_bytes")]
    pub recording_max_bytes: u64,
    #[serde(default = "default_recording_cleanup_target_bytes")]
    pub recording_cleanup_target_bytes: u64,
    #[serde(default = "default_recording_reserve_bytes")]
    pub recording_reserve_bytes: u64,
    /// T0.1 decision: retention also runs on a timer, not only after a segment
    /// completes and at startup.
    #[serde(default = "default_retention_check_interval_seconds")]
    pub retention_check_interval_seconds: u64,
}

/// Where audio comes from.
///
/// `alsa` is the single-box deployment the project shipped first; `net` is the
/// split deployment where a capture node streams to this host (see
/// [`crate::net_capture`]). The default keeps existing installations working.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    Alsa,
    Net,
}

impl Default for SourceKind {
    fn default() -> Self {
        SourceKind::Alsa
    }
}

impl SourceKind {
    pub fn as_str(self) -> &'static str {
        match self {
            SourceKind::Alsa => "alsa",
            SourceKind::Net => "net",
        }
    }
}

/// Capture source configuration (split deployment).
///
/// In `net` mode the server does not open a microphone at all: it listens for a
/// capture node and feeds the same ring the ALSA path writes to. The expected
/// audio layout stays in `[audio]` so there is exactly one place that defines
/// the format the server is willing to record.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceConfig {
    #[serde(default)]
    pub kind: SourceKind,
    /// Interface to listen on. Loopback keeps the audio stream off the LAN; the
    /// normal split deployment uses `0.0.0.0` on a trusted network.
    #[serde(default = "default_source_bind_address")]
    pub bind_address: String,
    #[serde(default = "default_source_port")]
    pub port: u16,
    /// Node id to accept. `0` accepts the first node that announces itself.
    #[serde(default)]
    pub device_id: u32,
    /// How long a missing sequence number is held in the reorder window before
    /// it is declared lost. Larger tolerates more reordering, at more latency.
    #[serde(default = "default_jitter_buffer_ms")]
    pub jitter_buffer_ms: u32,
    /// No packet for this long marks the node disconnected on the status page.
    #[serde(default = "default_idle_timeout_ms")]
    pub idle_timeout_ms: u32,
}

impl Default for SourceConfig {
    fn default() -> Self {
        SourceConfig {
            kind: SourceKind::default(),
            bind_address: default_source_bind_address(),
            port: default_source_port(),
            device_id: 0,
            jitter_buffer_ms: default_jitter_buffer_ms(),
            idle_timeout_ms: default_idle_timeout_ms(),
        }
    }
}

impl SourceConfig {
    fn validate(&self, errors: &mut Vec<String>) {
        if self.kind == SourceKind::Net {
            if self.bind_address.trim().is_empty() {
                errors.push("source.bind_address: must not be empty".to_string());
            } else if self.bind_address.parse::<std::net::IpAddr>().is_err() {
                errors.push(format!(
                    "source.bind_address: must be an IP address, got {:?}",
                    self.bind_address
                ));
            }
            if self.port == 0 {
                errors.push("source.port: must not be 0".to_string());
            }
            if !(20..=2_000).contains(&self.jitter_buffer_ms) {
                errors.push(format!(
                    "source.jitter_buffer_ms: must be within 20..=2000, got {}",
                    self.jitter_buffer_ms
                ));
            }
            if !(500..=120_000).contains(&self.idle_timeout_ms) {
                errors.push(format!(
                    "source.idle_timeout_ms: must be within 500..=120000, got {}",
                    self.idle_timeout_ms
                ));
            }
        }
    }
}

impl Default for ServerConfig {
    fn default() -> Self {
        ServerConfig {
            bind_address: default_bind_address(),
            port: default_port(),
            http_threads: default_http_threads(),
            timezone: String::new(),
            web_dir: default_web_dir(),
        }
    }
}

impl Default for SttConfig {
    fn default() -> Self {
        SttConfig {
            enabled: false,
            mode: default_stt_mode(),
            stream_command: default_stt_stream_command(),
            socket_path: String::new(),
            engine: default_stt_engine(),
            locale: default_stt_locale(),
            recognizer: Vec::new(),
            lanes: default_primary_lanes(),
        }
    }
}

impl SttConfig {
    fn validate(&self, errors: &mut Vec<String>) {
        // Refused whatever `enabled` says: a config that still names the old
        // mode is describing a pipeline this binary does not have, and starting
        // with it would record a silent night.
        if self.mode == SttMode::Segment {
            errors.push(
                "stt.mode = \"segment\" 已移除：每条录音存在的理由就是它产生了文本或事件，没有\"事后补给\"的对象"
                    .to_string(),
            );
            return;
        }
        if !self.enabled {
            return;
        }
        if self.stream_command.trim().is_empty() {
            errors.push(
                "stt.stream_command: must name a launcher when stt.mode = \"stream\"".to_string(),
            );
        }
        if self.engine.trim().is_empty() {
            errors.push("stt.engine: must be non-empty when stt.enabled is true".to_string());
        }
        if self.locale.trim().is_empty() {
            errors.push(
                "stt.locale: must name the recogniser language when stt.mode = \"stream\"".to_string(),
            );
        }
        if self.lanes.is_empty() {
            errors.push("stt.lanes: must list at least one of text, snore".to_string());
        }
        for lane in &self.lanes {
            if !KNOWN_LANES.contains(&lane.as_str()) {
                errors.push(format!("stt.lanes: unknown lane \"{lane}\" (known: text, snore)"));
            }
        }
        let mut names = vec!["primary".to_string()];
        for (i, r) in self.recognizer.iter().enumerate() {
            let at = format!("stt.recognizer[{i}]");
            if r.name.trim().is_empty() || !r.name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
                errors.push(format!("{at}.name: must be non-empty ascii letters, digits, '-' or '_'"));
            } else if names.contains(&r.name) {
                errors.push(format!("{at}.name: \"{}\" is already used", r.name));
            } else {
                names.push(r.name.clone());
            }
            if r.stream_command.trim().is_empty() {
                errors.push(format!("{at}.stream_command: must name a launcher"));
            }
            if r.lanes.is_empty() {
                errors.push(format!("{at}.lanes: must list at least one of text, snore"));
            }
            for lane in &r.lanes {
                if !KNOWN_LANES.contains(&lane.as_str()) {
                    errors.push(format!("{at}.lanes: unknown lane \"{lane}\" (known: text, snore)"));
                }
            }
        }
    }
}

impl Default for AudioConfig {
    fn default() -> Self {
        AudioConfig {
            alsa_device: String::new(),
            device_rate_hz: default_device_rate_hz(),
            device_channels: default_device_channels(),
            format: default_format(),
            period_ms: default_period_ms(),
            buffer_ms: default_buffer_ms(),
            segment_duration_seconds: default_segment_duration_seconds(),
            mono_channel_index: 0,
            rumble_highpass_hz: default_rumble_highpass_hz(),
            capture_ring_ms: default_capture_ring_ms(),
            recording_queue_ms: default_recording_queue_ms(),
            detection_queue_ms: default_detection_queue_ms(),
            gap_tolerance_ms: default_gap_tolerance_ms(),
            gap_rotate_seconds: default_gap_rotate_seconds(),
            header_flush_interval_seconds: default_header_flush_interval_seconds(),
            shutdown_timeout_seconds: default_shutdown_timeout_seconds(),
        }
    }
}

impl Default for DetectorConfig {
    fn default() -> Self {
        DetectorConfig {
            detection_rate_hz: default_detection_rate_hz(),
            frame_ms: default_frame_ms(),
            band_low_hz: default_band_low_hz(),
            band_high_hz: default_band_high_hz(),
            absolute_floor_dbfs: default_absolute_floor_dbfs(),
            noise_margin_db: default_noise_margin_db(),
            band_margin_db: default_band_margin_db(),
            band_ratio_min: default_band_ratio_min(),
            noise_floor_window_seconds: default_noise_floor_window_seconds(),
            noise_floor_percentile: default_noise_floor_percentile(),
            noise_floor_min_seconds: default_noise_floor_min_seconds(),
            noise_floor_reset_gap_seconds: default_noise_floor_reset_gap_seconds(),
            noise_freeze_max_seconds: default_noise_freeze_max_seconds(),
            filter_warmup_ms: default_filter_warmup_ms(),
            attack_window_frames: default_attack_window_frames(),
            attack_min_hits: default_attack_min_hits(),
            hangover_frames: default_hangover_frames(),
            min_event_ms: default_min_event_ms(),
            merge_gap_ms: default_merge_gap_ms(),
            max_event_seconds: default_max_event_seconds(),
        }
    }
}

fn default_bind_address() -> String {
    "127.0.0.1".to_string()
}
fn default_port() -> u16 {
    8080
}
fn default_source_bind_address() -> String {
    // Split deployments always need the LAN; loopback would make the feature
    // silently do nothing, which is worse than being reachable on a trusted net.
    "0.0.0.0".to_string()
}
fn default_source_port() -> u16 {
    8091
}
fn default_jitter_buffer_ms() -> u32 {
    150
}
fn default_idle_timeout_ms() -> u32 {
    5_000
}
fn default_http_threads() -> usize {
    2
}
fn default_web_dir() -> String {
    "web/dist".to_string()
}
fn default_stt_mode() -> SttMode {
    SttMode::Stream
}

fn default_stt_stream_command() -> String {
    "steno-stt-sidecar".to_string()
}

fn default_stt_locale() -> String {
    "zh-CN".to_string()
}

fn default_stt_engine() -> String {
    "apple-speechanalyzer".to_string()
}
fn default_device_rate_hz() -> u32 {
    16_000
}
fn default_device_channels() -> u16 {
    1
}
fn default_format() -> SampleFormat {
    SampleFormat::S16Le
}
fn default_period_ms() -> u32 {
    10
}
fn default_buffer_ms() -> u32 {
    200
}
fn default_segment_duration_seconds() -> u64 {
    3600
}
fn default_rumble_highpass_hz() -> u32 {
    120
}

fn default_capture_ring_ms() -> u32 {
    500
}
fn default_recording_queue_ms() -> u32 {
    2000
}
fn default_detection_queue_ms() -> u32 {
    1000
}
fn default_gap_tolerance_ms() -> u32 {
    20
}
fn default_gap_rotate_seconds() -> u64 {
    5
}
fn default_header_flush_interval_seconds() -> u64 {
    30
}
fn default_shutdown_timeout_seconds() -> u64 {
    10
}
fn default_detection_rate_hz() -> u32 {
    DETECTION_RATE_HZ
}
fn default_frame_ms() -> u32 {
    10
}
fn default_band_low_hz() -> f32 {
    80.0
}
fn default_band_high_hz() -> f32 {
    1500.0
}
fn default_absolute_floor_dbfs() -> f32 {
    -50.0
}
fn default_noise_margin_db() -> f32 {
    8.0
}
fn default_band_margin_db() -> f32 {
    6.0
}
fn default_band_ratio_min() -> f32 {
    0.35
}
fn default_noise_floor_window_seconds() -> u32 {
    30
}
fn default_noise_floor_percentile() -> u32 {
    20
}
fn default_noise_floor_min_seconds() -> u32 {
    10
}
fn default_noise_floor_reset_gap_seconds() -> u64 {
    60
}
fn default_noise_freeze_max_seconds() -> u32 {
    30
}
fn default_filter_warmup_ms() -> u32 {
    100
}
fn default_attack_window_frames() -> u32 {
    5
}
fn default_attack_min_hits() -> u32 {
    3
}
fn default_hangover_frames() -> u32 {
    50
}
fn default_min_event_ms() -> u32 {
    200
}
fn default_merge_gap_ms() -> u32 {
    800
}
fn default_max_event_seconds() -> u32 {
    15
}
fn default_recording_max_bytes() -> u64 {
    20 * 1024 * 1024 * 1024
}
fn default_recording_cleanup_target_bytes() -> u64 {
    19 * 1024 * 1024 * 1024
}
fn default_recording_reserve_bytes() -> u64 {
    512 * 1024 * 1024
}
fn default_retention_check_interval_seconds() -> u64 {
    60
}
fn default_systemctl_command() -> String {
    "systemctl".to_string()
}
fn default_server_unit() -> String {
    "steno-monitor".to_string()
}
fn default_node_unit() -> String {
    "steno-node".to_string()
}
fn default_control_timeout_seconds() -> u64 {
    15
}

impl Config {
    /// Loads, parses and validates the configuration file at `path`.
    pub fn load(path: &Path) -> Result<Config, ConfigLoadError> {
        let text = std::fs::read_to_string(path).map_err(|source| ConfigLoadError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        Config::from_toml_str(&text).map_err(|e| match e {
            ConfigLoadError::Parse { source, .. } => ConfigLoadError::Parse {
                path: path.to_path_buf(),
                source,
            },
            other => other,
        })
    }

    /// Parses and validates a TOML document that is already in memory.
    pub fn from_toml_str(text: &str) -> Result<Config, ConfigLoadError> {
        let config: Config = toml::from_str(text).map_err(|source| ConfigLoadError::Parse {
            path: PathBuf::from("<memory>"),
            source,
        })?;
        config.validate()?;
        Ok(config)
    }

    /// Path from `STENO_MONITOR_CONFIG`, falling back to the packaged default.
    pub fn resolve_path() -> PathBuf {
        match std::env::var(CONFIG_PATH_ENV) {
            Ok(value) if !value.trim().is_empty() => PathBuf::from(value),
            _ => PathBuf::from(DEFAULT_CONFIG_PATH),
        }
    }

    /// Loads the configuration named by the environment, or the default path.
    pub fn load_from_env() -> Result<Config, ConfigLoadError> {
        Config::load(&Config::resolve_path())
    }

    /// Runs every check listed in TECH_SPEC §11 and returns all failures.
    pub fn validate(&self) -> Result<(), ConfigError> {
        let mut errors = Vec::new();
        self.server.validate(&mut errors);
        self.audio.validate(&mut errors);
        self.source.validate(&mut errors);
        self.detector.validate(&mut errors);
        self.stt.validate(&mut errors);
        self.control
            .validate(&self.server.bind_address, &mut errors);
        self.storage.validate(&mut errors);
        if errors.is_empty() {
            Ok(())
        } else {
            Err(ConfigError(errors))
        }
    }
}

impl ServerConfig {
    fn validate(&self, errors: &mut Vec<String>) {
        if !(1..=8).contains(&self.http_threads) {
            errors.push(format!(
                "server.http_threads: must be within 1..=8, got {}",
                self.http_threads
            ));
        }
        if self.port == 0 {
            errors.push("server.port: must not be 0".to_string());
        }
        if self.bind_address.trim().is_empty() {
            errors.push("server.bind_address: must not be empty".to_string());
        } else if self.bind_address.parse::<std::net::IpAddr>().is_err() {
            errors.push(format!(
                "server.bind_address: must be an IP address, got {:?}",
                self.bind_address
            ));
        }
        if !self.timezone.is_empty() && self.timezone.parse::<chrono_tz::Tz>().is_err() {
            errors.push(format!(
                "server.timezone: must be an IANA timezone name or empty for the host timezone, got {:?}",
                self.timezone
            ));
        }
    }

    /// Resolved timezone, or `None` for "use the host's local time".
    pub fn timezone(&self) -> Option<chrono_tz::Tz> {
        if self.timezone.is_empty() {
            None
        } else {
            self.timezone.parse::<chrono_tz::Tz>().ok()
        }
    }
}

impl AudioConfig {
    fn validate(&self, errors: &mut Vec<String>) {
        if !(1..=2).contains(&self.device_channels) {
            errors.push(format!(
                "audio.device_channels: MVP supports 1 or 2 channels, got {}",
                self.device_channels
            ));
        }
        if self.mono_channel_index >= self.device_channels {
            errors.push(format!(
                "audio.mono_channel_index: must be < audio.device_channels ({}), got {}",
                self.device_channels, self.mono_channel_index
            ));
        }
        if self.rumble_highpass_hz > 400 {
            errors.push(format!(
                "audio.rumble_highpass_hz: must be 0..=400 (0 disables), got {}",
                self.rumble_highpass_hz
            ));
        }
        if !SUPPORTED_CAPTURE_RATES.contains(&self.device_rate_hz) {
            errors.push(format!(
                "audio.device_rate_hz: must be one of {:?} (TECH_SPEC §5.1), got {}; this is a hint for the device, the wire is fixed at {WIRE_RATE_HZ} Hz",
                SUPPORTED_CAPTURE_RATES, self.device_rate_hz
            ));
        }
        if self.period_ms == 0 {
            errors.push("audio.period_ms: must be > 0".to_string());
        }
        if self.buffer_ms == 0 {
            errors.push("audio.buffer_ms: must be > 0".to_string());
        }
        if self.gap_tolerance_ms == 0 {
            errors.push(
                "audio.gap_tolerance_ms: must be > 0, otherwise every block boundary registers a gap"
                    .to_string(),
            );
        }
        let two_periods = self.period_ms.saturating_mul(2);
        for (name, value) in [
            ("audio.capture_ring_ms", self.capture_ring_ms),
            ("audio.recording_queue_ms", self.recording_queue_ms),
            ("audio.detection_queue_ms", self.detection_queue_ms),
        ] {
            if value < two_periods {
                errors.push(format!(
                    "{name}: must be at least 2 periods ({two_periods} ms), got {value}"
                ));
            }
        }
        if self.segment_duration_seconds == 0 {
            errors.push("audio.segment_duration_seconds: must be > 0".to_string());
        }
        let segment_bytes = self.max_segment_bytes();
        if segment_bytes >= RIFF_MAX_BYTES {
            errors.push(format!(
                "audio.segment_duration_seconds: {} s at the wire format ({} Hz x {} ch x 2 B) is {} bytes, which does not fit classic RIFF (< {} bytes)",
                self.segment_duration_seconds,
                WIRE_RATE_HZ,
                WIRE_CHANNELS,
                segment_bytes,
                RIFF_MAX_BYTES
            ));
        }
        if self.header_flush_interval_seconds == 0 {
            errors.push("audio.header_flush_interval_seconds: must be > 0".to_string());
        }
        if self.shutdown_timeout_seconds == 0 {
            errors.push("audio.shutdown_timeout_seconds: must be > 0".to_string());
        }
    }

    /// Byte size of one full segment of raw PCM, before the WAV header.
    ///
    /// Computed from the wire format, not from the device hint: the WAV on disk
    /// is always [`WIRE_RATE_HZ`] × [`WIRE_CHANNELS`], because the conversion
    /// happens before anything is recorded.
    pub fn max_segment_bytes(&self) -> u64 {
        u64::from(WIRE_RATE_HZ)
            * u64::from(WIRE_CHANNELS)
            * self.format.bytes_per_sample()
            * self.segment_duration_seconds
    }

    /// Frame count that triggers a `duration` rotation (TECH_SPEC §4.3).
    pub fn segment_frames(&self) -> u64 {
        u64::from(WIRE_RATE_HZ) * self.segment_duration_seconds
    }

    /// Frames held by the SPSC capture ring.
    pub fn capture_ring_frames(&self) -> usize {
        (u64::from(WIRE_RATE_HZ) * u64::from(self.capture_ring_ms) / 1000) as usize
    }

    /// Bytes per frame of interleaved PCM as stored on disk.
    pub fn block_align(&self) -> u64 {
        u64::from(WIRE_CHANNELS) * self.format.bytes_per_sample()
    }
}

impl DetectorConfig {
    fn validate(&self, errors: &mut Vec<String>) {
        if self.detection_rate_hz != DETECTION_RATE_HZ {
            errors.push(format!(
                "detector.detection_rate_hz: MVP fixes this at {DETECTION_RATE_HZ}, got {}",
                self.detection_rate_hz
            ));
        }
        if self.frame_ms == 0 || !(self.detection_rate_hz * self.frame_ms).is_multiple_of(1000) {
            errors.push(format!(
                "detector.frame_ms: {} ms at {} Hz is not a whole number of samples",
                self.frame_ms, self.detection_rate_hz
            ));
        }
        if self.band_low_hz <= 0.0 || !self.band_low_hz.is_finite() {
            errors.push(format!(
                "detector.band_low_hz: must be a positive frequency, got {}",
                self.band_low_hz
            ));
        }
        let nyquist = self.detection_rate_hz as f32 / 2.0;
        if self.band_high_hz >= nyquist || !self.band_high_hz.is_finite() {
            errors.push(format!(
                "detector.band_high_hz: must stay below the {nyquist} Hz Nyquist limit, got {}",
                self.band_high_hz
            ));
        }
        if self.band_low_hz >= self.band_high_hz {
            errors.push(format!(
                "detector.band_low_hz: must be < detector.band_high_hz ({}), got {}",
                self.band_high_hz, self.band_low_hz
            ));
        }
        if !self.absolute_floor_dbfs.is_finite() || self.absolute_floor_dbfs > -1.0 {
            errors.push(format!(
                "detector.absolute_floor_dbfs: must be a finite dBFS value well below full scale, got {}",
                self.absolute_floor_dbfs
            ));
        }
        for (name, value) in [
            ("detector.noise_margin_db", self.noise_margin_db),
            ("detector.band_margin_db", self.band_margin_db),
        ] {
            if !value.is_finite() || value <= 0.0 {
                errors.push(format!(
                    "{name}: must be a positive number of dB, got {value}"
                ));
            }
        }
        if !self.band_ratio_min.is_finite() || !(0.0..=1.0).contains(&self.band_ratio_min) {
            errors.push(format!(
                "detector.band_ratio_min: must be within 0.0..=1.0, got {}",
                self.band_ratio_min
            ));
        }
        if self.noise_floor_percentile < 1 || self.noise_floor_percentile > 50 {
            errors.push(format!(
                "detector.noise_floor_percentile: must be within 1..=50, got {}",
                self.noise_floor_percentile
            ));
        }
        if self.noise_floor_min_seconds > self.noise_floor_window_seconds {
            errors.push(format!(
                "detector.noise_floor_min_seconds: must be <= detector.noise_floor_window_seconds ({}), got {}",
                self.noise_floor_window_seconds, self.noise_floor_min_seconds
            ));
        }
        if self.noise_floor_window_seconds == 0 {
            errors.push("detector.noise_floor_window_seconds: must be > 0".to_string());
        }
        if self.attack_window_frames == 0 {
            errors.push("detector.attack_window_frames: must be > 0".to_string());
        }
        if self.attack_min_hits == 0 || self.attack_min_hits > self.attack_window_frames {
            errors.push(format!(
                "detector.attack_min_hits: must be within 1..=detector.attack_window_frames ({}), got {}",
                self.attack_window_frames, self.attack_min_hits
            ));
        }
        if self.hangover_frames == 0 {
            errors.push("detector.hangover_frames: must be >= 1".to_string());
        }
        if self.min_event_ms == 0 {
            errors.push("detector.min_event_ms: must be > 0".to_string());
        }
        if u64::from(self.max_event_seconds) * 1000 <= u64::from(self.min_event_ms) {
            errors.push(format!(
                "detector.max_event_seconds: {} s must be longer than detector.min_event_ms ({} ms)",
                self.max_event_seconds, self.min_event_ms
            ));
        }
        if self.filter_warmup_ms > 0 && self.frame_ms == 0 {
            errors.push(
                "detector.filter_warmup_ms: cannot be applied without a frame length".to_string(),
            );
        }
    }

    /// Runs the service's own `[detector]` validation on an in-memory value.
    ///
    /// The effective fields are serialized into the smallest TOML document
    /// `Config::from_toml_str` accepts and parsed back through it, so offline
    /// tools reject exactly what startup would reject instead of keeping a second
    /// copy of the rules that could drift (tasks.md T2.7, T8.6).
    pub fn validate_standalone(&self) -> Result<(), String> {
        let document = format!(
            "[detector]\ndetection_rate_hz = {}\nframe_ms = {}\nband_low_hz = {}\nband_high_hz = {}\nabsolute_floor_dbfs = {}\nnoise_margin_db = {}\nband_margin_db = {}\nband_ratio_min = {}\nnoise_floor_window_seconds = {}\nnoise_floor_percentile = {}\nnoise_floor_min_seconds = {}\nnoise_floor_reset_gap_seconds = {}\nnoise_freeze_max_seconds = {}\nfilter_warmup_ms = {}\nattack_window_frames = {}\nattack_min_hits = {}\nhangover_frames = {}\nmin_event_ms = {}\nmerge_gap_ms = {}\nmax_event_seconds = {}\n[storage]\nrecording_dir = \"/tmp/steno-monitor-validate\"\ndatabase_path = \"/tmp/steno-monitor-validate/app.db\"\n",
            self.detection_rate_hz,
            self.frame_ms,
            self.band_low_hz,
            self.band_high_hz,
            self.absolute_floor_dbfs,
            self.noise_margin_db,
            self.band_margin_db,
            self.band_ratio_min,
            self.noise_floor_window_seconds,
            self.noise_floor_percentile,
            self.noise_floor_min_seconds,
            self.noise_floor_reset_gap_seconds,
            self.noise_freeze_max_seconds,
            self.filter_warmup_ms,
            self.attack_window_frames,
            self.attack_min_hits,
            self.hangover_frames,
            self.min_event_ms,
            self.merge_gap_ms,
            self.max_event_seconds,
        );
        Config::from_toml_str(&document)
            .map(|_| ())
            .map_err(|error| match error {
                ConfigLoadError::Invalid(invalid) => format!(
                    "detection parameters are not usable:\n  {}\n(startup runs the same validation, so this would fail there too)",
                    invalid.0.join("\n  ")
                ),
                other => format!("detection parameters are not usable: {other}"),
            })
    }

    /// Samples per detection frame.
    pub fn frame_samples(&self) -> usize {
        (self.detection_rate_hz * self.frame_ms / 1000) as usize
    }

    /// Frames skipped after a reset while the IIR chain settles (TECH_SPEC §5.3).
    pub fn warmup_frames(&self) -> u64 {
        u64::from(self.filter_warmup_ms).div_ceil(u64::from(self.frame_ms.max(1)))
    }

    /// Size of the noise-floor ring window in frames.
    pub fn noise_floor_window_frames(&self) -> usize {
        (self.noise_floor_window_seconds as usize) * 1000 / self.frame_ms.max(1) as usize
    }

    /// Minimum number of frames before the noise floor is considered known.
    pub fn noise_floor_min_frames(&self) -> usize {
        (self.noise_floor_min_seconds as usize) * 1000 / self.frame_ms.max(1) as usize
    }
}

impl StorageConfig {
    fn validate(&self, errors: &mut Vec<String>) {
        for (name, path) in [
            ("storage.recording_dir", &self.recording_dir),
            ("storage.database_path", &self.database_path),
        ] {
            if !path.is_absolute() {
                errors.push(format!(
                    "{name}: must be an absolute path, got {}",
                    path.display()
                ));
            }
        }
        if self.recording_max_bytes == 0 {
            errors.push("storage.recording_max_bytes: must be > 0".to_string());
        }
        if self.recording_cleanup_target_bytes >= self.recording_max_bytes {
            errors.push(format!(
                "storage.recording_cleanup_target_bytes: must be < storage.recording_max_bytes ({}), got {}",
                self.recording_max_bytes, self.recording_cleanup_target_bytes
            ));
        }
        if self.retention_check_interval_seconds == 0 {
            errors.push("storage.retention_check_interval_seconds: must be > 0".to_string());
        }
    }

    /// Total bytes a single recording may occupy, used by the 4 GiB guard.
    pub fn reserve_bytes(&self) -> u64 {
        self.recording_reserve_bytes
    }
}

#[cfg(test)]
mod control_tests {
    use super::*;

    fn document(extra: &str) -> String {
        format!(
            "[storage]\nrecording_dir = \"/tmp/steno-control-test\"\ndatabase_path = \"/tmp/steno-control-test/app.db\"\n{extra}"
        )
    }

    #[test]
    fn control_is_off_by_default_and_needs_no_loopback_check() {
        let config = Config::from_toml_str(&document(""))
            .expect("a config without [control] must still validate");
        assert!(!config.control.enabled);
        assert_eq!(config.control.server_unit, "steno-monitor");
        assert_eq!(config.control.node_unit, "steno-node");
    }

    #[test]
    fn a_second_recogniser_is_accepted_and_listed_after_primary() {
        let config = Config::from_toml_str(&document(
            "[stt]\nenabled = true\n[[stt.recognizer]]\nname = \"apple\"\nstream_command = \"steno-stt-sidecar\"\nlanes = [\"snore\"]\n",
        ))
        .expect("a named extra recogniser must validate");
        let all = config.stt.recognizers();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].name, "primary");
        assert_eq!(all[1].lanes, vec!["snore".to_string()]);
    }

    #[test]
    fn a_bad_recogniser_entry_is_refused_with_its_key_named() {
        let error = Config::from_toml_str(&document(
            "[stt]\nenabled = true\n[[stt.recognizer]]\nname = \"primary\"\nstream_command = \"x\"\nlanes = [\"video\"]\n",
        ))
        .expect_err("duplicate name and unknown lane must not validate");
        let message = error.to_string();
        assert!(message.contains("stt.recognizer[0].name"), "got: {message}");
        assert!(message.contains("unknown lane"), "got: {message}");
    }

    #[test]
    fn enabling_control_on_a_loopback_bind_is_accepted() {
        let config = Config::from_toml_str(&document(
            "[control]\nenabled = true\n",
        ))
        .expect("loopback control must validate");
        assert!(config.control.enabled);
    }

    #[test]
    fn enabling_control_on_a_lan_bind_is_refused_by_name() {
        let error = Config::from_toml_str(&document(
            "[server]\nbind_address = \"0.0.0.0\"\n[control]\nenabled = true\n",
        ))
        .expect_err("a LAN-reachable kill switch must not validate");
        let message = error.to_string();
        assert!(
            message.contains("control.enabled"),
            "the offending key must be named, got: {message}"
        );
    }

    #[test]
    fn a_zero_control_timeout_is_refused() {
        let error = Config::from_toml_str(&document(
            "[control]\nenabled = true\ntimeout_seconds = 0\n",
        ))
        .expect_err("a zero timeout must not validate");
        assert!(error.to_string().contains("control.timeout_seconds"));
    }
}

#[cfg(test)]
mod stt_tests {
    use super::*;

    fn document(stt: &str) -> String {
        format!(
            "[storage]\nrecording_dir = \"/tmp/steno-stt-test\"\ndatabase_path = \"/tmp/steno-stt-test/app.db\"\n{stt}"
        )
    }

    #[test]
    fn segment_mode_is_refused_by_name_even_when_disabled() {
        let error = Config::from_toml_str(&document("[stt]\nmode = \"segment\"\n"))
            .expect_err("the removed mode must not load");
        let message = error.to_string();
        assert!(
            message.contains("stt.mode = \"segment\" 已移除"),
            "the refusal must say what happened, got: {message}"
        );
    }

    #[test]
    fn streaming_still_needs_a_launcher_and_a_locale() {
        let error = Config::from_toml_str(&document(
            "[stt]\nenabled = true\nstream_command = \"\"\nlocale = \"\"\n",
        ))
        .expect_err("an enabled recogniser with no launcher must not validate");
        let message = error.to_string();
        assert!(message.contains("stt.stream_command"), "got: {message}");
        assert!(message.contains("stt.locale"), "got: {message}");
    }
}
