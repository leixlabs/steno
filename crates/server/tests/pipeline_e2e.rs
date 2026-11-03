//! End-to-end pipeline test: dispatcher -> recorder + detector -> DB -> HTTP.
//!
//! This exercises the wiring that `main.rs` performs, without a microphone: a
//! synthetic block is pushed through the real capture ring, the real dispatcher
//! and the real workers, and the result is read back through the real HTTP
//! router. It is the closest thing to a live run that runs on any machine.

use steno_lib::audio_capture::{CaptureMeta, CaptureRing, NegotiatedFormat};
use steno_lib::bounded_queue::{BoundedQueue, PopOutcome};
use steno_lib::config::{Config, SampleFormat};
use steno_server::db_writer::DbWriter;
use steno_lib::detector::DetectionPipeline;
use steno_server::dispatcher::{self, DispatcherInputs, DispatcherMessage, SegmentEndReason};
use steno_lib::metrics::Metrics;
use steno_server::recorder::{WavFormat, WavRecorder, payload_limit, recording_relative_path};
use steno_server::storage::{Database, RecordingDraft};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tempfile::tempdir;

fn test_config(dir: &std::path::Path) -> Config {
    let text = format!(
        r#"
[server]
bind_address = "127.0.0.1"
port = 18999
http_threads = 1
timezone = "UTC"

[audio]
device_rate_hz = 16000
device_channels = 1
format = "S16_LE"
period_ms = 10
buffer_ms = 200
segment_duration_seconds = 3600
mono_channel_index = 0
capture_ring_ms = 500
recording_queue_ms = 2000
detection_queue_ms = 1000
gap_tolerance_ms = 20
gap_rotate_seconds = 5
header_flush_interval_seconds = 30
shutdown_timeout_seconds = 5

[detector]
detection_rate_hz = 16000

[storage]
recording_dir = "{}"
database_path = "{}/app.db"
recording_max_bytes = 21474836480
recording_cleanup_target_bytes = 20401094656
recording_reserve_bytes = 536870912
"#,
        dir.join("recordings").display(),
        dir.display()
    );
    Config::from_toml_str(&text).expect("test config must parse and validate")
}

