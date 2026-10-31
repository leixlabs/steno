//! Runtime assembly (TECH_SPEC §4): wires capture, dispatch, recording,
//! detection, storage and HTTP into one process, and tears them down in the
//! documented order.
//!
//! Thread layout, from §4:
//!
//! ```text
//! CPAL callback -> capture ring -> dispatcher -> { recorder worker, detector worker }
//!                                     |                    |
//!                                     +--> DB writer  <-----+
//! HTTP server (Tokio) reads the same DB and metrics
//! ```
//!
//! The dispatcher owns every segmentation decision; the two workers below only
//! react to the ordered messages it produces. Nothing here re-implements that
//! logic — this file exists to start things in the right order and stop them in
//! the right order.
//!
//! Shutdown order (§4.3): stop the stream, flush the dispatcher, let the recorder
//! finish the current segment (`end_reason=shutdown`), flush pending detector
//! events, then drain the DB writer. If the whole sequence exceeds
//! `shutdown_timeout_seconds`, the remaining `.partial` is deliberately left for
//! the startup recovery scan rather than being force-completed.

use steno_lib::audio_capture::{self, CaptureRing, NegotiatedFormat};
use steno_lib::bounded_queue::{BoundedQueue, PopOutcome};
use steno_lib::config::{Config, SampleFormat, SourceKind, WIRE_CHANNELS, WIRE_RATE_HZ};
use steno_server::db_writer::{DbWriter, DbWriterHandle};
use steno_lib::detector::{
    DetectionPipeline, Discontinuity, EndReason, EventDraft, SegmentContext, State,
};
use steno_server::dispatcher::{self, DispatcherInputs, DispatcherMessage, SegmentEndReason};
use steno_lib::error::{Error, Result};
use steno_server::http_server::{self, HttpState, MicrophoneState, RecordingState, RuntimeStatus};
use steno_lib::metrics::{DetectorState, Metrics};
use steno_server::net_capture;
use steno_server::recorder::{WavFormat, WavRecorder, payload_limit, segment_relative_path};
use steno_server::recovery;
use steno_server::retention::{RetentionConfig, RetentionManager};
use steno_server::storage::{Database, SegmentCompleted, SegmentStatus};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// Control slots reserved per queue (§4.3: never dropped, at least 16).
const CONTROL_SLOTS: usize = 16;

/// The running capture source.
///
/// Dropping it stops capture, which is exactly what the shutdown sequence needs:
/// a `cpal::Stream` halts when dropped, and [`net_capture::NetCaptureHandle`]
/// signals its receive thread and joins it. Keeping both behind one type lets the
/// rest of the process stay agnostic about where audio came from.
///
/// The local variant carries the format-conversion thread as well: dropping the
/// stream stops the microphone, and `LocalWireCapture::drop` stops the converter
/// once it has drained what was already captured.
#[allow(dead_code)]
enum CaptureHandle {
    Alsa {
        stream: cpal::Stream,
        local: audio_capture::LocalWireCapture,
    },
    Net(net_capture::NetCaptureHandle),
}

fn main() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    // Colour only when stderr is a terminal. Redirected to a file — which is
    // what systemd, `tools/dev.sh` and every `> log` do — escape sequences are
    // noise that breaks anything reading the log line by line.
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .init();

    let config_path = Config::resolve_path();
    let config = match Config::load(&config_path) {
        Ok(config) => config,
        Err(error) => {
            // A configuration error is fatal and must say exactly what is wrong:
            // starting with a silently-repaired config would record to the wrong
            // place, which is worse than not starting.
            eprintln!("steno-monitor: {error}");
            std::process::exit(2);
        }
    };
    let config = Arc::new(config);
    tracing::info!(path = %config_path.display(), "configuration loaded");

    if let Err(error) = run(config) {
        tracing::error!(%error, "fatal error");
        std::process::exit(1);
    }
}

