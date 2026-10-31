//! HTTP API and static portal (TECH_SPEC §8, §9; tasks T5.1–T5.6).
//!
//! # Public interface
//!
//! ```ignore
//! pub struct HttpState { /* ... */ }
//!
//! impl HttpState {
//!     pub fn new(
//!         config: Config,
//!         database: Arc<Mutex<Database>>,
//!         metrics: Arc<Metrics>,
//!         runtime: Arc<RuntimeStatus>,
//!     ) -> Self;
//!     pub fn with_web_root(self, root: impl Into<PathBuf>) -> Self;
//! }
//!
//! pub fn router(state: Arc<HttpState>) -> axum::Router;
//! pub async fn serve(state: Arc<HttpState>) -> steno_lib::error::Result<()>;
//! pub fn build_runtime(config: &ServerConfig) -> std::io::Result<tokio::runtime::Runtime>;
//! ```
//!
//! `main` wires capture/dispatch and HTTP together by constructing an
//! [`HttpState`] and either calling [`serve`] or handing [`router`] to its own
//! server. [`RuntimeStatus`] is the one piece of state the HTTP layer cannot
//! derive from [`Metrics`]: recording/microphone/time quality are known only to
//! the supervisor, so they are pushed in through that handle and default to
//! `unknown` (TECH_SPEC §12 forbids inventing an `ok`).
//!
//! # Design notes
//!
//! * `rusqlite::Connection` is `Send` but not `Sync`, so the shared database is
//!   an `Arc<Mutex<Database>>`. Every lock is taken and released inside a
//!   synchronous helper; no guard is ever held across an `.await`.
//! * Audio bytes are streamed from the file in [`AUDIO_CHUNK_BYTES`] chunks with
//!   `tokio::fs`, so a long playback request never loads a whole segment into
//!   memory and never touches an audio thread.
//! * The playback path comes only from the database `relative_path` joined to
//!   `storage.recording_dir`. Request data never contributes path components.
//! * The portal is read from `<storage.recording_dir>/../web`. That directory is
//!   resolved once at construction ([`HttpState::with_web_root`] overrides it)
//!   and every request path is confined to it after canonicalization.
//!
//! # Range requests
//!
//! A single `bytes=a-b`, `bytes=a-` or `bytes=-n` range is honoured. Anything
//! that is not a single well-formed range (multiple ranges, another unit, junk)
//! is ignored and answered with the full `200`, which RFC 9110 permits. A
//! well-formed range that selects no byte (start past the end, `-0`, any range
//! on an empty file) is answered with `416`.

use steno_lib::config::{Config, ServerConfig, SourceKind, SttMode};
use steno_lib::error::{Error, Result};
use steno_lib::metrics::Metrics;
use crate::control::{self, Action, Target};
use crate::transcript_hub::{TranscriptEvent, TranscriptHub};
use crate::recorder::validate_relative_path;
use crate::storage::{TranscriptRow, local_day_range, parse_date, Database, EventRow, ReviewStatus, SegmentRow};
use steno_lib::timeline::{GapRecord, TimeQuality};
use axum::body::{Body, Bytes};
use axum::extract::rejection::{JsonRejection, QueryRejection};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path as RoutePath, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::SeekFrom;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncSeekExt};

/// API prefix for every JSON endpoint (TECH_SPEC §8).
pub const API_PREFIX: &str = "/api/v1";
/// Fixed read size for streamed audio; a request never buffers more than this.
const AUDIO_CHUNK_BYTES: usize = 64 * 1024;
/// Widest `/days` window accepted, so one request cannot fan out without bound.
const MAX_DAYS_SPAN: i64 = 366;

// ---------------------------------------------------------------------------
// Error envelope
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct ErrorResponse {
    error: ErrorDetail,
}

#[derive(Debug, Serialize)]
struct ErrorDetail {
    code: &'static str,
    message: String,
}

/// The single error shape every endpoint uses: `{"error":{...}}`.
fn error_response(status: StatusCode, code: &'static str, message: impl Into<String>) -> Response {
    let body = ErrorResponse {
        error: ErrorDetail {
            code,
            message: message.into(),
        },
    };
    match serde_json::to_vec(&body) {
        Ok(bytes) => (status, [(header::CONTENT_TYPE, "application/json")], bytes).into_response(),
        Err(_) => (status, [(header::CONTENT_TYPE, "text/plain")], code).into_response(),
    }
}

fn json_response<T: Serialize>(status: StatusCode, value: &T) -> Response {
    match serde_json::to_vec(value) {
        Ok(bytes) => (status, [(header::CONTENT_TYPE, "application/json")], bytes).into_response(),
        Err(error) => {
            tracing::error!(%error, "failed to serialize API response");
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                "response serialization failed",
            )
        }
    }
}

fn internal_error(error: &Error) -> Response {
    tracing::error!(%error, "HTTP request failed");
    error_response(StatusCode::INTERNAL_SERVER_ERROR, "internal", error.to_string())
}

// ---------------------------------------------------------------------------
// Supervisor-reported status
// ---------------------------------------------------------------------------

/// Recording health as reported by the supervisor (TECH_SPEC §12).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordingState {
    Unknown,
    Ok,
    Degraded,
    Stopped,
}

impl RecordingState {
    pub fn as_str(self) -> &'static str {
        match self {
            RecordingState::Unknown => "unknown",
            RecordingState::Ok => "ok",
            RecordingState::Degraded => "degraded",
            RecordingState::Stopped => "stopped",
        }
    }

    fn code(self) -> u8 {
        match self {
            RecordingState::Unknown => 0,
            RecordingState::Ok => 1,
            RecordingState::Degraded => 2,
            RecordingState::Stopped => 3,
        }
    }

    fn from_code(value: u8) -> Self {
        match value {
            1 => RecordingState::Ok,
            2 => RecordingState::Degraded,
            3 => RecordingState::Stopped,
            _ => RecordingState::Unknown,
        }
    }
}

/// Microphone health as reported by the supervisor (TECH_SPEC §12).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MicrophoneState {
    Unknown,
    Ok,
    Disconnected,
}

impl MicrophoneState {
    pub fn as_str(self) -> &'static str {
        match self {
            MicrophoneState::Unknown => "unknown",
            MicrophoneState::Ok => "ok",
            MicrophoneState::Disconnected => "disconnected",
        }
    }

    fn code(self) -> u8 {
        match self {
            MicrophoneState::Unknown => 0,
            MicrophoneState::Ok => 1,
            MicrophoneState::Disconnected => 2,
        }
    }

    fn from_code(value: u8) -> Self {
        match value {
            1 => MicrophoneState::Ok,
            2 => MicrophoneState::Disconnected,
            _ => MicrophoneState::Unknown,
        }
    }
}

#[derive(Debug, Clone, Default)]
struct Reported {
    device: String,
    capture_rate_hz: u32,
    channels: u16,
    sample_format: String,
    time_quality: Option<TimeQuality>,
    time_synced: bool,
    clock_drift_ppm: Option<f64>,
}

/// Live values `/status` needs that no other module can supply on its own.
///
/// Everything starts at `unknown`/empty, and `HttpState::new` fills the capture
/// format in from the configuration so the endpoint reports the *configured*
/// stream until the supervisor reports the negotiated one. The supervisor
/// (the dispatcher/capture wiring in `main`) owns the setters.
#[derive(Debug)]
pub struct RuntimeStatus {
    running: AtomicBool,
    recording: AtomicU8,
    microphone: AtomicU8,
    reported: Mutex<Reported>,
    launch: Mutex<Option<LaunchInfo>>,
}

/// How this process was started. Reported by the API so an operator can answer
/// "which binary, with which config file, under which pid" without shell access
/// to the host — the same question systemd answers with `ExecStart`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchInfo {
    pub argv: Vec<String>,
    pub config_path: Option<String>,
    pub pid: u32,
}

impl Default for RuntimeStatus {
    fn default() -> Self {
        RuntimeStatus {
            running: AtomicBool::new(false),
            recording: AtomicU8::new(RecordingState::Unknown.code()),
            microphone: AtomicU8::new(MicrophoneState::Unknown.code()),
            reported: Mutex::new(Reported::default()),
            launch: Mutex::new(None),
        }
    }
}

impl RuntimeStatus {
    pub fn new() -> Self {
        Self::default()
    }

