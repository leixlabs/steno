//! Speech-to-text for finished segments.
//!
//! ## Why a trait and not "just call Apple"
//!
//! Speech recognition is the one part of this system that cannot be written
//! once and forgotten: Apple's `SFSpeechRecognizer` only exists on Apple
//! platforms, `whisper.cpp` needs a multi-gigabyte model, and hosted ASR costs
//! money per minute and needs the audio to leave the LAN. All three are
//! reasonable; which one is right depends on where the server runs.
//!
//! So the pipeline depends on [`Transcriber`] and nothing else. Adding whisper
//! later means writing one more impl, not touching the recorder, the database or
//! the API.
//!
//! ## Why an external process
//!
//! The Apple backend is a small Swift sidecar (`apple-stt-sidecar`) rather
//! than FFI. Speech recognition is one platform's framework, it changes with
//! every OS release, and it must not drag a macOS-only build dependency into a
//! server that is also expected to run on Linux. A separate process is also the
//! only way to pick a *different* backend later without rebuilding this one.

use std::path::Path;
use std::process::Command;

use serde::{Deserialize, Serialize};

/// One recognised phrase, positioned relative to the start of the segment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscriptSegment {
    pub start_ms: i64,
    pub end_ms: i64,
    pub text: String,
    #[serde(default)]
    pub confidence: Option<f32>,
}

/// The result of transcribing one recording segment.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Transcript {
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub segments: Vec<TranscriptSegment>,
    /// Which engine produced this text, stored with the row so that a mixed
    /// history stays interpretable after a backend change.
    #[serde(default)]
    pub engine: String,
    #[serde(default)]
    pub language: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum SttError {
    #[error("speech-to-text is disabled")]
    Disabled,
    #[error("failed to run transcriber {program}: {source}")]
    Spawn {
        program: String,
        #[source]
        source: std::io::Error,
    },
    #[error("transcriber {program} exited with {status}: {stderr}")]
    Failed {
        program: String,
        status: String,
        stderr: String,
    },
    #[error("transcriber returned unparsable JSON: {0}")]
    Parse(String),
    #[error("transcriber produced no output")]
    Empty,
}

pub type SttResult<T> = std::result::Result<T, SttError>;

/// Anything that can turn a WAV file into text.
///
/// Implementations must be safe to call from a worker thread; the pipeline calls
/// them one segment at a time.
pub trait Transcriber: Send + Sync {
    /// Stable identifier stored alongside each transcript.
    fn engine(&self) -> &str;

    /// Transcribe `wav`, which is always 16 kHz mono PCM written by the recorder.
    fn transcribe(&self, wav: &Path) -> SttResult<Transcript>;
}

/// Backend that shells out to an external recogniser.
///
/// The contract is deliberately tiny so that the backend can be written in any
/// language: run `program <wav>`, print one JSON object on stdout. That is
/// exactly what `apple-stt-sidecar` does, and it is also what a whisper
/// wrapper would do.
pub struct CommandTranscriber {
    program: String,
    extra_args: Vec<String>,
    engine: String,
}

impl CommandTranscriber {
    pub fn new(program: impl Into<String>, extra_args: Vec<String>, engine: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            extra_args,
            engine: engine.into(),
        }
    }
}

impl Transcriber for CommandTranscriber {
    fn engine(&self) -> &str {
        &self.engine
    }

    fn transcribe(&self, wav: &Path) -> SttResult<Transcript> {
        let output = Command::new(&self.program)
            .args(&self.extra_args)
            .arg(wav)
            .output()
            .map_err(|source| SttError::Spawn {
                program: self.program.clone(),
                source,
            })?;

        if !output.status.success() {
            return Err(SttError::Failed {
                program: self.program.clone(),
                status: output.status.to_string(),
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            });
        }
        if output.stdout.is_empty() {
            return Err(SttError::Empty);
        }
        let mut transcript = parse_transcript(&output.stdout)?;
        if transcript.engine.is_empty() {
            transcript.engine = self.engine.clone();
        }
        Ok(transcript)
    }
}

/// Parse the sidecar's stdout.
///
/// Kept separate from process handling so the wire shape can be tested without
/// spawning anything.
pub fn parse_transcript(raw: &[u8]) -> SttResult<Transcript> {
    serde_json::from_slice::<Transcript>(raw).map_err(|error| SttError::Parse(error.to_string()))
}

/// Placeholder used when transcription is turned off; it keeps call sites free
/// of `Option` handling while making the disabled state explicit in the UI.
pub struct DisabledTranscriber;

impl Transcriber for DisabledTranscriber {
    fn engine(&self) -> &str {
        "disabled"
    }

    fn transcribe(&self, _wav: &Path) -> SttResult<Transcript> {
        Err(SttError::Disabled)
    }
}

// ---------------------------------------------------------------------------
// Worker
// ---------------------------------------------------------------------------

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use chrono::Utc;

use crate::storage::{Database, TranscriptRow};