fn run(config: Arc<Config>) -> steno_lib::error::Result<()> {
    let metrics = Arc::new(Metrics::new());
    let runtime = Arc::new(RuntimeStatus::new());
    // The transcript fan-out. Created before the worker and the HTTP state on
    // purpose: the worker publishes into it, HTTP subscribes to it, and neither
    // needs to know the other exists.
    let transcript_hub = steno_server::transcript_hub::shared();
    // What this process was actually started with. Read from the environment
    // again rather than from a value carried around, so it is the same path the
    // configuration was loaded from.
    runtime.set_launch(
        std::env::args().collect(),
        Some(steno_lib::config::Config::resolve_path().display().to_string()),
    );
    let shutting_down = Arc::new(AtomicBool::new(false));

    // --- storage -----------------------------------------------------------
    let database = Database::open(&config.storage.database_path)?;
    database.migrate()?;

    // The HTTP server and the DB writer must not share one connection (rusqlite
    // connections are not `Sync`), so each side opens its own from the same file.
    let http_database = Database::open(&config.storage.database_path)?;
    let db_writer = DbWriter::spawn(database, {
        let metrics = Arc::clone(&metrics);
        move |error: String| {
            metrics.set_last_error(error.clone());
            metrics.degraded.store(true, Ordering::Relaxed);
        }
    });
    let db_handle = db_writer.handle();

    // --- startup recovery (T3.6) ------------------------------------------
    // Runs before capture so it is the only writer of these paths.
    match recovery::scan(&config.storage.recording_dir, &http_database) {
        Ok(report) => {
            if !report.recovered.is_empty() {
                tracing::warn!(
                    count = report.recovered.len(),
                    "recovered interrupted recordings"
                );
            }
            if !report.restored.is_empty() {
                tracing::warn!(
                    count = report.restored.len(),
                    "restored segments a previous scan had marked missing"
                );
            }
            for partial in &report.unverifiable {
                tracing::error!(path = %partial.path.display(), reason = %partial.reason, "unverifiable partial left unpublished");
            }
            for orphan in &report.orphans {
                tracing::warn!(path = %orphan.display(), "WAV file has no database row");
            }
            if !report.unverifiable.is_empty() || !report.orphans.is_empty() {
                metrics.degraded.store(true, Ordering::Relaxed);
            }
        }
        Err(error) => {
            // A failed scan must not be papered over: continuing would record on
            // top of an inventory we know is wrong.
            tracing::error!(%error, "startup recovery scan failed");
            return Err(error);
        }
    }

    // --- capacity check (T3.6: after recovery, before recording) ----------
    let retention = RetentionManager::new(
        config.storage.recording_dir.clone(),
        RetentionConfig {
            max_bytes: config.storage.recording_max_bytes,
            cleanup_target_bytes: config.storage.recording_cleanup_target_bytes,
            reserve_bytes: config.storage.recording_reserve_bytes,
        },
    )?;
    match retention.enforce(&http_database, free_bytes(&config.storage.recording_dir)) {
        Ok(report) => {
            metrics
                .storage_pressure
                .store(report.storage_pressure, Ordering::Relaxed);
            metrics
                .recording_stopped_low_disk
                .store(report.should_stop_recording, Ordering::Relaxed);
            if report.should_stop_recording {
                tracing::error!("free space below reserve and nothing could be reclaimed");
            }
        }
        Err(error) => tracing::warn!(%error, "initial retention check failed"),
    }

    // --- capture source ---------------------------------------------------
    // Two formats have to be told apart here, and conflating them is what this
    // wiring exists to prevent:
    //
    // * the **device** format — whatever the microphone could be opened at, or,
    //   for a network source, whatever the node's own microphone clocks at;
    // * the **wire** format — 16 kHz mono S16, the one constant every consumer
    //   downstream is written against.
    //
    // Only the device format is negotiable. Everything the dispatcher, the
    // recorder, the detector and the recogniser see is the wire format, so a
    // 48 kHz stereo microphone and a 16 kHz mono node produce the same WAV.
    let mut alsa_selection: Option<audio_capture::CaptureSelection> = None;
    let device: NegotiatedFormat = match config.source.kind {
        SourceKind::Alsa => {
            let host = cpal::default_host();
            match audio_capture::select_device(&host, &config.audio) {
                Ok(selection) => {
                    let info = selection.info.clone();
                    alsa_selection = Some(selection);
                    info
                }
                Err(error) => {
                    // No microphone is a normal deployment state, not a crash:
                    // the HTTP server and Portal still come up and report
                    // `disconnected`. Only the capture-dependent status stays
                    // unknown.
                    tracing::error!(%error, "no usable capture device; starting without recording");
                    runtime.set_microphone(http_server::MicrophoneState::Disconnected);
                    metrics.set_last_error(error.to_string());
                    return serve_without_capture(config, http_database, metrics, runtime);
                }
            }
        }
        SourceKind::Net => {
            // The node converts before sending, so what arrives is already the
            // wire format; `net_capture` refuses anything that announces
            // otherwise. The node's *device* rate is not visible from here — it
            // reports it in its own INFO message, which is the honest place for
            // a fact about another host.
            NegotiatedFormat {
                device_name: format!("net:{}:{}", config.source.bind_address, config.source.port),
                rate_hz: WIRE_RATE_HZ,
                channels: WIRE_CHANNELS,
                format: SampleFormat::S16Le,
            }
        }
    };
    let wire = NegotiatedFormat {
        device_name: device.device_name.clone(),
        rate_hz: WIRE_RATE_HZ,
        channels: WIRE_CHANNELS,
        format: SampleFormat::S16Le,
    };
    runtime.set_device(device.device_name.clone());
    runtime.set_capture_format(wire.rate_hz, wire.channels, wire.format.as_str());
    runtime.set_device_format(device.rate_hz, device.channels);
    tracing::info!(
        device = %device.device_name,
        device_rate_hz = device.rate_hz,
        device_channels = device.channels,
        wire_rate_hz = wire.rate_hz,
        wire_channels = wire.channels,
        source = config.source.kind.as_str(),
        "capture format resolved; the wire format is fixed"
    );

    let ring = Arc::new(CaptureRing::new(
        wire.rate_hz,
        wire.channels,
        config.audio.capture_ring_ms,
        config.audio.period_ms,
        config.audio.buffer_ms,
    ));
    let recording_queue = Arc::new(BoundedQueue::new(
        queue_slots(config.audio.recording_queue_ms, config.audio.period_ms),
        CONTROL_SLOTS,
    ));
    let detection_queue = Arc::new(BoundedQueue::new(
        queue_slots(config.audio.detection_queue_ms, config.audio.period_ms),
        CONTROL_SLOTS,
    ));
    // The recogniser lane is deliberately short: if transcription cannot keep
    // up, dropping audio is the right answer — it must never apply backpressure
    // to recording.
    let stt_queue = starts_streaming(&config).then(|| {
        Arc::new(BoundedQueue::new(
            queue_slots(2_000, config.audio.period_ms),
            CONTROL_SLOTS,
        ))
    });

    // --- workers ----------------------------------------------------------
    let recorder = spawn_recorder_worker(
        Arc::clone(&config),
        Arc::clone(&recording_queue),
        db_handle.clone(),
        Arc::clone(&metrics),
    )?;
    let detector = spawn_detector_worker(
        Arc::clone(&config),
        Arc::clone(&detection_queue),
        db_handle.clone(),
        Arc::clone(&metrics),
    )?;

    let mut dispatcher = dispatcher::spawn(DispatcherInputs {
        config: Arc::clone(&config),
        metrics: Arc::clone(&metrics),
        capture: Arc::clone(&ring),
        recording_queue: Arc::clone(&recording_queue),
        detection_queue: Arc::clone(&detection_queue),
        stt_queue: stt_queue.clone(),
        db: db_handle.clone(),
        shutting_down: Arc::clone(&shutting_down),
        negotiated: wire.clone(),
    });

    // Transcription sits entirely off the audio path: it reads finished segments
    // from the database, so it can start before capture and can never block it.
    let stt_worker = start_transcription(
        &config,
        Arc::clone(&shutting_down),
        stt_queue.clone(),
        Arc::clone(&metrics),
        Arc::clone(&transcript_hub),
    );

    let capture = match config.source.kind {
        SourceKind::Alsa => {
            let selection = alsa_selection.take().ok_or_else(|| {
                steno_lib::error::Error::Internal(
                    "alsa source selected but no device was resolved".into(),
                )
            })?;
            // The callback pushes the device's own frames into `device_ring`; a
            // converter thread turns them into wire blocks in `ring`, which is
            // what the dispatcher and therefore everything else reads. This is
            // the single-box equivalent of what the node does before sending:
            // one conversion, next to the microphone, and every consumer after
            // it speaks one format.
            let device_ring = Arc::new(CaptureRing::new(
                device.rate_hz,
                device.channels,
                config.audio.capture_ring_ms,
                config.audio.period_ms,
                config.audio.buffer_ms,
            ));
            let mut local = audio_capture::LocalWireCapture::spawn(
                Arc::clone(&device_ring),
                Arc::clone(&ring),
                &device,
                config.audio.mono_channel_index,
                Arc::clone(&metrics),
            )
            .map_err(|error| steno_lib::error::Error::Audio(format!("capture format: {error}")))?;
            match audio_capture::start_stream(
                selection,
                Arc::clone(&device_ring),
                Arc::clone(&metrics),
                Arc::clone(&shutting_down),
                1,
            ) {
                Ok(stream) => CaptureHandle::Alsa { stream, local },
                Err(error) => {
                    tracing::error!(%error, "failed to start capture stream");
                    metrics.set_last_error(error.to_string());
                    runtime.set_microphone(http_server::MicrophoneState::Disconnected);
                    // The dispatcher is already running and will simply never see
                    // audio; shut it down cleanly instead of exiting with threads
                    // mid-flight.
                    local.stop();
                    shutting_down.store(true, Ordering::Relaxed);
                    dispatcher.request_shutdown();
                    recording_queue.close();
                    detection_queue.close();
                    let _ =
                        dispatcher.join(Duration::from_secs(config.audio.shutdown_timeout_seconds));
                    return serve_without_capture(config, http_database, metrics, runtime);
                }
            }
        }
        SourceKind::Net => {
            let net_config = net_capture::NetCaptureConfig {
                bind_address: config.source.bind_address.clone(),
                port: config.source.port,
                device_id: config.source.device_id,
                jitter_buffer_ms: config.source.jitter_buffer_ms,
                idle_timeout_ms: config.source.idle_timeout_ms,
            };
            match net_capture::spawn(net_config, Arc::clone(&ring), Arc::clone(&metrics)) {
                Ok(handle) => CaptureHandle::Net(handle),
                Err(error) => {
                    tracing::error!(%error, "failed to start net capture");
                    metrics.set_last_error(error.to_string());
                    shutting_down.store(true, Ordering::Relaxed);
                    dispatcher.request_shutdown();
                    recording_queue.close();
                    detection_queue.close();
                    let _ =
                        dispatcher.join(Duration::from_secs(config.audio.shutdown_timeout_seconds));
                    return serve_without_capture(config, http_database, metrics, runtime);
                }
            }
        }
    };

    runtime.set_running(true);
    runtime.set_recording(RecordingState::Ok);
    // With a network source "ok" at this point would be a lie: the socket is
    // listening, but no node has attached yet. The status page derives the real
    // value from `net_source_connected` in that case.
    if config.source.kind == SourceKind::Alsa {
        runtime.set_microphone(MicrophoneState::Ok);
    } else {
        runtime.set_microphone(MicrophoneState::Disconnected);
    }
    tracing::info!("recording pipeline running");

    // --- HTTP + signal handling (blocking) --------------------------------
    let http_state = Arc::new(
        HttpState::new(
            (*config).clone(),
            Arc::new(Mutex::new(http_database)),
            Arc::clone(&metrics),
            Arc::clone(&runtime),
        )
        .with_transcript_hub(Arc::clone(&transcript_hub))
        .with_web_root(web_root(&config)),
    );

    let http_threads = config.server.http_threads.clamp(1, 8);
    let server = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(http_threads)
        .enable_all()
        .build()
        .map_err(|e| steno_lib::error::Error::Internal(format!("tokio runtime: {e}")))?;

    let (shutdown_tx, shutdown_rx) = std::sync::mpsc::channel::<()>();
    std::thread::Builder::new()
        .name("steno-signal".into())
        .spawn(move || {
            // §4.3: SIGTERM/SIGINT begin the graceful sequence below.
            if wait_for_termination().is_ok() {
                let _ = shutdown_tx.send(());
            }
        })
        .map_err(|e| steno_lib::error::Error::Internal(format!("signal thread: {e}")))?;

    let server_state = Arc::clone(&http_state);
    // The HTTP listener has to be told to stop: `axum::serve` never returns by
    // itself, so without this the join below would block forever after SIGTERM
    // (and `systemctl stop` would sit there until its own TimeoutStopSec).
    let (http_stop_tx, http_stop_rx) = tokio::sync::watch::channel(false);
    let server_thread = thread::Builder::new()
        .name("steno-http".into())
        .spawn(move || {
            let result = server.block_on(async move {
                let mut stop_rx = http_stop_rx;
                let stop = async move {
                    // Resolves when the flag flips; the sender is dropped on
                    // shutdown too, which also ends the wait.
                    let _ = stop_rx.changed().await;
                };
                http_server::serve_with_shutdown(server_state, stop).await
            });
            if let Err(error) = result {
                tracing::error!(%error, "HTTP server stopped");
            }
        })
        .map_err(|e| steno_lib::error::Error::Internal(format!("http thread: {e}")))?;

    // Block until a termination signal arrives.
    let _ = shutdown_rx.recv();
    tracing::info!("shutdown requested");
    shutting_down.store(true, Ordering::Relaxed);
    runtime.set_running(false);
    runtime.set_recording(RecordingState::Stopped);

    // --- graceful shutdown, in the documented order -----------------------
    let deadline = Instant::now() + Duration::from_secs(config.audio.shutdown_timeout_seconds);

    // 1. Stop capture first so no new audio enters the ring.
    drop(capture);
    // 2. Ask the dispatcher to flush its remaining block and close the segment.
    dispatcher.request_shutdown();
    if !dispatcher.join(remaining(deadline)) {
        tracing::error!(
            "dispatcher did not stop before the shutdown timeout; leaving .partial for recovery"
        );
    }
    // 3. Close the queues so both workers drain what remains and exit.
    recording_queue.close();
    detection_queue.close();
    // 4. Wait for the recorder (it completes the segment) and detector (it flushes
    //    pending events) before draining the DB writer, so their writes are seen.
    let recorder_ok = join_worker(recorder, "recorder", remaining(deadline));
    let detector_ok = join_worker(detector, "detector", remaining(deadline));
    drop(db_handle);
    if !db_writer.shutdown(remaining(deadline)) {
        tracing::error!("database writer did not drain before the shutdown timeout");
    }
    // 5. The transcription worker owns its own connection and observes the same
    //    shutdown flag; join it so a sidecar process is not left behind.
    if let Some(handle) = stt_worker {
        let _ = handle.join();
    }
    // 6. Last, close the listener. Everything the operator could still want to
    //    read during the drain stays reachable until here.
    let _ = http_stop_tx.send(true);
    let _ = server_thread.join();

    if recorder_ok && detector_ok {
        tracing::info!("shutdown complete");
    }
    Ok(())
}

