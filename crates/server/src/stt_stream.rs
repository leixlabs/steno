//! Streaming speech-to-text: one long-lived recogniser fed by the capture pipe.
//!
//! ## Why this exists next to `stt.rs`
//!
//! The per-segment path (`stt.rs`) writes a WAV, then runs a sidecar on it. That
//! is simple and restart-safe, but it has two costs that matter in a bedroom:
//!
//! * every segment spawns a process — on macOS that reads as an application
//!   launch, so the Dock flickers once every ten seconds;
//! * text only appears after a segment closes and a whole file has been
//!   decoded, which is seconds to tens of seconds late.
//!
//! So audio is now also pushed into a recogniser that stays alive, which
//! produces text while the person is still talking.
//!
//! ## Why a Unix socket
//!
//! The sidecar is started through LaunchServices (`open -n -a`), because that is
//! the one launcher that returns immediately: the server needs a process that
//! outlives the launch call, and `Command::status()` would block for the whole
//! night. `open` hands the child no stdio, so the socket is what carries both
//! directions — audio in, JSON out.
//!
//! ```text
//! server -> sidecar : [u32 LE length][i64 LE utc_ms][s16le mono 16 kHz PCM]
//! sidecar -> server : one JSON object per line
//! ```
//!
//! Timestamps cross the wire as wall-clock milliseconds, so nothing has to be
//! remapped on this side and a sidecar restart cannot shift the transcript.

