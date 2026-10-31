//! Capture-only node for the split deployment.
//!
//! This binary owns the microphone and nothing else. It reads the ALSA device,
//! chops the stream into datagrams and sends them to the server; it never opens
//! a file, never runs detection and never listens on a socket. Everything that
//! needs a filesystem, a database or a UI lives on the server.
//!
//! ```text
//! ALSA callback -> CaptureRing -> [this binary] -> UDP -> server
//! ```
//!
//! It reuses [`steno_lib::audio_capture`] unchanged, including device
//! discovery and format negotiation, so the node behaves exactly like the
//! single-box build did while the microphone was local.
//!
//! # What this deliberately does not do
//!
//! * **No retransmission buffer.** If the server or the WiFi link is down, audio
//!   is lost and the server records an honest gap; nothing is queued for replay.
//!   The optional `[archive]` writer is not a queue — it is the copy of record
//!   for a night the uplink may deliberately gate, and it patches its header on
//!   a timer so a power cut cannot cost more than that window.
//! * **No retransmission.** UDP never blocks, so the capture callback can never
//!   be stalled by the network. A late or missing datagram becomes a gap, which
//!   the server already knows how to represent (TECH_SPEC §4.3).
//! * **No wall-clock.** The node has no RTC. It sends its own monotonic counter;
//!   the server anchors it to a trustworthy clock.

mod archive;

use archive::WavArchive;
use serde::Deserialize;
use steno_lib::audio_capture::{self, CaptureRing};
use steno_lib::config::AudioConfig;
use steno_lib::dsp::HighPass;
use steno_lib::metrics::Metrics;
use steno_lib::vad::{GateDecision, GateTuning, VadGate};
use steno_lib::protocol::{self, Header, Hello, Kind};
use std::net::UdpSocket;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Where to look for the node config when nothing is passed on the command line.
const DEFAULT_CONFIG_PATH: &str = "/etc/steno-monitor/node.toml";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct NodeFile {
    node: NodeSettings,
    #[serde(default)]
    audio: AudioConfig,
    /// The whole night, written locally. Required before the uplink may gate.
    #[serde(default)]
    archive: ArchiveSettings,
    #[serde(default)]
    gate: GateSettings,
}

/// Local continuous archive. `dir` empty disables it.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct ArchiveSettings {
    #[serde(default)]
    dir: String,
    /// Duration of one file. Ten minutes keeps a crash's cost small and the
    /// directory readable.
    #[serde(default = "default_archive_segment_seconds")]
    segment_seconds: u64,
    /// Ceiling for everything this writer owns; oldest files go first.
    #[serde(default = "default_archive_max_bytes")]
    max_bytes: u64,
}

impl Default for ArchiveSettings {
    fn default() -> Self {
        ArchiveSettings {
            dir: String::new(),
            segment_seconds: default_archive_segment_seconds(),
            max_bytes: default_archive_max_bytes(),
        }
    }
}

/// Uplink gating. Off by default: it changes what the server receives.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct GateSettings {
    #[serde(default)]
    enabled: bool,
    #[serde(default = "default_gate_margin_db")]
    margin_db: f32,
    /// How long the room must stay quiet before the uplink closes. This is the
    /// unit-of-meaning knob, not a tail: 2 s treated every burst as its own
    /// thing and produced a 2.1 s median fragment on a real night.
    #[serde(default = "default_gate_close_quiet_ms")]
    close_quiet_ms: u32,
    #[serde(default = "default_gate_warmup_ms")]
    warmup_ms: u32,
    #[serde(default = "default_gate_floor_window_ms")]
    floor_window_ms: u32,
}

impl Default for GateSettings {
    fn default() -> Self {
        let tuning = GateTuning::default();
        GateSettings {
            enabled: false,
            margin_db: tuning.margin_db,
            close_quiet_ms: tuning.close_quiet_ms,
            warmup_ms: tuning.warmup_ms,
            floor_window_ms: tuning.floor_window_ms,
        }
    }
}

impl GateSettings {
    fn tuning(&self) -> GateTuning {
        GateTuning {
            margin_db: self.margin_db,
            close_quiet_ms: self.close_quiet_ms,
            warmup_ms: self.warmup_ms,
            floor_window_ms: self.floor_window_ms,
        }
    }
}