    /// True once capture and dispatch are actually running. Gates every `ok`
    /// that would otherwise be invented for a pipeline that never started.
    pub fn set_running(&self, running: bool) {
        self.running.store(running, Ordering::Relaxed);
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Relaxed)
    }

    /// Records the command line this process was started with.
    pub fn set_launch(&self, argv: Vec<String>, config_path: Option<String>) {
        if let Ok(mut slot) = self.launch.lock() {
            *slot = Some(LaunchInfo {
                argv,
                config_path,
                pid: std::process::id(),
            });
        }
    }

    pub fn launch(&self) -> Option<LaunchInfo> {
        self.launch.lock().ok().and_then(|slot| slot.clone())
    }

    pub fn set_recording(&self, state: RecordingState) {
        self.recording.store(state.code(), Ordering::Relaxed);
    }

    pub fn recording(&self) -> RecordingState {
        RecordingState::from_code(self.recording.load(Ordering::Relaxed))
    }

    pub fn set_microphone(&self, state: MicrophoneState) {
        self.microphone.store(state.code(), Ordering::Relaxed);
    }

    pub fn microphone(&self) -> MicrophoneState {
        MicrophoneState::from_code(self.microphone.load(Ordering::Relaxed))
    }

    pub fn set_device(&self, device: impl Into<String>) {
        if let Ok(mut reported) = self.reported.lock() {
            reported.device = device.into();
        }
    }

    /// Reports the format the device negotiation actually settled on.
    pub fn set_capture_format(&self, rate_hz: u32, channels: u16, sample_format: impl Into<String>) {
        if let Ok(mut reported) = self.reported.lock() {
            reported.capture_rate_hz = rate_hz;
            reported.channels = channels;
            reported.sample_format = sample_format.into();
        }
    }

    /// Reports the timeline state (`time_quality`, `time_synced`, drift).
    pub fn set_time(&self, quality: Option<TimeQuality>, synced: bool, drift_ppm: Option<f64>) {
        if let Ok(mut reported) = self.reported.lock() {
            reported.time_quality = quality;
            reported.time_synced = synced;
            reported.clock_drift_ppm = drift_ppm;
        }
    }

    fn seed_capture_format(&self, rate_hz: u32, channels: u16, sample_format: &str) {
        if let Ok(mut reported) = self.reported.lock() {
            if reported.capture_rate_hz == 0 {
                reported.capture_rate_hz = rate_hz;
            }
            if reported.channels == 0 {
                reported.channels = channels;
            }
            if reported.sample_format.is_empty() {
                reported.sample_format = sample_format.to_string();
            }
        }
    }

    fn reported(&self) -> Reported {
        self.reported
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

/// Everything the handlers need. See the module docs for the constructor.
pub struct HttpState {
    config: Config,
    database: Arc<Mutex<Database>>,
    metrics: Arc<Metrics>,
    runtime: Arc<RuntimeStatus>,
    web_root: PathBuf,
    /// Live transcript fan-out. `None` in tests and in a build without
    /// transcription; `/api/v1/transcript` then answers 503 rather than hanging.
    transcripts: Option<Arc<TranscriptHub>>,
}

impl HttpState {
    /// Builds the shared state. `runtime` is the handle the supervisor updates;
    /// the configured capture format is used as its initial value.
    pub fn new(
        config: Config,
        database: Arc<Mutex<Database>>,
        metrics: Arc<Metrics>,
        runtime: Arc<RuntimeStatus>,
    ) -> Self {
        runtime.seed_capture_format(
            config.audio.preferred_rate_hz,
            config.audio.channels,
            config.audio.format.as_str(),
        );
        // `server.web_dir` is the single place the UI location is configured;
        // the old "sibling of the recording directory" guess is kept only as a
        // fallback for configs written before the field existed.
        let web_root = if config.server.web_dir.trim().is_empty() {
            config
                .storage
                .recording_dir
                .parent()
                .map(|parent| parent.join("web"))
                .unwrap_or_else(|| PathBuf::from("web"))
        } else {
            PathBuf::from(&config.server.web_dir)
        };
        HttpState {
            config,
            database,
            metrics,
            runtime,
            web_root,
            transcripts: None,
        }
    }

    /// Overrides the directory the portal is served from (defaults to
    /// `<storage.recording_dir>/../web`).
    pub fn with_web_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.web_root = root.into();
        self
    }

    /// Attaches the live transcript fan-out.
    pub fn with_transcript_hub(mut self, hub: Arc<TranscriptHub>) -> Self {
        self.transcripts = Some(hub);
        self
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn runtime(&self) -> &Arc<RuntimeStatus> {
        &self.runtime
    }

    pub fn web_root(&self) -> &Path {
        &self.web_root
    }

    /// Runs `f` with the database lock held. The closure is synchronous, which
    /// is what keeps the guard from crossing an `.await`.
    fn with_database<T>(&self, f: impl FnOnce(&Database) -> Result<T>) -> Result<T> {
        let guard = self
            .database
            .lock()
            .map_err(|_| Error::Internal("database lock poisoned".to_string()))?;
        f(&guard)
    }
}

// ---------------------------------------------------------------------------
// Routing
// ---------------------------------------------------------------------------

/// Builds the full router: API, portal, and error fallbacks.
pub fn router(state: Arc<HttpState>) -> Router {
    Router::new()
        // The UI is a single-page app: one catch-all serves the built assets and
        // falls back to index.html so client-side routes survive a refresh.
        // Static API routes are more specific and therefore win the match.
        .route("/", get(portal_index))
        .route("/{*path}", get(portal_asset))
        .route("/api/v1/status", get(get_status))
        .route("/api/v1/days", get(get_days))
        .route("/api/v1/segments", get(get_segments))
        .route("/api/v1/segments/{id}/gaps", get(get_gaps))
        .route("/api/v1/segments/{id}/audio", get(get_audio))
        .route("/api/v1/transcripts", get(get_transcripts))
        .route("/api/v1/utterances", get(get_utterances))
        // Live transcript. WebSocket rather than SSE because it is the same
        // origin, the same cookies, and one bidirectional socket is enough; the
        // client never sends anything after the upgrade (see `stream_transcript`).
        .route("/api/v1/transcript", get(stream_transcript))
        .route("/api/v1/segments/{id}/transcript", get(get_segment_transcript))
        .route("/api/v1/events", get(get_events))
        .route("/api/v1/events/{id}", get(get_event).patch(patch_event))
        // The only endpoint that changes machine state; see the `control` module
        // for why it is off by default and loopback-only.
        .route("/api/v1/control", get(get_control).post(post_control))
        .fallback(not_found)
        .method_not_allowed_fallback(method_not_allowed)
        .with_state(state)
}

/// Binds the configured address and serves until the process stops.
///
/// Run this on a Tokio runtime sized by [`build_runtime`] so
/// `server.http_threads` is honoured.
pub async fn serve(state: Arc<HttpState>) -> Result<()> {
    serve_with_shutdown(state, std::future::pending::<()>()).await
}

/// [`serve`], but the listener closes when `shutdown` completes.
///
/// This exists because a listener with no stop signal outlives the process that
/// owns it: `axum::serve` never returns on its own, so a caller that waits on
/// its thread (`main`'s `server_thread.join()`) blocks forever after SIGTERM.
/// Stopping has to be requested, not inherited.
pub async fn serve_with_shutdown<F>(state: Arc<HttpState>, shutdown: F) -> Result<()>
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    let address = format!(
        "{}:{}",
        state.config.server.bind_address, state.config.server.port
    );
    let listener = tokio::net::TcpListener::bind(&address)
        .await
        .map_err(|error| Error::Http(format!("cannot bind {address}: {error}")))?;
    tracing::info!(%address, "HTTP API listening");
    let app = router(state);
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .await
        .map_err(|error| Error::Http(error.to_string()))
}

/// Builds the multi-threaded runtime the HTTP server should run on, sized by
/// the already-validated `server.http_threads` (TECH_SPEC §11: 1..=8).
pub fn build_runtime(config: &ServerConfig) -> std::io::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(config.http_threads)
        .enable_all()
        .build()
}

async fn not_found() -> Response {
    error_response(StatusCode::NOT_FOUND, "not_found", "no such endpoint")
}

async fn method_not_allowed() -> Response {
    error_response(
        StatusCode::METHOD_NOT_ALLOWED,
        "method_not_allowed",
        "method not allowed for this endpoint",
    )
}