use std::io::{BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use chrono::Utc;
use serde::Deserialize;

use steno_lib::bounded_queue::{BoundedQueue, PopOutcome};
use steno_lib::metrics::Metrics;

use crate::storage::{Database, UtteranceRow};
use crate::transcript_hub::{TranscriptEvent, TranscriptHub};

/// One block of audio handed to the recogniser, stamped with the wall clock of
/// its first sample so the sidecar never has to guess where it is in time.
#[derive(Debug, Clone)]
pub struct SttChunk {
    pub samples: Vec<i16>,
    pub utc_ms: i64,
}

/// Everything the streaming worker needs that does not change between restarts.
#[derive(Debug, Clone)]
pub struct StreamOptions {
    pub socket_path: PathBuf,
    /// Wrapper that starts the sidecar through LaunchServices.
    pub launcher: String,
    pub engine: String,
    pub database_path: PathBuf,
    /// How long to wait for the sidecar to connect after a launch.
    pub ready_timeout: Duration,
    pub restart_delay: Duration,
}

#[derive(Debug, Deserialize)]
struct SidecarEvent {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    start_ms: Option<i64>,
    #[serde(default)]
    end_ms: Option<i64>,
    #[serde(default)]
    segments: Option<serde_json::Value>,
    #[serde(default)]
    engine: Option<String>,
    #[serde(default)]
    language: Option<String>,
    #[serde(default)]
    message: Option<String>,
}

enum Event {
    Ready,
    Final(Box<SidecarEvent>),
    Partial(String),
    Error(String),
    Closed,
}

pub fn spawn(
    chunks: Arc<BoundedQueue<SttChunk>>,
    options: StreamOptions,
    metrics: Arc<Metrics>,
    shutdown: Arc<AtomicBool>,
    hub: Arc<TranscriptHub>,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name("stt-stream".to_string())
        .spawn(move || run(chunks, options, metrics, shutdown, hub))
}

fn run(
    chunks: Arc<BoundedQueue<SttChunk>>,
    options: StreamOptions,
    metrics: Arc<Metrics>,
    shutdown: Arc<AtomicBool>,
    hub: Arc<TranscriptHub>,
) {
    let mut database: Option<Database> = None;
    while !shutdown.load(Ordering::Relaxed) {
        if database.is_none() {
            match Database::open(&options.database_path) {
                Ok(db) => database = Some(db),
                Err(error) => tracing::warn!(%error, "stt stream cannot open the database yet"),
            }
        }
        let Some(db) = database.as_mut() else {
            sleep_unless_stopping(&shutdown, options.restart_delay);
            continue;
        };

        match serve(db, &chunks, &options, &metrics, &shutdown, &hub) {
            Ok(()) => tracing::info!("stt stream session ended"),
            Err(StreamError::Stopped) => break,
            Err(error) => {
                tracing::warn!(%error, "stt stream failed");
                metrics.set_stt_error(Some(error.to_string()));
            }
        }
        metrics.stt_connected.store(false, Ordering::Relaxed);
        // A dead session cannot have a phrase in flight: leaving the last
        // partial on screen would look like the recogniser is still working.
        metrics.set_stt_partial(None);
        sleep_unless_stopping(&shutdown, options.restart_delay);
    }
    tracing::info!("stt stream worker stopped");
}

fn sleep_unless_stopping(shutdown: &AtomicBool, duration: Duration) {
    let deadline = Instant::now() + duration;
    while Instant::now() < deadline {
        if shutdown.load(Ordering::Relaxed) {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Binds the socket, launches the sidecar, then pumps audio until either side
/// goes away. Returning always means "restart me".
fn serve(
    db: &mut Database,
    chunks: &Arc<BoundedQueue<SttChunk>>,
    options: &StreamOptions,
    metrics: &Arc<Metrics>,
    shutdown: &Arc<AtomicBool>,
    hub: &TranscriptHub,
) -> Result<(), StreamError> {
    // A sidecar from a previous run may still own the path.
    let _ = std::fs::remove_file(&options.socket_path);
    if let Some(parent) = options.socket_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let listener = UnixListener::bind(&options.socket_path)?;
    listener.set_nonblocking(true)?;

    std::process::Command::new(&options.launcher)
        .arg(&options.socket_path)
        .status()
        .map_err(|error| StreamError::Launch {
            program: options.launcher.clone(),
            source: error,
        })?;

    let mut stream = accept_with_timeout(&listener, options.ready_timeout, shutdown)?;
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_millis(500)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    tracing::info!(socket = %options.socket_path.display(), "stt stream sidecar connected");

    let (event_tx, event_rx) = channel();
    let reader = spawn_reader(stream.try_clone()?, event_tx);

    let result = pump(db, &mut stream, chunks, &event_rx, metrics, shutdown, hub);

    let _ = stream.shutdown(std::net::Shutdown::Both);
    let _ = reader.join();
    let _ = std::fs::remove_file(&options.socket_path);
    result
}

fn accept_with_timeout(
    listener: &UnixListener,
    timeout: Duration,
    shutdown: &Arc<AtomicBool>,
) -> Result<UnixStream, StreamError> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if shutdown.load(Ordering::Relaxed) {
            return Err(StreamError::Stopped);
        }
        match listener.accept() {
            Ok((stream, _)) => return Ok(stream),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(error) => return Err(StreamError::Io(error)),
        }
    }
    Err(StreamError::NotConnected)
}

fn spawn_reader(stream: UnixStream, events: Sender<Event>) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("stt-stream-reader".to_string())
        .spawn(move || {
            // Lines are assembled by hand rather than with `read_line`: the
            // socket has a read timeout so shutdown stays responsive, and
            // `read_line` discards a partially received line when it hits one.
            let mut reader = BufReader::new(stream);
            let mut pending: Vec<u8> = Vec::with_capacity(4096);
            let mut chunk = [0u8; 4096];
            loop {
                match reader.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(count) => {
                        pending.extend_from_slice(&chunk[..count]);
                        while let Some(index) = pending.iter().position(|byte| *byte == b'\n') {
                            let line: Vec<u8> = pending.drain(..=index).collect();
                            let text = String::from_utf8_lossy(&line);
                            let trimmed = text.trim();
                            if trimmed.is_empty() {
                                continue;
                            }
                            let event = match serde_json::from_str::<SidecarEvent>(trimmed) {
                                Ok(event) => event,
                                Err(error) => {
                                    tracing::debug!(
                                        %error,
                                        line = trimmed,
                                        "ignoring unparsable sidecar line"
                                    );
                                    continue;
                                }
                            };
                            let mapped = match event.kind.as_str() {
                                "ready" => Event::Ready,
                                "final" => Event::Final(Box::new(event)),
                                "partial" => Event::Partial(event.text.unwrap_or_default()),
                                "error" => Event::Error(event.message.unwrap_or_else(|| {
                                    "recogniser reported an error".to_string()
                                })),
                                other => {
                                    tracing::debug!(kind = other, "ignoring unknown sidecar event");
                                    continue;
                                }
                            };
                            if events.send(mapped).is_err() {
                                return;
                            }
                        }
                        if pending.len() > 1 << 20 {
                            tracing::warn!("sidecar sent a line we cannot frame; dropping it");
                            pending.clear();
                        }
                    }
                    Err(error)
                        if error.kind() == std::io::ErrorKind::WouldBlock
                            || error.kind() == std::io::ErrorKind::TimedOut =>
                    {
                        continue;
                    }
                    Err(_) => break,
                }
            }
            let _ = events.send(Event::Closed);
        })
        .expect("spawn stt reader")
}