fn default_archive_segment_seconds() -> u64 {
    600
}
fn default_archive_max_bytes() -> u64 {
    20 * 1024 * 1024 * 1024
}
fn default_gate_margin_db() -> f32 {
    // Below the detector's `noise_margin_db` (8 dB) so the gate can never cut a
    // block the detector would have called active.
    6.0
}
fn default_gate_close_quiet_ms() -> u32 {
    20_000
}
fn default_gate_warmup_ms() -> u32 {
    5_000
}
fn default_gate_floor_window_ms() -> u32 {
    30_000
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct NodeSettings {
    /// Shown on the server's status page. Not used for routing.
    #[serde(default = "default_device_name")]
    device_name: String,
    server_host: String,
    server_port: u16,
    /// Identifies this node to the server. `0` makes the server accept any.
    #[serde(default)]
    device_id: u32,
    /// How often HELLO is repeated. A restarted server forgets the announced
    /// format, and PCM is refused until a HELLO arrives, so this must be short.
    #[serde(default = "default_hello_interval_ms")]
    hello_interval_ms: u64,
}

fn default_device_name() -> String {
    "steno-node".to_string()
}
fn default_hello_interval_ms() -> u64 {
    1_000
}

#[derive(Debug)]
enum NodeError {
    Io(std::io::Error),
    Config(String),
    Capture(String),
    Thread(String),
}

impl std::fmt::Display for NodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NodeError::Io(error) => write!(f, "{error}"),
            NodeError::Config(message) => write!(f, "config: {message}"),
            NodeError::Capture(message) => write!(f, "capture: {message}"),
            NodeError::Thread(message) => write!(f, "thread: {message}"),
        }
    }
}

impl From<std::io::Error> for NodeError {
    fn from(error: std::io::Error) -> Self {
        NodeError::Io(error)
    }
}

static SHUTDOWN: AtomicBool = AtomicBool::new(false);

extern "C" fn handle_signal(_signal: libc::c_int) {
    SHUTDOWN.store(true, Ordering::Relaxed);
}

fn install_signal_handlers() {
    // SAFETY: the handler only stores into an atomic, which is async-signal-safe.
    unsafe {
        libc::signal(
            libc::SIGINT,
            handle_signal as *const () as libc::sighandler_t,
        );
        libc::signal(
            libc::SIGTERM,
            handle_signal as *const () as libc::sighandler_t,
        );
    }
}

fn config_path() -> PathBuf {
    if let Some(argument) = std::env::args().nth(1) {
        return PathBuf::from(argument);
    }
    if let Ok(value) = std::env::var("STENO_NODE_CONFIG") {
        if !value.trim().is_empty() {
            return PathBuf::from(value);
        }
    }
    PathBuf::from(DEFAULT_CONFIG_PATH)
}

/// Everything one config file supplies.
struct Loaded {
    node: NodeSettings,
    audio: AudioConfig,
    archive: ArchiveSettings,
    gate: GateSettings,
}