/// Serves the Portal and API with no capture pipeline, used when no microphone is
/// available so the operator can still see why. It never claims to be recording.
///
/// This still honours SIGTERM/SIGINT: without a capture pipeline there is no
/// segment to close, but the process must remain stoppable like any other
/// service rather than needing `kill -9`.
/// True when audio should be mirrored into the recogniser lane.
fn starts_streaming(config: &Config) -> bool {
    config.stt.enabled && config.stt.mode == steno_lib::config::SttMode::Stream
}

/// Starts the speech-to-text worker when `[stt] enabled = true`.
///
/// Returns `None` when transcription is off, and also when the worker thread
/// cannot be created: a missing recogniser must never stop the recorder.
fn start_transcription(
    config: &Config,
    shutting_down: Arc<AtomicBool>,
    stt_queue: Option<Arc<BoundedQueue<steno_server::stt_stream::SttChunk>>>,
    metrics: Arc<Metrics>,
    hub: Arc<steno_server::transcript_hub::TranscriptHub>,
) -> Option<std::thread::JoinHandle<()>> {
    if !config.stt.enabled {
        return None;
    }
    metrics.set_stt_engine(config.stt.engine.clone());
    if config.stt.mode == steno_lib::config::SttMode::Stream {
        let Some(queue) = stt_queue else {
            tracing::warn!("streaming speech-to-text has no audio lane; it will stay idle");
            return None;
        };
        let socket_path = if config.stt.socket_path.trim().is_empty() {
            std::env::temp_dir().join(format!("steno-stt-{}.sock", std::process::id()))
        } else {
            std::path::PathBuf::from(config.stt.socket_path.trim())
        };
        // Recorded before the worker starts so the control centre shows the
        // socket that is really in use (an empty config value is resolved here).
        metrics.set_stt_launch(
            config.stt.stream_command.clone(),
            vec![socket_path.display().to_string()],
        );
        let options = steno_server::stt_stream::StreamOptions {
            socket_path,
            launcher: config.stt.stream_command.clone(),
            engine: config.stt.engine.clone(),
            locale: config.stt.locale.clone(),
            database_path: config.storage.database_path.clone(),
            ready_timeout: Duration::from_secs(30),
            restart_delay: Duration::from_secs(5),
        };
        return match steno_server::stt_stream::spawn(queue, options, metrics, shutting_down, hub) {
            Ok(handle) => {
                tracing::info!(engine = %config.stt.engine, "streaming speech-to-text enabled");
                Some(handle)
            }
            Err(error) => {
                tracing::error!(%error, "cannot start the streaming speech-to-text worker");
                None
            }
        };
    }
    metrics.set_stt_launch(config.stt.command.clone(), config.stt.args.clone());
    let transcriber: Arc<dyn steno_server::stt::Transcriber> =
        Arc::new(steno_server::stt::CommandTranscriber::new(
            config.stt.command.clone(),
            config.stt.args.clone(),
            config.stt.engine.clone(),
        ));
    let options = steno_server::stt::WorkerOptions {
        database_path: config.storage.database_path.clone(),
        recording_dir: config.storage.recording_dir.clone(),
        engine: config.stt.engine.clone(),
        min_segment_seconds: config.stt.min_segment_seconds as f64,
        poll_interval: Duration::from_secs(config.stt.poll_interval_seconds),
        batch: 4,
    };
    match steno_server::stt::spawn(transcriber, options, shutting_down) {
        Ok(handle) => {
            tracing::info!(engine = %config.stt.engine, "speech-to-text enabled");
            Some(handle)
        }
        Err(error) => {
            tracing::error!(%error, "cannot start the speech-to-text worker");
            None
        }
    }
}