// ---------------------------------------------------------------------------
// Query parameters
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct DateQuery {
    date: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DaysQuery {
    from: Option<String>,
    to: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ReviewBody {
    review_status: String,
}

/// A rejected query parameter. Kept as a small error type rather than a
/// `Response` so the failure path stays cheap.
enum ParamError {
    Missing(&'static str),
    Invalid(String),
}

impl ParamError {
    fn into_response(self) -> Response {
        match self {
            ParamError::Missing(name) => error_response(
                StatusCode::BAD_REQUEST,
                "invalid_parameter",
                format!("missing required query parameter `{name}` (expected YYYY-MM-DD)"),
            ),
            ParamError::Invalid(message) => {
                error_response(StatusCode::BAD_REQUEST, "invalid_parameter", message)
            }
        }
    }
}

fn required_date(
    value: Option<&str>,
    name: &'static str,
) -> std::result::Result<NaiveDate, ParamError> {
    let Some(raw) = value.map(str::trim).filter(|raw| !raw.is_empty()) else {
        return Err(ParamError::Missing(name));
    };
    parse_date(raw).map_err(|error| ParamError::Invalid(error.to_string()))
}

// ---------------------------------------------------------------------------
// /status
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct StatusView {
    service: ServiceView,
    recording: RecordingView,
    microphone: MicrophoneView,
    detector: DetectorView,
    storage: StorageView,
    capture: CaptureView,
    source: SourceView,
    queues: QueuesView,
    dsp: DspView,
    time: TimeView,
    stt: SttView,
    signal_state: &'static str,
}

/// Streaming transcription state. `partial` is the phrase being formed right
/// now: showing it is what makes the UI feel live, and it is never stored.
///
/// `enabled` / `mode` / `state` exist because `connected` alone cannot be read
/// by a client: it is `false` when transcription was never switched on and also
/// when it is switched on but the recogniser has not attached yet, and it stays
/// `false` forever under `mode = "segment"` where there is no live channel at
/// all. Those two cases must not look the same in the Portal.
#[derive(Debug, Serialize)]
struct SttView {
    enabled: bool,
    mode: &'static str,
    /// One of `off`, `starting`, `ready`, `segment`, `error`.
    state: &'static str,
    connected: bool,
    engine: String,
    finals_total: u64,
    dropped_blocks: u64,
    partial: Option<String>,
    error: Option<String>,
    /// The command line the worker uses, filled in when it starts. `null` when
    /// transcription is off, because nothing is running to describe.
    launch: Option<SttLaunchView>,
    /// How many WebSocket clients are listening to `/api/v1/transcript`. Zero is
    /// normal; a number that stays above zero after every page is closed means a
    /// socket leaked.
    stream_clients: usize,
}

#[derive(Debug, Serialize)]
struct SttLaunchView {
    command: String,
    args: Vec<String>,
}

/// The word the Portal branches on. Derived, never stored, and ordered so the
/// most actionable fact wins: an error is reported even before a connection.
fn stt_state(config: &Config, connected: bool, error: Option<&str>) -> &'static str {
    if !config.stt.enabled {
        return "off";
    }
    if error.is_some() {
        return "error";
    }
    // Per-segment transcription has no long-lived channel, so `connected` is
    // meaningless there and must not be rendered as a fault.
    if config.stt.mode == SttMode::Segment {
        return "segment";
    }
    if connected { "ready" } else { "starting" }
}

#[derive(Debug, Serialize)]
struct ServiceView {
    state: &'static str,
    version: &'static str,
    now_utc: DateTime<Utc>,
    last_error: Option<String>,
    /// `null` when the process did not record it (tests, embedded use).
    launch: Option<LaunchView>,
}

#[derive(Debug, Serialize)]
struct LaunchView {
    argv: Vec<String>,
    config_path: Option<String>,
    pid: u32,
}

impl From<LaunchInfo> for LaunchView {
    fn from(info: LaunchInfo) -> Self {
        LaunchView {
            argv: info.argv,
            config_path: info.config_path,
            pid: info.pid,
        }
    }
}

#[derive(Debug, Serialize)]
struct RecordingView {
    state: &'static str,
}

#[derive(Debug, Serialize)]
struct MicrophoneView {
    state: &'static str,
    device: String,
    capture_rate_hz: u32,
    channels: u16,
    sample_format: String,
}

#[derive(Debug, Serialize)]
struct DetectorView {
    state: &'static str,
    noise_floor_dbfs: Option<f32>,
    band_noise_floor_dbfs: Option<f32>,
    events_total: u64,
    events_discarded_short: u64,
    event_fk_retries: u64,
}

#[derive(Debug, Serialize)]
struct StorageView {
    /// `None` when the database could not answer. Reporting `0` would read as
    /// "this pool is empty", which is a different and more alarming claim.
    used_bytes: Option<u64>,
    max_bytes: u64,
    /// What retention deletes down to (must stay below `max_bytes`).
    cleanup_target_bytes: u64,
    /// Free space kept for SQLite and the OS.
    reserve_bytes: u64,
    free_bytes: Option<u64>,
    recording_dir: String,
    storage_pressure: bool,
    /// Complete/interrupted segments the byte total is made of.
    segment_count: Option<u64>,
    oldest_started_at_utc: Option<DateTime<Utc>>,
    newest_ended_at_utc: Option<DateTime<Utc>>,
}

#[derive(Debug, Serialize)]
struct CaptureView {
    xrun_count: u64,
    lost_frames_total: u64,
    capture_ring_overflow_frames: u64,
    dropped_detection_blocks: u64,
    gap_detection_limited: bool,
    clipped_samples_total: u64,
}

/// Split-deployment counters. Present for both source kinds so a client does not
/// have to branch, but only meaningful when `kind = "net"`.
#[derive(Debug, Serialize)]
struct SourceView {
    kind: &'static str,
    /// Whether a capture node has announced itself and is still sending.
    node_connected: bool,
    /// Wall clock of the newest datagram, or `null` before the first one. This
    /// is the heartbeat a client should show: `node_connected` says whether the
    /// node counts as present, this says how fresh the evidence is.
    last_packet_at_utc: Option<DateTime<Utc>>,
    packets_received: u64,
    packets_lost: u64,
    packets_duplicate: u64,
    packets_reordered: u64,
    protocol_errors: u64,
    /// The node's own description of itself (argv, config path, version), as it
    /// sent it. `null` until a node reports — which an older node never does.
    node_report: Option<serde_json::Value>,
    node_report_at_utc: Option<DateTime<Utc>>,
    /// Frames the node said it did not send (uplink gating). Divide by
    /// `microphone.capture_rate_hz` for seconds; `0` means nothing was gated.
    gated_frames: u64,
    gate_announcements: u64,
}

#[derive(Debug, Serialize)]
struct QueuesView {
    recording_high_water: u64,
    detection_high_water: u64,
    recording_control_slots: u64,
    detection_control_slots: u64,
}

#[derive(Debug, Serialize)]
struct DspView {
    count: u64,
    avg_ns: u64,
    p99_ns: u64,
    max_ns: u64,
}

#[derive(Debug, Serialize)]
struct TimeView {
    time_quality: &'static str,
    time_synced: bool,
    clock_drift_ppm: Option<f64>,
}

/// Microphone state for the status page.
///
/// With `source.kind = "net"` there is no local device to ask: the only truth is
/// whether a capture node has attached and is still sending, so that flag drives
/// the reported state. Reporting `ok` because a thread happens to be running
/// would tell the operator "recording is fine" while the bedroom is silent.
fn microphone_state(state: &HttpState, metrics: &Metrics) -> MicrophoneState {
    if state.config.source.kind == SourceKind::Net {
        return if metrics.net_source_connected.load(Ordering::Relaxed) {
            MicrophoneState::Ok
        } else {
            MicrophoneState::Disconnected
        };
    }
    state.runtime.microphone()
}

async fn get_status(State(state): State<Arc<HttpState>>) -> Response {
    let metrics = &state.metrics;
    // A failure here must not take the status page down; the byte fields go
    // `null` and the reason goes to the log rather than being invented as 0.
    let pool = match state.with_database(|db| db.pool_summary()) {
        Ok(pool) => Some(pool),
        Err(error) => {
            tracing::warn!(%error, "status could not read the recording pool");
            None
        }
    };
    let reported = state.runtime.reported();
    let degraded = metrics.degraded.load(Ordering::Relaxed);
    let signal_state = if metrics.signal_silent.load(Ordering::Relaxed) {
        "silent"
    } else if state.runtime.is_running() {
        "ok"
    } else {
        "unknown"
    };
    let dsp = metrics.dsp_block_time.summary();

    json_response(
        StatusCode::OK,
        &StatusView {
            service: ServiceView {
                state: if degraded { "degraded" } else { "ok" },
                version: env!("CARGO_PKG_VERSION"),
                now_utc: Utc::now(),
                last_error: metrics.last_error(),
                launch: state.runtime.launch().map(LaunchView::from),
            },
            recording: RecordingView {
                state: state.runtime.recording().as_str(),
            },
            microphone: MicrophoneView {
                state: microphone_state(&state, metrics).as_str(),
                device: reported.device,
                capture_rate_hz: reported.capture_rate_hz,
                channels: reported.channels,
                sample_format: reported.sample_format,
            },
            detector: DetectorView {
                state: metrics.detector_state().as_str(),
                noise_floor_dbfs: metrics.noise_floor_dbfs(),
                band_noise_floor_dbfs: metrics.band_noise_floor_dbfs(),
                events_total: metrics.events_total.load(Ordering::Relaxed),
                events_discarded_short: metrics.events_discarded_short.load(Ordering::Relaxed),
                event_fk_retries: metrics.event_fk_retries.load(Ordering::Relaxed),
            },
            storage: StorageView {
                used_bytes: pool.as_ref().map(|pool| pool.used_bytes),
                max_bytes: state.config.storage.recording_max_bytes,
                cleanup_target_bytes: state.config.storage.recording_cleanup_target_bytes,
                reserve_bytes: state.config.storage.recording_reserve_bytes,
                free_bytes: free_bytes(&state.config.storage.recording_dir),
                recording_dir: state.config.storage.recording_dir.display().to_string(),
                storage_pressure: metrics.storage_pressure.load(Ordering::Relaxed),
                segment_count: pool.as_ref().map(|pool| pool.segment_count),
                oldest_started_at_utc: pool.as_ref().and_then(|pool| pool.oldest_started_at_utc),
                newest_ended_at_utc: pool.as_ref().and_then(|pool| pool.newest_ended_at_utc),
            },
            capture: CaptureView {
                xrun_count: metrics.xrun_count.load(Ordering::Relaxed),
                lost_frames_total: metrics.lost_frames_total.load(Ordering::Relaxed),
                capture_ring_overflow_frames: metrics
                    .capture_ring_overflow_frames
                    .load(Ordering::Relaxed),
                dropped_detection_blocks: metrics
                    .dropped_detection_blocks
                    .load(Ordering::Relaxed),
                gap_detection_limited: metrics.gap_detection_limited.load(Ordering::Relaxed),
                clipped_samples_total: metrics.clipped_samples_total.load(Ordering::Relaxed),
            },
            source: SourceView {
                kind: state.config.source.kind.as_str(),
                node_connected: metrics.net_source_connected.load(Ordering::Relaxed),
                last_packet_at_utc: metrics
                    .net_last_packet_unix_ms()
                    .and_then(DateTime::from_timestamp_millis),
                packets_received: metrics.net_packets_received.load(Ordering::Relaxed),
                packets_lost: metrics.net_packets_lost.load(Ordering::Relaxed),
                packets_duplicate: metrics.net_packets_duplicate.load(Ordering::Relaxed),
                packets_reordered: metrics.net_packets_reordered.load(Ordering::Relaxed),
                protocol_errors: metrics.net_protocol_errors.load(Ordering::Relaxed),
                node_report: metrics
                    .node_report()
                    .and_then(|text| serde_json::from_str(&text).ok()),
                node_report_at_utc: match metrics.net_node_report_unix_ms.load(Ordering::Relaxed) {
                    0 => None,
                    ms => DateTime::from_timestamp_millis(ms),
                },
                gated_frames: metrics.net_gated_frames.load(Ordering::Relaxed),
                gate_announcements: metrics.net_gate_announcements.load(Ordering::Relaxed),
            },
            queues: QueuesView {
                recording_high_water: metrics.recording_queue_high_water.load(Ordering::Relaxed),
                detection_high_water: metrics.detection_queue_high_water.load(Ordering::Relaxed),
                recording_control_slots: metrics
                    .recording_queue_control_slots
                    .load(Ordering::Relaxed),
                detection_control_slots: metrics
                    .detection_queue_control_slots
                    .load(Ordering::Relaxed),
            },
            dsp: DspView {
                count: dsp.count,
                avg_ns: dsp.avg_ns,
                p99_ns: dsp.p99_ns,
                max_ns: dsp.max_ns,
            },
            time: TimeView {
                time_quality: reported.time_quality.map_or("unknown", TimeQuality::as_str),
                time_synced: reported.time_synced,
                clock_drift_ppm: reported.clock_drift_ppm,
            },
            stt: SttView {
                enabled: state.config.stt.enabled,
                mode: state.config.stt.mode.as_str(),
                state: stt_state(&state.config, metrics.stt_connected.load(Ordering::Relaxed), metrics.stt_error().as_deref()),
                connected: metrics.stt_connected.load(Ordering::Relaxed),
                engine: metrics.stt_engine(),
                finals_total: metrics.stt_finals.load(Ordering::Relaxed),
                dropped_blocks: metrics.dropped_stt_blocks.load(Ordering::Relaxed),
                partial: metrics.stt_partial(),
                error: metrics.stt_error(),
                launch: metrics.stt_launch().map(|(command, args)| SttLaunchView {
                    command,
                    args,
                }),
                stream_clients: state
                    .transcripts
                    .as_ref()
                    .map(|hub| hub.subscribers())
                    .unwrap_or(0),
            },
            signal_state,
        },
    )
}

/// Free bytes on the filesystem holding the recordings, or `None` when the
/// platform or the call cannot answer (the field is nullable on purpose).
#[cfg(unix)]
fn free_bytes(path: &Path) -> Option<u64> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let path = CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut stats = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: `path` is a valid NUL-terminated C string and `stats` is a valid
    // pointer to uninitialized memory of the exact struct `statvfs` fills in.
    let result = unsafe { libc::statvfs(path.as_ptr(), stats.as_mut_ptr()) };
    if result != 0 {
        return None;
    }
    // SAFETY: a zero return means `statvfs` initialized the whole struct.
    let stats = unsafe { stats.assume_init() };
    // `c_ulong` is 32-bit on armv7 and 64-bit on aarch64/x86_64, so these casts
    // are load-bearing on the Pi even though they are no-ops on a 64-bit host.
    #[allow(clippy::unnecessary_cast)]
    Some((stats.f_bavail as u64).saturating_mul(stats.f_frsize as u64))
}

#[cfg(not(unix))]
fn free_bytes(_path: &Path) -> Option<u64> {
    None
}

// ---------------------------------------------------------------------------
// Views shared by several endpoints
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct SegmentView {
    id: String,
    started_at_utc: DateTime<Utc>,
    ended_at_utc: Option<DateTime<Utc>>,
    capture_rate_hz: u32,
    channels: u16,
    sample_format: String,
    frame_count: u64,
    size_bytes: u64,
    duration_ms: u64,
    status: String,
    time_quality: String,
    end_reason: Option<String>,
    lost_frames_total: u64,
    gap_count: u64,
    xrun_count: u64,
    clock_drift_ppm: Option<f64>,
    audio_url: String,
}

#[derive(Debug, Serialize)]
struct EventView {
    id: String,
    segment_id: String,
    capture_rate_hz: u32,
    start_offset_frames: u64,
    end_offset_frames: u64,
    started_at_utc: DateTime<Utc>,
    ended_at_utc: DateTime<Utc>,
    duration_ms: u32,
    rule_score: Option<f32>,
    detector_version: String,
    peak_dbfs: Option<f32>,
    mean_level_dbfs: Option<f32>,
    mean_band_ratio: Option<f32>,
    noise_floor_dbfs: Option<f32>,
    end_reason: String,
    review_status: String,
    continued: bool,
}

#[derive(Debug, Serialize)]
struct GapView {
    offset_frames: u64,
    lost_frames: u64,
    kind: &'static str,
    estimate_source: &'static str,
    at_utc: DateTime<Utc>,
}

/// Audio duration of a segment from the authoritative stored frame count, so a
/// segment containing gaps is not overstated by wall-clock differences.
fn duration_ms(frame_count: u64, capture_rate_hz: u32) -> u64 {
    if capture_rate_hz == 0 {
        return 0;
    }
    frame_count.saturating_mul(1000) / u64::from(capture_rate_hz)
}

fn segment_view(segment: &SegmentRow) -> SegmentView {
    SegmentView {
        id: segment.id.clone(),
        started_at_utc: segment.started_at_utc,
        ended_at_utc: segment.ended_at_utc,
        capture_rate_hz: segment.capture_rate_hz,
        channels: segment.channels,
        sample_format: segment.sample_format.clone(),
        frame_count: segment.frame_count,
        size_bytes: segment.size_bytes,
        duration_ms: duration_ms(segment.frame_count, segment.capture_rate_hz),
        status: segment.status.clone(),
        time_quality: segment.time_quality.clone(),
        end_reason: segment.end_reason.clone(),
        lost_frames_total: segment.lost_frames_total,
        gap_count: segment.gap_count,
        xrun_count: segment.xrun_count,
        clock_drift_ppm: segment.clock_drift_ppm,
        audio_url: format!("{API_PREFIX}/segments/{}/audio", segment.id),
    }
}

fn event_view(event: &EventRow, capture_rate_hz: u32) -> EventView {
    EventView {
        id: event.id.clone(),
        segment_id: event.segment_id.clone(),
        capture_rate_hz,
        start_offset_frames: event.start_offset_frames,
        end_offset_frames: event.end_offset_frames,
        started_at_utc: event.started_at_utc,
        ended_at_utc: event.ended_at_utc,
        duration_ms: event.duration_ms,
        rule_score: event.rule_score,
        detector_version: event.detector_version.clone(),
        peak_dbfs: event.peak_dbfs,
        mean_level_dbfs: event.mean_level_dbfs,
        mean_band_ratio: event.mean_band_ratio,
        noise_floor_dbfs: event.noise_floor_dbfs,
        end_reason: event.end_reason.clone(),
        review_status: event.review_status.clone(),
        continued: event.continued,
    }
}

/// Capture rate for an event: the rate of the segment it belongs to, falling
/// back to the detection rate only if the segment row has disappeared.
fn capture_rate_for(db: &Database, segment_id: &str, fallback: u32) -> Result<u32> {
    Ok(db
        .segment(segment_id)?
        .map_or(fallback, |segment| segment.capture_rate_hz))
}

// ---------------------------------------------------------------------------
// /days
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct DaysView {
    from: String,
    to: String,
    days: Vec<DayView>,
}

#[derive(Debug, Serialize)]
struct DayView {
    date: String,
    segment_count: usize,
    event_count: usize,
    recorded_ms: u64,
    lost_frames_total: u64,
}

async fn get_days(
    State(state): State<Arc<HttpState>>,
    query: std::result::Result<Query<DaysQuery>, QueryRejection>,
) -> Response {
    let Query(query) = match query {
        Ok(query) => query,
        Err(rejection) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                "invalid_parameter",
                rejection.body_text(),
            );
        }
    };
    let from = match required_date(query.from.as_deref(), "from") {
        Ok(date) => date,
        Err(error) => return error.into_response(),
    };
    let to = match required_date(query.to.as_deref(), "to") {
        Ok(date) => date,
        Err(error) => return error.into_response(),
    };
    if from > to {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_parameter",
            format!("`from` ({from}) must not be after `to` ({to})"),
        );
    }
    if (to - from).num_days() >= MAX_DAYS_SPAN {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_parameter",
            format!("requested span must be at most {MAX_DAYS_SPAN} days"),
        );
    }

    let timezone = state.config.server.timezone();
    let collected = state.with_database(|db| {
        let mut days = Vec::new();
        let mut date = from;
        loop {
            let (start, end) = local_day_range(date, timezone);
            let segments = db.segments_between(start, end)?;
            let events = db.events_between(start, end)?;
            // Only nights that actually hold data, per TECH_SPEC §8/§9: the date
            // list is a recording list, not a calendar.
            if !segments.is_empty() || !events.is_empty() {
                days.push(DayView {
                    date: date.format("%Y-%m-%d").to_string(),
                    segment_count: segments.len(),
                    event_count: events.len(),
                    recorded_ms: segments
                        .iter()
                        .map(|segment| duration_ms(segment.frame_count, segment.capture_rate_hz))
                        .sum(),
                    lost_frames_total: segments.iter().map(|s| s.lost_frames_total).sum(),
                });
            }
            if date == to {
                break;
            }
            match date.succ_opt() {
                Some(next) => date = next,
                None => break,
            }
        }
        Ok(days)
    });

    match collected {
        Ok(days) => json_response(
            StatusCode::OK,
            &DaysView {
                from: from.format("%Y-%m-%d").to_string(),
                to: to.format("%Y-%m-%d").to_string(),
                days,
            },
        ),
        Err(error) => internal_error(&error),
    }
}