fn load(path: &PathBuf) -> Result<Loaded, NodeError> {
    let text = std::fs::read_to_string(path)?;
    let file: NodeFile = toml::from_str(&text)
        .map_err(|error| NodeError::Config(format!("{}: {error}", path.display())))?;

    file.gate
        .tuning()
        .validate()
        .map_err(|error| NodeError::Config(error.replace("gate.", "[gate] ")))?;
    // Gating without a local copy would not be "saving bandwidth", it would be
    // discarding audio and calling it policy. Refuse it by name.
    if file.gate.enabled && file.archive.dir.trim().is_empty() {
        return Err(NodeError::Config(
            "[gate] enabled = true requires [archive] dir: the uplink may only stop sending \
             audio that is written somewhere else first"
                .into(),
        ));
    }

    let settings = file.node;
    if settings.server_host.trim().is_empty() {
        return Err(NodeError::Config("node.server_host must not be empty".into()));
    }
    if settings.server_port == 0 {
        return Err(NodeError::Config("node.server_port must not be 0".into()));
    }
    if !(100..=10_000).contains(&settings.hello_interval_ms) {
        return Err(NodeError::Config(
            "node.hello_interval_ms must be within 100..=10000".into(),
        ));
    }
    if !steno_lib::config::SUPPORTED_CAPTURE_RATES.contains(&file.audio.device_rate_hz) {
        return Err(NodeError::Config(format!(
            "audio.device_rate_hz: must be one of {:?}, got {}; this is a hint for the device, the wire is fixed at {} Hz",
            steno_lib::config::SUPPORTED_CAPTURE_RATES,
            file.audio.device_rate_hz,
            steno_lib::config::WIRE_RATE_HZ
        )));
    }
    if !(1..=2).contains(&file.audio.device_channels) {
        return Err(NodeError::Config(format!(
            "audio.device_channels: must be 1 or 2, got {}",
            file.audio.device_channels
        )));
    }
    // Checked against the *configured* channel count here; the negotiated one is
    // checked again before the first block is converted, because the device may
    // open with fewer channels than asked for.
    if file.audio.mono_channel_index >= file.audio.device_channels {
        return Err(NodeError::Config(format!(
            "audio.mono_channel_index: must be < audio.device_channels ({}), got {}",
            file.audio.device_channels, file.audio.mono_channel_index
        )));
    }
    Ok(Loaded {
        node: settings,
        audio: file.audio,
        archive: file.archive,
        gate: file.gate,
    })
}

/// A session id that changes on every restart, so the server can tell a fresh
/// stream from a continuation. `SystemTime` is only a fallback: on a machine
/// without an RTC it can repeat, which is why the server also detects a
/// restarted stream by its frame counter.
fn new_session_id() -> u32 {
    use std::io::Read;
    // Never `fs::read("/dev/urandom")`: it is a character device with no end, so
    // that call would read forever.
    if let Ok(mut file) = std::fs::File::open("/dev/urandom") {
        let mut buf = [0u8; 4];
        if file.read_exact(&mut buf).is_ok() {
            return u32::from_le_bytes(buf);
        }
    }
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.subsec_nanos() ^ elapsed.as_secs() as u32)
        .unwrap_or(0x5EED)
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    if let Err(error) = run() {
        tracing::error!(%error, "steno-node stopped");
        std::process::exit(1);
    }
}