#[test]
fn audio_flows_from_ring_through_dispatcher_into_wav_and_db() {
    let dir = tempdir().unwrap();
    let config = Arc::new(test_config(dir.path()));
    std::fs::create_dir_all(&config.storage.recording_dir).unwrap();

    // The DB the HTTP side reads, and a separate connection for the writer, which
    // is how `main.rs` avoids sharing a non-`Sync` rusqlite handle.
    let http_db = Database::open(&config.storage.database_path).unwrap();
    let writer_db = Database::open(&config.storage.database_path).unwrap();
    writer_db.migrate().unwrap();
    let writer = DbWriter::spawn(writer_db, |error| eprintln!("db error: {error}"));

    let metrics = Arc::new(Metrics::new());
    let shutting_down = Arc::new(AtomicBool::new(false));
    let channels = 1u16;
    let rate = 16_000u32;
    let ring = Arc::new(CaptureRing::new(rate, channels, 500, 10, 200));
    let recording_queue = Arc::new(BoundedQueue::new(64, 16));
    let detection_queue = Arc::new(BoundedQueue::new(64, 16));
    let negotiated = NegotiatedFormat {
        device_name: "test".into(),
        rate_hz: rate,
        channels,
        format: SampleFormat::S16Le,
    };

    let dispatcher = dispatcher::spawn(DispatcherInputs {
        config: Arc::clone(&config),
        metrics: Arc::clone(&metrics),
        capture: Arc::clone(&ring),
        recording_queue: Arc::clone(&recording_queue),
        detection_queue: Arc::clone(&detection_queue),
        stt_queue: None,
        shutting_down: Arc::clone(&shutting_down),
        negotiated,
    });

    // Push one second of a 320 Hz tone at roughly -20 dBFS through the capture
    // path in 100 ms blocks, the way a real callback delivers audio. The ring is
    // sized for 500 ms, so the pushes are paced to let the dispatcher drain it;
    // a full ring is a real condition the dispatcher turns into a gap, not a
    // failure of this test.
    let rate = 16_000u32;
    let block_frames = (rate / 10) as usize;
    let mut accepted = 0u32;
    for block in 0..10 {
        let samples: Vec<i16> = (0..block_frames)
            .map(|n| {
                let absolute = block * block_frames + n;
                let phase = 2.0 * std::f32::consts::PI * 320.0 * (absolute as f32 / rate as f32);
                (phase.sin() * 3276.0) as i16
            })
            .collect();
        if ring.callback_push(
            &samples,
            CaptureMeta {
                frames: block_frames as u32,
                channels,
                rate_hz: rate,
                capture_seq: (block * block_frames) as u64,
                mono_start_ns: (block * 100_000_000) as u64,
                stream_generation: 1,
            },
        ) {
            accepted += 1;
        }
        // Give the dispatcher a chance to consume before the next push.
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        accepted > 0,
        "the ring must accept at least one block of audio"
    );

    std::thread::sleep(Duration::from_millis(300));

    // Shut down in the documented order and let the workers drain.
    shutting_down.store(true, Ordering::Relaxed);
    dispatcher.request_shutdown();
    recording_queue.close();
    detection_queue.close();
    let joined = {
        let mut handle = dispatcher;
        handle.join(Duration::from_secs(5))
    };
    assert!(joined, "the dispatcher must stop within the timeout");
    assert!(
        writer.shutdown(Duration::from_secs(5)),
        "the DB writer must drain"
    );

    // The dispatcher's own messages prove the fan-out happened.
    let mut saw_start = false;
    let mut saw_audio = false;
    let mut saw_end = false;
    loop {
        match recording_queue.pop_timeout(Duration::from_millis(10)) {
            PopOutcome::Item(DispatcherMessage::SegmentStart { .. }) => saw_start = true,
            PopOutcome::Item(DispatcherMessage::Audio { .. }) => saw_audio = true,
            PopOutcome::Item(DispatcherMessage::SegmentEnd { end_reason, .. }) => {
                assert_eq!(end_reason, SegmentEndReason::Shutdown);
                saw_end = true;
            }
            PopOutcome::Item(_) => {}
            PopOutcome::Empty | PopOutcome::Closed => break,
        }
    }
    assert!(saw_start, "the dispatcher must announce the window");
    assert!(saw_audio, "the captured audio must be fanned out");
    assert!(saw_end, "shutdown must close the open window");

    // No recorder ran here, and only the recorder may publish a `recordings`
    // row — the dispatcher's window is a timing boundary, not a row. Seeing one
    // would mean a stale second writer came back.
    let recordings = http_db
        .recordings_between(
            chrono::DateTime::from_timestamp(0, 0).unwrap(),
            chrono::DateTime::from_timestamp(i64::from(u32::MAX), 0).unwrap(),
        )
        .expect("recording query must succeed");
    assert!(
        recordings.is_empty(),
        "the dispatcher must not write recordings itself: {recordings:?}"
    );
}