fn serve_without_capture(
    config: Arc<Config>,
    database: Database,
    metrics: Arc<Metrics>,
    runtime: Arc<RuntimeStatus>,
) -> Result<()> {
    runtime.set_recording(RecordingState::Stopped);
    let state = Arc::new(
        HttpState::new(
            (*config).clone(),
            Arc::new(Mutex::new(database)),
            metrics,
            Arc::clone(&runtime),
        )
        .with_web_root(web_root(&config)),
    );
    let threads = config.server.http_threads.clamp(1, 8);
    let server = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(threads)
        .enable_all()
        .build()
        .map_err(|e| Error::Internal(format!("tokio runtime: {e}")))?;

    let (shutdown_tx, shutdown_rx) = std::sync::mpsc::channel::<()>();
    thread::Builder::new()
        .name("steno-signal".into())
        .spawn(move || {
            if wait_for_termination().is_ok() {
                let _ = shutdown_tx.send(());
            }
        })
        .map_err(|e| Error::Internal(format!("signal thread: {e}")))?;

    server.block_on(async move {
        let serve = http_server::serve(state);
        let stop = async move {
            // `recv` blocks, so it runs on the blocking pool to avoid stalling
            // the async worker threads.
            let _ = tokio::task::spawn_blocking(move || shutdown_rx.recv()).await;
        };
        tokio::select! {
            result = serve => result.map_err(|e| Error::Internal(format!("http server: {e}"))),
            () = stop => {
                tracing::info!("shutdown requested");
                Ok(())
            }
        }
    })
}