fn run() -> Result<(), NodeError> {
    let path = config_path();
    let Loaded {
        node: settings,
        audio,
        archive: archive_settings,
        gate: gate_settings,
    } = load(&path)?;
    tracing::info!(config = %path.display(), "loaded node config");

    install_signal_handlers();

    // --- device negotiation (identical to the single-box build) ------------
    let host = cpal::default_host();
    let selection = audio_capture::select_device(&host, &audio)
        .map_err(|error| NodeError::Capture(error.to_string()))?;
    let negotiated = selection.info.clone();
    // The device may only clock at 48 kHz stereo. Everything past this point —
    // the archive, the gate, the packets and therefore the server's recording,
    // detector and recogniser — works in the wire format, so the conversion
    // happens once, here, instead of being assumed away. The wire format is a
    // constant shared with the server: it is not negotiated, not configured, and
    // not allowed to differ, so there is nothing for the two machines to
    // disagree about.
    let converter = audio_capture::WireConverter::new(
        negotiated.rate_hz,
        negotiated.channels,
        audio.mono_channel_index,
    )
    .map_err(|error| NodeError::Config(format!("capture format: {error}")))?;
    tracing::info!(
        device = %negotiated.device_name,
        device_rate_hz = negotiated.rate_hz,
        device_channels = negotiated.channels,
        mono_channel_index = audio.mono_channel_index,
        wire_rate_hz = steno_lib::config::WIRE_RATE_HZ,
        wire_channels = steno_lib::config::WIRE_CHANNELS,
        "capture device negotiated; the wire format is fixed"
    );

    // A condenser capsule at high gain is mostly rumble. Filtering it out here —
    // once, before the archive and the wire — means the local copy and the
    // recogniser hear the same thing, so replaying a file reproduces its text.
    let rumble = if audio.rumble_highpass_hz == 0 {
        tracing::info!("rumble filter disabled (audio.rumble_highpass_hz = 0)");
        None
    } else {
        let filter = HighPass::new(
            audio_capture::WireConverter::RATE_HZ as f32,
            audio.rumble_highpass_hz as f32,
        )
        .map_err(|error| NodeError::Config(format!("audio.rumble_highpass_hz: {error}")))?;
        tracing::info!(
            cutoff_hz = filter.cutoff_hz(),
            "rumble high-pass enabled on the capture path"
        );
        Some(filter)
    };

    // The wire is mono, so packet capacity is in mono frames regardless of what
    // the device delivers.
    let max_frames_per_packet = (protocol::MAX_PCM_PAYLOAD_BYTES / protocol::block_align(1)).max(1);

    let metrics = Arc::new(Metrics::new());
    let ring = Arc::new(CaptureRing::new(
        negotiated.rate_hz,
        negotiated.channels,
        audio.capture_ring_ms,
        audio.period_ms,
        audio.buffer_ms,
    ));

    let server_addr = format!("{}:{}", settings.server_host.trim(), settings.server_port);
    let socket = UdpSocket::bind("0.0.0.0:0")?;
    // UDP sends do not block, but a full socket buffer would surface as ENOBUFS
    // and drop the datagram; a large send buffer smooths occasional stalls.
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        let value: libc::c_int = 1 << 20;
        // SAFETY: the fd is owned by `socket` for the duration of the call.
        unsafe {
            libc::setsockopt(
                socket.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_SNDBUF,
                &value as *const libc::c_int as *const libc::c_void,
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            );
        }
    }
    tracing::info!(server = %server_addr, "sending capture to server");

    // UDP is lossy by design and the local archive is the copy that must survive.
    // A *blocking* send breaks that trade: when the WiFi link stalls, the send
    // blocks long enough for the 500 ms capture ring to overflow, and the audio
    // that gets dropped is not a packet here and there but a continuous 14% of
    // every sentence — which is how a working recogniser produces fragments.
    socket.set_nonblocking(true)?;

    let stop = Arc::new(AtomicBool::new(false));

    // --- forwarder thread --------------------------------------------------
    let forward_stop = Arc::clone(&stop);
    let forward_ring = Arc::clone(&ring);
    let device_name = settings.device_name.clone();
    let device_id = settings.device_id;
    let hello_interval = Duration::from_millis(settings.hello_interval_ms);
    // What travels on the wire, not what the device clocks at. HELLO still
    // announces it — the server uses the announcement to refuse a node built
    // against a different wire format — but the value is a constant on both
    // sides rather than something either machine chooses.
    let channels: u16 = steno_lib::config::WIRE_CHANNELS;
    let rate_hz = steno_lib::config::WIRE_RATE_HZ;
    let device_channels = negotiated.channels;
    let device_rate_hz = negotiated.rate_hz;
    let session_id = new_session_id();

    // The archive is opened before the gate: if it cannot be written, gating
    // must not start, because there would be no copy of what was skipped.
    let archive = if archive_settings.dir.trim().is_empty() {
        tracing::info!("no [archive] dir; the node keeps no local copy of the night");
        None
    } else {
        let archive = WavArchive::open(
            archive_settings.dir.trim(),
            archive_settings.segment_seconds,
            archive_settings.max_bytes,
            rate_hz,
        )?;
        tracing::info!(
            dir = %archive.dir().display(),
            segment_seconds = archive_settings.segment_seconds,
            max_bytes = archive_settings.max_bytes,
            "writing a local archive of the whole night"
        );
        Some(archive)
    };
    let gate = if gate_settings.enabled {
        if archive.is_none() {
            // Unreachable through `load`, but the invariant is worth enforcing
            // where it is used too.
            return Err(NodeError::Config(
                "[gate] enabled = true requires a writable [archive] dir".into(),
            ));
        }
        let gate = VadGate::new(gate_settings.tuning());
        tracing::info!(
            margin_db = gate_settings.margin_db,
            close_quiet_ms = gate_settings.close_quiet_ms,
            warmup_ms = gate_settings.warmup_ms,
            "uplink gating enabled: the gate opens on sound and closes after a long quiet"
        );
        Some(gate)
    } else {
        None
    };
    // Built before the forwarder takes ownership of the socket address.
    let report = node_report_json(
        &path,
        settings.device_id,
        &server_addr,
        rate_hz,
        channels,
        device_rate_hz,
        device_channels,
    );

    let forwarder = std::thread::Builder::new()
        .name("steno-node-send".into())
        .spawn(move || {
            forward_capture(
                socket,
                server_addr,
                forward_ring,
                forward_stop,
                ForwardConfig {
                    device_name,
                    report,
                    archive,
                    gate,
                    converter,
                    rumble,
                    device_id,
                    session_id,
                    channels,
                    rate_hz,
                    hello_interval,
                    max_frames_per_packet,
                },
            );
        })
        .map_err(|error| NodeError::Thread(error.to_string()))?;

    // --- capture -----------------------------------------------------------
    let shutting_down = Arc::new(AtomicBool::new(false));
    let stream = audio_capture::start_stream(
        selection,
        Arc::clone(&ring),
        Arc::clone(&metrics),
        Arc::clone(&shutting_down),
        1,
    )
    .map_err(|error| NodeError::Capture(error.to_string()))?;

    tracing::info!("capture running");

    while !SHUTDOWN.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(100));
    }

    tracing::info!("shutdown requested");
    // Stop capture first, then let the forwarder flush what is already queued so
    // the server does not lose the last fraction of a second.
    shutting_down.store(true, Ordering::Relaxed);
    drop(stream);
    let drain = Duration::from_millis(u64::from(audio.capture_ring_ms.min(1_000)) + 100);
    std::thread::sleep(drain);
    stop.store(true, Ordering::Relaxed);
    let _ = forwarder.join();
    tracing::info!("stopped");
    Ok(())
}