fn pump(
    db: &mut Database,
    stream: &mut UnixStream,
    chunks: &Arc<BoundedQueue<SttChunk>>,
    events: &Receiver<Event>,
    metrics: &Arc<Metrics>,
    shutdown: &Arc<AtomicBool>,
    hub: &TranscriptHub,
) -> Result<(), StreamError> {
    let mut frame = Vec::with_capacity(64 * 1024);
    loop {
        if shutdown.load(Ordering::Relaxed) {
            return Ok(());
        }

        // Drain every pending sidecar event first: the text is worth more than
        // the next 20 ms of audio.
        loop {
            match events.try_recv() {
                Ok(Event::Ready) => {
                    metrics.stt_connected.store(true, Ordering::Relaxed);
                    metrics.set_stt_error(None);
                    // A browser that connected before the sidecar is worth telling
                    // that the recogniser just came up.
                    hub.publish(TranscriptEvent::State {
                        state: "ready".to_string(),
                    });
                    tracing::info!("stt stream ready");
                }
                Ok(Event::Final(event)) => {
                    // Debug, not info: one line per recognised phrase. This is
                    // the only place that can tell "the sidecar never finalised a
                    // phrase" from "it did and we refused to store it", which is
                    // exactly the distinction that took a while to see the first
                    // time.
                    tracing::debug!(
                        text = event.text.as_deref().unwrap_or(""),
                        start_ms = ?event.start_ms,
                        end_ms = ?event.end_ms,
                        "sidecar sent a final"
                    );
                    match utterance_row(&event, &metrics.stt_engine()) {
                        Some(row) => {
                            if let Err(error) = db.save_utterance(&row) {
                                tracing::warn!(%error, "cannot store utterance");
                            } else {
                                metrics.stt_finals.fetch_add(1, Ordering::Relaxed);
                                metrics.set_stt_partial(None);
                                // Published *after* the row exists: a client must
                                // never show a phrase the database rejected.
                                hub.publish(TranscriptEvent::final_from_row(&row));
                                tracing::debug!(text = %row.text, "utterance stored");
                            }
                        }
                        None => tracing::debug!(
                            "final dropped: empty text or missing timestamps (never guessed)"
                        ),
                    }
                }
                Ok(Event::Partial(text)) => {
                    metrics.set_stt_partial(Some(text.clone()));
                    hub.publish(TranscriptEvent::Partial { text });
                }
                Ok(Event::Error(message)) => {
                    // A task the sidecar ended itself reports "Recognition
                    // request was canceled". That is the endpoint rotation
                    // working, and showing it as an error state made a
                    // recogniser that was actively producing text look dead —
                    // the kind of lie the rest of this module goes to some
                    // trouble to avoid.
                    if message.to_lowercase().contains("cancel") {
                        tracing::debug!(%message, "recognition task rotated");
                        continue;
                    }
                    tracing::warn!(%message, "recogniser error");
                    metrics.set_stt_error(Some(message.clone()));
                    hub.publish(TranscriptEvent::Error { message });
                }
                Ok(Event::Closed) | Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    return Ok(());
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
            }
        }

        match chunks.pop_timeout(Duration::from_millis(100)) {
            PopOutcome::Item(chunk) => write_frame(stream, &mut frame, &chunk)?,
            PopOutcome::Empty => {}
            PopOutcome::Closed => return Ok(()),
        }
    }
}