/// Recorder worker: owns the WAV writer and reacts to dispatcher messages.
fn spawn_recorder_worker(
    config: Arc<Config>,
    queue: Arc<BoundedQueue<DispatcherMessage>>,
    db: steno_server::db_writer::DbWriterHandle,
    metrics: Arc<Metrics>,
) -> steno_lib::error::Result<JoinHandle<bool>> {
    thread::Builder::new()
        .name("steno-recorder".into())
        .spawn(move || {
            let mut open: Option<(String, WavRecorder, SegmentEndMeta)> = None;
            let mut ok = true;
            loop {
                match queue.pop_timeout(Duration::from_millis(100)) {
                    PopOutcome::Item(message) => {
                        if let Err(error) =
                            handle_recorder_message(&config, &db, &metrics, &mut open, message)
                        {
                            // A disk error must be visible and must not corrupt the
                            // segment silently: abandon the partial for recovery.
                            tracing::error!(%error, "recorder failed");
                            metrics.set_last_error(error.to_string());
                            metrics.degraded.store(true, Ordering::Relaxed);
                            ok = false;
                        }
                    }
                    PopOutcome::Empty => {}
                    PopOutcome::Closed => break,
                }
            }
            // Queue closed: flush whatever segment is still open.
            if let Some((id, recorder, meta)) = open.take()
                && let Err(error) =
                    finish_segment(&db, id, recorder, meta, SegmentEndReason::Shutdown)
            {
                tracing::error!(%error, "failed to finish the final segment");
                ok = false;
            }
            ok
        })
        .map_err(|e| steno_lib::error::Error::Internal(format!("recorder thread: {e}")))
}

/// Ending metadata the recorder needs to write a complete row.
#[derive(Debug, Clone)]
struct SegmentEndMeta {
    started_at_utc: chrono::DateTime<chrono::Utc>,
    capture_rate_hz: u32,
}

