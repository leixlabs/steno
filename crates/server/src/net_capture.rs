//! Server-side capture from a remote node over UDP (`source.kind = "net"`).
//!
//! This is the receiving half of the split deployment. It is a drop-in
//! replacement for [`steno_lib::audio_capture`]: both feed the same
//! [`CaptureRing`] through [`steno_lib::audio_capture::CaptureRing::callback_push`],
//! so the dispatcher, recorder, detector, storage layer and Portal are unaware
//! of where the audio came from.
//!
//! # Timeline: why arrival time is not used
//!
//! `CaptureMeta.mono_start_ns` is derived from the node's own monotonic
//! timestamp, not from when the datagram arrived:
//!
//! ```text
//! mono_start_ns = anchor_ns + (node_t_mono_us - node_t0_us) * 1000
//! ```
//!
//! `anchor_ns` is the server's [`monotonic_now_ns`] at the first packet of the
//! session, so every value stays inside the same clock domain the dispatcher
//! uses for its wall-clock anchor, `clock_drift_ppm` and gap accounting.
//!
//! Two properties follow, and both matter:
//!
//! * **WiFi jitter cannot fake a gap.** The node's timer is smooth, so a packet
//!   that arrives 40 ms late does not look like 40 ms of missing audio.
//! * **Drift is preserved, not hidden.** Deriving the timeline from frame
//!   counts instead would make `clock_drift_ppm` read ~0 by construction,
//!   because the recording's own errors would define the yardstick. Anchoring
//!   on the node's timer keeps "how many frames did it claim" and "how much time
//!   actually passed" as two independent measurements, which is the whole point
//!   of the metric.
//!
//! Real loss shows up as a jump in `first_frame` *and* in the node timer, so the
//! existing gap logic in [`crate::dispatcher`] registers it with the correct
//! duration — this module does not manufacture gap records itself.
//!
//! # Ordering
//!
//! Datagrams can arrive out of order. A small reorder window (config
//! `source.jitter_buffer_ms`, default 150 ms) holds blocks by sequence number
//! and releases them contiguously. When a sequence number is still missing after
//! the window expires, it is declared lost and the stream skips ahead; the
//! downstream gap machinery records the hole.

use chrono::Utc;
use steno_lib::audio_capture::{CaptureMeta, CaptureRing};
use steno_lib::metrics::Metrics;
use steno_lib::protocol::{self, Header, Hello, Kind, ProtocolError};
use steno_lib::timeline::monotonic_now_ns;
use std::io;
use std::net::UdpSocket;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

/// Largest receive buffer we ask the kernel for. At 261 kbps this is roughly a
/// minute of queued audio, which absorbs scheduling stalls and GC-like pauses
/// without dropping datagrams.
const SOCKET_RCVBUF_BYTES: i32 = 2 * 1024 * 1024;
/// How long a single `recv_from` blocks before the loop re-checks shutdown.
const READ_TIMEOUT_MS: u64 = 50;
/// A sequence gap larger than this is treated as a new session rather than a
/// loss streak: it can only come from a node with a reused session id.
const MAX_SKIP_PACKETS: i32 = 10_000;

#[derive(Debug, Clone)]
pub struct NetCaptureConfig {
    pub bind_address: String,
    pub port: u16,
    /// Expected node id. `0` accepts the first node that says HELLO.
    pub device_id: u32,
    /// Announced format the server will accept. Anything else is refused rather
    /// than silently reinterpreted, matching the project's "no silent fallback"
    /// rule (TECH_SPEC §11).
    pub rate_hz: u32,
    pub channels: u16,
    pub jitter_buffer_ms: u32,
    /// No packet for this long marks the source connected = false.
    pub idle_timeout_ms: u32,
}