struct ForwardConfig {
    device_name: String,
    /// JSON the node sends about itself (argv, config path, version). Sent on
    /// the HELLO timer so a restarted server learns it again without the node
    /// having to be restarted.
    report: String,
    /// Continuous local copy of the night; `None` when `[archive] dir` is empty.
    archive: Option<WavArchive>,
    /// Uplink gate; `None` sends everything.
    gate: Option<VadGate>,
    /// Device format to wire format. Everything below the forwarder sees
    /// 16 kHz mono because of this.
    converter: audio_capture::WireConverter,
    /// Rumble filter, applied to the wire-format block before anything sees it.
    rumble: Option<HighPass>,
    device_id: u32,
    session_id: u32,
    channels: u16,
    rate_hz: u32,
    hello_interval: Duration,
    max_frames_per_packet: usize,
}

fn forward_capture(
    socket: UdpSocket,
    server_addr: String,
    ring: Arc<CaptureRing>,
    stop: Arc<AtomicBool>,
    mut config: ForwardConfig,
) {
    let mut archive = config.archive.take();
    let mut gate = config.gate.take();
    let mut converter = config.converter;
    let mut rumble = config.rumble.take();
    let mut archive_errors: u64 = 0;
    let channels_in_block = usize::from(config.channels.max(1));
    let hello = Hello {
        rate_hz: config.rate_hz,
        channels: config.channels,
        device_name: config.device_name.clone(),
    };
    let mut header = Header {
        kind: Kind::Hello,
        device_id: config.device_id,
        session_id: config.session_id,
        packet_seq: 0,
        first_frame: 0,
        t_mono_us: 0,
    };

    let mut packet = Vec::with_capacity(
        protocol::HEADER_LEN + protocol::MAX_PCM_PAYLOAD_BYTES,
    );
    let mut block: Vec<i16> = Vec::new();
    // The converted block lives here so the filter can work in place without
    // reallocating once per 20 ms for a whole night.
    let mut wire_scratch: Vec<i16> = Vec::new();
    let mut seq: u32 = 0;
    // Frames put on the wire since start. Not the capture ring's counter: the
    // ring counts device frames, and one device frame is not one wire frame.
    let mut wire_frame: u64 = 0;
    // The self-report can gain a `rate_mismatch` field mid-run, so it is kept as
    // a string that the hello path re-reads rather than a frozen constant.
    let mut report_json = config.report.clone();
    let mut guard = ArrivalGuard::default();
    let device_rate_hz = converter.device_rate_hz();
    let mut last_hello = SystemTime::now()
        .checked_sub(config.hello_interval)
        .unwrap_or(UNIX_EPOCH);
    let mut send_errors: u64 = 0;

    while !stop.load(Ordering::Relaxed) {
        // Repeat HELLO on a timer: a restarted server has forgotten the format
        // and will refuse PCM until it hears one again.
        if SystemTime::now()
            .duration_since(last_hello)
            .map(|elapsed| elapsed >= config.hello_interval)
            .unwrap_or(true)
        {
            header.kind = Kind::Hello;
            protocol::encode_hello_packet(&header, &hello, &mut packet);
            if let Err(error) = socket.send_to(&packet, &server_addr) {
                send_errors = report_send_error(send_errors, &error);
            }
            // What this node is running. The server cannot see another host's
            // process table, so the node has to say it itself; a failure here is
            // not worth failing the stream over.
            // Counters only this side can see, and the difference between "the
            // audio was never captured" and "the recogniser misheard it".
            refresh_report(&mut report_json, ring.overflow_frames(), send_errors, wire_frame);
            header.kind = Kind::Info;
            protocol::encode_info_packet(&header, report_json.as_bytes(), &mut packet);
            if let Err(error) = socket.send_to(&packet, &server_addr) {
                send_errors = report_send_error(send_errors, &error);
            }
            last_hello = SystemTime::now();
        }

        let Some(meta) = ring.pop_into(&mut block) else {
            std::thread::sleep(Duration::from_millis(2));
            continue;
        };
        if block.is_empty() {
            continue;
        }

        // Device format in, wire format out. A block of 48 kHz stereo is
        // reduced to one channel and resampled here; everything below this line
        // — the archive, the gate, the packet headers — is the wire format, which
        // is what HELLO announces and what every consumer on the other side
        // assumes without being told again.
        let converted = converter.convert(&block);
        wire_scratch.clear();
        wire_scratch.extend_from_slice(converted);
        if let Some(filter) = rumble.as_mut() {
            filter.process_block(&mut wire_scratch);
        }
        let pcm: &[i16] = &wire_scratch;
        let frames = (pcm.len() / channels_in_block) as u64;
        let block_start = wire_frame;
        wire_frame += frames;
        check_arrival_rate(&mut guard, &mut report_json, meta, device_rate_hz);

        // Archive first, always: the gate only decides what leaves the device,
        // never what is kept.
        if let Some(archive) = archive.as_mut() {
            if let Err(error) = archive.push(pcm) {
                archive_errors += 1;
                if archive_errors <= 5 || archive_errors % 500 == 0 {
                    tracing::warn!(%error, count = archive_errors, "cannot write the local archive");
                }
            }
        }

        // Gate: a block that is deliberately not sent still occupies its frames
        // in the timeline, so the range is announced before transmission resumes.
        if let Some(gate) = gate.as_mut() {
            // The frame index is counted in frames actually put on the wire, the
            // same numbering that goes into the packet header — the gate never
            // keeps its own counter, so the ranges it reports cannot drift from
            // the audio it sent.
            let verdict = gate.observe(level_dbfs(pcm), block_start, frames, config.rate_hz);
            if let Some((from, to)) = verdict.skipped {
                header.kind = Kind::Gate;
                header.first_frame = from;
                header.t_mono_us = meta.mono_start_ns / 1_000;
                protocol::encode_gate_packet(&header, from, to, &mut packet);
                if let Err(error) = socket.send_to(&packet, &server_addr) {
                    send_errors = report_send_error(send_errors, &error);
                }
            }
            if verdict.decision == GateDecision::Skip {
                continue;
            }
        }

        let channels = usize::from(config.channels.max(1));
        let chunk_samples = config.max_frames_per_packet * channels;
        let mut frame_offset: u64 = 0;
        // Normally one block is one datagram. Splitting only happens if the
        // device delivered an unusually large period.
        for chunk in pcm.chunks(chunk_samples) {
            let chunk_frames = (chunk.len() / channels) as u64;
            header.kind = Kind::Pcm;
            header.packet_seq = seq;
            header.first_frame = block_start + frame_offset;
            header.t_mono_us = meta.mono_start_ns / 1_000
                + frame_offset.saturating_mul(1_000_000) / u64::from(config.rate_hz.max(1));
            protocol::encode_pcm_packet(&header, chunk, &mut packet);
            if let Err(error) = socket.send_to(&packet, &server_addr) {
                send_errors = report_send_error(send_errors, &error);
            }
            seq = seq.wrapping_add(1);
            frame_offset += chunk_frames;
        }
    }
    // Close the archive cleanly so the last file carries a real header.
    if let Some(archive) = archive.as_mut() {
        if let Err(error) = archive.finish() {
            tracing::warn!(%error, "cannot finalise the local archive");
        }
    }
}