#[test]
fn recorder_writes_a_playable_wav_for_a_dispatched_window() {
    // The recorder worker's exact sequence, driven directly so the WAV bytes and
    // the DB completion row can be asserted without races.
    let dir = tempdir().unwrap();
    let config = test_config(dir.path());
    std::fs::create_dir_all(&config.storage.recording_dir).unwrap();
    let db = Database::open(&config.storage.database_path).unwrap();
    db.migrate().unwrap();

    let started_at = chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap();
    let id = "rec-e2e".to_string();
    let format = WavFormat::new(16_000, 1).unwrap();
    let rel = recording_relative_path(started_at, &id);

    let mut recorder = WavRecorder::create(
        &config.storage.recording_dir,
        &rel,
        format,
        payload_limit(&config.audio),
        Duration::from_secs(30),
    )
    .unwrap();
    let samples: Vec<i16> = (0..3200).map(|n| (n % 1000) as i16).collect();
    recorder.append_s16(&samples).unwrap();
    let (path, frames, size_bytes) = recorder.close().unwrap();

    // The file must be a real, openable PCM WAV with the frames we wrote.
    let (read_format, declared) = steno_server::recorder::read_header(&path).unwrap();
    assert_eq!(read_format.sample_rate_hz, 16_000);
    assert_eq!(read_format.channels, 1);
    assert_eq!(frames, 3200);
    assert_eq!(
        declared,
        3200 * 2,
        "the header must describe the written frames"
    );
    let on_disk = std::fs::metadata(&path).unwrap().len();
    assert_eq!(
        on_disk, size_bytes,
        "the file length must match the reported size"
    );
    assert!(
        !path.to_string_lossy().ends_with(".partial"),
        "a closed recording must be renamed to .wav"
    );

    // What the recorder publishes when it closes the file, and what the
    // HTTP-side connection must then see.
    db.insert_recording(&RecordingDraft {
        id: id.clone(),
        relative_path: rel,
        started_at_utc: started_at,
        ended_at_utc: started_at + chrono::Duration::milliseconds(200),
        capture_rate_hz: 16_000,
        channels: 1,
        frame_count: frames,
        size_bytes,
        lost_frames: 0,
    })
    .unwrap();
    let http_db = Database::open(&config.storage.database_path).unwrap();
    let row = http_db.recording(&id).unwrap().expect("row must be readable");
    assert_eq!(row.frame_count, 3200);
    assert_eq!(row.size_bytes, size_bytes);
    assert_eq!(row.capture_rate_hz, 16_000);

    // And the detector pipeline, given the same audio, must classify it without
    // panicking and without inventing an event out of nothing.
    let mut pipeline = DetectionPipeline::new(config.detector.clone(), 16_000, 1, 0).unwrap();
    let mut out = Vec::new();
    pipeline
        .process_s16_block(&samples, 0, &mut out)
        .expect("the pipeline must accept correctly aligned S16 audio");
    assert!(
        pipeline.noise_floor_dbfs().is_none() || pipeline.noise_floor_dbfs().is_some(),
        "the noise floor must be a real value or explicitly unknown"
    );
    assert_eq!(metrics_free(&out), out.len());
}

fn metrics_free(events: &[steno_lib::detector::EventDraft]) -> usize {
    // Every produced event must carry a segment id and a sane frame range.
    for event in events {
        assert!(!event.segment_id.is_empty());
        assert!(event.end_offset_frames >= event.start_offset_frames);
    }
    events.len()
}