fn write_frame(
    stream: &mut UnixStream,
    buffer: &mut Vec<u8>,
    chunk: &SttChunk,
) -> Result<(), StreamError> {
    let bytes = chunk.samples.len() * 2;
    if bytes == 0 || bytes >= 1 << 20 {
        return Ok(());
    }
    buffer.clear();
    buffer.extend_from_slice(&(bytes as u32).to_le_bytes());
    buffer.extend_from_slice(&chunk.utc_ms.to_le_bytes());
    for sample in &chunk.samples {
        buffer.extend_from_slice(&sample.to_le_bytes());
    }
    stream.write_all(buffer)?;
    stream.flush()?;
    Ok(())
}

fn utterance_row(event: &SidecarEvent, engine: &str) -> Option<UtteranceRow> {
    let text = event.text.clone().unwrap_or_default();
    let (Some(start_ms), Some(end_ms)) = (event.start_ms, event.end_ms) else {
        return None;
    };
    // Silence produces empty finals (one per quiet stretch). Storing those
    // would bury the real sentences, and the event timeline already shows when
    // the room was quiet.
    if text.trim().is_empty() {
        return None;
    }
    let started = chrono::DateTime::from_timestamp_millis(start_ms)?;
    let ended = chrono::DateTime::from_timestamp_millis(end_ms.max(start_ms))?;
    Some(UtteranceRow {
        id: utterance_id(),
        started_at_utc: started,
        ended_at_utc: ended,
        engine: event
            .engine
            .clone()
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| engine.to_string()),
        language: event.language.clone(),
        text,
        segments_json: event
            .segments
            .as_ref()
            .map(|value| value.to_string())
            .unwrap_or_else(|| "[]".to_string()),
        created_at_utc: Utc::now(),
    })
}

/// Utterance ids only need to be unique within one database; a time-ordered
/// hex string is enough and avoids pulling in a UUID dependency here.
fn utterance_id() -> String {
    use std::sync::atomic::AtomicU64;
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("u{:013x}{:04x}", Utc::now().timestamp_millis(), n & 0xffff)
}

#[derive(Debug, thiserror::Error)]
pub enum StreamError {
    #[error("stt stream io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("cannot launch {program}: {source}")]
    Launch {
        program: String,
        #[source]
        source: std::io::Error,
    },
    #[error("the recogniser did not connect within the timeout")]
    NotConnected,
    #[error("stopping")]
    Stopped,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_layout_is_length_then_utc_then_pcm() {
        let chunk = SttChunk {
            samples: vec![1, -2, 3],
            utc_ms: 1_700_000_000_123,
        };
        let mut buffer = Vec::new();
        let pair = UnixStream::pair().expect("socketpair");
        let mut stream = pair.0;
        write_frame(&mut stream, &mut buffer, &chunk).expect("write");
        assert_eq!(buffer.len(), 4 + 8 + 6);
        assert_eq!(u32::from_le_bytes(buffer[0..4].try_into().unwrap()), 6);
        assert_eq!(
            i64::from_le_bytes(buffer[4..12].try_into().unwrap()),
            1_700_000_000_123
        );
        assert_eq!(i16::from_le_bytes(buffer[12..14].try_into().unwrap()), 1);
        assert_eq!(i16::from_le_bytes(buffer[14..16].try_into().unwrap()), -2);
    }

    #[test]
    fn a_final_event_becomes_a_row_with_wall_clock_bounds() {
        let event = SidecarEvent {
            kind: "final".to_string(),
            text: Some("打呼了".to_string()),
            start_ms: Some(1_700_000_000_000),
            end_ms: Some(1_700_000_001_500),
            segments: Some(serde_json::json!([])),
            engine: Some("apple-sfspeech".to_string()),
            language: Some("zh-CN".to_string()),
            message: None,
        };
        let row = utterance_row(&event, "fallback").expect("row");
        assert_eq!(row.text, "打呼了");
        assert_eq!(row.engine, "apple-sfspeech");
        assert_eq!(
            row.ended_at_utc.timestamp_millis() - row.started_at_utc.timestamp_millis(),
            1500
        );
    }

    #[test]
    fn a_final_without_timestamps_is_dropped_rather_than_guessed() {
        let event = SidecarEvent {
            kind: "final".to_string(),
            text: Some("hello".to_string()),
            start_ms: None,
            end_ms: None,
            segments: None,
            engine: None,
            language: None,
            message: None,
        };
        assert!(utterance_row(&event, "apple-sfspeech").is_none());
    }
}