/// Watches that the rate a device *reports* is the rate it really delivers.
///
/// This is the one invariant that cannot be checked at negotiation time: ALSA
/// accepts a sample rate the endpoint cannot clock and rounds it to one it can,
/// without reporting an error, so a 48 kHz stereo microphone can describe itself
/// as 16 kHz mono and nothing downstream is able to tell. Frames per second of
/// wall clock is the only honest measurement available, and a deviation is worth
/// an error: 5% already shifts every formant, and the 3x case turns speech into
/// rumble that no recogniser can read.
///
/// It reports once, names both rates, and adds the finding to the self-report so
/// the control centre shows it next to the node's own version and config path.
#[derive(Default)]
struct ArrivalGuard {
    /// (monotonic ns, device frames already seen) at the last checkpoint.
    checkpoint: Option<(u64, u64)>,
    device_frames: u64,
    reported: bool,
}

fn check_arrival_rate(
    guard: &mut ArrivalGuard,
    report_json: &mut String,
    meta: audio_capture::CaptureMeta,
    expected_hz: u32,
) {
    // Five seconds averages out the jitter of a 20 ms callback; anything shorter
    // reports a sample rate that is really scheduler noise.
    const WINDOW_NS: u64 = 5_000_000_000;
    // The capture ring starts empty and fills to its target depth over the first
    // fraction of a second, so the first seconds of a stream deliver *fewer*
    // frames than were captured — measuring from zero would report 13500 Hz for a
    // perfectly honest 16 kHz device. The window opens once the ring is at its
    // steady state.
    const WARMUP_NS: u64 = 3_000_000_000;
    if guard.checkpoint.is_none() && meta.mono_start_ns < WARMUP_NS {
        guard.device_frames += u64::from(meta.frames);
        return;
    }
    let (start_ns, start_frames) = *guard
        .checkpoint
        .get_or_insert((meta.mono_start_ns, guard.device_frames));
    let elapsed_ns = meta.mono_start_ns.saturating_sub(start_ns);
    if !guard.reported && elapsed_ns >= WINDOW_NS {
        let frames = guard.device_frames - start_frames;
        let observed_hz = frames as f64 * 1e9 / elapsed_ns as f64;
        let ratio = observed_hz / f64::from(expected_hz.max(1));
        guard.checkpoint = Some((meta.mono_start_ns, guard.device_frames));
        if (ratio - 1.0).abs() > 0.05 {
            guard.reported = true;
            tracing::error!(
                expected_hz,
                observed_hz = observed_hz.round() as u64,
                ratio,
                "the device is not delivering the rate it reported; every sample on the wire is time-stretched"
            );
            if let Ok(mut value) = serde_json::from_str::<serde_json::Value>(report_json) {
                value["rate_mismatch"] = serde_json::json!({
                    "expected_hz": expected_hz,
                    "observed_hz": observed_hz.round() as u64,
                });
                *report_json = value.to_string();
            }
        }
    }
    guard.device_frames += u64::from(meta.frames);
}