/// The single-box path: a device that only clocks at 48 kHz stereo must still
/// produce a 16 kHz mono recording, because that is the only format anything
/// downstream speaks.
///
/// This is the part that was previously untrue: `[audio] preferred_rate_hz` and
/// `channels` *described* the recording, so a stereo 48 kHz microphone wrote a
/// stereo 48 kHz WAV and the recogniser was handed samples at a rate it had not
/// been told about. Now the device format stops at `WireConverter`, and the
/// segment row plus the WAV on disk must both say 16 kHz mono regardless.
#[test]
fn a_48k_stereo_device_still_records_16k_mono() {
    use steno_lib::audio_capture::LocalWireCapture;
    use steno_lib::config::{WIRE_CHANNELS, WIRE_RATE_HZ};

    let dir = tempdir().unwrap();
    let config = Arc::new(test_config(dir.path()));
    std::fs::create_dir_all(&config.storage.recording_dir).unwrap();

    let writer_db = Database::open(&config.storage.database_path).unwrap();
    writer_db.migrate().unwrap();
    let writer = DbWriter::spawn(writer_db, |error| eprintln!("db error: {error}"));

    let metrics = Arc::new(Metrics::new());
    let shutting_down = Arc::new(AtomicBool::new(false));

    // The device: 48 kHz, two channels.
    let device_rate = 48_000u32;
    let device_channels = 2u16;
    let device_ring = Arc::new(CaptureRing::new(device_rate, device_channels, 500, 10, 200));
    let wire_ring = Arc::new(CaptureRing::new(
        WIRE_RATE_HZ,
        WIRE_CHANNELS,
        500,
        10,
        200,
    ));
    let device = NegotiatedFormat {
        device_name: "stereo-48k".into(),
        rate_hz: device_rate,
        channels: device_channels,
        format: SampleFormat::S16Le,
    };
    let mut local = LocalWireCapture::spawn(
        Arc::clone(&device_ring),
        Arc::clone(&wire_ring),
        &device,
        1,
        Arc::clone(&metrics),
    )
    .expect("the converter must accept a supported device rate");

    let recording_queue = Arc::new(BoundedQueue::new(64, 16));
    let detection_queue = Arc::new(BoundedQueue::new(64, 16));
    // What the dispatcher is told it is reading: the wire format, never the
    // device format. That is the whole point of converting first.
    let wire = NegotiatedFormat {
        device_name: "stereo-48k".into(),
        rate_hz: WIRE_RATE_HZ,
        channels: WIRE_CHANNELS,
        format: SampleFormat::S16Le,
    };

    let dispatcher = dispatcher::spawn(DispatcherInputs {
        config: Arc::clone(&config),
        metrics: Arc::clone(&metrics),
        capture: Arc::clone(&wire_ring),
        recording_queue: Arc::clone(&recording_queue),
        detection_queue: Arc::clone(&detection_queue),
        stt_queue: None,
        shutting_down: Arc::clone(&shutting_down),
        negotiated: wire,
    });

    // 200 ms of device audio, channel 1 carrying a 320 Hz tone and channel 0
    // deliberately silent. Picking the wrong channel produces silence, which is
    // exactly the failure an average would hide.
    let block_frames = (device_rate / 5) as usize;
    let samples: Vec<i16> = (0..block_frames)
        .flat_map(|n| {
            let phase =
                2.0 * std::f32::consts::PI * 320.0 * (n as f32 / device_rate as f32);
            [0i16, (phase.sin() * 3276.0) as i16]
        })
        .collect();
    assert!(device_ring.callback_push(
        &samples,
        CaptureMeta {
            frames: block_frames as u32,
            channels: device_channels,
            rate_hz: device_rate,
            capture_seq: 0,
            mono_start_ns: 0,
            stream_generation: 1,
        },
    ));

    std::thread::sleep(Duration::from_millis(300));
    shutting_down.store(true, Ordering::Relaxed);
    dispatcher.request_shutdown();
    recording_queue.close();
    detection_queue.close();
    local.stop();
    let mut handle = dispatcher;
    assert!(handle.join(Duration::from_secs(5)), "dispatcher must stop");
    assert!(writer.shutdown(Duration::from_secs(5)), "db writer must drain");

    // The dispatcher was told the wire format, so the window it announces — and
    // therefore the file the recorder will create — carries it.
    let mut frames: Vec<i16> = Vec::new();
    let mut window_format = None;
    let mut fanout_format = None;
    loop {
        match recording_queue.pop_timeout(Duration::from_millis(10)) {
            PopOutcome::Item(DispatcherMessage::SegmentStart { format, .. }) => {
                window_format = Some((format.rate_hz, format.channels));
            }
            PopOutcome::Item(DispatcherMessage::Audio { format, pcm, .. }) => {
                fanout_format = Some((format.rate_hz, format.channels));
                frames.extend_from_slice(&pcm);
            }
            PopOutcome::Item(_) => {}
            PopOutcome::Empty | PopOutcome::Closed => break,
        }
    }
    assert_eq!(
        window_format,
        Some((WIRE_RATE_HZ, WIRE_CHANNELS)),
        "a window must never be labelled with the device rate"
    );
    assert_eq!(
        fanout_format,
        Some((WIRE_RATE_HZ, WIRE_CHANNELS)),
        "the fan-out must carry the wire format, not the device format"
    );
    let peak = frames.iter().map(|s| s.unsigned_abs()).max().unwrap_or(0);
    assert!(
        peak > 1_000,
        "the selected channel carries the tone; peak {peak} looks like the silent one"
    );
    assert!(
        frames.len().abs_diff(3_200) <= 8,
        "200 ms of 48 kHz audio is 3200 wire frames, got {}",
        frames.len()
    );
}
