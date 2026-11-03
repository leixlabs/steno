//! Integration tests for the HTTP API (TECH_SPEC §8, §12; tasks T5.1–T5.6).
//!
//! Every test drives the real router through `tower::ServiceExt::oneshot`, so
//! the assertions cover the same code path a browser or the portal would use.

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{HeaderMap, Request, StatusCode, header};
use chrono::{DateTime, TimeZone, Utc};
use serde_json::Value;
use steno_lib::config::{
    AudioConfig, Config, DetectorConfig, ServerConfig, SourceConfig, StorageConfig,
};
use futures_util::StreamExt;
use steno_server::http_server::{
    HttpState, MicrophoneState, RecordingState, RuntimeStatus, router, serve,
};
use steno_lib::metrics::Metrics;
use steno_server::recorder::{WavFormat, write_header};
use steno_server::storage::{
    Database, EpisodeRow, RecordingDraft, ReviewStatus,
};
use steno_lib::timeline::TimeQuality;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use tempfile::TempDir;
use tower::ServiceExt;

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

struct Harness {
    state: Arc<HttpState>,
    router: Router,
    database: Arc<Mutex<Database>>,
    metrics: Arc<Metrics>,
    runtime: Arc<RuntimeStatus>,
    recording_dir: PathBuf,
    root: PathBuf,
    _dir: TempDir,
}

fn utc(seconds: i64) -> DateTime<Utc> {
    Utc.timestamp_opt(seconds, 0).single().unwrap()
}

fn harness_with_timezone(timezone: &str) -> Harness {
    harness_with(tempfile::tempdir().unwrap(), timezone, 0)
}

fn harness_with(dir: TempDir, timezone: &str, port: u16) -> Harness {
    harness_build(dir, timezone, port, |_| {})
}

/// Builds a harness and lets the caller adjust the configuration first, which is
/// how the `[stt]` and `[control]` branches get exercised without a second copy
/// of the wiring.
fn harness_build(
    dir: TempDir,
    timezone: &str,
    port: u16,
    mutate: impl FnOnce(&mut Config),
) -> Harness {
    let root = dir.path().to_path_buf();
    let recording_dir = root.join("recordings");
    std::fs::create_dir_all(&recording_dir).unwrap();
    let database = Arc::new(Mutex::new(Database::in_memory().unwrap()));
    let metrics = Arc::new(Metrics::new());
    let runtime = Arc::new(RuntimeStatus::new());
    let mut config = Config {
        server: ServerConfig {
            bind_address: "127.0.0.1".to_string(),
            port,
            http_threads: 1,
            timezone: timezone.to_string(),
            web_dir: String::new(),
        },
        audio: AudioConfig::default(),
        source: SourceConfig::default(),
        detector: DetectorConfig::default(),
        stt: Default::default(),
        control: Default::default(),
        storage: StorageConfig {
            recording_dir: recording_dir.clone(),
            database_path: root.join("steno.sqlite"),
            recording_max_bytes: 1 << 30,
            recording_cleanup_target_bytes: 1 << 29,
            recording_reserve_bytes: 1 << 20,
            retention_check_interval_seconds: 60,
        },
    };
    mutate(&mut config);
    let state = Arc::new(HttpState::new(
        config,
        Arc::clone(&database),
        Arc::clone(&metrics),
        Arc::clone(&runtime),
    ));
    Harness {
        router: router(Arc::clone(&state)),
        state,
        database,
        metrics,
        runtime,
        recording_dir,
        root,
        _dir: dir,
    }
}

fn harness() -> Harness {
    // A fixed timezone keeps every date boundary assertion independent of the
    // machine running the tests.
    harness_with_timezone("UTC")
}

struct Raw {
    status: StatusCode,
    headers: HeaderMap,
    body: Vec<u8>,
}

impl Raw {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or_else(|error| {
            panic!("body is not JSON ({error}): {}", String::from_utf8_lossy(&self.body))
        })
    }

    fn header(&self, name: header::HeaderName) -> Option<&str> {
        self.headers.get(name).and_then(|value| value.to_str().ok())
    }
}

async fn send(harness: &Harness, request: Request<Body>) -> Raw {
    let response = harness.router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = to_bytes(response.into_body(), 4 * 1024 * 1024).await.unwrap();
    Raw {
        status,
        headers,
        body: body.to_vec(),
    }
}

async fn get(harness: &Harness, uri: &str) -> Raw {
    let request = Request::builder().uri(uri).body(Body::empty()).unwrap();
    send(harness, request).await
}

async fn head(harness: &Harness, uri: &str, range: Option<&str>) -> Raw {
    let mut builder = Request::builder().method("HEAD").uri(uri);
    if let Some(range) = range {
        builder = builder.header(header::RANGE, range);
    }
    send(harness, builder.body(Body::empty()).unwrap()).await
}

async fn get_range(harness: &Harness, uri: &str, range: &str) -> Raw {
    let request = Request::builder()
        .uri(uri)
        .header(header::RANGE, range)
        .body(Body::empty())
        .unwrap();
    send(harness, request).await
}

