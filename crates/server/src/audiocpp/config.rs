//! What the sidecar needs from this machine, and how it refuses to guess.
//!
//! ## Why the model is not on the wire
//!
//! The socket carries audio, text and session state; it deliberately carries no
//! `--model`. Which GGUF is loaded is a local fact about the host — switching
//! engines means switching files on disk — and the previous deployment expressed
//! it through a shell wrapper plus a `.conf` file that `install.sh` rewrote. That
//! indirection produced exactly one memorable bug: a reinstall without the
//! environment variable replaced the path with an empty string, and `/status`
//! showed only `launcher exited with exit status: 2`.
//!
//! So the same three facts come from the environment, and every one of them is
//! validated here, before the engine is spawned. A recogniser that starts
//! without weights prints `ready` and then produces nothing all night, which in
//! the database is indistinguishable from a quiet room.
//!
//! The **language** is not here: it arrives on the wire, because it is the
//! server's decision (`stt.locale`) and two sources for one value is how they
//! drift apart.

use std::path::{Path, PathBuf};

use steno_lib::config::WIRE_RATE_HZ;

/// The `engine` label on `ready` and on every stored utterance.
///
/// Deliberately the same string the Swift backend used: rows written before and
/// after this port must read the same, or a mixed history becomes unreadable.
pub const ENGINE_LABEL: &str = "audiocpp-qwen3-asr";

pub const DEFAULT_CHUNK_SECONDS: f64 = 5.0;
pub const MIN_CHUNK_SECONDS: f64 = 1.0;
pub const MAX_CHUNK_SECONDS: f64 = 30.0;

/// The language codes the Qwen3-ASR family accepts, mirrored from the model
/// spec. Enforced here because the engine does not enforce it: `--language xx-YY`
/// is accepted for anything and still produces a transcript, just not in that
/// language. A typo would otherwise look like a working session.
pub const SUPPORTED_LANGUAGES: [&str; 30] = [
    "zh", "en", "yue", "ar", "de", "fr", "es", "pt", "id", "it", "ko", "ru", "th", "vi", "ja",
    "tr", "hi", "ms", "nl", "sv", "da", "fi", "pl", "cs", "fil", "fa", "el", "hu", "mk", "ro",
];

#[derive(Debug, Clone)]
pub struct SidecarConfig {
    pub socket: PathBuf,
    pub model: PathBuf,
    pub chunk_seconds: f64,
    pub binary: PathBuf,
}

impl SidecarConfig {
    /// Window length in frames. The engine works on its own grid of this width;
    /// every reported line belongs to exactly one cell of it.
    pub fn chunk_frames(&self) -> u64 {
        let frames = self.chunk_seconds * f64::from(WIRE_RATE_HZ);
        if frames.is_finite() && frames >= 1.0 {
            frames.round() as u64
        } else {
            1
        }
    }