/// RMS of one block in dBFS. Digital silence maps to -120, not to -infinity, so
/// the gate's floor tracker never has to handle NaN.
fn level_dbfs(samples: &[i16]) -> f32 {
    if samples.is_empty() {
        return -120.0;
    }
    let mut sum = 0.0f64;
    for sample in samples {
        let value = f64::from(*sample);
        sum += value * value;
    }
    let rms = (sum / samples.len() as f64).sqrt();
    if rms <= 0.0 {
        -120.0
    } else {
        (20.0 * (rms / 32_768.0).log10()) as f32
    }
}

/// Folds the node's own counters into the self-report.
///
/// These are the numbers that answer "why is the transcript bad" without
/// listening to anything: frames the capture ring had to throw away, datagrams
/// the socket refused, and how many frames were actually put on the wire.
fn refresh_report(
    report_json: &mut String,
    ring_overflow_frames: u64,
    send_errors: u64,
    wire_frames: u64,
) {
    let Ok(mut value) = serde_json::from_str::<serde_json::Value>(report_json) else {
        return;
    };
    value["ring_overflow_frames"] = serde_json::json!(ring_overflow_frames);
    value["send_errors"] = serde_json::json!(send_errors);
    value["wire_frames"] = serde_json::json!(wire_frames);
    *report_json = value.to_string();
}