async fn patch(harness: &Harness, uri: &str, body: &str) -> Raw {
    let request = Request::builder()
        .method("PATCH")
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    send(harness, request).await
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

struct RecordingSpec {
    id: &'static str,
    started: DateTime<Utc>,
    ended: DateTime<Utc>,
    frames: u64,
    rate: u32,
    relative_path: String,
    size_bytes: u64,
    lost_frames: u64,
}

impl RecordingSpec {
    fn new(id: &'static str, started: DateTime<Utc>, relative_path: &str) -> Self {
        RecordingSpec {
            id,
            started,
            ended: started + chrono::Duration::minutes(30),
            frames: 16_000,
            rate: 16_000,
            relative_path: relative_path.to_string(),
            size_bytes: 44,
            lost_frames: 0,
        }
    }
}

fn add_recording(harness: &Harness, spec: &RecordingSpec) {
    let db = harness.database.lock().unwrap();
    db.insert_recording(&RecordingDraft {
        id: spec.id.to_string(),
        relative_path: PathBuf::from(&spec.relative_path),
        started_at_utc: spec.started,
        ended_at_utc: spec.ended,
        capture_rate_hz: spec.rate,
        channels: 1,
        frame_count: spec.frames,
        size_bytes: spec.size_bytes,
        lost_frames: spec.lost_frames,
    })
    .unwrap();
}

fn add_episode(
    harness: &Harness,
    id: &str,
    recording_id: Option<&str>,
    started: DateTime<Utc>,
    duration_ms: i64,
    source: &str,
    label: Option<&str>,
    confidence: Option<f32>,
) {
    let db = harness.database.lock().unwrap();
    db.insert_episode(&EpisodeRow {
        id: id.to_string(),
        recording_id: recording_id.map(str::to_string),
        started_at_utc: started,
        ended_at_utc: started + chrono::Duration::milliseconds(duration_ms),
        source: source.to_string(),
        label: label.map(str::to_string),
        confidence,
        review_status: ReviewStatus::Unreviewed,
        created_at_utc: started,
    })
    .unwrap();
}

fn add_rule_episode(harness: &Harness, id: &str, recording_id: Option<&str>, started: DateTime<Utc>) {
    add_episode(harness, id, recording_id, started, 200, "rule-v1", None, Some(0.75));
}

/// Writes a real 44-byte-header WAV with a deterministic payload and returns its
/// exact byte contents.
fn write_recording(harness: &Harness, relative: &str, payload_len: usize) -> Vec<u8> {
    let path = harness.recording_dir.join(relative);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let payload: Vec<u8> = (0..payload_len).map(|index| (index % 251) as u8).collect();
    let mut file = std::fs::File::create(&path).unwrap();
    write_header(
        &mut file,
        &WavFormat::new(16_000, 1).unwrap(),
        payload_len as u64,
    )
    .unwrap();
    file.write_all(&payload).unwrap();
    file.sync_all().unwrap();
    std::fs::read(&path).unwrap()
}

// ---------------------------------------------------------------------------
// /status
// ---------------------------------------------------------------------------

#[tokio::test]
async fn status_reports_unknown_rather_than_inventing_health() {
    let harness = harness();
    let response = get(&harness, "/api/v1/status").await;
    assert_eq!(response.status, StatusCode::OK);
    let body = response.json();

    assert_eq!(body["service"]["state"], "ok");
    assert_eq!(body["service"]["version"], "0.1.0");
    assert!(body["service"]["last_error"].is_null());
    DateTime::parse_from_rfc3339(body["service"]["now_utc"].as_str().unwrap()).unwrap();

    assert_eq!(body["recording"]["state"], "unknown");
    assert_eq!(body["microphone"]["state"], "unknown");
    assert_eq!(body["microphone"]["device"], "");
    // The configured stream is reported until the negotiation reports its own.
    assert_eq!(body["microphone"]["capture_rate_hz"], 16_000);
    assert_eq!(body["microphone"]["channels"], 1);
    assert_eq!(body["microphone"]["sample_format"], "S16_LE");

    assert_eq!(body["detector"]["state"], "unknown");
    assert!(body["detector"]["noise_floor_dbfs"].is_null());
    assert!(body["detector"]["band_noise_floor_dbfs"].is_null());
    assert_eq!(body["detector"]["events_total"], 0);
    assert_eq!(body["detector"]["events_discarded_short"], 0);
    assert_eq!(body["detector"]["event_fk_retries"], 0);

    assert_eq!(body["storage"]["bytes"], 0);
    assert_eq!(body["storage"]["recordings"], 0);
    assert_eq!(body["storage"]["max_bytes"], 1 << 30);
    assert_eq!(body["storage"]["recording_dir"], harness.recording_dir.display().to_string());
    assert_eq!(body["storage"]["storage_pressure"], false);
    #[cfg(unix)]
    assert!(body["storage"]["free_bytes"].is_u64());

    assert_eq!(body["capture"]["xrun_count"], 0);
    assert_eq!(body["capture"]["lost_frames_total"], 0);
    assert_eq!(body["capture"]["capture_ring_overflow_frames"], 0);
    assert_eq!(body["capture"]["dropped_detection_blocks"], 0);
    assert_eq!(body["capture"]["gap_detection_limited"], false);
    assert_eq!(body["capture"]["clipped_samples_total"], 0);

    assert_eq!(body["queues"]["recording_high_water"], 0);
    assert_eq!(body["queues"]["detection_high_water"], 0);
    assert_eq!(body["queues"]["recording_control_slots"], 0);
    assert_eq!(body["queues"]["detection_control_slots"], 0);

    assert_eq!(body["dsp"]["count"], 0);
    assert_eq!(body["dsp"]["avg_ns"], 0);
    assert_eq!(body["dsp"]["p99_ns"], 0);
    assert_eq!(body["dsp"]["max_ns"], 0);

    assert_eq!(body["time"]["time_quality"], "unknown");
    assert_eq!(body["time"]["time_synced"], false);
    assert!(body["time"]["clock_drift_ppm"].is_null());

    assert_eq!(body["signal_state"], "unknown");
}

#[tokio::test]
async fn status_reflects_metrics_and_supervisor_reports() {
    let harness = harness();
    harness.runtime.set_running(true);
    harness.runtime.set_recording(RecordingState::Stopped);
    harness.runtime.set_microphone(MicrophoneState::Disconnected);
    harness.runtime.set_device("USB PnP Sound Device");
    // The recording format and the device format are two different facts now:
    // the status page has to report both, or a converted device would look like
    // it was recorded at its own rate.
    harness.runtime.set_capture_format(16_000, 1, "S16_LE");
    harness.runtime.set_device_format(48_000, 2);
    harness
        .runtime
        .set_time(Some(TimeQuality::Corrected), true, Some(4.25));

    harness.metrics.events_total.store(9, Ordering::Relaxed);
    harness.metrics.events_discarded_short.store(2, Ordering::Relaxed);
    harness.metrics.event_fk_retries.store(1, Ordering::Relaxed);
    harness.metrics.xrun_count.store(3, Ordering::Relaxed);
    harness.metrics.lost_frames_total.store(480, Ordering::Relaxed);
    harness.metrics.clipped_samples_total.store(17, Ordering::Relaxed);
    harness.metrics.storage_pressure.store(true, Ordering::Relaxed);
    harness.metrics.degraded.store(true, Ordering::Relaxed);
    harness.metrics.set_noise_floor_dbfs(Some(-63.5));
    harness.metrics.set_band_noise_floor_dbfs(Some(-70.25));
    harness.metrics.set_last_error("CPAL stream error");

    let body = get(&harness, "/api/v1/status").await.json();

    assert_eq!(body["service"]["state"], "degraded");
    assert_eq!(body["service"]["last_error"], "CPAL stream error");
    assert_eq!(body["recording"]["state"], "stopped");
    assert_eq!(body["microphone"]["state"], "disconnected");
    assert_eq!(body["microphone"]["device"], "USB PnP Sound Device");
    assert_eq!(body["microphone"]["capture_rate_hz"], 16_000);
    assert_eq!(body["microphone"]["channels"], 1);
    assert_eq!(body["microphone"]["device_rate_hz"], 48_000);
    assert_eq!(body["microphone"]["device_channels"], 2);
    assert_eq!(body["detector"]["noise_floor_dbfs"], -63.5);
    assert_eq!(body["detector"]["band_noise_floor_dbfs"], -70.25);
    assert_eq!(body["detector"]["events_total"], 9);
    assert_eq!(body["detector"]["events_discarded_short"], 2);
    assert_eq!(body["detector"]["event_fk_retries"], 1);
    assert_eq!(body["capture"]["xrun_count"], 3);
    assert_eq!(body["capture"]["lost_frames_total"], 480);
    assert_eq!(body["capture"]["clipped_samples_total"], 17);
    assert_eq!(body["storage"]["storage_pressure"], true);
    assert_eq!(body["time"]["time_quality"], "corrected");
    assert_eq!(body["time"]["time_synced"], true);
    assert_eq!(body["time"]["clock_drift_ppm"], 4.25);
    // Not silent, and the pipeline is running: the only case that may say `ok`.
    assert_eq!(body["signal_state"], "ok");

    harness.metrics.signal_silent.store(true, Ordering::Relaxed);
    let body = get(&harness, "/api/v1/status").await.json();
    assert_eq!(body["signal_state"], "silent");

    // A muted capsule: audio arrives, so `signal_silent` is false, and the
    // microphone is nonetheless hearing nothing. Reporting `ok` here is what
    // let a dead input look healthy, so `dead` has to be its own state.
    harness.metrics.signal_silent.store(false, Ordering::Relaxed);
    harness.metrics.signal_dead.store(true, Ordering::Relaxed);
    let body = get(&harness, "/api/v1/status").await.json();
    assert_eq!(body["signal_state"], "dead");
}

#[tokio::test]
async fn status_counts_recorded_bytes_from_the_database() {
    let harness = harness();
    let mut spec = RecordingSpec::new("rec-bytes", utc(1_735_689_600), "recordings/2025/01/01/a.wav");
    spec.size_bytes = 1_000;
    add_recording(&harness, &spec);
    let body = get(&harness, "/api/v1/status").await.json();
    assert_eq!(body["storage"]["bytes"], 1_000);
}

#[tokio::test]
async fn status_counts_the_recordings_behind_the_byte_total() {
    let harness = harness();
    let mut oldest = RecordingSpec::new("rec-old", utc(1_735_689_600), "recordings/2025/01/01/a.wav");
    oldest.size_bytes = 400;
    add_recording(&harness, &oldest);
    let mut newest = RecordingSpec::new("rec-new", utc(1_735_693_200), "recordings/2025/01/01/b.wav");
    newest.size_bytes = 600;
    add_recording(&harness, &newest);

    let body = get(&harness, "/api/v1/status").await.json();
    let storage = &body["storage"];
    assert_eq!(storage["bytes"], 1_000);
    assert_eq!(storage["recordings"], 2);
    // Retention's target and the OS reserve are part of the pool's definition,
    // not something a client should have to read the config file for.
    assert_eq!(storage["cleanup_target_bytes"], 1 << 29);
    assert_eq!(storage["reserve_bytes"], 1 << 20);
}

#[tokio::test]
async fn an_empty_pool_reports_zero_bytes_instead_of_the_epoch() {
    let harness = harness();
    let body = get(&harness, "/api/v1/status").await.json();
    assert_eq!(body["storage"]["bytes"], 0);
    assert_eq!(body["storage"]["recordings"], 0);
}

#[tokio::test]
async fn status_reports_the_last_packet_as_a_heartbeat_not_a_guess() {
    let harness = harness();
    // No node has ever spoken in this process: the field is null, not "now".
    let body = get(&harness, "/api/v1/status").await.json();
    assert!(body["source"]["last_packet_at_utc"].is_null());
    assert_eq!(body["source"]["node_connected"], false);

    harness.metrics.note_net_packet(1_735_689_600_000);
    let body = get(&harness, "/api/v1/status").await.json();
    assert_eq!(body["source"]["last_packet_at_utc"], "2025-01-01T00:00:00Z");
}

/// Without a transcription worker there is nothing to stream: the upgrade must be
/// refused with a reason, not accepted and left hanging. A client that gets an
/// open socket which never speaks cannot tell that apart from a silent room.
///
/// This has to go through a real TCP connection: `WebSocketUpgrade` is an
/// extractor, so it answers 426 for a handshake-less request *before* the handler
/// runs. Only a genuine upgrade request reaches the guard being tested.
#[tokio::test]
async fn the_transcript_stream_refuses_to_open_without_a_worker() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let port = free_port();
    let harness = harness_with(tempfile::tempdir().unwrap(), "UTC", port);
    let state = Arc::clone(&harness.state);
    let server = tokio::spawn(async move { serve(state).await });

    let address = format!("127.0.0.1:{port}");
    let mut stream = None;
    for _ in 0..200 {
        match tokio::net::TcpStream::connect(&address).await {
            Ok(connected) => {
                stream = Some(connected);
                break;
            }
            Err(_) => tokio::time::sleep(std::time::Duration::from_millis(10)).await,
        }
    }
    let mut stream = stream.expect("serve() never accepted a connection");

    stream
        .write_all(
            b"GET /api/v1/transcript HTTP/1.1\r\n\
              Host: localhost\r\n\
              Connection: Upgrade\r\n\
              Upgrade: websocket\r\n\
              Sec-WebSocket-Version: 13\r\n\
              Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n",
        )
        .await
        .unwrap();
    // Read to the end of the *headers*, not to EOF: a 503 keeps the connection
    // alive, so `read_to_end` would wait for a close that never comes.
    let mut raw = Vec::new();
    let mut chunk = [0u8; 512];
    while !raw.windows(4).any(|window| window == b"\r\n\r\n") {
        let read = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            stream.read(&mut chunk),
        )
        .await
        .expect("the server did not answer within 5 s")
        .expect("read failed");
        if read == 0 {
            break;
        }
        raw.extend_from_slice(&chunk[..read]);
    }
    let response = String::from_utf8_lossy(&raw);
    assert!(
        response.starts_with("HTTP/1.1 503"),
        "expected 503, got: {}",
        response.lines().next().unwrap_or("")
    );
    assert!(
        response.contains("transcription is not running"),
        "the reason must be stated, got: {response}"
    );
    assert!(
        !response.starts_with("HTTP/1.1 101"),
        "the socket must not be upgraded when nothing can feed it"
    );
    server.abort();
}