// ---------------------------------------------------------------------------
// /segments
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct SegmentsView {
    date: String,
    segments: Vec<SegmentView>,
}

async fn get_segments(
    State(state): State<Arc<HttpState>>,
    query: std::result::Result<Query<DateQuery>, QueryRejection>,
) -> Response {
    let Query(query) = match query {
        Ok(query) => query,
        Err(rejection) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                "invalid_parameter",
                rejection.body_text(),
            );
        }
    };
    let date = match required_date(query.date.as_deref(), "date") {
        Ok(date) => date,
        Err(error) => return error.into_response(),
    };
    let (start, end) = local_day_range(date, state.config.server.timezone());

    match state.with_database(|db| db.segments_between(start, end)) {
        Ok(rows) => json_response(
            StatusCode::OK,
            &SegmentsView {
                date: date.format("%Y-%m-%d").to_string(),
                segments: rows.iter().map(segment_view).collect(),
            },
        ),
        Err(error) => internal_error(&error),
    }
}

// ---------------------------------------------------------------------------
// /segments/{id}/gaps
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct GapsView {
    segment_id: String,
    capture_rate_hz: u32,
    gaps: Vec<GapView>,
}

async fn get_gaps(
    State(state): State<Arc<HttpState>>,
    RoutePath(id): RoutePath<String>,
) -> Response {
    let found = state.with_database(|db| {
        let Some(segment) = db.segment(&id)? else {
            return Ok(None);
        };
        let gaps = db.gaps_for_segment(&id)?;
        Ok(Some((segment.capture_rate_hz, gaps)))
    });

    match found {
        Ok(Some((capture_rate_hz, gaps))) => json_response(
            StatusCode::OK,
            &GapsView {
                segment_id: id,
                capture_rate_hz,
                gaps: gaps
                    .iter()
                    .map(|gap: &GapRecord| GapView {
                        offset_frames: gap.offset_frames,
                        lost_frames: gap.lost_frames,
                        kind: gap.kind.as_str(),
                        estimate_source: gap.estimate_source.as_str(),
                        at_utc: gap.at_utc,
                    })
                    .collect(),
            },
        ),
        Ok(None) => error_response(
            StatusCode::NOT_FOUND,
            "not_found",
            format!("no segment with id {id}"),
        ),
        Err(error) => internal_error(&error),
    }
}

