//! Streaming speech-to-text and snore classification, orchestrated from here.
//!
//! ## What this worker is responsible for
//!
//! Capture belongs to the node, analysis belongs to the sidecar, and this is the
//! thing in between that decides: when the sidecar runs, what language it listens
//! for, which audio it gets, where the results are stored, and when it is told to
//! finish. None of those are facts the sidecar could know on its own.
//!
//! ```text
//! stt queue ──► writer task ──► [ 8-byte header | payload ] ──► sidecar
//!                                    ▲                        │
//!                                    └── control / config ◄────┘
//! reader task ──► store task (rusqlite) ──► transcripts + episodes
//!             └─► metrics / TranscriptHub ──► Portal
//! ```
//!
//! ## Why a Unix socket and not stdin
//!
//! The sidecar is started through LaunchServices (`open -n -a`), because that is
//! the one launcher that returns immediately: the server needs a process that
//! outlives the launch call, and `Command::status()` on the bundle itself would
//! block for the whole night. `open` hands the child no stdio, so the socket is
//! what carries both directions — audio up, commands down. (`--mode stdio` exists
//! for embedding and for the probe script; the protocol is the same either way,
//! which is why [`read_frame`](crate::sidecar_wire::read_frame) is written against
//! `AsyncRead` rather than against a socket.)
//!
//! ## Why async here, threads elsewhere
//!
//! The audio path above this worker is real-time-ish and built on bounded queues
//! and dedicated threads; there is exactly one socket to talk to, and it needs to
//! be read and written concurrently without one direction stalling the other.
//! That is what `select!` over a reader task, a writer task and a shutdown
//! watcher buys — and the two blocking bits that remain (the audio queue's
//! timed pop and SQLite) live in their own blocking tasks, so neither can stall
//! the wire.
//!
//! ## Failure is visible
//!
//! A session that ends for any reason is restarted after `restart_delay`, and the
//! reason is kept in `Metrics::set_stt_error` until the next `ready`. The one
//! thing never done here is pretending: a phrase that the database refused is not
//! published, and an unknown frame is a warning, not a shrug.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use chrono::Utc;
use tokio::io::AsyncWriteExt;
use tokio::net::unix::OwnedReadHalf;
use tokio::net::unix::OwnedWriteHalf;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;

use steno_lib::bounded_queue::{BoundedQueue, PopOutcome};
use steno_lib::metrics::Metrics;

use crate::sidecar_wire::{
    self as wire, Config, Control, Direction, MsgType, SessionEvent, SnoreEvent,
    TranscriptEvent as SidecarTranscript,
};
use crate::storage::{ClassifiedEvent, Database, TranscriptRow};
use crate::transcript_hub::{TranscriptEvent, TranscriptHub};

/// `episodes.source` for a row produced by the sidecar's classifier. The
/// server's own detector writes `rule-v1`; the two can annotate the same second
/// of audio and must never be conflated.
pub const CLASSIFIER_SOURCE: &str = "apple-soundanalysis-v1";

/// A block of audio handed to the recogniser, stamped with the wall clock of its
/// first sample so the sidecar never has to guess where it is in time.
#[derive(Debug, Clone)]
pub struct SttChunk {
    pub samples: Vec<i16>,
    pub utc_ms: i64,
}

/// Everything the streaming worker needs that does not change between restarts.
#[derive(Debug, Clone)]
pub struct StreamOptions {
    /// Which configured recogniser this worker drives (`primary` or a
    /// `[[stt.recognizer]]` name). Keys its entry in `Metrics`.
    pub name: String,
    /// Lanes the server wants from it. Results from a lane that was not asked
    /// for are dropped, never stored.
    pub lanes: Vec<String>,
    pub socket_path: PathBuf,
    /// Wrapper that starts the sidecar through LaunchServices.
    pub launcher: String,
    /// Label written into every stored row that the sidecar did not label itself.
    pub engine: String,
    /// Language the recogniser should listen for. Sent in the `config` frame that
    /// precedes the first audio block, because the language is the orchestrator's
    /// decision — the sidecar must not have to guess it from a locale default.
    pub locale: String,
    pub database_path: PathBuf,
    /// How long to wait for the sidecar to connect after a launch.
    pub ready_timeout: Duration,
    pub restart_delay: Duration,
}