/// Everything the worker needs that does not change between passes.
#[derive(Debug, Clone)]
pub struct WorkerOptions {
    pub database_path: PathBuf,
    pub recording_dir: PathBuf,
    /// Engine name written even for failures, so a broken backend is visible.
    pub engine: String,
    pub min_segment_seconds: f64,
    pub poll_interval: Duration,
    pub batch: usize,
}

/// Starts the transcription loop on its own thread.
///
/// The worker is deliberately the dumbest possible consumer: it asks the
/// database for finished segments without a transcript, transcribes one, writes
/// the row, repeats. There is no in-memory queue, so:
///
/// * a crash or a restart loses nothing — the unfinished work is still a query;
/// * a slow recogniser never applies backpressure to recording;
/// * ordering is by segment start time, so a backlog drains oldest-first.
pub fn spawn(
    transcriber: Arc<dyn Transcriber>,
    options: WorkerOptions,
    shutdown: Arc<AtomicBool>,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name("stt-worker".to_string())
        .spawn(move || run(transcriber, options, shutdown))
}

fn run(transcriber: Arc<dyn Transcriber>, options: WorkerOptions, shutdown: Arc<AtomicBool>) {
    let mut database: Option<Database> = None;
    while !shutdown.load(Ordering::Relaxed) {
        if database.is_none() {
            match Database::open(&options.database_path) {
                Ok(db) => {
                    tracing::info!(
                        engine = transcriber.engine(),
                        "transcription worker attached"
                    );
                    database = Some(db);
                }
                Err(error) => {
                    tracing::warn!(%error, "transcription worker cannot open the database yet");
                }
            }
        }

        if let Some(db) = database.as_ref() {
            match drain_once(db, transcriber.as_ref(), &options) {
                Ok(0) => {}
                Ok(count) => tracing::debug!(count, "transcribed segments"),
                Err(error) => {
                    tracing::warn!(%error, "transcription pass failed; reopening the database");
                    database = None;
                }
            }
        }

        // Sleep in short slices so shutdown is not delayed by a long interval.
        let deadline = std::time::Instant::now() + options.poll_interval;
        while std::time::Instant::now() < deadline {
            if shutdown.load(Ordering::Relaxed) {
                return;
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    }
}

fn drain_once(
    database: &Database,
    transcriber: &dyn Transcriber,
    options: &WorkerOptions,
) -> Result<usize, steno_lib::error::Error> {
    let pending = database
        .segments_awaiting_transcript(options.min_segment_seconds, options.batch)?;
    let mut done = 0usize;
    for item in pending {
        let wav = options.recording_dir.join(&item.relative_path);
        let row = if !wav.is_file() {
            // The retention pass may have deleted the file between the query and
            // the decode. Record it so the row is not retried forever.
            failed_row(&item.segment_id, options, "audio file is missing", item.audio_seconds)
        } else {
            match transcriber.transcribe(&wav) {
                Ok(transcript) => TranscriptRow {
                    segment_id: item.segment_id.clone(),
                    engine: if transcript.engine.is_empty() {
                        options.engine.clone()
                    } else {
                        transcript.engine.clone()
                    },
                    language: transcript.language.clone(),
                    text: transcript.text,
                    segments_json: serde_json::to_string(&transcript.segments)
                        .unwrap_or_else(|_| "[]".to_string()),
                    status: "ok".to_string(),
                    error: None,
                    audio_seconds: Some(item.audio_seconds),
                    created_at_utc: Utc::now(),
                },
                Err(error) => {
                    tracing::warn!(segment = %item.segment_id, %error, "transcription failed");
                    failed_row(&item.segment_id, options, &error.to_string(), item.audio_seconds)
                }
            }
        };
        database.save_transcript(&row)?;
        done += 1;
    }
    Ok(done)
}

fn failed_row(
    segment_id: &str,
    options: &WorkerOptions,
    error: &str,
    audio_seconds: f64,
) -> TranscriptRow {
    TranscriptRow {
        segment_id: segment_id.to_string(),
        engine: options.engine.clone(),
        language: None,
        text: String::new(),
        segments_json: "[]".to_string(),
        status: "failed".to_string(),
        error: Some(error.to_string()),
        audio_seconds: Some(audio_seconds),
        created_at_utc: Utc::now(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sidecar_output() {
        let raw = r#"{"text":"打呼了","engine":"apple-sfspeech","segments":[{"start_ms":0,"end_ms":900,"text":"打呼了","confidence":0.82}]}"#;
        let transcript = parse_transcript(raw.as_bytes()).expect("valid payload");
        assert_eq!(transcript.text, "打呼了");
        assert_eq!(transcript.segments.len(), 1);
        assert_eq!(transcript.segments[0].end_ms, 900);
    }

    #[test]
    fn tolerates_minimal_payload() {
        let transcript = parse_transcript(br#"{"text":"hello"}"#).expect("text only");
        assert!(transcript.segments.is_empty());
        assert!(transcript.engine.is_empty());
    }

    #[test]
    fn rejects_garbage() {
        assert!(matches!(
            parse_transcript(b"not json"),
            Err(SttError::Parse(_))
        ));
    }
}