// ---------------------------------------------------------------------------
// /events
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct EventsView {
    date: String,
    /// Nominal rate for the day (the detector's fixed 16 kHz in the MVP). Each
    /// event also carries the capture rate of its own segment, which is what a
    /// client must use to convert offsets to seconds.
    capture_rate_hz: u32,
    events: Vec<EventView>,
}

#[derive(Debug, Serialize)]
struct UtterancesView {
    date: String,
    utterances: Vec<UtteranceView>,
}

#[derive(Debug, Serialize)]
struct UtteranceView {
    id: String,
    started_at_utc: chrono::DateTime<chrono::Utc>,
    ended_at_utc: chrono::DateTime<chrono::Utc>,
    engine: String,
    language: Option<String>,
    text: String,
    /// Phrase timings inside the utterance, already parsed for the UI.
    segments: serde_json::Value,
}

fn utterance_view(row: &crate::storage::UtteranceRow) -> UtteranceView {
    UtteranceView {
        id: row.id.clone(),
        started_at_utc: row.started_at_utc,
        ended_at_utc: row.ended_at_utc,
        engine: row.engine.clone(),
        language: row.language.clone(),
        text: row.text.clone(),
        segments: serde_json::from_str(&row.segments_json)
            .unwrap_or_else(|_| serde_json::Value::Array(Vec::new())),
    }
}

async fn get_utterances(
    State(state): State<Arc<HttpState>>,
    query: std::result::Result<Query<DateQuery>, QueryRejection>,
) -> Response {
    let Query(query) = match query {
        Ok(query) => query,
        Err(rejection) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                "invalid_parameter",
                rejection.body_text(),
            );
        }
    };
    let date = match required_date(query.date.as_deref(), "date") {
        Ok(date) => date,
        Err(error) => return error.into_response(),
    };
    let (start, end) = local_day_range(date, state.config.server.timezone());

    match state.with_database(|db| db.utterances_between(start, end)) {
        Ok(rows) => json_response(
            StatusCode::OK,
            &UtterancesView {
                date: date.format("%Y-%m-%d").to_string(),
                utterances: rows.iter().map(utterance_view).collect(),
            },
        ),
        Err(error) => internal_error(&error),
    }
}

#[derive(Debug, Serialize)]
struct TranscriptsView {
    date: String,
    transcripts: Vec<TranscriptView>,
}

#[derive(Debug, Serialize)]
struct SegmentTranscriptView {
    segment_id: String,
    transcript: Option<TranscriptView>,
}

#[derive(Debug, Serialize)]
struct TranscriptView {
    segment_id: String,
    engine: String,
    language: Option<String>,
    text: String,
    /// Phrase timings, already parsed so the UI does not have to.
    segments: serde_json::Value,
    status: String,
    error: Option<String>,
    audio_seconds: Option<f64>,
    created_at_utc: chrono::DateTime<chrono::Utc>,
}

fn transcript_view(row: &TranscriptRow) -> TranscriptView {
    TranscriptView {
        segment_id: row.segment_id.clone(),
        engine: row.engine.clone(),
        language: row.language.clone(),
        text: row.text.clone(),
        segments: serde_json::from_str(&row.segments_json)
            .unwrap_or_else(|_| serde_json::Value::Array(Vec::new())),
        status: row.status.clone(),
        error: row.error.clone(),
        audio_seconds: row.audio_seconds,
        created_at_utc: row.created_at_utc,
    }
}

async fn get_transcripts(
    State(state): State<Arc<HttpState>>,
    query: std::result::Result<Query<DateQuery>, QueryRejection>,
) -> Response {
    let Query(query) = match query {
        Ok(query) => query,
        Err(rejection) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                "invalid_parameter",
                rejection.body_text(),
            );
        }
    };
    let date = match required_date(query.date.as_deref(), "date") {
        Ok(date) => date,
        Err(error) => return error.into_response(),
    };
    let (start, end) = local_day_range(date, state.config.server.timezone());

    match state.with_database(|db| db.transcripts_between(start, end)) {
        Ok(rows) => json_response(
            StatusCode::OK,
            &TranscriptsView {
                date: date.format("%Y-%m-%d").to_string(),
                transcripts: rows.iter().map(transcript_view).collect(),
            },
        ),
        Err(error) => internal_error(&error),
    }
}

async fn get_segment_transcript(
    State(state): State<Arc<HttpState>>,
    RoutePath(id): RoutePath<String>,
) -> Response {
    match state.with_database(|db| db.transcript_for_segment(&id)) {
        Ok(transcript) => json_response(
            StatusCode::OK,
            &SegmentTranscriptView {
                segment_id: id,
                transcript: transcript.as_ref().map(transcript_view),
            },
        ),
        Err(error) => internal_error(&error),
    }
}