/// The node's description of itself, as JSON.
///
/// The server cannot read another host's process table, so "what is the node
/// running" can only be answered by the node. `argv` is the real command line,
/// including the config path it was started with — that is what an operator
/// needs to answer "which unit is this and where does its config live".
fn node_report_json(
    config_path: &std::path::Path,
    device_id: u32,
    server_addr: &str,
    rate_hz: u32,
    channels: u16,
    device_rate_hz: u32,
    device_channels: u16,
) -> String {
    let argv: Vec<String> = std::env::args().collect();
    let reported_at = std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0);
    serde_json::json!({
        "argv": argv,
        "config": config_path.display().to_string(),
        "version": env!("CARGO_PKG_VERSION"),
        "device_id": device_id,
        "server": server_addr,
        // What travels on the wire.
        "rate_hz": rate_hz,
        "channels": channels,
        // What the microphone clocks at. Reported separately because the two are
        // allowed to differ now — the node converts between them, and an operator
        // looking at a stretched or unintelligible transcript needs to see which
        // rate the device actually opened at.
        "device_rate_hz": device_rate_hz,
        "device_channels": device_channels,
        "reported_at_unix_s": reported_at,
    })
    .to_string()
}

fn report_send_error(previous: u64, error: &std::io::Error) -> u64 {
    let count = previous + 1;
    if count <= 5 || count % 500 == 0 {
        tracing::warn!(%error, count, "send failed");
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `deployment/node.example.toml` is what gets copied onto a Pi. Parsing it here means
    /// an example that no longer validates fails `cargo test` instead of failing
    /// at 23:00 on a bedside device.
    #[test]
    fn the_shipped_example_parses_and_validates() {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../deployment/node.example.toml");
        load(&path).unwrap_or_else(|error| {
            panic!("{} no longer validates: {error}", path.display())
        });
    }

    /// The node's report is the only source of truth about another host's
    /// process, so it must be valid JSON that names the binary and the config it
    /// was started with — the two things an operator needs to reproduce it.
    #[test]
    fn the_self_report_names_the_binary_and_its_config() {
        let report = node_report_json(
            std::path::Path::new("/etc/steno-node/node.toml"),
            1,
            "192.168.1.10:8091",
            16_000,
            1,
            48_000,
            2,
        );
        let parsed: serde_json::Value = serde_json::from_str(&report).expect("valid JSON");
        assert!(
            parsed["argv"]
                .as_array()
                .is_some_and(|argv| !argv.is_empty()),
            "argv must carry the real command line: {report}"
        );
        assert_eq!(parsed["config"], "/etc/steno-node/node.toml");
        assert_eq!(parsed["server"], "192.168.1.10:8091");
        // The wire format and the device format are separate facts: a node that
        // opened a 48 kHz stereo microphone still says "16 kHz mono" for what it
        // sends, and names 48 kHz / 2ch as what it captured from.
        assert_eq!(parsed["rate_hz"], 16_000);
        assert_eq!(parsed["channels"], 1);
        assert_eq!(parsed["device_rate_hz"], 48_000);
        assert_eq!(parsed["device_channels"], 2);
        assert!(parsed["version"].is_string());
    }
}