/// A write that does not complete in this long is a stalled peer, not a slow one:
/// the sidecar either drains the socket or is not there any more.
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
/// How long the sidecar gets to flush its last finals after `control: stop`.
const STOP_GRACE: Duration = Duration::from_secs(5);
/// Outbound frames in flight. Small on purpose: audio is a live stream, and a
/// deep buffer would only convert a stall into latency.
const OUTBOUND_DEPTH: usize = 8;

#[derive(Debug)]
enum Outbound {
    Audio(SttChunk),
    Control(Control),
    Config(Config),
}

/// Work handed to the single thread that owns the database connection.
#[derive(Debug)]
enum Stored {
    Phrase(Box<TranscriptRow>),
    Snore(Box<SnoreEvent>),
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
    // One runtime, one thread, for the whole worker: there is exactly one socket
    // and no fan-out, so a multi-threaded runtime would only add scheduling
    // noise to a latency-sensitive loop.
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            tracing::error!(%error, "cannot start the stt stream runtime");
            return;
        }
    };

    while !shutdown.load(Ordering::Relaxed) {
        match runtime.block_on(serve(&chunks, &options, &metrics, &shutdown, &hub)) {
            Ok(()) => tracing::info!("stt stream session ended"),
            Err(StreamError::Stopped) => break,
            Err(error) => {
                tracing::warn!(%error, "stt stream failed");
                metrics.update_recognizer(&options.name, |r| r.error = Some(error.to_string()));
            }
        }
        metrics.update_recognizer(&options.name, |r| {
            r.connected = false;
            r.lanes.clear();
        });
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

/// Binds the socket, launches the sidecar, then pumps both directions until
/// either side goes away. Returning always means "restart me".
async fn serve(
    chunks: &Arc<BoundedQueue<SttChunk>>,
    options: &StreamOptions,
    metrics: &Arc<Metrics>,
    shutdown: &Arc<AtomicBool>,
    hub: &Arc<TranscriptHub>,
) -> Result<(), StreamError> {
    // A sidecar from a previous run may still own the path.
    let _ = std::fs::remove_file(&options.socket_path);
    if let Some(parent) = options.socket_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let listener = UnixListener::bind(&options.socket_path)?;

    launch_sidecar(options).await?;
    let stream = accept_with_timeout(&listener, options.ready_timeout, shutdown).await?;
    tracing::info!(socket = %options.socket_path.display(), "stt stream sidecar connected");

    let (read_half, write_half) = stream.into_split();

    // One rusqlite connection, one thread. WAL means the HTTP side reads freely
    // while this writes; nothing else may touch this connection.
    let database = Database::open(&options.database_path)?;
    let (store_tx, store_rx) = std::sync::mpsc::channel::<Stored>();
    let store_metrics = Arc::clone(metrics);
    let store_hub = Arc::clone(hub);
    let store_name = options.name.clone();
    let store = tokio::task::spawn_blocking(move || {
        store_loop(database, store_rx, store_metrics, store_hub, store_name)
    });

    let (outbound_tx, outbound_rx) = mpsc::channel::<Outbound>(OUTBOUND_DEPTH);
    // The session's language travels before any audio: a recogniser that starts
    // on the wrong language wastes a model load and produces a `ready` that is
    // already wrong.
    outbound_tx
        .send(Outbound::Config(Config::for_stream(
            &options.locale,
            options.lanes.iter().any(|l| l == "snore"),
        )))
        .await
        .map_err(|_| StreamError::Stalled)?;
    let writer = tokio::spawn(writer_loop(write_half, outbound_rx));

    let cancel = Arc::new(AtomicBool::new(false));
    let pump_queue = Arc::clone(chunks);
    let pump_tx = outbound_tx.clone();
    let pump_cancel = Arc::clone(&cancel);
    let pump = tokio::task::spawn_blocking(move || audio_pump(pump_queue, pump_tx, pump_cancel));

    let reader = read_loop(
        read_half,
        store_tx,
        Arc::clone(metrics),
        Arc::clone(hub),
        options.engine.clone(),
        options.name.clone(),
        options.lanes.clone(),
    );
    tokio::pin!(reader);

    let result = tokio::select! {
        biased;
        outcome = &mut reader => outcome,
        _ = wait_for_shutdown(shutdown) => {
            // Ask for a clean finish: the sidecar commits whatever is still
            // volatile and closes, which is the difference between losing the
            // last phrase of the night and keeping it.
            tracing::info!("stt stream stopping; asking the sidecar to finalise");
            let _ = outbound_tx.send(Outbound::Control(Control::stop())).await;
            match tokio::time::timeout(STOP_GRACE, &mut reader).await {
                Ok(outcome) => outcome,
                Err(_) => {
                    tracing::warn!("sidecar did not finish within {STOP_GRACE:?}");
                    Ok(())
                }
            }
        }
    };

    // Stop producing audio, then let the writer drain what is queued before the
    // half-close: a dropped queue means the sidecar's timeline has a hole it was
    // never told about.
    cancel.store(true, Ordering::Relaxed);
    let _ = tokio::time::timeout(Duration::from_secs(2), pump).await;
    drop(outbound_tx);
    let _ = tokio::time::timeout(Duration::from_secs(2), writer).await;
    let _ = tokio::time::timeout(Duration::from_secs(2), store).await;

    let _ = std::fs::remove_file(&options.socket_path);
    result
}

/// How long a launcher may take before it is assumed to be the sidecar itself.
///
/// Three shapes have to work here:
///
/// * a wrapper that hands the bundle to LaunchServices and exits 0 immediately
///   (`open -n -a …`) — the sidecar appears later, elsewhere;
/// * a launcher that *is* the sidecar and never exits — `steno-stt-sidecar-audiocpp`
///   runs until the session ends. Waiting for it, as this used to, would block
///   before `accept()` and deadlock the pipeline;
/// * a launcher that fails at startup and exits non-zero, whose reason must reach
///   the log instead of becoming a bare "did not connect".
///
/// Waiting this long before deciding covers the third without costing the second.
const LAUNCH_GRACE: Duration = Duration::from_millis(400);

async fn launch_sidecar(options: &StreamOptions) -> Result<(), StreamError> {
    let program = options.launcher.clone();
    let socket = options.socket_path.clone();
    let child = tokio::task::spawn_blocking(move || {
        std::process::Command::new(&program).arg(&socket).spawn()
    })
    .await
    .map_err(|error| StreamError::Io(std::io::Error::other(error)))?
    .map_err(|source| StreamError::Launch {
        program: options.launcher.clone(),
        source,
    })?;

    let mut child = child;
    let deadline = Instant::now() + LAUNCH_GRACE;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(status)) => {
                return Err(StreamError::Launch {
                    program: options.launcher.clone(),
                    source: std::io::Error::other(format!(
                        "launcher exited with {status} (its reason is the last line it printed)"
                    )),
                });
            }
            // Still running: on this path the launcher *is* the sidecar, and it will
            // run until the night is over. Hand it to a thread that reaps it so a
            // restart does not leave a zombie behind, and get on with accepting.
            Ok(None) if Instant::now() >= deadline => {
                let provider = options.launcher.clone();
                std::thread::Builder::new()
                    .name("steno-stt-reaper".into())
                    .spawn(move || {
                        if let Ok(status) = child.wait() {
                            tracing::info!(%provider, %status, "sidecar process exited");
                        }
                    })
                    .map_err(|source| StreamError::Launch {
                        program: options.launcher.clone(),
                        source,
                    })?;
                return Ok(());
            }
            Ok(None) => tokio::time::sleep(Duration::from_millis(10)).await,
            Err(source) => {
                return Err(StreamError::Launch {
                    program: options.launcher.clone(),
                    source,
                });
            }
        }
    }
}