fn handle_recorder_message(
    config: &Config,
    db: &steno_server::db_writer::DbWriterHandle,
    metrics: &Metrics,
    open: &mut Option<(String, WavRecorder, SegmentEndMeta)>,
    message: DispatcherMessage,
) -> steno_lib::error::Result<()> {
    match message {
        DispatcherMessage::SegmentStart {
            segment_id,
            started_at_utc,
            format,
            ..
        } => {
            // A segment can only start once the previous one has been closed.
            if let Some((id, recorder, meta)) = open.take() {
                finish_segment(db, id, recorder, meta, SegmentEndReason::Duration)?;
            }
            let rel = segment_relative_path(started_at_utc, &segment_id);
            let wav_format = WavFormat::new(format.rate_hz, format.channels)?;
            let recorder = WavRecorder::create(
                &config.storage.recording_dir,
                &rel,
                wav_format,
                payload_limit(&config.audio),
                Duration::from_secs(config.audio.header_flush_interval_seconds),
            )?;
            *open = Some((
                segment_id,
                recorder,
                SegmentEndMeta {
                    started_at_utc,
                    capture_rate_hz: format.rate_hz,
                },
            ));
        }
        DispatcherMessage::Audio {
            segment_id, pcm, ..
        } => {
            let Some((current_id, recorder, _)) = open.as_mut() else {
                // Audio for a segment we never started: the dispatcher always
                // sends SegmentStart first, so this is a real ordering bug and
                // must be reported, not written into the wrong file.
                return Err(steno_lib::error::Error::Internal(format!(
                    "audio arrived for segment {segment_id} before its SegmentStart"
                )));
            };
            if current_id != &segment_id {
                return Err(steno_lib::error::Error::Internal(format!(
                    "audio for segment {segment_id} while {current_id} is open"
                )));
            }
            match recorder.append_s16(&pcm) {
                Ok(_) => {}
                Err(error) => {
                    // The writer refused the block (usually the size limit). Do
                    // not drop it silently: report so the dispatcher's rotation
                    // can be revisited, and keep the partial recoverable.
                    metrics.set_last_error(error.to_string());
                    return Err(error);
                }
            }
        }
        DispatcherMessage::Gap { .. } => {
            // Gaps are registered with the DB by the dispatcher; the recorder only
            // needs to know the stored-frame timeline, which the dispatcher owns.
        }
        DispatcherMessage::SegmentEnd {
            segment_id,
            end_reason,
            ..
        } => {
            let Some((current_id, recorder, meta)) = open.take() else {
                return Err(steno_lib::error::Error::Internal(format!(
                    "SegmentEnd for {segment_id} with no open segment"
                )));
            };
            if current_id != segment_id {
                return Err(steno_lib::error::Error::Internal(format!(
                    "SegmentEnd for {segment_id} while {current_id} is open"
                )));
            }
            finish_segment(db, current_id, recorder, meta, end_reason)?;
        }
    }
    Ok(())
}

/// Completes a segment: update the header, sync, rename, then tell the DB.
fn finish_segment(
    db: &steno_server::db_writer::DbWriterHandle,
    id: String,
    recorder: WavRecorder,
    meta: SegmentEndMeta,
    reason: SegmentEndReason,
) -> steno_lib::error::Result<()> {
    let (_path, frames, size_bytes) = recorder.close()?;
    let ended_at = meta.started_at_utc
        + chrono::Duration::from_std(Duration::from_secs_f64(
            frames as f64 / f64::from(meta.capture_rate_hz.max(1)),
        ))
        .unwrap_or_else(|_| chrono::Duration::zero());
    db.segment_completed(SegmentCompleted {
        id,
        ended_at_utc: ended_at,
        frame_count: frames,
        size_bytes,
        end_reason: reason.as_str().to_string(),
        clock_drift_ppm: None,
        xrun_count: 0,
        gap_count: 0,
        lost_frames_total: 0,
        status: SegmentStatus::Complete,
    })
    .map_err(steno_lib::error::Error::Internal)
}

/// Detector worker: owns the DSP pipeline and turns audio into events.
fn spawn_detector_worker(
    config: Arc<Config>,
    queue: Arc<BoundedQueue<DispatcherMessage>>,
    db: DbWriterHandle,
    metrics: Arc<Metrics>,
) -> Result<JoinHandle<bool>> {
    thread::Builder::new()
        .name("steno-detector".into())
        .spawn(move || {
            let mut worker = DetectorWorker::new(config, db, metrics);
            let mut ok = true;
            loop {
                match queue.pop_timeout(Duration::from_millis(100)) {
                    PopOutcome::Item(message) => {
                        if let Err(error) = worker.handle(message) {
                            // T3.7: a detector failure must not stop recording. The
                            // pipeline is dropped and rebuilt on the next segment,
                            // and the status page shows the degradation.
                            tracing::error!(%error, "detector failed");
                            worker.fail(error);
                            ok = false;
                        }
                    }
                    PopOutcome::Empty => worker.publish_status(),
                    PopOutcome::Closed => break,
                }
            }
            // Queue closed: flush any pending candidate with `end_reason=shutdown`.
            if let Err(error) = worker.flush_on_shutdown() {
                tracing::error!(%error, "failed to flush the final events");
                ok = false;
            }
            ok
        })
        .map_err(|e| Error::Internal(format!("detector thread: {e}")))
}