async fn get_events(
    State(state): State<Arc<HttpState>>,
    query: std::result::Result<Query<DateQuery>, QueryRejection>,
) -> Response {
    let Query(query) = match query {
        Ok(query) => query,
        Err(rejection) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                "invalid_parameter",
                rejection.body_text(),
            );
        }
    };
    let date = match required_date(query.date.as_deref(), "date") {
        Ok(date) => date,
        Err(error) => return error.into_response(),
    };
    let (start, end) = local_day_range(date, state.config.server.timezone());
    let fallback_rate = state.config.detector.detection_rate_hz;

    let collected = state.with_database(|db| {
        let events = db.events_between(start, end)?;
        let segments = db.segments_between(start, end)?;
        let mut rates: HashMap<String, u32> = segments
            .into_iter()
            .map(|segment| (segment.id.clone(), segment.capture_rate_hz))
            .collect();
        let mut views = Vec::with_capacity(events.len());
        for event in &events {
            let rate = match rates.get(&event.segment_id) {
                Some(rate) => *rate,
                None => {
                    // An in-progress segment that began before the day starts is
                    // not returned by `segments_between`, so look it up directly.
                    let rate = capture_rate_for(db, &event.segment_id, fallback_rate)?;
                    rates.insert(event.segment_id.clone(), rate);
                    rate
                }
            };
            views.push(event_view(event, rate));
        }
        Ok(views)
    });

    match collected {
        Ok(events) => json_response(
            StatusCode::OK,
            &EventsView {
                date: date.format("%Y-%m-%d").to_string(),
                capture_rate_hz: fallback_rate,
                events,
            },
        ),
        Err(error) => internal_error(&error),
    }
}

#[derive(Debug, Serialize)]
struct EventDetailView {
    event: EventView,
    segment: SegmentView,
}

async fn get_event(
    State(state): State<Arc<HttpState>>,
    RoutePath(id): RoutePath<String>,
) -> Response {
    let fallback_rate = state.config.detector.detection_rate_hz;
    let found = state.with_database(|db| match db.event_with_segment(&id)? {
        Some((event, segment)) => Ok(Some((event, segment))),
        None => Ok(None),
    });

    match found {
        Ok(Some((event, segment))) => {
            let rate = capture_rate_or(&segment, fallback_rate);
            json_response(
                StatusCode::OK,
                &EventDetailView {
                    event: event_view(&event, rate),
                    segment: segment_view(&segment),
                },
            )
        }
        Ok(None) => error_response(
            StatusCode::NOT_FOUND,
            "not_found",
            format!("no event with id {id}"),
        ),
        Err(error) => internal_error(&error),
    }
}

fn capture_rate_or(segment: &SegmentRow, fallback: u32) -> u32 {
    if segment.capture_rate_hz > 0 {
        segment.capture_rate_hz
    } else {
        fallback
    }
}

/// `PATCH /api/v1/events/{id}` — only the three review labels are accepted.
/// Returns the updated event object itself.
async fn patch_event(
    State(state): State<Arc<HttpState>>,
    RoutePath(id): RoutePath<String>,
    body: std::result::Result<Json<ReviewBody>, JsonRejection>,
) -> Response {
    let Json(body) = match body {
        Ok(body) => body,
        Err(rejection) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                "invalid_body",
                format!("expected JSON body {{\"review_status\":...}}: {}", rejection.body_text()),
            );
        }
    };
    let status = match ReviewStatus::try_from(body.review_status.as_str()) {
        Ok(status) => status,
        Err(_) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                "invalid_review_status",
                format!(
                    "review_status must be one of \"snore\", \"not_snore\", \"unreviewed\", got {:?}",
                    body.review_status
                ),
            );
        }
    };

    let fallback_rate = state.config.detector.detection_rate_hz;
    let updated = state.with_database(|db| {
        if !db.set_review_status(&id, status)? {
            return Ok(None);
        }
        let Some(event) = db.event(&id)? else {
            return Ok(None);
        };
        let rate = capture_rate_for(db, &event.segment_id, fallback_rate)?;
        Ok(Some(event_view(&event, rate)))
    });

    match updated {
        Ok(Some(event)) => json_response(StatusCode::OK, &event),
        Ok(None) => error_response(
            StatusCode::NOT_FOUND,
            "not_found",
            format!("no event with id {id}"),
        ),
        Err(error) => internal_error(&error),
    }
}

// ---------------------------------------------------------------------------
// /control
// ---------------------------------------------------------------------------

/// Capability plus the live state of both units.
///
/// `systemctl is-active` is spawned on every poll on purpose: caching the answer
/// would let the page show a state the operator has already changed elsewhere
/// (`systemctl stop` in a shell, a unit that failed on its own).
#[derive(Debug, Serialize)]
struct ControlView {
    /// `[control] enabled`.
    enabled: bool,
    /// Whether systemd is actually usable here; when false every `state` is
    /// `unknown` and every action is refused with `reason`.
    available: bool,
    /// Operator-facing reason for `available = false`, derived from the probe.
    reason: Option<&'static str>,
    systemctl: String,
    units: Vec<UnitView>,
}

#[derive(Debug, Serialize)]
struct UnitView {
    target: &'static str,
    unit: String,
    /// `active`, `inactive`, `failed`, … or `unknown` when nothing could be
    /// asked. Never defaulted to `inactive`.
    state: &'static str,
}

#[derive(Debug, Serialize)]
struct ControlOutcome {
    ok: bool,
    /// The refusal reason from systemctl (or the probe), or `None` on success.
    message: Option<String>,
    /// State read back after the action, so the caller never has to assume it
    /// worked.
    control: ControlView,
}

#[derive(Debug, Deserialize)]
struct ControlBody {
    target: String,
    action: String,
}

/// Blocking: spawns up to two `systemctl is-active` calls.
fn control_view(config: &Config) -> ControlView {
    let availability = control::availability(config);
    ControlView {
        enabled: config.control.enabled,
        available: availability.is_ready(),
        reason: availability.reason(),
        systemctl: config.control.systemctl_command.clone(),
        units: [Target::Server, Target::Node]
            .iter()
            .map(|target| UnitView {
                target: target.as_str(),
                unit: target.unit(config).to_string(),
                state: control::unit_state(config, *target),
            })
            .collect(),
    }
}

async fn get_control(State(state): State<Arc<HttpState>>) -> Response {
    match tokio::task::spawn_blocking(move || control_view(&state.config)).await {
        Ok(view) => json_response(StatusCode::OK, &view),
        Err(error) => internal_error(&Error::Internal(format!("control probe panicked: {error}"))),
    }
}

/// Runs one systemctl action.
///
/// Stopping or restarting the `server` unit takes this process down with it, so
/// the action is performed *before* the response is written and the client may
/// see the connection drop instead of a body. The Portal treats a failed
/// request after `stop`/`restart` on `server` as "likely applied" and lets the
/// next poll decide, rather than reporting a failure it cannot verify.
async fn post_control(
    State(state): State<Arc<HttpState>>,
    payload: std::result::Result<Json<ControlBody>, JsonRejection>,
) -> Response {
    let Json(body) = match payload {
        Ok(payload) => payload,
        Err(rejection) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                "invalid_body",
                rejection.body_text(),
            );
        }
    };
    let Some(target) = Target::parse(&body.target) else {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_parameter",
            format!(
                "target must be \"server\" or \"node\", got {:?}",
                body.target
            ),
        );
    };
    let Some(action) = Action::parse(&body.action) else {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_parameter",
            format!(
                "action must be \"start\", \"stop\" or \"restart\", got {:?}",
                body.action
            ),
        );
    };
    if !state.config.control.enabled {
        // Refused before anything is spawned, and refused the same way whether
        // or not the caller can see the config file.
        return error_response(
            StatusCode::FORBIDDEN,
            "control_disabled",
            "service control is disabled ([control] enabled = false)",
        );
    }

    let performed = tokio::task::spawn_blocking(move || {
        let result = control::perform(&state.config, target, action);
        let view = control_view(&state.config);
        (result, view)
    })
    .await;

    match performed {
        Ok((Ok(()), control)) => json_response(
            StatusCode::OK,
            &ControlOutcome {
                ok: true,
                message: None,
                control,
            },
        ),
        Ok((Err(error), control)) => {
            let status = match &error {
                control::ControlError::Unavailable(_) => StatusCode::FORBIDDEN,
                control::ControlError::Refused { .. } => StatusCode::CONFLICT,
                _ => StatusCode::INTERNAL_SERVER_ERROR,
            };
            json_response(
                status,
                &ControlOutcome {
                    ok: false,
                    message: Some(error.to_string()),
                    control,
                },
            )
        }
        Err(error) => internal_error(&Error::Internal(format!("control action panicked: {error}"))),
    }
}

// ---------------------------------------------------------------------------
// /transcript (WebSocket)
// ---------------------------------------------------------------------------