#[tokio::test]
async fn status_says_transcription_is_off_rather_than_disconnected() {
    let harness = harness();
    let body = get(&harness, "/api/v1/status").await.json();
    let stt = &body["stt"];
    assert_eq!(stt["enabled"], false);
    assert_eq!(stt["mode"], "stream");
    // The old payload only had `connected`, so "you never turned this on" and
    // "the recogniser died" rendered identically.
    assert_eq!(stt["state"], "off");
    assert_eq!(stt["connected"], false);
}

#[tokio::test]
async fn control_reports_why_it_cannot_act() {
    let harness = harness();
    let body = get(&harness, "/api/v1/control").await.json();
    assert_eq!(body["enabled"], false);
    assert_eq!(body["available"], false);
    assert!(
        body["reason"].as_str().unwrap_or_default().contains("控制已关闭"),
        "a disabled control centre must say so: {body}"
    );
    assert_eq!(body["units"][0]["target"], "server");
    assert_eq!(body["units"][0]["unit"], "steno-monitor");
    assert_eq!(body["units"][1]["unit"], "steno-node");
    // Nothing was asked, so nothing may be claimed.
    assert_eq!(body["units"][0]["state"], "unknown");
}

#[tokio::test]
async fn control_refuses_an_action_when_it_is_disabled() {
    let harness = harness();
    let request = Request::builder()
        .method("POST")
        .uri("/api/v1/control")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(r#"{"target":"server","action":"stop"}"#))
        .unwrap();
    let response = send(&harness, request).await;
    assert_eq!(response.status, StatusCode::FORBIDDEN);
    assert_eq!(response.json()["error"]["code"], "control_disabled");
}

#[tokio::test]
async fn control_rejects_a_target_or_action_outside_the_closed_set() {
    let harness = harness_build(tempfile::tempdir().unwrap(), "UTC", 0, |config| {
        config.control.enabled = true;
    });
    for body in [
        r#"{"target":"other","action":"stop"}"#,
        r#"{"target":"server","action":"destroy"}"#,
    ] {
        let request = Request::builder()
            .method("POST")
            .uri("/api/v1/control")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body))
            .unwrap();
        let response = send(&harness, request).await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST, "body: {body}");
        assert_eq!(response.json()["error"]["code"], "invalid_parameter");
    }
}