/// Detector thread state. Kept as a struct so a failure can reset the pipeline
/// and the format tracking together, with no way to leave them inconsistent.
struct DetectorWorker {
    config: Arc<Config>,
    db: DbWriterHandle,
    metrics: Arc<Metrics>,
    pipeline: Option<DetectionPipeline>,
    /// Capture format the current pipeline was built for, used to detect the
    /// `format_change` case without reaching into the pipeline's private fields.
    format: Option<(u32, u16)>,
}

impl DetectorWorker {
    fn new(config: Arc<Config>, db: DbWriterHandle, metrics: Arc<Metrics>) -> Self {
        DetectorWorker {
            config,
            db,
            metrics,
            pipeline: None,
            format: None,
        }
    }

    fn handle(&mut self, message: DispatcherMessage) -> Result<()> {
        match message {
            DispatcherMessage::SegmentStart {
                segment_id,
                started_at_utc,
                format,
                time_quality,
                boot_id,
            } => {
                let next_format = (format.rate_hz, format.channels);
                let format_changed = self.format.is_some_and(|current| current != next_format);
                if self.pipeline.is_none() || format_changed {
                    let pipeline = DetectionPipeline::new(
                        self.config.detector.clone(),
                        format.rate_hz,
                        format.channels,
                        // Always channel 0: the dispatcher only ever receives the
                        // wire format, which is mono. Which *device* channel
                        // became that mono stream was decided at capture (by the
                        // node, or by `WireConverter` on the single-box path), so
                        // there is nothing left to select here — and selecting a
                        // channel that does not exist would be an error rather
                        // than a config choice.
                        0,
                    )
                    .map_err(|e| Error::Internal(format!("detector setup: {e}")))?;
                    self.pipeline = Some(pipeline);
                    self.format = Some(next_format);
                }
                let context = SegmentContext {
                    segment_id,
                    capture_rate_hz: format.rate_hz,
                    started_at_utc,
                    offset_frames_at_anchor: 0,
                    gaps: Vec::new(),
                    time_quality: time_quality.as_str().to_string(),
                    boot_id,
                };
                let drained = self
                    .pipeline
                    .as_mut()
                    .expect("pipeline was just ensured")
                    .begin_segment(context, 0, format_changed);
                self.deliver(drained)
            }
            DispatcherMessage::Audio {
                pcm,
                segment_offset_frames,
                ..
            } => {
                let Some(pipeline) = self.pipeline.as_mut() else {
                    // Audio before any SegmentStart: the dispatcher guarantees the
                    // ordering, so this is not expected. Ignoring is safe here
                    // because there is no segment context to attribute events to.
                    return Ok(());
                };
                let mut drained = Vec::new();
                pipeline
                    .process_s16_block(&pcm, segment_offset_frames, &mut drained)
                    .map_err(|e| Error::Internal(format!("detector: {e}")))?;
                self.deliver(drained)
            }
            DispatcherMessage::Gap {
                lost_frames,
                offset_frames,
                ..
            } => {
                // §5.4: a gap resets the detector so no event spans the hole.
                if let Some(pipeline) = self.pipeline.as_mut() {
                    let drained =
                        pipeline.discontinuity(Discontinuity::Gap, offset_frames, lost_frames);
                    self.deliver(drained)?;
                }
                Ok(())
            }
            DispatcherMessage::SegmentEnd { end_reason, .. } => {
                if let Some(pipeline) = self.pipeline.as_mut() {
                    let drained = pipeline.close(map_end_reason(end_reason), false);
                    self.deliver(drained)?;
                }
                Ok(())
            }
        }
    }

    /// Sends freshly produced events to the DB writer and counts them. An event
    /// that cannot be enqueued is reported rather than silently dropped.
    fn deliver(&mut self, drained: Vec<EventDraft>) -> Result<()> {
        for event in drained {
            self.metrics.events_total.fetch_add(1, Ordering::Relaxed);
            self.db
                .event(event)
                .map_err(|e| Error::Internal(format!("event write failed: {e}")))?;
        }
        Ok(())
    }

    /// §4.3: pending events are flushed with `end_reason=shutdown` at exit.
    fn flush_on_shutdown(&mut self) -> Result<()> {
        let Some(pipeline) = self.pipeline.as_mut() else {
            return Ok(());
        };
        let drained = pipeline.close(EndReason::Shutdown, false);
        self.deliver(drained)
    }

    /// Mirrors the detector's own state into the status page, but only while the
    /// pipeline is healthy: a failed detector reports `unknown`, never a stale
    /// "idle" that would imply it is watching.
    fn publish_status(&self) {
        let Some(pipeline) = self.pipeline.as_ref() else {
            return;
        };
        self.metrics
            .set_detector_state(map_detector_state(pipeline.state()));
        self.metrics
            .set_noise_floor_dbfs(pipeline.noise_floor_dbfs());
        self.metrics
            .set_band_noise_floor_dbfs(pipeline.band_noise_floor_dbfs());
        self.metrics
            .signal_silent
            .store(pipeline.signal_is_silent(), Ordering::Relaxed);
    }

    fn fail(&mut self, error: Error) {
        self.metrics.set_last_error(error.to_string());
        self.metrics.set_detector_state(DetectorState::Unknown);
        self.metrics.degraded.store(true, Ordering::Relaxed);
        self.pipeline = None;
        self.format = None;
    }
}