/// Pushes recognised text as the recogniser produces it.
///
/// Polling `/status` gave the page a 4 s window in which a partial could appear
/// and be replaced before the next poll: the live line stuttered, and a phrase
/// that was spoken and finalised inside that window was never shown at all.
///
/// The socket only carries *server decisions*: a `final` arrives after its row is
/// written, so a client cannot display a phrase the database rejected. A slow
/// client is dropped forward rather than allowed to stall the worker, and it is
/// told how much it missed (`lagged`) instead of silently rendering a transcript
/// with holes.
async fn stream_transcript(
    State(state): State<Arc<HttpState>>,
    upgrade: WebSocketUpgrade,
) -> Response {
    let Some(hub) = state.transcripts.clone() else {
        // No transcription worker in this process: say so instead of holding a
        // socket open that will never say anything.
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "error": "transcription is not running in this process",
            })),
        )
            .into_response();
    };
    let engine = state.metrics.stt_engine();
    let finals_total = state.metrics.stt_finals.load(Ordering::Relaxed);
    upgrade.on_upgrade(move |socket| pump_transcript(socket, hub, engine, finals_total))
}

/// Forwards hub events to one client until it goes away.
async fn pump_transcript(
    mut socket: WebSocket,
    hub: Arc<TranscriptHub>,
    engine: String,
    finals_total: u64,
) {
    let mut receiver = hub.subscribe();
    // Tell the client the stream is live and what it is *not* going to receive:
    // everything before this moment is history, fetched over HTTP.
    let hello = TranscriptEvent::Ready {
        engine,
        finals_total,
    };
    if send_event(&mut socket, &hello).await.is_err() {
        return;
    }

    loop {
        tokio::select! {
            // A client that closes sends a Close frame; without reading we would
            // only notice on the next failed write.
            incoming = socket.recv() => {
                match incoming {
                    None | Some(Ok(Message::Close(_))) | Some(Err(_)) => return,
                    // Nothing is expected from the client, but a ping must be
                    // answered and an unknown frame ignored rather than dropped.
                    Some(Ok(_)) => {}
                }
            }
            event = receiver.recv() => {
                match event {
                    Ok(event) => {
                        if send_event(&mut socket, &event).await.is_err() {
                            return;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(missed)) => {
                        // Honest: the page can say it lost events instead of
                        // showing a transcript that quietly skips a minute.
                        let notice = serde_json::json!({
                            "type": "lagged",
                            "missed": missed,
                        });
                        if socket
                            .send(Message::Text(notice.to_string().into()))
                            .await
                            .is_err()
                        {
                            return;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                }
            }
        }
    }
}

async fn send_event(socket: &mut WebSocket, event: &TranscriptEvent) -> Result<(), ()> {
    let json = serde_json::to_string(event).map_err(|_| ())?;
    socket
        .send(Message::Text(json.into()))
        .await
        .map_err(|_| ())
}

// ---------------------------------------------------------------------------
// /segments/{id}/audio
// ---------------------------------------------------------------------------

/// Outcome of interpreting a `Range` header against a known file length.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RangeSelection {
    /// No usable range: answer with the whole file and `200`.
    Full,
    /// Inclusive byte offsets, answered with `206`.
    Partial { start: u64, end: u64 },
    /// A well-formed range that selects nothing: `416`.
    Unsatisfiable,
}

fn parse_range_header(header_value: Option<&str>, total: u64) -> RangeSelection {
    let Some(raw) = header_value else {
        return RangeSelection::Full;
    };
    let raw = raw.trim();
    let Some((unit, spec)) = raw.split_once('=') else {
        return RangeSelection::Full;
    };
    if !unit.trim().eq_ignore_ascii_case("bytes") {
        return RangeSelection::Full;
    }
    let spec = spec.trim();
    // Multiple ranges are explicitly unsupported: answer with the full file.
    if spec.contains(',') {
        return RangeSelection::Full;
    }
    let Some((first, last)) = spec.split_once('-') else {
        return RangeSelection::Full;
    };
    let (first, last) = (first.trim(), last.trim());

    if first.is_empty() {
        // Suffix form: the last `n` bytes.
        let Ok(suffix) = last.parse::<u64>() else {
            return RangeSelection::Full;
        };
        if suffix == 0 || total == 0 {
            return RangeSelection::Unsatisfiable;
        }
        return RangeSelection::Partial {
            start: total.saturating_sub(suffix),
            end: total - 1,
        };
    }

    let Ok(start) = first.parse::<u64>() else {
        return RangeSelection::Full;
    };
    if start >= total {
        return RangeSelection::Unsatisfiable;
    }
    if last.is_empty() {
        return RangeSelection::Partial {
            start,
            end: total - 1,
        };
    }
    let Ok(end) = last.parse::<u64>() else {
        return RangeSelection::Full;
    };
    if end < start {
        // Invalid syntax, not an unsatisfiable range: ignore it (RFC 9110).
        return RangeSelection::Full;
    }
    RangeSelection::Partial {
        start,
        end: end.min(total - 1),
    }
}

fn etag_for(segment_id: &str, size_bytes: u64) -> String {
    format!("\"{segment_id}-{size_bytes}\"")
}

fn insert_header(response: &mut Response, name: header::HeaderName, value: &str) {
    match HeaderValue::from_str(value) {
        Ok(value) => {
            response.headers_mut().insert(name, value);
        }
        Err(error) => tracing::warn!(%error, "could not encode response header"),
    }
}

/// Builds an audio-endpoint error: always the standard envelope, plus the range
/// metadata a client needs to recover.
fn audio_error(
    status: StatusCode,
    code: &'static str,
    message: impl Into<String>,
    total: Option<u64>,
    etag: Option<&str>,
) -> Response {
    let mut response = error_response(status, code, message);
    response
        .headers_mut()
        .insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    if let Some(total) = total {
        insert_header(
            &mut response,
            header::CONTENT_RANGE,
            &format!("bytes */{total}"),
        );
    }
    if let Some(etag) = etag {
        insert_header(&mut response, header::ETAG, etag);
    }
    response
}

/// Streams `len` bytes of `file` starting at the current position in fixed-size
/// chunks. The file is never read into a single buffer.
fn audio_body(file: tokio::fs::File, len: u64) -> Body {
    let stream = futures_util::stream::unfold((file, len), |(mut file, remaining)| async move {
        if remaining == 0 {
            return None;
        }
        let wanted = remaining.min(AUDIO_CHUNK_BYTES as u64) as usize;
        let mut buffer = vec![0u8; wanted];
        match file.read(&mut buffer).await {
            Ok(0) => None,
            Ok(read) => {
                buffer.truncate(read);
                let read = read as u64;
                Some((
                    Ok::<Bytes, std::io::Error>(Bytes::from(buffer)),
                    (file, remaining - read),
                ))
            }
            Err(error) => Some((Err(error), (file, 0))),
        }
    });
    Body::from_stream(stream)
}

async fn get_audio(
    State(state): State<Arc<HttpState>>,
    RoutePath(id): RoutePath<String>,
    headers: HeaderMap,
) -> Response {
    let segment = match state.with_database(|db| db.segment(&id)) {
        Ok(Some(segment)) => segment,
        Ok(None) => {
            return error_response(
                StatusCode::NOT_FOUND,
                "not_found",
                format!("no segment with id {id}"),
            );
        }
        Err(error) => return internal_error(&error),
    };

    match segment.status.as_str() {
        "deleted" => {
            return audio_error(
                StatusCode::GONE,
                "audio_expired",
                "the recording for this segment was deleted by retention",
                None,
                None,
            );
        }
        "recording" => {
            return audio_error(
                StatusCode::CONFLICT,
                "audio_not_ready",
                "the segment is still being recorded",
                None,
                None,
            );
        }
        "missing" => {
            return audio_error(
                StatusCode::NOT_FOUND,
                "audio_missing",
                "the recording file for this segment is missing",
                None,
                None,
            );
        }
        _ => {}
    }

    // The only source of a playback path: the DB `relative_path` joined to the
    // configured recordings root. Nothing from the request is used here.
    let relative = Path::new(&segment.relative_path);
    if let Err(error) = validate_relative_path(relative) {
        tracing::error!(segment_id = %segment.id, %error, "stored recording path is not safe to serve");
        return internal_error(&error);
    }
    let path = state.config.storage.recording_dir.join(relative);

    let mut file = match tokio::fs::File::open(&path).await {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            tracing::warn!(
                segment_id = %segment.id,
                path = %path.display(),
                "segment row exists but its recording file is missing"
            );
            return audio_error(
                StatusCode::NOT_FOUND,
                "audio_missing",
                "the recording file for this segment is missing",
                None,
                None,
            );
        }
        Err(error) => return internal_error(&Error::io(&path, error)),
    };

    let file_len = match file.metadata().await {
        Ok(metadata) => metadata.len(),
        Err(error) => return internal_error(&Error::io(&path, error)),
    };
    if file_len != segment.size_bytes {
        tracing::warn!(
            segment_id = %segment.id,
            db_size_bytes = segment.size_bytes,
            file_size_bytes = file_len,
            "database size_bytes disagrees with the recording file; serving the real length"
        );
    }
    let etag = etag_for(&segment.id, file_len);

    let (start, length, status) = match parse_range_header(
        headers.get(header::RANGE).and_then(|value| value.to_str().ok()),
        file_len,
    ) {
        RangeSelection::Full => (0, file_len, StatusCode::OK),
        RangeSelection::Partial { start, end } => (start, end - start + 1, StatusCode::PARTIAL_CONTENT),
        RangeSelection::Unsatisfiable => {
            return audio_error(
                StatusCode::RANGE_NOT_SATISFIABLE,
                "range_not_satisfiable",
                format!("requested range is outside the {file_len}-byte recording"),
                Some(file_len),
                Some(&etag),
            );
        }
    };

    if start > 0
        && let Err(error) = file.seek(SeekFrom::Start(start)).await
    {
        return internal_error(&Error::io(&path, error));
    }

    let mut response = Response::builder()
        .status(status)
        .body(audio_body(file, length))
        .unwrap_or_else(|error| {
            tracing::error!(%error, "could not build audio response");
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                "could not build the audio response",
            )
        });
    {
        let out = response.headers_mut();
        out.insert(header::CONTENT_TYPE, HeaderValue::from_static("audio/wav"));
        out.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    }
    insert_header(&mut response, header::CONTENT_LENGTH, &length.to_string());
    insert_header(&mut response, header::ETAG, &etag);
    if status == StatusCode::PARTIAL_CONTENT {
        insert_header(
            &mut response,
            header::CONTENT_RANGE,
            &format!("bytes {start}-{}/{file_len}", start + length - 1),
        );
    }
    response
}