    /// `--audio-chunk-seconds` as the engine wants to read it: `5`, not `5.0`.
    pub fn chunk_argument(&self) -> String {
        if (self.chunk_seconds.fract()).abs() < f64::EPSILON {
            format!("{}", self.chunk_seconds as i64)
        } else {
            format!("{}", self.chunk_seconds)
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("STENO_STT_MODEL is not set: a recogniser without weights reports ready and then produces nothing all night")]
    ModelUnset,
    #[error("STENO_STT_MODEL={0} is neither a .gguf file nor a directory")]
    ModelMissing(String),
    #[error("STENO_STT_MODEL={0} holds no .gguf file")]
    ModelEmpty(String),
    #[error("STENO_STT_MODEL={dir} holds {count} .gguf files ({files}); pass one of them directly — the engine refuses to pick")]
    ModelAmbiguous {
        dir: String,
        count: usize,
        files: String,
    },
    #[error("STENO_STT_CHUNK={0} is not a number of seconds")]
    ChunkNotANumber(String),
    #[error("STENO_STT_CHUNK={0} is outside {MIN_CHUNK_SECONDS}..{MAX_CHUNK_SECONDS}: the engine's fixed chunker needs a positive window, and past 30 s it is a segment, not a window")]
    ChunkOutOfRange(f64),
    #[error("audiocpp binary {0} is not executable")]
    BinaryMissing(String),
    #[error("locale {asked} has no Qwen3-ASR language code; use one of {supported} (the engine accepts any string and silently ignores it, which is why it is refused here)", supported = SUPPORTED_LANGUAGES.join(", "))]
    LanguageUnsupported { asked: String },
}

/// Reads and validates everything the engine needs. `socket` is the one
/// argument the launch contract passes.
pub fn from_env(socket: PathBuf) -> Result<SidecarConfig, ConfigError> {
    let model = resolve_model(&model_setting())?;
    let chunk_seconds = chunk_setting()?;
    let binary = resolve_binary(&binary_setting())?;
    Ok(SidecarConfig {
        socket,
        model,
        chunk_seconds,
        binary,
    })
}

fn model_setting() -> String {
    std::env::var("STENO_STT_MODEL").unwrap_or_default()
}

fn binary_setting() -> String {
    let value = std::env::var("STENO_STT_AUDIOCPP_BIN").unwrap_or_default();
    let trimmed = value.trim();
    if trimmed.is_empty() {
        "audiocpp_cli".to_string()
    } else {
        trimmed.to_string()
    }
}

fn chunk_setting() -> Result<f64, ConfigError> {
    let raw = std::env::var("STENO_STT_CHUNK").unwrap_or_default();
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(DEFAULT_CHUNK_SECONDS);
    }
    let value: f64 = trimmed
        .parse()
        .map_err(|_| ConfigError::ChunkNotANumber(trimmed.to_string()))?;
    if !value.is_finite() || !(MIN_CHUNK_SECONDS..=MAX_CHUNK_SECONDS).contains(&value) {
        return Err(ConfigError::ChunkOutOfRange(value));
    }
    Ok(value)
}

/// Resolves `STENO_STT_MODEL` to the one `.gguf` the engine will load.
///
/// A directory is accepted only while it is unambiguous. The engine itself
/// refuses a directory holding two packages ("pass one of them directly"), and
/// discovering that after spawning means a session that reports no text at all
/// for reasons the server can only see as an engine message — so the same
/// refusal happens here, before the child exists.
pub fn resolve_model(value: &str) -> Result<PathBuf, ConfigError> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(ConfigError::ModelUnset);
    }
    let path = PathBuf::from(trimmed);
    if path.is_file() {
        return if trimmed.ends_with(".gguf") {
            Ok(path)
        } else {
            Err(ConfigError::ModelMissing(trimmed.to_string()))
        };
    }
    if !path.is_dir() {
        return Err(ConfigError::ModelMissing(trimmed.to_string()));
    }
    let mut found: Vec<PathBuf> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&path) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            if name.to_string_lossy().ends_with(".gguf") {
                found.push(entry.path());
            }
        }
    }
    match found.len() {
        0 => Err(ConfigError::ModelEmpty(trimmed.to_string())),
        1 => Ok(found.remove(0)),
        count => {
            found.sort();
            let names: Vec<String> = found
                .iter()
                .filter_map(|path| path.file_name())
                .map(|name| name.to_string_lossy().to_string())
                .collect();
            Err(ConfigError::ModelAmbiguous {
                dir: trimmed.to_string(),
                count,
                files: names.join(", "),
            })
        }
    }
}

/// Either an absolute/relative path with a slash, or a name looked up on `PATH`.
pub fn resolve_binary(value: &str) -> Result<PathBuf, ConfigError> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(ConfigError::BinaryMissing(value.to_string()));
    }
    if trimmed.contains('/') {
        let path = PathBuf::from(trimmed);
        return if is_executable(&path) {
            Ok(path)
        } else {
            Err(ConfigError::BinaryMissing(trimmed.to_string()))
        };
    }
    let search = std::env::var("PATH").unwrap_or_else(|_| "/usr/local/bin:/usr/bin:/bin".to_string());
    for directory in search.split(':').filter(|entry| !entry.is_empty()) {
        let candidate = Path::new(directory).join(trimmed);
        if is_executable(&candidate) {
            return Ok(candidate);
        }
    }
    Err(ConfigError::BinaryMissing(trimmed.to_string()))
}

fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        match std::fs::metadata(path) {
            Ok(metadata) => metadata.is_file() && metadata.permissions().mode() & 0o111 != 0,
            Err(_) => false,
        }
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

/// `"auto"` means "omit `--language` and let the model detect". Everything else
/// must reduce to a code the family knows.
pub fn resolve_language(locale: &str) -> Result<String, ConfigError> {
    let trimmed = locale.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("auto") {
        return Ok("auto".to_string());
    }
    let primary = trimmed
        .split('-')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    if SUPPORTED_LANGUAGES.contains(&primary.as_str()) {
        Ok(primary)
    } else {
        Err(ConfigError::LanguageUnsupported {
            asked: trimmed.to_string(),
        })
    }
}

/// The `--check` report: every fact a human needs to tell "misconfigured" from
/// "quiet", collected rather than short-circuited, so one broken path does not
/// hide the others.
pub fn check_report() -> (String, bool) {
    let mut problems: Vec<String> = Vec::new();

    let (binary, version) = match resolve_binary(&binary_setting()) {
        Ok(path) => {
            let version = binary_version(&path);
            (Some(path), version)
        }
        Err(error) => {
            problems.push(error.to_string());
            (None, None)
        }
    };

    let (model, bytes) = match resolve_model(&model_setting()) {
        Ok(path) => {
            let bytes = std::fs::metadata(&path).ok().map(|metadata| metadata.len());
            (Some(path), bytes)
        }
        Err(error) => {
            problems.push(error.to_string());
            (None, None)
        }
    };

    let chunk = match chunk_setting() {
        Ok(value) => Some(value),
        Err(error) => {
            problems.push(error.to_string());
            None
        }
    };

    let available = problems.is_empty();
    let report = serde_json::json!({
        "engine": ENGINE_LABEL,
        "available": available,
        "binary": binary.as_ref().map(|path| path.display().to_string()),
        "version": version,
        "model": model.as_ref().map(|path| path.display().to_string()),
        "model_bytes": bytes,
        "chunk_seconds": chunk,
        "capabilities": chunk.map(|value| format!("{value} s windows, partials off")),
        "reason": if available { None } else { Some(problems.join("; ")) },
    });
    (report.to_string(), available)
}

fn binary_version(binary: &Path) -> Option<String> {
    let output = std::process::Command::new(binary)
        .arg("--version")
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_locale_reduces_to_its_primary_subtag() {
        assert_eq!(resolve_language("zh-CN").unwrap(), "zh");
        assert_eq!(resolve_language("YUE-hant").unwrap(), "yue");
        assert_eq!(resolve_language("auto").unwrap(), "auto");
        assert_eq!(resolve_language("").unwrap(), "auto");
    }

    #[test]
    fn an_unsupported_locale_is_refused_rather_than_passed_through() {
        assert!(matches!(
            resolve_language("eo"),
            Err(ConfigError::LanguageUnsupported { .. })
        ));
    }

    #[test]
    fn a_missing_model_directory_is_a_named_error() {
        assert!(matches!(
            resolve_model("/nonexistent/qwen3_asr"),
            Err(ConfigError::ModelMissing(_))
        ));
        assert!(matches!(resolve_model("  "), Err(ConfigError::ModelUnset)));
    }

    #[test]
    fn chunk_frames_follow_the_wire_rate() {
        let config = SidecarConfig {
            socket: PathBuf::from("/tmp/sock"),
            model: PathBuf::from("/tmp/model"),
            chunk_seconds: 5.0,
            binary: PathBuf::from("/tmp/audiocpp_cli"),
        };
        assert_eq!(config.chunk_frames(), 80_000);
        assert_eq!(config.chunk_argument(), "5");
    }
}