async fn accept_with_timeout(
    listener: &UnixListener,
    timeout: Duration,
    shutdown: &Arc<AtomicBool>,
) -> Result<UnixStream, StreamError> {
    let deadline = Instant::now() + timeout;
    loop {
        if shutdown.load(Ordering::Relaxed) {
            return Err(StreamError::Stopped);
        }
        match tokio::time::timeout(Duration::from_millis(100), listener.accept()).await {
            Ok(Ok((stream, _))) => return Ok(stream),
            Ok(Err(error)) => return Err(StreamError::Io(error)),
            Err(_) => {
                if Instant::now() >= deadline {
                    return Err(StreamError::NotConnected);
                }
            }
        }
    }
}

async fn wait_for_shutdown(shutdown: &Arc<AtomicBool>) {
    while !shutdown.load(Ordering::Relaxed) {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

// MARK: - audio out

/// Drains the STT queue into the outbound channel.
///
/// Blocking on purpose: the queue's `pop_timeout` is a timed condvar wait, and
/// putting it on the runtime would park an async worker for 100 ms at a time.
///
/// Deliberately **not** an `async fn`, even though nothing here awaits. It runs
/// inside `spawn_blocking`, which takes a synchronous closure: an `async fn` would
/// compile there as "returns a future nobody polls", and every block would be
/// dropped in silence. That is not a hypothetical — it is what this function was
/// first written as, and the queue counter was the only symptom.
fn audio_pump(
    chunks: Arc<BoundedQueue<SttChunk>>,
    outbound: mpsc::Sender<Outbound>,
    cancel: Arc<AtomicBool>,
) {
    while !cancel.load(Ordering::Relaxed) {
        match chunks.pop_timeout(Duration::from_millis(100)) {
            PopOutcome::Item(chunk) => {
                if outbound.blocking_send(Outbound::Audio(chunk)).is_err() {
                    return;
                }
            }
            PopOutcome::Empty => {}
            PopOutcome::Closed => return,
        }
    }
}

async fn writer_loop(
    mut writer: OwnedWriteHalf,
    mut outbound: mpsc::Receiver<Outbound>,
) -> Result<(), StreamError> {
    let mut buffer: Vec<u8> = Vec::with_capacity(64 * 1024);
    while let Some(item) = outbound.recv().await {
        match item {
            Outbound::Audio(chunk) => {
                if chunk.samples.is_empty() {
                    continue;
                }
                wire::encode_audio(chunk.utc_ms, &chunk.samples, &mut buffer);
                write_with_timeout(&mut writer, MsgType::Audio, &buffer).await?;
            }
            Outbound::Control(control) => {
                let payload = serde_json::to_vec(&control)?;
                write_with_timeout(&mut writer, MsgType::Control, &payload).await?;
            }
            Outbound::Config(config) => {
                let payload = serde_json::to_vec(&config)?;
                write_with_timeout(&mut writer, MsgType::Config, &payload).await?;
            }
        }
    }
    // Half-close so the sidecar sees a clean end of audio rather than a reset.
    let _ = writer.shutdown().await;
    Ok(())
}

async fn write_with_timeout(
    writer: &mut OwnedWriteHalf,
    kind: MsgType,
    payload: &[u8],
) -> Result<(), StreamError> {
    match tokio::time::timeout(WRITE_TIMEOUT, wire::write_frame(writer, kind, payload)).await {
        Ok(result) => Ok(result?),
        Err(_) => Err(StreamError::Stalled),
    }
}

// MARK: - events in

async fn read_loop(
    mut reader: OwnedReadHalf,
    store: std::sync::mpsc::Sender<Stored>,
    metrics: Arc<Metrics>,
    hub: Arc<TranscriptHub>,
    engine: String,
    name: String,
    lanes: Vec<String>,
) -> Result<(), StreamError> {
    let wants_text = lanes.iter().any(|l| l == "text");
    let wants_snore = lanes.iter().any(|l| l == "snore");
    let mut frames: u64 = 0;
    loop {
        let Some(frame) = wire::read_frame(&mut reader, Direction::SidecarToServer).await? else {
            tracing::info!(frames, "sidecar closed the stream");
            return Ok(());
        };
        frames += 1;
        match frame.kind {
            MsgType::Transcript if !wants_text => {
                tracing::debug!(recognizer = %name, "transcript dropped: the text lane was not requested");
            }
            MsgType::Transcript => {
                let event: SidecarTranscript = serde_json::from_slice(&frame.payload)?;
                match event.kind.as_str() {
                    "final" => {
                        // Debug, not info: one line per recognised phrase. This is
                        // the only place that can tell "the sidecar never
                        // finalised a phrase" from "it did and we refused to
                        // store it".
                        tracing::debug!(
                            text = %event.text,
                            start_ms = ?event.start_ms,
                            end_ms = ?event.end_ms,
                            "sidecar sent a final"
                        );
                        match phrase_row(&event, &engine) {
                            Some(row) => {
                                if store.send(Stored::Phrase(Box::new(row))).is_err() {
                                    return Ok(());
                                }
                            }
                            None => tracing::debug!(
                                "final dropped: empty text or missing timestamps (never guessed)"
                            ),
                        }
                    }
                    "partial" => {
                        metrics.set_stt_partial(Some(event.text.clone()));
                        hub.publish(TranscriptEvent::Partial { text: event.text });
                    }
                    other => tracing::debug!(kind = other, "ignoring unknown transcript kind"),
                }
            }
            MsgType::Snore if !wants_snore => {
                tracing::debug!(recognizer = %name, "snore episode dropped: the snore lane was not requested");
            }
            MsgType::Snore => {
                let event: SnoreEvent = serde_json::from_slice(&frame.payload)?;
                if !event.is_usable() {
                    tracing::warn!(
                        start_ms = event.start_ms,
                        end_ms = event.end_ms,
                        label = %event.label,
                        "sidecar sent an unusable snore episode"
                    );
                    continue;
                }
                if store.send(Stored::Snore(Box::new(event))).is_err() {
                    return Ok(());
                }
            }
            MsgType::Session => {
                let event: SessionEvent = serde_json::from_slice(&frame.payload)?;
                handle_session(&event, &metrics, &hub, &name);
            }
            other => tracing::warn!(kind = %other, "ignoring an uplink frame the server has no use for"),
        }
    }
}

/// The lanes a `ready` frame declares. A sidecar that predates the `lanes`
/// field is read as what it unambiguously said: text always, snore only when it
/// named a snore lane.
fn declared_lanes(event: &SessionEvent) -> Vec<String> {
    if let Some(lanes) = &event.lanes {
        return lanes.clone();
    }
    let mut lanes = vec!["text".to_string()];
    if event.snore.is_some() {
        lanes.push("snore".to_string());
    }
    lanes
}

fn handle_session(
    event: &SessionEvent,
    metrics: &Arc<Metrics>,
    hub: &Arc<TranscriptHub>,
    name: &str,
) {
    match event.state.as_str() {
        "ready" => {
            let lanes = declared_lanes(event);
            metrics.update_recognizer(name, |r| {
                r.connected = true;
                r.error = None;
                r.lanes = lanes.clone();
                r.engine = event.engine.clone();
            });
            for missing in metrics
                .recognizers()
                .into_iter()
                .find(|r| r.name == name)
                .map(|r| r.missing_lanes())
                .unwrap_or_default()
            {
                tracing::warn!(
                    recognizer = name,
                    lane = %missing,
                    offered = ?lanes,
                    "this recogniser does not offer a lane the server asked for"
                );
            }
            // A browser that connected before the sidecar is worth telling that
            // the recogniser just came up.
            hub.publish(TranscriptEvent::State {
                state: "ready".to_string(),
            });
            tracing::info!(
                recognizer = name,
                lanes = ?lanes,
                engine = event.engine.as_deref().unwrap_or("unknown"),
                locale = event.locale.as_deref().unwrap_or("unknown"),
                snore = event.snore.as_deref().unwrap_or("off"),
                "stt stream ready"
            );
        }
        "error" => {
            let message = session_message(event);
            tracing::warn!(%message, "recogniser error");
            metrics.update_recognizer(name, |r| r.error = Some(message.clone()));
            hub.publish(TranscriptEvent::Error { message });
        }
        // `status` answers a `control: status`, `degraded` says one lane is
        // running and another is not, `stopped` is the sidecar agreeing to go.
        // None of them is a failure, and none may clear the error state that a
        // real failure set.
        other => tracing::info!(
            state = other,
            message = event.message.as_deref().unwrap_or(""),
            "stt stream notice"
        ),
    }
}

fn session_message(event: &SessionEvent) -> String {
    event
        .message
        .clone()
        .filter(|message| !message.is_empty())
        .unwrap_or_else(|| format!("the recogniser reported {}", event.state))
}

// MARK: - storage

/// The single thread that writes results. Publishing happens *after* the row
/// exists: a client must never show a phrase the database rejected.
fn store_loop(
    database: Database,
    events: std::sync::mpsc::Receiver<Stored>,
    metrics: Arc<Metrics>,
    hub: Arc<TranscriptHub>,
    name: String,
) {
    for event in events {
        match event {
            Stored::Phrase(row) => match database.insert_transcript(&row) {
                Ok(()) => {
                    metrics.stt_finals.fetch_add(1, Ordering::Relaxed);
                    metrics.update_recognizer(&name, |r| r.finals += 1);
                    metrics.set_stt_partial(None);
                    // Published only after the row exists: a client must never
                    // show a phrase the database rejected.
                    hub.publish(TranscriptEvent::final_from_row(&row));
                    tracing::debug!(text = %row.text, "phrase stored");
                }
                Err(error) => tracing::warn!(%error, "cannot store phrase"),
            },
            Stored::Snore(event) => {
                let draft = classified_event(&event);
                // The recording is not known here — the file may not have closed
                // yet, and the audio may already have been rotated away. The
                // episode is still a true annotation about that stretch of time.
                match database.insert_episode(&draft.episode_row(None)) {
                    Ok(()) => {
                        metrics.stt_snores.fetch_add(1, Ordering::Relaxed);
                        metrics.update_recognizer(&name, |r| r.snores += 1);
                        // Published after the row exists, like a final: a client
                        // must never show an episode the database rejected.
                        hub.publish(TranscriptEvent::Snore {
                            start_ms: event.start_ms,
                            end_ms: event.end_ms,
                            label: event.label.clone(),
                            peak: event.peak,
                            mean: event.mean,
                            windows: event.windows,
                            threshold: event.threshold,
                        });
                        tracing::info!(
                            start_ms = event.start_ms,
                            end_ms = event.end_ms,
                            peak = event.peak,
                            windows = event.windows,
                            "snore episode stored"
                        );
                    }
                    Err(error) => tracing::warn!(%error, "cannot store snore episode"),
                }
            }
        }
    }
}

fn classified_event(event: &SnoreEvent) -> ClassifiedEvent {
    ClassifiedEvent {
        started_at_utc: chrono::DateTime::from_timestamp_millis(event.start_ms)
            .unwrap_or_else(Utc::now),
        ended_at_utc: chrono::DateTime::from_timestamp_millis(event.end_ms)
            .unwrap_or_else(Utc::now),
        label: event.label.clone(),
        confidence: event.peak,
        threshold: event.threshold,
        source: CLASSIFIER_SOURCE.to_string(),
    }
}

fn phrase_row(event: &SidecarTranscript, engine: &str) -> Option<TranscriptRow> {
    let text = event.text.clone();
    let (Some(start_ms), Some(end_ms)) = (event.start_ms, event.end_ms) else {
        return None;
    };
    // Silence produces empty finals. Storing those would bury the real
    // sentences, and the event timeline already shows when the room was quiet.
    if text.trim().is_empty() {
        return None;
    }
    let started = chrono::DateTime::from_timestamp_millis(start_ms)?;
    let ended = chrono::DateTime::from_timestamp_millis(end_ms.max(start_ms))?;
    Some(TranscriptRow {
        id: transcript_id(),
        recording_id: None,
        started_at_utc: started,
        ended_at_utc: ended,
        text,
        engine: event
            .engine
            .clone()
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| engine.to_string()),
        language: event.language.clone(),
        created_at_utc: Utc::now(),
    })
}

/// Transcript ids only need to be unique within one database; a time-ordered hex
/// string is enough and avoids pulling in a UUID dependency here.
fn transcript_id() -> String {
    use std::sync::atomic::AtomicU64;
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("u{:013x}{:04x}", Utc::now().timestamp_millis(), n & 0xffff)
}

#[derive(Debug, thiserror::Error)]
pub enum StreamError {
    #[error("stt stream io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("stt stream protocol error: {0}")]
    Wire(#[from] wire::WireError),
    #[error("stt stream payload error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("stt stream storage error: {0}")]
    Storage(#[from] steno_lib::error::Error),
    #[error("cannot launch {program}: {source}")]
    Launch {
        program: String,
        #[source]
        source: std::io::Error,
    },
    #[error("the recogniser did not connect within the timeout")]
    NotConnected,
    #[error("the sidecar stopped reading; write timed out")]
    Stalled,
    #[error("stopping")]
    Stopped,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snore(start_ms: i64, end_ms: i64) -> SnoreEvent {
        SnoreEvent {
            start_ms,
            end_ms,
            label: "snoring".to_string(),
            peak: 0.81,
            mean: 0.74,
            windows: 4,
            threshold: 0.5,
            end_reason: "normal".to_string(),
        }
    }

    #[test]
    fn the_audio_pump_drains_the_queue_into_the_writer_channel() {
        // The pump runs inside `spawn_blocking`, so it must be a synchronous
        // function: an `async fn` there returns an unpolled future and the queue
        // stops draining while every block is counted as dropped. This test fails
        // in exactly that case, which is the way it was first written.
        let queue = Arc::new(BoundedQueue::new(8, 1));
        queue
            .push_audio(SttChunk {
                samples: vec![1, 2, 3],
                utc_ms: 42,
            })
            .expect("push");
        let (tx, mut rx) = mpsc::channel::<Outbound>(4);
        let cancel = Arc::new(AtomicBool::new(false));

        let pump_queue = Arc::clone(&queue);
        let pump_cancel = Arc::clone(&cancel);
        let pump = std::thread::spawn(move || audio_pump(pump_queue, tx, pump_cancel));

        // Bounded wait rather than `blocking_recv`: if the pump is not draining,
        // this must fail in two seconds instead of hanging the test suite.
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut received = None;
        while Instant::now() < deadline {
            if let Ok(item) = rx.try_recv() {
                received = Some(item);
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let received = received.expect("the pump never drained the queue");
        match received {
            Outbound::Audio(chunk) => {
                assert_eq!(chunk.samples, vec![1, 2, 3]);
                assert_eq!(chunk.utc_ms, 42);
            }
            other => panic!("expected audio, got {other:?}"),
        }
        cancel.store(true, Ordering::Relaxed);
        pump.join().expect("pump thread");
    }

    #[test]
    fn a_classified_episode_keeps_its_clock_and_its_confidence() {
        let draft = classified_event(&snore(1_700_000_000_000, 1_700_000_004_200));
        assert_eq!(draft.source, CLASSIFIER_SOURCE);
        assert_eq!(draft.confidence, 0.81);
        assert_eq!(draft.label, "snoring");
        assert_eq!(
            draft.ended_at_utc.timestamp_millis() - draft.started_at_utc.timestamp_millis(),
            4_200
        );
    }

    #[test]
    fn a_final_event_becomes_a_row_with_wall_clock_bounds() {
        let event = SidecarTranscript {
            kind: "final".to_string(),
            text: "打呼了".to_string(),
            start_ms: Some(1_700_000_000_000),
            end_ms: Some(1_700_000_001_500),
            engine: Some("apple-sfspeech".to_string()),
            language: Some("zh-CN".to_string()),
            segments: Some(serde_json::json!([])),
        };
        let row = phrase_row(&event, "fallback").expect("row");
        assert_eq!(row.text, "打呼了");
        assert_eq!(row.engine, "apple-sfspeech");
        assert_eq!(
            row.ended_at_utc.timestamp_millis() - row.started_at_utc.timestamp_millis(),
            1500
        );
    }

    #[test]
    fn a_final_without_timestamps_is_dropped_rather_than_guessed() {
        let event = SidecarTranscript {
            kind: "final".to_string(),
            text: "hello".to_string(),
            start_ms: None,
            end_ms: None,
            engine: None,
            language: None,
            segments: None,
        };
        assert!(phrase_row(&event, "apple-speechanalyzer").is_none());
    }

    /// A sidecar that *is* the launcher (`steno-stt-sidecar-audiocpp`) runs until the
    /// night is over. Waiting for it, as this used to, blocks before `accept()` and
    /// the pipeline never carries a byte — the deadlock this test exists to prevent.
    #[tokio::test]
    async fn a_launcher_that_keeps_running_is_not_waited_for() {
        let directory = tempfile::tempdir().expect("tempdir");
        let script = directory.path().join("slow-launcher.sh");
        std::fs::write(&script, "#!/bin/sh\nsleep 30\n").expect("write");
        make_executable(&script);

        let options = launcher_options(script.clone());
        let started = std::time::Instant::now();
        launch_sidecar(&options).await.expect("a long-lived launcher is accepted");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "launch waited for the launcher to exit: {} ms",
            started.elapsed().as_millis()
        );
    }

    /// The other shape: a launcher that refuses to start. Its exit code has to reach
    /// the caller, or a missing model becomes a bare "did not connect".
    ///
    /// `/usr/bin/false` rather than a generated script: resolving a shebang and a
    /// temporary file adds enough startup latency that a loaded machine can look
    /// like a long-lived launcher, which is the branch next door.
    #[tokio::test]
    async fn a_launcher_that_exits_nonzero_is_reported() {
        let options = launcher_options(std::path::PathBuf::from("/usr/bin/false"));
        match launch_sidecar(&options).await {
            Err(StreamError::Launch { .. }) => {}
            other => panic!("expected a launch error, got {other:?}"),
        }
    }

    fn launcher_options(launcher: std::path::PathBuf) -> StreamOptions {
        StreamOptions {
            name: "primary".to_string(),
            lanes: vec!["text".to_string()],
            socket_path: std::env::temp_dir().join("steno-launch-test.sock"),
            launcher: launcher.display().to_string(),
            engine: "test".to_string(),
            locale: "zh-CN".to_string(),
            database_path: std::path::PathBuf::from(":memory:"),
            ready_timeout: std::time::Duration::from_secs(1),
            restart_delay: std::time::Duration::from_secs(1),
        }
    }

    fn make_executable(path: &std::path::Path) {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(path).expect("metadata").permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(path, permissions).expect("chmod");
    }
}