impl Default for NetCaptureConfig {
    fn default() -> Self {
        Self {
            bind_address: "0.0.0.0".to_string(),
            port: 8091,
            device_id: 0,
            rate_hz: 16_000,
            channels: 1,
            jitter_buffer_ms: 150,
            idle_timeout_ms: 5_000,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum NetCaptureError {
    #[error("cannot bind UDP capture socket on {addr}: {source}")]
    Bind {
        addr: String,
        #[source]
        source: io::Error,
    },
    #[error("capture socket error: {0}")]
    Socket(#[from] io::Error),
    #[error("cannot start the net capture thread: {0}")]
    Thread(io::Error),
    #[error("invalid source config: {0}")]
    Config(String),
}

/// Handle owning the receive thread. Dropping it stops capture and waits for the
/// thread, mirroring what dropping a [`cpal::Stream`] does on the local path.
pub struct NetCaptureHandle {
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl NetCaptureHandle {
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl Drop for NetCaptureHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(join) = self.join.take() {
            // The receive loop wakes at least every READ_TIMEOUT_MS, so this
            // cannot hang indefinitely on a quiet link.
            let _ = join.join();
        }
    }
}

struct Pending {
    seq: u32,
    first_frame: u64,
    t_mono_us: u64,
    frames: u32,
    samples: Vec<i16>,
}

struct Session {
    device_id: u32,
    session_id: u32,
    next_seq: u32,
    /// Node timer at the first accepted packet; the timeline's zero.
    t0_us: u64,
    /// Server monotonic ns that `t0_us` maps to.
    anchor_ns: u64,
    /// Guards against a node timer that steps backwards.
    last_delta_us: u64,
    generation: u32,
    pushed_frames: u64,
    hello: Option<Hello>,
    /// Highest `first_frame` seen, used to notice a sender restart that reused
    /// its session id.
    max_first_frame: u64,
}

struct Receiver {
    cfg: NetCaptureConfig,
    ring: Arc<CaptureRing>,
    metrics: Arc<Metrics>,
    socket: UdpSocket,
    session: Option<Session>,
    pending: Vec<Pending>,
    last_progress_ns: u64,
    last_packet_ns: u64,
    generation_counter: u32,
    idle_reported: bool,
}

impl Receiver {
    fn new(cfg: NetCaptureConfig, ring: Arc<CaptureRing>, metrics: Arc<Metrics>) -> Result<Self, NetCaptureError> {
        let addr = format!("{}:{}", cfg.bind_address, cfg.port);
        let socket = UdpSocket::bind(&addr).map_err(|source| NetCaptureError::Bind {
            addr: addr.clone(),
            source,
        })?;
        socket.set_read_timeout(Some(Duration::from_millis(READ_TIMEOUT_MS)))?;
        enlarge_receive_buffer(&socket);
        tracing::info!(%addr, "net capture listening");
        Ok(Self {
            cfg,
            ring,
            metrics,
            socket,
            session: None,
            pending: Vec::with_capacity(16),
            last_progress_ns: monotonic_now_ns(),
            last_packet_ns: monotonic_now_ns(),
            generation_counter: 0,
            idle_reported: false,
        })
    }

    fn run(mut self, stop: Arc<AtomicBool>) {
        let mut buffer = vec![0u8; protocol::HEADER_LEN + protocol::MAX_PCM_PAYLOAD_BYTES];
        let mut samples: Vec<i16> = Vec::with_capacity(protocol::MAX_PCM_PAYLOAD_BYTES / 2);
        while !stop.load(Ordering::Relaxed) {
            match self.socket.recv_from(&mut buffer) {
                Ok((len, _from)) => {
                    let now = monotonic_now_ns();
                    self.metrics.net_packets_received.fetch_add(1, Ordering::Relaxed);
                    self.metrics.note_net_packet(Utc::now().timestamp_millis());
                    self.last_packet_ns = now;
                    // A packet means the node is back, so re-arm the idle
                    // detector: otherwise the first silence of a session would
                    // latch the flag and the status page would claim the node
                    // is online long after it stopped.
                    self.idle_reported = false;
                    self.handle_datagram(&buffer[..len], now, &mut samples);
                    self.flush_expired_gaps(now);
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock
                    || error.kind() == io::ErrorKind::TimedOut =>
                {
                    let now = monotonic_now_ns();
                    self.flush_expired_gaps(now);
                    self.check_idle(now);
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => {
                    // A persistent socket error is worth surfacing but must not
                    // spin: back off a little and keep the status page honest.
                    tracing::error!(%error, "net capture receive failed");
                    self.metrics.set_last_error(format!("net capture receive failed: {error}"));
                    std::thread::sleep(Duration::from_millis(200));
                }
            }
        }
        tracing::info!("net capture stopped");
    }

    fn handle_datagram(&mut self, data: &[u8], now: u64, samples: &mut Vec<i16>) {
        let header = match Header::decode(data) {
            Ok(header) => header,
            Err(error) => {
                self.metrics.net_protocol_errors.fetch_add(1, Ordering::Relaxed);
                self.metrics
                    .set_last_error(format!("net capture protocol error: {error}"));
                return;
            }
        };

        if self.cfg.device_id != 0 && header.device_id != self.cfg.device_id {
            self.metrics.net_foreign_packets.fetch_add(1, Ordering::Relaxed);
            return;
        }

        // A different session id means the node rebooted or restarted its
        // sender: drop the reorder window and re-anchor the timeline.
        let session_changed = match &self.session {
            None => true,
            Some(session) => {
                session.session_id != header.session_id || session.device_id != header.device_id
            }
        };
        if session_changed {
            self.start_session(&header, now);
        }

        match header.kind {
            Kind::Hello => self.handle_hello(data, &header),
            Kind::Pcm => self.handle_pcm(data, &header, now, samples),
            Kind::Info => self.handle_info(data),
            Kind::Gate => self.handle_gate(data),
            Kind::HelloAck => {}
        }
    }

    /// The node telling us it deliberately skipped a frame range.
    ///
    /// Counted, not interpreted as loss: the frames were never on the wire. The
    /// gap they leave in the timeline is real and the recorder still closes over
    /// it, but `packets_lost` and the capture health counters stay clean — a node
    /// that gates is behaving, not failing.
    fn handle_gate(&mut self, data: &[u8]) {
        match protocol::decode_gate_payload(&data[protocol::HEADER_LEN..]) {
            Ok((from, to)) => {
                self.metrics
                    .net_gated_frames
                    .fetch_add(to - from, Ordering::Relaxed);
                self.metrics
                    .net_gate_announcements
                    .fetch_add(1, Ordering::Relaxed);
            }
            Err(error) => {
                self.metrics
                    .net_protocol_errors
                    .fetch_add(1, Ordering::Relaxed);
                self.metrics
                    .set_last_error(format!("net capture bad gate announcement: {error}"));
            }
        }
    }

    /// The node's description of itself. Stored verbatim: the server has no
    /// schema for it, and inventing defaults for fields a node did not send is
    /// exactly the kind of guess this project does not make.
    fn handle_info(&mut self, data: &[u8]) {
        let payload = &data[protocol::HEADER_LEN..];
        if payload.is_empty() || payload.len() > protocol::MAX_INFO_PAYLOAD_BYTES {
            self.metrics
                .net_protocol_errors
                .fetch_add(1, Ordering::Relaxed);
            return;
        }
        match std::str::from_utf8(payload) {
            // Parsed once here only to prove it is JSON; the text is what gets
            // handed to the API so nothing is lost in a round trip.
            Ok(text) if serde_json::from_str::<serde_json::Value>(text).is_ok() => {
                self.metrics
                    .set_node_report(text, Utc::now().timestamp_millis());
            }
            Ok(_) | Err(_) => {
                self.metrics
                    .net_protocol_errors
                    .fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    fn handle_hello(&mut self, data: &[u8], header: &Header) {
        let hello = match Hello::decode_payload(&data[protocol::HEADER_LEN..]) {
            Ok(hello) => hello,
            Err(error) => {
                self.metrics.net_protocol_errors.fetch_add(1, Ordering::Relaxed);
                self.metrics
                    .set_last_error(format!("net capture bad HELLO: {error}"));
                return;
            }
        };
        if hello.rate_hz != self.cfg.rate_hz || hello.channels != self.cfg.channels {
            // Refuse rather than reinterpret: recording audio at a rate the
            // server did not expect would corrupt every duration downstream.
            self.metrics.net_protocol_errors.fetch_add(1, Ordering::Relaxed);
            self.metrics.set_last_error(format!(
                "net capture format mismatch: node announced {} Hz {} ch, server expects {} Hz {} ch",
                hello.rate_hz, hello.channels, self.cfg.rate_hz, self.cfg.channels
            ));
            tracing::error!(
                node_rate_hz = hello.rate_hz,
                node_channels = hello.channels,
                server_rate_hz = self.cfg.rate_hz,
                server_channels = self.cfg.channels,
                "capture node announced a format the server is not configured for"
            );
            return;
        }
        if let Some(session) = self.session.as_mut() {
            // HELLO repeats every second so a restarted server can re-learn the
            // format; only the first one of a session is worth a log line.
            let first_of_session = session.hello.is_none();
            session.hello = Some(hello.clone());
            if first_of_session {
                tracing::info!(
                    device_id = header.device_id,
                    session_id = header.session_id,
                    device = %hello.device_name,
                    rate_hz = hello.rate_hz,
                    channels = hello.channels,
                    "capture node attached"
                );
            }
        }
        self.metrics.net_source_connected.store(true, Ordering::Relaxed);
        // The node is back, so an idle-timeout error left over from the outage
        // no longer describes the world. Anything else stays: it may be a
        // storage or detector problem that has nothing to do with this socket.
        if let Some(error) = self.metrics.last_error()
            && error.starts_with("net capture:")
        {
            self.metrics.clear_last_error();
        }
    }

    fn handle_pcm(&mut self, data: &[u8], header: &Header, now: u64, samples: &mut Vec<i16>) {
        let Some(session) = self.session.as_ref() else {
            return;
        };
        if session.hello.is_none() {
            // Never place audio until the format has been confirmed.
            self.metrics.net_protocol_errors.fetch_add(1, Ordering::Relaxed);
            return;
        }

        let payload = &data[protocol::HEADER_LEN..];
        let frames = match protocol::validate_pcm_payload(payload, self.cfg.channels) {
            Ok(frames) => frames,
            Err(error) => {
                self.metrics.net_protocol_errors.fetch_add(1, Ordering::Relaxed);
                self.report_protocol_error(error);
                return;
            }
        };
        if frames == 0 {
            return;
        }
        if protocol::decode_pcm(payload, samples).is_err() {
            self.metrics.net_protocol_errors.fetch_add(1, Ordering::Relaxed);
            return;
        }

        // A sender that restarts while reusing its session id would otherwise
        // look like an endless run of duplicates, because its sequence numbers
        // start again at 0. Its frame counter is monotonic within a session, so
        // a large backward jump is the tell.
        let reorder_frames =
            (u64::from(self.cfg.rate_hz) * u64::from(self.cfg.jitter_buffer_ms) / 1000).max(1);
        let restarted = self
            .session
            .as_ref()
            .map(|session| header.first_frame.saturating_add(reorder_frames) < session.max_first_frame)
            .unwrap_or(false);
        if restarted {
            tracing::warn!("capture stream restarted without a new session id; re-anchoring");
            self.start_session(header, now);
        }

        let seq = header.packet_seq;
        let next_seq = self.session.as_ref().map(|s| s.next_seq).unwrap_or(seq);
        let distance = seq.wrapping_sub(next_seq) as i32;
        if distance < 0 {
            // Already delivered or skipped past: a duplicate or a straggler.
            self.metrics.net_packets_duplicate.fetch_add(1, Ordering::Relaxed);
            return;
        }
        if self.pending.iter().any(|pending| pending.seq == seq) {
            self.metrics.net_packets_duplicate.fetch_add(1, Ordering::Relaxed);
            return;
        }
        if distance > 0 {
            self.metrics.net_packets_reordered.fetch_add(1, Ordering::Relaxed);
        }

        self.pending.push(Pending {
            seq,
            first_frame: header.first_frame,
            t_mono_us: header.t_mono_us,
            frames: frames as u32,
            samples: samples.clone(),
        });
        self.drain(now);
    }

    fn report_protocol_error(&self, error: ProtocolError) {
        static LAST: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let count = self.metrics.net_protocol_errors.load(Ordering::Relaxed);
        // Log the first few and then every hundredth, so a broken node cannot
        // flood the journal while still remaining visible.
        if count <= 5 || count % 100 == 0 {
            LAST.store(count, Ordering::Relaxed);
            self.metrics
                .set_last_error(format!("net capture payload error: {error}"));
            tracing::warn!(%error, count, "rejected pcm packet");
        }
    }

    fn drain(&mut self, now: u64) {
        loop {
            let next_seq = match self.session.as_ref() {
                Some(session) => session.next_seq,
                None => break,
            };
            let position = self
                .pending
                .iter()
                .position(|pending| pending.seq == next_seq);
            let Some(position) = position else {
                break;
            };
            let packet = self.pending.remove(position);
            self.emit(packet);
            if let Some(session) = self.session.as_mut() {
                session.next_seq = session.next_seq.wrapping_add(1);
            }
            self.last_progress_ns = now;
        }
    }

    /// Declare a sequence number lost once the reorder window has expired.
    ///
    /// No gap record is written here: the skipped `first_frame`/timer delta is
    /// what the dispatcher already turns into a gap with the correct duration.
    fn flush_expired_gaps(&mut self, now: u64) {
        if self.pending.is_empty() {
            return;
        }
        let window_ns = u64::from(self.cfg.jitter_buffer_ms) * 1_000_000;
        if now.saturating_sub(self.last_progress_ns) < window_ns {
            return;
        }
        let Some(next_seq) = self.session.as_ref().map(|s| s.next_seq) else {
            return;
        };

        // Pick the packet with the smallest forward distance from next_seq.
        let mut best = 0usize;
        for index in 1..self.pending.len() {
            let candidate = self.pending[index].seq.wrapping_sub(next_seq) as i32;
            let current = self.pending[best].seq.wrapping_sub(next_seq) as i32;
            if candidate < current {
                best = index;
            }
        }
        let skipped = self.pending[best].seq.wrapping_sub(next_seq) as i32;
        if skipped <= 0 {
            return;
        }
        if skipped > MAX_SKIP_PACKETS {
            // A jump this large is a new session with a reused id, not loss.
            tracing::warn!(skipped, "capture sequence jumped; restarting session");
            let header = Header {
                kind: Kind::Pcm,
                device_id: self.session.as_ref().map(|s| s.device_id).unwrap_or(0),
                session_id: self
                    .session
                    .as_ref()
                    .map(|s| s.session_id.wrapping_add(1))
                    .unwrap_or(0),
                packet_seq: self.pending[best].seq,
                first_frame: self.pending[best].first_frame,
                t_mono_us: self.pending[best].t_mono_us,
            };
            self.start_session(&header, now);
            return;
        }

        self.metrics
            .net_packets_lost
            .fetch_add(skipped as u64, Ordering::Relaxed);
        if let Some(session) = self.session.as_mut() {
            session.next_seq = self.pending[best].seq;
        }
        self.last_progress_ns = now;
        self.drain(now);
    }

    fn emit(&mut self, packet: Pending) {
        let Some(session) = self.session.as_mut() else {
            return;
        };
        let delta_us = packet
            .t_mono_us
            .saturating_sub(session.t0_us)
            .max(session.last_delta_us);
        session.last_delta_us = delta_us;
        session.max_first_frame = session.max_first_frame.max(packet.first_frame);
        let mono_start_ns = session.anchor_ns.saturating_add(delta_us.saturating_mul(1_000));
        let capture_seq = session.pushed_frames;
        let generation = session.generation;
        session.pushed_frames = session.pushed_frames.saturating_add(u64::from(packet.frames));

        let meta = CaptureMeta {
            frames: packet.frames,
            channels: self.cfg.channels,
            rate_hz: self.cfg.rate_hz,
            capture_seq,
            mono_start_ns,
            stream_generation: generation,
        };
        if !self.ring.callback_push(&packet.samples, meta) {
            self.metrics
                .capture_ring_overflow_frames
                .fetch_add(u64::from(packet.frames), Ordering::Relaxed);
        }
        self.metrics
            .capture_ring_high_water_frames
            .fetch_max(self.ring.high_water_frames(), Ordering::Relaxed);
    }

    fn start_session(&mut self, header: &Header, now: u64) {
        self.generation_counter = self.generation_counter.wrapping_add(1).max(1);
        if self.session.is_some() {
            tracing::warn!(
                device_id = header.device_id,
                session_id = header.session_id,
                generation = self.generation_counter,
                "capture node session changed; re-anchoring timeline"
            );
        }
        self.pending.clear();
        self.session = Some(Session {
            device_id: header.device_id,
            session_id: header.session_id,
            next_seq: header.packet_seq,
            t0_us: header.t_mono_us,
            anchor_ns: now,
            last_delta_us: 0,
            generation: self.generation_counter,
            pushed_frames: 0,
            hello: None,
            max_first_frame: header.first_frame,
        });
        self.last_progress_ns = now;
        self.metrics.net_source_connected.store(false, Ordering::Relaxed);
    }

    fn check_idle(&mut self, now: u64) {
        let idle_ns = u64::from(self.cfg.idle_timeout_ms) * 1_000_000;
        if self.idle_reported {
            return;
        }
        if now.saturating_sub(self.last_packet_ns) < idle_ns {
            return;
        }
        self.idle_reported = true;
        self.metrics.net_source_connected.store(false, Ordering::Relaxed);
        if self.session.is_some() {
            tracing::warn!(
                idle_ms = self.cfg.idle_timeout_ms,
                "capture node has sent nothing for the idle timeout"
            );
            self.metrics.set_last_error(format!(
                "net capture: no packets from the capture node for {} ms",
                self.cfg.idle_timeout_ms
            ));
        }
    }
}

/// Ask the kernel for a large receive buffer. Failure is not fatal — the default
/// buffer still works, it simply tolerates less burst.
#[cfg(unix)]
fn enlarge_receive_buffer(socket: &UdpSocket) {
    use std::os::unix::io::AsRawFd;
    let fd = socket.as_raw_fd();
    let value: libc::c_int = SOCKET_RCVBUF_BYTES;
    // SAFETY: the fd is owned by `socket` and outlives this call; the pointer is
    // valid for the size passed.
    let result = unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_RCVBUF,
            &value as *const libc::c_int as *const libc::c_void,
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        )
    };
    if result != 0 {
        let error = io::Error::last_os_error();
        tracing::warn!(%error, "could not enlarge the UDP receive buffer");
    }
}

#[cfg(not(unix))]
fn enlarge_receive_buffer(_socket: &UdpSocket) {}

/// Bind the socket and start the receive thread.
///
/// Binding happens before the thread starts so a port conflict fails startup
/// immediately, the same way a missing microphone fails the local path.
pub fn spawn(
    cfg: NetCaptureConfig,
    ring: Arc<CaptureRing>,
    metrics: Arc<Metrics>,
) -> Result<NetCaptureHandle, NetCaptureError> {
    if cfg.rate_hz == 0 {
        return Err(NetCaptureError::Config("rate_hz must not be 0".into()));
    }
    if !(1..=2).contains(&cfg.channels) {
        return Err(NetCaptureError::Config(format!(
            "channels must be 1 or 2, got {}",
            cfg.channels
        )));
    }
    if cfg.jitter_buffer_ms == 0 {
        return Err(NetCaptureError::Config("jitter_buffer_ms must not be 0".into()));
    }

    let receiver = Receiver::new(cfg, ring, metrics)?;
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = Arc::clone(&stop);
    let join = std::thread::Builder::new()
        .name("steno-net-capture".into())
        .spawn(move || receiver.run(thread_stop))
        .map_err(NetCaptureError::Thread)?;
    Ok(NetCaptureHandle {
        stop,
        join: Some(join),
    })
}