// ---------------------------------------------------------------------------
// Static portal
// ---------------------------------------------------------------------------

async fn portal_index(State(state): State<Arc<HttpState>>) -> Response {
    serve_portal_file(&state, "index.html").await
}

async fn portal_asset(
    State(state): State<Arc<HttpState>>,
    RoutePath(path): RoutePath<String>,
) -> Response {
    let relative = path.trim_start_matches('/');
    // The SPA catch-all also matches unknown API paths. Answer those with the
    // JSON error envelope rather than letting a caller receive index.html or a
    // bare 404 — a client parsing `{"error":...}` should never see HTML.
    if relative == "api" || relative.starts_with("api/") {
        return error_response(
            StatusCode::NOT_FOUND,
            "not_found",
            format!("no route for /{relative}"),
        );
    }
    let relative = if relative.is_empty() {
        "index.html"
    } else {
        relative
    };
    serve_portal_file(&state, relative).await
}

/// Rejects anything that is not a plain relative path below the portal root:
/// absolute paths, `..`, `.` and root components never reach the filesystem.
fn portal_path_is_safe(relative: &str) -> bool {
    let path = Path::new(relative);
    if relative.is_empty() || path.is_absolute() {
        return false;
    }
    let mut has_name = false;
    for component in path.components() {
        match component {
            Component::Normal(_) => has_name = true,
            _ => return false,
        }
    }
    has_name
}

fn content_type_for(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("html") => "text/html; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("js" | "mjs") => "text/javascript; charset=utf-8",
        Some("json") => "application/json",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("ico") => "image/x-icon",
        Some("webmanifest") => "application/manifest+json",
        Some("woff2") => "font/woff2",
        _ => "application/octet-stream",
    }
}

async fn serve_portal_file(state: &HttpState, relative: &str) -> Response {
    let root = &state.web_root;
    if !root.is_dir() {
        return error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "portal_unavailable",
            format!(
                "the portal directory {} is not installed; nothing is served at this path",
                root.display()
            ),
        );
    }
    if !portal_path_is_safe(relative) {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_path",
            "portal paths must be relative and must not contain `..`",
        );
    }

    let canonical_root = match tokio::fs::canonicalize(root).await {
        Ok(path) => path,
        Err(error) => {
            return error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "portal_unavailable",
                format!("cannot read the portal directory {}: {error}", root.display()),
            );
        }
    };
    let candidate = root.join(relative);
    let canonical = match tokio::fs::canonicalize(&candidate).await {
        Ok(path) => path,
        Err(_) => {
            // SPA fallback: the UI's client-side routes have no extension and no
            // file on disk, so serve the app shell and let the router decide.
            // A *missing asset* (one with an extension) stays a hard 404 —
            // returning index.html for a broken .js would hide deploy mistakes.
            let looks_like_route = Path::new(relative).extension().is_none();
            if looks_like_route && relative != "index.html" {
                let shell = root.join("index.html");
                if let Ok(bytes) = tokio::fs::read(&shell).await {
                    let mut response = Response::new(Body::from(bytes));
                    response.headers_mut().insert(
                        header::CONTENT_TYPE,
                        HeaderValue::from_static("text/html; charset=utf-8"),
                    );
                    response
                        .headers_mut()
                        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
                    return response;
                }
            }
            return error_response(
                StatusCode::NOT_FOUND,
                "not_found",
                format!("no portal file at {relative}"),
            );
        }
    };
    // Symlinks may not lead outside the portal root.
    if !canonical.starts_with(&canonical_root) {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_path",
            "portal path escapes the portal directory",
        );
    }

    match tokio::fs::read(&canonical).await {
        Ok(bytes) => {
            let mut response = Response::new(Body::from(bytes));
            response.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static(content_type_for(&canonical)),
            );
            response
        }
        Err(error) => internal_error(&Error::io(&canonical, error)),
    }
}

// ---------------------------------------------------------------------------
// Unit tests for the pure helpers
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn range_parsing_covers_the_three_supported_forms() {
        assert_eq!(parse_range_header(None, 10), RangeSelection::Full);
        assert_eq!(parse_range_header(Some(""), 10), RangeSelection::Full);
        assert_eq!(
            parse_range_header(Some("bytes=0-3"), 10),
            RangeSelection::Partial { start: 0, end: 3 }
        );
        assert_eq!(
            parse_range_header(Some("bytes=4-"), 10),
            RangeSelection::Partial { start: 4, end: 9 }
        );
        assert_eq!(
            parse_range_header(Some("bytes=-3"), 10),
            RangeSelection::Partial { start: 7, end: 9 }
        );
        assert_eq!(
            parse_range_header(Some("bytes=0-99"), 10),
            RangeSelection::Partial { start: 0, end: 9 }
        );
    }

    #[test]
    fn range_parsing_separates_ignored_from_unsatisfiable() {
        // Not a single well-formed byte range: the whole file is sent instead.
        for ignored in [
            "bytes=0-1,5-6",
            "items=0-1",
            "bytes=abc",
            "bytes=5-1",
            "bytes=1-2-3",
            "bytes",
        ] {
            assert_eq!(
                parse_range_header(Some(ignored), 10),
                RangeSelection::Full,
                "{ignored}"
            );
        }
        // Well formed but selecting nothing.
        for unsatisfiable in ["bytes=10-", "bytes=11-20", "bytes=-0", "bytes=0-"] {
            let total = if unsatisfiable == "bytes=0-" { 0 } else { 10 };
            assert_eq!(
                parse_range_header(Some(unsatisfiable), total),
                RangeSelection::Unsatisfiable,
                "{unsatisfiable}"
            );
        }
    }

    #[test]
    fn portal_paths_are_confined_to_the_root() {
        assert!(portal_path_is_safe("index.html"));
        assert!(portal_path_is_safe("assets/app.js"));
        assert!(!portal_path_is_safe(""));
        assert!(!portal_path_is_safe("../secret"));
        assert!(!portal_path_is_safe("assets/../../secret"));
        assert!(!portal_path_is_safe("/etc/passwd"));
        assert!(!portal_path_is_safe("./index.html"));
    }

    #[test]
    fn content_types_cover_the_portal_assets() {
        assert_eq!(content_type_for(Path::new("index.html")), "text/html; charset=utf-8");
        assert_eq!(content_type_for(Path::new("app.css")), "text/css; charset=utf-8");
        assert_eq!(content_type_for(Path::new("app.js")), "text/javascript; charset=utf-8");
        assert_eq!(content_type_for(Path::new("logo.svg")), "image/svg+xml");
        assert_eq!(content_type_for(Path::new("x.bin")), "application/octet-stream");
    }

    #[test]
    fn runtime_status_defaults_to_unknown_and_round_trips() {
        let runtime = RuntimeStatus::new();
        assert_eq!(runtime.recording(), RecordingState::Unknown);
        assert_eq!(runtime.microphone(), MicrophoneState::Unknown);
        assert!(!runtime.is_running());
        runtime.set_recording(RecordingState::Stopped);
        runtime.set_microphone(MicrophoneState::Disconnected);
        runtime.set_running(true);
        assert_eq!(runtime.recording().as_str(), "stopped");
        assert_eq!(runtime.microphone().as_str(), "disconnected");
        assert!(runtime.is_running());
    }

    #[test]
    fn duration_uses_the_stored_frame_count() {
        assert_eq!(duration_ms(16_000, 16_000), 1000);
        assert_eq!(duration_ms(0, 16_000), 0);
        assert_eq!(duration_ms(16_000, 0), 0);
    }

    #[test]
    fn etag_matches_the_documented_shape() {
        assert_eq!(etag_for("seg-1", 44), "\"seg-1-44\"");
    }
}