fn map_detector_state(state: State) -> DetectorState {
    match state {
        State::WarmingUp => DetectorState::WarmingUp,
        State::Idle => DetectorState::Idle,
        State::Candidate => DetectorState::Candidate,
        State::Pending => DetectorState::Pending,
    }
}

fn map_end_reason(reason: SegmentEndReason) -> EndReason {
    match reason {
        SegmentEndReason::Duration => EndReason::MaxDuration,
        SegmentEndReason::Gap => EndReason::Gap,
        SegmentEndReason::TimeStep => EndReason::Gap,
        SegmentEndReason::FormatChange => EndReason::SegmentEnd,
        SegmentEndReason::SizeLimit => EndReason::SegmentEnd,
        SegmentEndReason::Shutdown => EndReason::Shutdown,
        SegmentEndReason::Error => EndReason::SegmentEnd,
    }
}

/// Converts a configured millisecond queue depth into audio slots.
fn queue_slots(queue_ms: u32, period_ms: u32) -> usize {
    let period = period_ms.max(1) as usize;
    (queue_ms as usize).div_ceil(period).max(1)
}

/// Resolves `server.web_dir`, falling back to the historical
/// "`<recording_dir>/../web`" guess for configs written before the field existed.
fn web_root(config: &Config) -> PathBuf {
    if !config.server.web_dir.trim().is_empty() {
        return PathBuf::from(&config.server.web_dir);
    }
    config
        .storage
        .recording_dir
        .parent()
        .map(|parent| parent.join("web"))
        .unwrap_or_else(|| PathBuf::from("web"))
}

/// Remaining time until `deadline`, never negative.
fn remaining(deadline: Instant) -> Duration {
    deadline.saturating_duration_since(Instant::now())
}

fn join_worker(handle: JoinHandle<bool>, name: &str, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while !handle.is_finished() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
    if !handle.is_finished() {
        tracing::error!(
            worker = name,
            "worker did not stop before the shutdown timeout"
        );
        return false;
    }
    match handle.join() {
        Ok(ok) => ok,
        Err(_) => {
            tracing::error!(worker = name, "worker panicked");
            false
        }
    }
}

fn free_bytes(path: &std::path::Path) -> Option<u64> {
    let info = fs2_free(path)?;
    Some(info)
}

#[cfg(unix)]
fn fs2_free(path: &std::path::Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: `statvfs` fills the struct we pass and only reads `c_path`.
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) } != 0 {
        return None;
    }
    // `statvfs` field widths differ between platforms (`f_bavail` is u32 on
    // macOS and u64 on Linux), so the product is computed in u64 explicitly.
    let available = u64::from(stat.f_bavail);
    let fragment = stat.f_frsize as u64;
    Some(available.saturating_mul(fragment))
}

#[cfg(not(unix))]
fn fs2_free(_path: &std::path::Path) -> Option<u64> {
    None
}

/// Blocks until SIGTERM/SIGINT arrives.
///
/// Uses the classic self-pipe trick: the signal handler does one async-signal-safe
/// `write`, and this thread blocks in `read`. That keeps all real shutdown work
/// out of the handler, where almost nothing is safe to call.
#[cfg(unix)]
fn wait_for_termination() -> std::result::Result<(), ()> {
    let (read_fd, write_fd) = pipe_pair().ok_or(())?;
    SIGNAL_WRITE_FD.store(write_fd, Ordering::Relaxed);

    // SAFETY: `handle_signal` is an `extern "C" fn` matching what the C signal
    // API expects. The cast goes through the function-pointer type instead of
    // casting the function item straight to an integer.
    unsafe {
        let handler = handle_signal as extern "C" fn(libc::c_int);
        libc::signal(libc::SIGTERM, handler as libc::sighandler_t);
        libc::signal(libc::SIGINT, handler as libc::sighandler_t);
    }

    let mut byte = [0u8; 1];
    // SAFETY: reads one byte from our own pipe; it stays open for the process
    // lifetime, so this blocks until the handler writes or the process dies.
    let read = unsafe { libc::read(read_fd, byte.as_mut_ptr().cast(), 1) };
    // The read end is no longer needed once a signal has arrived.
    unsafe { libc::close(read_fd) };
    if read > 0 { Ok(()) } else { Err(()) }
}

#[cfg(not(unix))]
fn wait_for_termination() -> std::result::Result<(), ()> {
    // No signal handling on this platform; the caller exits without a graceful
    // shutdown rather than pretending one happened.
    Err(())
}

#[cfg(unix)]
static SIGNAL_WRITE_FD: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);

#[cfg(unix)]
extern "C" fn handle_signal(_signal: libc::c_int) {
    let fd = SIGNAL_WRITE_FD.load(Ordering::Relaxed);
    if fd >= 0 {
        let byte = [1u8];
        // SAFETY: `write` to a pipe is async-signal-safe, which is the only kind
        // of call permitted inside a signal handler.
        unsafe {
            libc::write(fd, byte.as_ptr().cast(), 1);
        }
    }
}

#[cfg(unix)]
fn pipe_pair() -> Option<(libc::c_int, libc::c_int)> {
    let mut fds = [0 as libc::c_int; 2];
    // SAFETY: `pipe` writes both descriptors into the array we own.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return None;
    }
    Some((fds[0], fds[1]))
}