#[tokio::test]
async fn control_never_claims_success_it_could_not_verify() {
    // Enable control but point it at a unit that cannot exist, so the assertion
    // holds on a systemd host too (and can never touch the real deployment
    // unit). With no systemd, the probe refuses before spawning anything.
    let harness = harness_build(tempfile::tempdir().unwrap(), "UTC", 0, |config| {
        config.control.enabled = true;
        config.control.server_unit = "steno-control-test-does-not-exist".to_string();
        config.control.node_unit = "steno-control-test-does-not-exist".to_string();
    });
    let request = Request::builder()
        .method("POST")
        .uri("/api/v1/control")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(r#"{"target":"server","action":"restart"}"#))
        .unwrap();
    let response = send(&harness, request).await;
    assert_ne!(response.status, StatusCode::OK, "no systemd, no success");
    let body = response.json();
    assert_eq!(body["ok"], false);
    assert!(
        body["message"].as_str().is_some_and(|text| !text.is_empty()),
        "a refusal must carry the reason: {body}"
    );
}

// ---------------------------------------------------------------------------
// /days, /recordings, /events
// ---------------------------------------------------------------------------

#[tokio::test]
async fn days_lists_only_nights_that_hold_data() {
    let harness = harness();
    // 2025-01-01T12:00:00Z and 2025-01-01T20:00:00Z.
    let mut first = RecordingSpec::new("rec-1", utc(1_735_732_800), "recordings/2025/01/01/a.wav");
    first.frames = 16_000;
    add_recording(&harness, &first);
    let mut second = RecordingSpec::new("rec-2", utc(1_735_761_600), "recordings/2025/01/01/b.wav");
    second.frames = 32_000;
    add_recording(&harness, &second);
    add_rule_episode(&harness, "evt-1", Some("rec-1"), utc(1_735_732_810));

    let body = get(&harness, "/api/v1/days?from=2025-01-01&to=2025-01-03")
        .await
        .json();
    assert_eq!(body["from"], "2025-01-01");
    assert_eq!(body["to"], "2025-01-03");
    let days = body["days"].as_array().unwrap();
    assert_eq!(days.len(), 1, "only the night with data is listed: {days:?}");
    assert_eq!(days[0]["date"], "2025-01-01");
    assert_eq!(days[0]["recording_count"], 2);
    assert_eq!(days[0]["episode_count"], 1);
    assert_eq!(days[0]["recorded_ms"], 3_000);
    assert_eq!(days[0]["lost_frames"], 0);
}

#[tokio::test]
async fn recordings_list_the_file_a_client_can_play() {
    let harness = harness();
    // 2025-01-01T23:30Z -> 2025-01-02T00:30Z.
    let mut crossing = RecordingSpec::new(
        "rec-cross",
        utc(1_735_774_200),
        "recordings/2025/01/01/cross.wav",
    );
    crossing.ended = utc(1_735_777_800);
    crossing.lost_frames = 80;
    add_recording(&harness, &crossing);
    // 2025-01-02T10:00Z -> 2025-01-02T10:30Z.
    add_recording(
        &harness,
        &RecordingSpec::new("rec-morning", utc(1_735_812_000), "recordings/2025/01/02/m.wav"),
    );

    // A recording belongs to the day it started on; the schema has no status to
    // reconcile, so a file is either there or it is not.
    let first = get(&harness, "/api/v1/recordings?date=2025-01-01").await.json();
    assert_eq!(first["date"], "2025-01-01");
    let recordings = first["recordings"].as_array().unwrap();
    assert_eq!(recordings.len(), 1);
    assert_eq!(recordings[0]["id"], "rec-cross");

    let second = get(&harness, "/api/v1/recordings?date=2025-01-02").await.json();
    assert_eq!(second["recordings"].as_array().unwrap().len(), 1);
    assert_eq!(second["recordings"][0]["id"], "rec-morning");

    let recording = &first["recordings"][0];
    assert_eq!(recording["capture_rate_hz"], 16_000);
    assert_eq!(recording["channels"], 1);
    assert_eq!(recording["frame_count"], 16_000);
    assert_eq!(recording["size_bytes"], 44);
    assert_eq!(recording["duration_ms"], 1_000);
    assert_eq!(recording["lost_frames"], 80);
    assert!(recording["started_at_utc"].is_string());
    assert!(recording["ended_at_utc"].is_string());
    assert_eq!(recording["audio_url"], "/api/v1/recordings/rec-cross/audio");
}

#[tokio::test]
async fn date_boundaries_follow_the_configured_timezone() {
    let harness = harness_with_timezone("America/New_York");
    // 2025-01-01T03:00Z is 2024-12-31 22:00 in New York.
    let mut spec = RecordingSpec::new(
        "rec-evening",
        utc(1_735_700_400),
        "recordings/2025/01/01/evening.wav",
    );
    spec.ended = utc(1_735_700_700);
    add_recording(&harness, &spec);

    let previous = get(&harness, "/api/v1/recordings?date=2024-12-31").await.json();
    assert_eq!(previous["recordings"].as_array().unwrap().len(), 1);
    let next = get(&harness, "/api/v1/recordings?date=2025-01-01").await.json();
    assert_eq!(next["recordings"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn episodes_carry_their_source_and_label() {
    let harness = harness();
    add_recording(
        &harness,
        &RecordingSpec::new("rec-1", utc(1_735_732_800), "recordings/2025/01/01/a.wav"),
    );
    add_rule_episode(&harness, "evt-rule", Some("rec-1"), utc(1_735_732_812));
    add_episode(
        &harness,
        "evt-classifier",
        None,
        utc(1_735_732_900),
        4_000,
        "apple-soundanalysis-v1",
        Some("snoring"),
        Some(0.81),
    );

    let body = get(&harness, "/api/v1/events?date=2025-01-01").await.json();
    assert_eq!(body["date"], "2025-01-01");
    let events = body["events"].as_array().unwrap();
    assert_eq!(events.len(), 2);

    let rule = &events[0];
    assert_eq!(rule["id"], "evt-rule");
    assert_eq!(rule["recording_id"], "rec-1");
    assert_eq!(rule["source"], "rule-v1");
    assert!(rule["label"].is_null());
    assert_eq!(rule["confidence"], 0.75);
    assert_eq!(rule["duration_ms"], 200);
    assert_eq!(rule["review_status"], "unreviewed");
    assert!(rule["started_at_utc"].is_string());
    assert!(rule["ended_at_utc"].is_string());

    let classifier = &events[1];
    assert_eq!(classifier["recording_id"], serde_json::Value::Null);
    assert_eq!(classifier["source"], "apple-soundanalysis-v1");
    assert_eq!(classifier["label"], "snoring");
    assert_eq!(classifier["confidence"], 0.81);
    assert_eq!(classifier["duration_ms"], 4_000);

    // The old epoch vocabulary must be gone, not merely unused.
    for gone in ["segment_id", "capture_rate_hz", "start_offset_frames", "rule_score"] {
        assert!(events[0].get(gone).is_none(), "{gone} is not part of an episode: {rule}");
    }
}

#[tokio::test]
async fn episode_detail_returns_the_episode() {
    let harness = harness();
    add_recording(
        &harness,
        &RecordingSpec::new("rec-1", utc(1_735_732_800), "recordings/2025/01/01/a.wav"),
    );
    add_rule_episode(&harness, "evt-1", Some("rec-1"), utc(1_735_732_812));

    let response = get(&harness, "/api/v1/events/evt-1").await;
    assert_eq!(response.status, StatusCode::OK);
    let body = response.json();
    assert_eq!(body["id"], "evt-1");
    assert_eq!(body["recording_id"], "rec-1");
    assert_eq!(body["source"], "rule-v1");
    assert_eq!(body["review_status"], "unreviewed");

    let missing = get(&harness, "/api/v1/events/nope").await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
    assert_eq!(missing.json()["error"]["code"], "not_found");
}

#[tokio::test]
async fn dates_without_data_return_empty_arrays() {
    let harness = harness();
    let recordings = get(&harness, "/api/v1/recordings?date=2030-05-05").await;
    assert_eq!(recordings.status, StatusCode::OK);
    assert_eq!(recordings.json()["recordings"].as_array().unwrap().len(), 0);

    let events = get(&harness, "/api/v1/events?date=2030-05-05").await;
    assert_eq!(events.status, StatusCode::OK);
    assert_eq!(events.json()["events"].as_array().unwrap().len(), 0);

    let days = get(&harness, "/api/v1/days?from=2030-05-05&to=2030-05-07").await;
    assert_eq!(days.status, StatusCode::OK);
    assert_eq!(days.json()["days"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn invalid_parameters_use_the_error_envelope() {
    let harness = harness();
    for uri in [
        "/api/v1/recordings",
        "/api/v1/recordings?date=",
        "/api/v1/recordings?date=2025-13-01",
        "/api/v1/recordings?date=01-01-2025",
        "/api/v1/events?date=not-a-date",
        "/api/v1/days?from=2025-01-01",
        "/api/v1/days?to=2025-01-01",
        "/api/v1/days?from=2025-01-02&to=2025-01-01",
        "/api/v1/days?from=2025-01-01&to=2030-01-01",
    ] {
        let response = get(&harness, uri).await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST, "{uri}");
        let body = response.json();
        assert_eq!(body["error"]["code"], "invalid_parameter", "{uri}");
        assert!(body["error"]["message"].is_string(), "{uri}");
    }
}

#[tokio::test]
async fn unknown_routes_and_methods_use_the_error_envelope() {
    let harness = harness();
    let missing = get(&harness, "/api/v1/nope").await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
    assert_eq!(missing.json()["error"]["code"], "not_found");

    let request = Request::builder()
        .method("POST")
        .uri("/api/v1/status")
        .body(Body::empty())
        .unwrap();
    let wrong_method = send(&harness, request).await;
    assert_eq!(wrong_method.status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(wrong_method.json()["error"]["code"], "method_not_allowed");
}

// ---------------------------------------------------------------------------
// PATCH /api/v1/events/{id}
// ---------------------------------------------------------------------------

#[tokio::test]
async fn patch_accepts_the_three_labels_and_returns_the_event() {
    let harness = harness();
    add_recording(
        &harness,
        &RecordingSpec::new("rec-1", utc(1_735_732_800), "recordings/2025/01/01/a.wav"),
    );
    add_rule_episode(&harness, "evt-1", Some("rec-1"), utc(1_735_732_812));

    for (label, expected) in [
        ("snore", ReviewStatus::Snore),
        ("not_snore", ReviewStatus::NotSnore),
        ("unreviewed", ReviewStatus::Unreviewed),
    ] {
        let response = patch(
            &harness,
            "/api/v1/events/evt-1",
            &format!("{{\"review_status\":\"{label}\"}}"),
        )
        .await;
        assert_eq!(response.status, StatusCode::OK, "{label}");
        let body = response.json();
        assert_eq!(body["id"], "evt-1");
        assert_eq!(body["review_status"], label);
        assert_eq!(body["recording_id"], "rec-1");
        let stored = harness
            .database
            .lock()
            .unwrap()
            .episode("evt-1")
            .unwrap()
            .unwrap();
        assert_eq!(stored.review_status, expected);
    }
}

#[tokio::test]
async fn patch_rejects_anything_else() {
    let harness = harness();
    add_recording(
        &harness,
        &RecordingSpec::new("rec-1", utc(1_735_732_800), "recordings/2025/01/01/a.wav"),
    );
    add_rule_episode(&harness, "evt-1", Some("rec-1"), utc(1_735_732_812));

    for body in [
        "{\"review_status\":\"maybe\"}",
        "{\"review_status\":\"\"}",
        "{\"review_status\":\"SNORE\"}",
        "{}",
        "{\"review_status\":3}",
        "not json",
    ] {
        let response = patch(&harness, "/api/v1/events/evt-1", body).await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST, "{body}");
        let json = response.json();
        assert!(json["error"]["code"].is_string(), "{body}");
    }

    // A rejected patch must not have changed the stored label.
    let stored = harness
        .database
        .lock()
        .unwrap()
        .episode("evt-1")
        .unwrap()
        .unwrap();
    assert_eq!(stored.review_status, ReviewStatus::Unreviewed);

    let missing = patch(
        &harness,
        "/api/v1/events/nope",
        "{\"review_status\":\"snore\"}",
    )
    .await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
    assert_eq!(missing.json()["error"]["code"], "not_found");
}

// ---------------------------------------------------------------------------
// Audio range endpoint
// ---------------------------------------------------------------------------

const PAYLOAD: usize = 100;

fn audio_recording(harness: &Harness, id: &'static str) -> Vec<u8> {
    let relative = format!("recordings/2025/01/01/{id}.wav");
    let bytes = write_recording(harness, &relative, PAYLOAD);
    let mut spec = RecordingSpec::new(id, utc(1_735_732_800), &relative);
    spec.size_bytes = bytes.len() as u64;
    add_recording(harness, &spec);
    bytes
}

#[tokio::test]
async fn audio_returns_the_whole_file_with_range_metadata() {
    let harness = harness();
    let bytes = audio_recording(&harness, "rec-audio");
    let response = get(&harness, "/api/v1/recordings/rec-audio/audio").await;

    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.header(header::CONTENT_TYPE).unwrap(), "audio/wav");
    assert_eq!(response.header(header::ACCEPT_RANGES).unwrap(), "bytes");
    assert_eq!(
        response.header(header::CONTENT_LENGTH).unwrap(),
        bytes.len().to_string()
    );
    assert_eq!(
        response.header(header::ETAG).unwrap(),
        format!("\"rec-audio-{}\"", bytes.len())
    );
    assert!(response.header(header::CONTENT_RANGE).is_none());
    assert_eq!(response.body, bytes);
}

#[tokio::test]
async fn audio_honours_the_three_range_forms() {
    let harness = harness();
    let bytes = audio_recording(&harness, "rec-range");
    let total = bytes.len();
    let uri = "/api/v1/recordings/rec-range/audio";

    for (range, expected_start, expected_end) in [
        ("bytes=0-9", 0usize, 9usize),
        ("bytes=10-", 10, total - 1),
        ("bytes=-4", total - 4, total - 1),
        ("bytes=0-9999", 0, total - 1),
    ] {
        let response = get_range(&harness, uri, range).await;
        assert_eq!(response.status, StatusCode::PARTIAL_CONTENT, "{range}");
        assert_eq!(
            response.header(header::CONTENT_RANGE).unwrap(),
            format!("bytes {expected_start}-{expected_end}/{total}"),
            "{range}"
        );
        assert_eq!(
            response.header(header::CONTENT_LENGTH).unwrap(),
            (expected_end - expected_start + 1).to_string(),
            "{range}"
        );
        assert_eq!(
            response.body,
            bytes[expected_start..=expected_end].to_vec(),
            "{range}"
        );
        assert_eq!(
            response.header(header::ETAG).unwrap(),
            format!("\"rec-range-{total}\"")
        );
    }
}

#[tokio::test]
async fn audio_ignores_multi_ranges_and_rejects_unsatisfiable_ones() {
    let harness = harness();
    let bytes = audio_recording(&harness, "rec-multi");
    let uri = "/api/v1/recordings/rec-multi/audio";

    // Multiple ranges are not supported: RFC 9110 allows the full response.
    for range in ["bytes=0-1,5-6", "bytes=0-1, 5-6", "items=0-1", "bytes=5-1"] {
        let response = get_range(&harness, uri, range).await;
        assert_eq!(response.status, StatusCode::OK, "{range}");
        assert_eq!(response.body, bytes, "{range}");
    }

    let response = get_range(&harness, uri, "bytes=999999-").await;
    assert_eq!(response.status, StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(
        response.header(header::CONTENT_RANGE).unwrap(),
        format!("bytes */{}", bytes.len())
    );
    assert_eq!(response.header(header::ACCEPT_RANGES).unwrap(), "bytes");
    let body = response.json();
    assert_eq!(body["error"]["code"], "range_not_satisfiable");
}

#[tokio::test]
async fn audio_supports_head_with_and_without_a_range() {
    let harness = harness();
    let bytes = audio_recording(&harness, "rec-head");
    let uri = "/api/v1/recordings/rec-head/audio";

    let response = head(&harness, uri, None).await;
    assert_eq!(response.status, StatusCode::OK);
    assert!(response.body.is_empty(), "HEAD carries no body");
    assert_eq!(
        response.header(header::CONTENT_LENGTH).unwrap(),
        bytes.len().to_string()
    );
    assert_eq!(response.header(header::ACCEPT_RANGES).unwrap(), "bytes");
    assert_eq!(response.header(header::CONTENT_TYPE).unwrap(), "audio/wav");

    let response = head(&harness, uri, Some("bytes=0-9")).await;
    assert_eq!(response.status, StatusCode::PARTIAL_CONTENT);
    assert!(response.body.is_empty());
    assert_eq!(response.header(header::CONTENT_LENGTH).unwrap(), "10");
    assert_eq!(
        response.header(header::CONTENT_RANGE).unwrap(),
        format!("bytes 0-9/{}", bytes.len())
    );
}

#[tokio::test]
async fn audio_reports_a_missing_row_and_a_missing_file() {
    let harness = harness();
    audio_recording(&harness, "rec-present");

    let missing = get(&harness, "/api/v1/recordings/nope/audio").await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
    assert_eq!(missing.json()["error"]["code"], "not_found");

    // A row whose file vanished is a 404, never a fake stream. There is no
    // status column to turn this into a 409 or a 410.
    let mut orphan = RecordingSpec::new(
        "rec-orphan",
        utc(1_735_732_800),
        "recordings/2025/01/01/orphan.wav",
    );
    orphan.size_bytes = 144;
    add_recording(&harness, &orphan);
    let orphaned = get(&harness, "/api/v1/recordings/rec-orphan/audio").await;
    assert_eq!(orphaned.status, StatusCode::NOT_FOUND);
    assert_eq!(orphaned.json()["error"]["code"], "audio_missing");
}

#[tokio::test]
async fn audio_serves_a_file_whose_size_disagrees_with_the_database() {
    let harness = harness();
    let bytes = audio_recording(&harness, "rec-drift");
    // Corrupt the stored size the way a half-written completion could.
    {
        let db = harness.database.lock().unwrap();
        db.connection()
            .execute(
                "UPDATE recordings SET size_bytes=1 WHERE id='rec-drift'",
                [],
            )
            .unwrap();
    }
    let response = get(&harness, "/api/v1/recordings/rec-drift/audio").await;
    assert_eq!(response.status, StatusCode::OK);
    // The real file length wins so ranges stay consistent with the bytes sent.
    assert_eq!(
        response.header(header::CONTENT_LENGTH).unwrap(),
        bytes.len().to_string()
    );
    assert_eq!(
        response.header(header::ETAG).unwrap(),
        format!("\"rec-drift-{}\"", bytes.len())
    );
    assert_eq!(response.body, bytes);
}

// ---------------------------------------------------------------------------
// Portal
// ---------------------------------------------------------------------------

#[tokio::test]
async fn portal_serves_index_and_assets() {
    let harness = harness();
    let web = harness.root.join("web");
    std::fs::create_dir_all(web.join("assets")).unwrap();
    std::fs::write(web.join("index.html"), b"<html>portal</html>").unwrap();
    std::fs::write(web.join("assets/app.js"), b"console.log('portal')").unwrap();

    let index = get(&harness, "/").await;
    assert_eq!(index.status, StatusCode::OK);
    assert_eq!(index.header(header::CONTENT_TYPE).unwrap(), "text/html; charset=utf-8");
    assert_eq!(index.body, b"<html>portal</html>");

    // A client-side route (no extension, no file) falls back to index.html so a
    // refresh on /night/2026-10-29 does not 404.
    let spa_route = get(&harness, "/night/2026-10-29").await;
    assert_eq!(spa_route.status, StatusCode::OK);
    assert_eq!(spa_route.body, b"<html>portal</html>");

    let asset = get(&harness, "/assets/app.js").await;
    assert_eq!(asset.status, StatusCode::OK);
    assert_eq!(asset.header(header::CONTENT_TYPE).unwrap(), "text/javascript; charset=utf-8");
    assert_eq!(asset.body, b"console.log('portal')");

    // A missing file *with* an extension is a real 404, not the SPA shell.
    let missing = get(&harness, "/assets/nope.js").await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
    assert_eq!(missing.json()["error"]["code"], "not_found");
}

#[tokio::test]
async fn portal_rejects_path_traversal() {
    let harness = harness();
    let web = harness.root.join("web");
    std::fs::create_dir_all(&web).unwrap();
    std::fs::write(web.join("index.html"), b"<html>portal</html>").unwrap();
    std::fs::write(harness.root.join("secret.txt"), b"top secret").unwrap();

    for uri in [
        "/web/%2e%2e/secret.txt",
        "/web/%2e%2e%2fsecret.txt",
        "/web/assets/%2e%2e/%2e%2e/secret.txt",
    ] {
        let response = get(&harness, uri).await;
        assert_ne!(response.status, StatusCode::OK, "{uri} must not be served");
        assert!(
            !String::from_utf8_lossy(&response.body).contains("top secret"),
            "{uri} leaked a file outside the portal root"
        );
    }
}

#[tokio::test]
async fn portal_without_a_web_directory_is_an_explicit_error() {
    let harness = harness();
    assert!(!harness.root.join("web").exists());
    let response = get(&harness, "/").await;
    assert_eq!(response.status, StatusCode::SERVICE_UNAVAILABLE);
    let body = response.json();
    assert_eq!(body["error"]["code"], "portal_unavailable");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("web"),
        "the message must name the expected directory"
    );
}

// ---------------------------------------------------------------------------
// Concurrency and real listening socket
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_partially_read_audio_response_does_not_block_other_requests() {
    let harness = harness();
    let relative = "recordings/2025/01/01/big.wav";
    let bytes = write_recording(&harness, relative, 4 * 1024 * 1024);
    let mut spec = RecordingSpec::new("rec-big", utc(1_735_732_800), relative);
    spec.size_bytes = bytes.len() as u64;
    add_recording(&harness, &spec);

    let request = Request::builder()
        .uri("/api/v1/recordings/rec-big/audio")
        .body(Body::empty())
        .unwrap();
    let response = harness.router.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // Consume one chunk and leave the 4 MiB response open: the rest is still
    // sitting on disk, not in memory.
    let mut stream = response.into_body().into_data_stream();
    let first = stream.next().await.unwrap().unwrap();
    assert!(
        first.len() <= 64 * 1024,
        "the stream must be chunked, got {} bytes",
        first.len()
    );

    // Another request is served while that stream is half-read.
    let status = get(&harness, "/api/v1/status").await;
    assert_eq!(status.status, StatusCode::OK);
    assert_eq!(status.json()["service"]["state"], "ok");
    let recordings = get(&harness, "/api/v1/recordings?date=2025-01-01").await;
    assert_eq!(recordings.status, StatusCode::OK);
    assert_eq!(recordings.json()["recordings"].as_array().unwrap().len(), 1);

    // Drain the remainder to prove the open range really was served in full.
    let mut received = first.len();
    while let Some(chunk) = stream.next().await {
        received += chunk.unwrap().len();
    }
    assert_eq!(received, bytes.len());
}

#[tokio::test]
async fn serve_answers_over_a_real_tcp_socket() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let port = free_port();
    let harness = harness_with(tempfile::tempdir().unwrap(), "UTC", port);
    let state = Arc::clone(&harness.state);
    let server = tokio::spawn(async move { serve(state).await });

    let address = format!("127.0.0.1:{port}");
    let mut stream = None;
    for _ in 0..200 {
        match tokio::net::TcpStream::connect(&address).await {
            Ok(connected) => {
                stream = Some(connected);
                break;
            }
            Err(_) => tokio::time::sleep(std::time::Duration::from_millis(10)).await,
        }
    }
    let mut stream = stream.expect("serve() never accepted a connection");

    stream
        .write_all(b"GET /api/v1/status HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await.unwrap();
    let text = String::from_utf8_lossy(&raw);
    assert!(text.starts_with("HTTP/1.1 200"), "{text}");
    assert!(text.contains("\"service\""), "{text}");
    assert!(text.contains("\"signal_state\""), "{text}");

    server.abort();
}

fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    port
}

// ---------------------------------------------------------------------------
// Wiring
// ---------------------------------------------------------------------------

#[test]
fn runtime_is_sized_by_the_configured_http_threads() {
    let config = ServerConfig {
        bind_address: "127.0.0.1".to_string(),
        port: 0,
        http_threads: 2,
        timezone: String::new(),
        web_dir: String::new(),
    };
    let runtime = steno_server::http_server::build_runtime(&config).unwrap();
    assert_eq!(runtime.metrics().num_workers(), 2);
}

#[test]
fn the_portal_root_defaults_next_to_the_recordings_directory() {
    let dir = tempfile::tempdir().unwrap();
    let recording_dir = dir.path().join("recordings");
    let state = HttpState::new(
        Config {
            server: ServerConfig {
                bind_address: "127.0.0.1".to_string(),
                port: 0,
                http_threads: 1,
                timezone: String::new(),
                web_dir: String::new(),
            },
            audio: AudioConfig::default(),
            source: SourceConfig::default(),
            detector: DetectorConfig::default(),
            stt: Default::default(),
            control: Default::default(),
            storage: StorageConfig {
                recording_dir: recording_dir.clone(),
                database_path: dir.path().join("db.sqlite"),
                recording_max_bytes: 1 << 30,
                recording_cleanup_target_bytes: 1 << 29,
                recording_reserve_bytes: 1 << 20,
                retention_check_interval_seconds: 60,
            },
        },
        Arc::new(Mutex::new(Database::in_memory().unwrap())),
        Arc::new(Metrics::new()),
        Arc::new(RuntimeStatus::new()),
    );
    assert_eq!(state.web_root(), dir.path().join("web"));
    assert_eq!(
        state
            .with_web_root(Path::new("/opt/steno/web"))
            .web_root(),
        Path::new("/opt/steno/web")
    );
}
