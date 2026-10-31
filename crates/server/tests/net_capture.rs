//! End-to-end tests for the split deployment's receive side.
//!
//! These run without any hardware: a test acts as the capture node and sends
//! real datagrams to [`steno_server::net_capture`], then inspects what landed in
//! the capture ring. That is the point of the split — the protocol and the
//! timeline reconstruction can be verified on a laptop before a node exists.

use steno_lib::audio_capture::CaptureRing;
use steno_lib::metrics::Metrics;
use steno_server::net_capture::{self, NetCaptureConfig};
use steno_lib::protocol::{self, Header, Hello, Kind};
use std::net::UdpSocket;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

const RATE_HZ: u32 = 16_000;
const CHANNELS: u16 = 1;
/// 40 ms at 16 kHz mono.
const FRAMES_PER_PACKET: u32 = 640;

fn free_port() -> u16 {
    // Bind an ephemeral port, learn it, drop the socket. A small race remains,
    // which is acceptable for tests and avoids hardcoding a port.
    let socket = UdpSocket::bind("127.0.0.1:0").expect("bind ephemeral");
    socket.local_addr().expect("local addr").port()
}

struct Node {
    socket: UdpSocket,
    target: String,
    session_id: u32,
    seq: u32,
    first_frame: u64,
    t_mono_us: u64,
}

impl Node {
    fn new(target: String) -> Self {
        Self {
            socket: UdpSocket::bind("127.0.0.1:0").expect("node bind"),
            target,
            session_id: 7,
            seq: 0,
            first_frame: 0,
            t_mono_us: 1_000,
        }
    }

    fn send_hello(&self, rate_hz: u32, channels: u16) {
        let header = Header {
            kind: Kind::Hello,
            device_id: 1,
            session_id: self.session_id,
            packet_seq: 0,
            first_frame: 0,
            t_mono_us: 0,
        };
        let hello = Hello {
            rate_hz,
            channels,
            device_name: "test-node".to_string(),
        };
        let mut buffer = Vec::new();
        protocol::encode_hello_packet(&header, &hello, &mut buffer);
        self.socket.send_to(&buffer, &self.target).expect("send hello");
    }

    /// Advance the headers as if a packet had been sent, without sending it.
    fn skip_pcm(&mut self) {
        self.seq = self.seq.wrapping_add(1);
        self.first_frame += u64::from(FRAMES_PER_PACKET);
        self.t_mono_us += 40_000;
    }

    /// Send one 40 ms packet of a constant non-zero signal.
    fn send_pcm(&mut self) {
        let samples = vec![1234i16; (FRAMES_PER_PACKET as usize) * usize::from(CHANNELS)];
        let header = Header {
            kind: Kind::Pcm,
            device_id: 1,
            session_id: self.session_id,
            packet_seq: self.seq,
            first_frame: self.first_frame,
            t_mono_us: self.t_mono_us,
        };
        let mut buffer = Vec::new();
        protocol::encode_pcm_packet(&header, &samples, &mut buffer);
        self.socket.send_to(&buffer, &self.target).expect("send pcm");
        self.seq = self.seq.wrapping_add(1);
        self.first_frame += u64::from(FRAMES_PER_PACKET);
        self.t_mono_us += 40_000;
    }
}

fn start_server(port: u16, jitter_ms: u32) -> (Arc<CaptureRing>, Arc<Metrics>, net_capture::NetCaptureHandle) {
    let ring = Arc::new(CaptureRing::new(RATE_HZ, CHANNELS, 500, 20, 400));
    let metrics = Arc::new(Metrics::new());
    let handle = net_capture::spawn(
        NetCaptureConfig {
            bind_address: "127.0.0.1".to_string(),
            port,
            device_id: 0,
            jitter_buffer_ms: jitter_ms,
            idle_timeout_ms: 2_000,
        },
        Arc::clone(&ring),
        Arc::clone(&metrics),
    )
    .expect("spawn net capture");
    (ring, metrics, handle)
}

fn pop_block(ring: &CaptureRing, timeout: Duration) -> Option<(steno_lib::audio_capture::CaptureMeta, Vec<i16>)> {
    let deadline = Instant::now() + timeout;
    let mut samples = Vec::new();
    while Instant::now() < deadline {
        if let Some(meta) = ring.pop_into(&mut samples) {
            return Some((meta, samples));
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    None
}

/// Poll a condition until it holds or the deadline passes.
fn eventually(mut condition: impl FnMut() -> bool, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if condition() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    condition()
}

#[test]
fn contiguous_packets_arrive_in_order_with_a_synthetic_timeline() {
    let port = free_port();
    let (ring, metrics, _handle) = start_server(port, 150);
    let mut node = Node::new(format!("127.0.0.1:{port}"));

    node.send_hello(RATE_HZ, CHANNELS);
    for _ in 0..6 {
        node.send_pcm();
    }

    let mut blocks = Vec::new();
    for _ in 0..6 {
        let block = pop_block(&ring, Duration::from_secs(3)).expect("block should arrive");
        blocks.push(block);
    }

    // Every packet is delivered with its real payload.
    for (meta, samples) in &blocks {
        assert_eq!(meta.frames, FRAMES_PER_PACKET);
        assert_eq!(meta.channels, CHANNELS);
        assert_eq!(meta.rate_hz, RATE_HZ);
        assert_eq!(samples.len(), FRAMES_PER_PACKET as usize);
        assert!(samples.iter().all(|sample| *sample == 1234));
    }

    // capture_seq walks the frame domain exactly as the ALSA path does.
    let seqs: Vec<u64> = blocks.iter().map(|(meta, _)| meta.capture_seq).collect();
    assert_eq!(seqs, vec![0, 640, 1280, 1920, 2560, 3200]);

    // The timeline is derived from the node timer, so consecutive blocks are
    // exactly 40 ms apart with no jitter, even though the send loop is bursty.
    for pair in blocks.windows(2) {
        let delta = pair[1].0.mono_start_ns - pair[0].0.mono_start_ns;
        assert_eq!(delta, 40_000_000, "timeline must be jitter free");
    }

    assert!(metrics.net_source_connected.load(std::sync::atomic::Ordering::Relaxed));
    assert_eq!(
        metrics.net_protocol_errors.load(std::sync::atomic::Ordering::Relaxed),
        0
    );
}

#[test]
fn a_lost_packet_becomes_a_time_gap_not_a_shortened_timeline() {
    let port = free_port();
    let (ring, metrics, _handle) = start_server(port, 60);
    let mut node = Node::new(format!("127.0.0.1:{port}"));

    node.send_hello(RATE_HZ, CHANNELS);
    node.send_pcm(); // seq 0
    node.send_pcm(); // seq 1
    node.skip_pcm(); // seq 2 is lost on the way
    node.send_pcm(); // seq 3

    // Two blocks are contiguous, then the third one jumps.
    let first = pop_block(&ring, Duration::from_secs(2)).expect("first");
    let second = pop_block(&ring, Duration::from_secs(2)).expect("second");
    assert_eq!(second.0.mono_start_ns - first.0.mono_start_ns, 40_000_000);

    // The third delivered packet must carry the hole with it: 80 ms of elapsed
    // node time for 40 ms of audio, which the dispatcher turns into a gap.
    let third = pop_block(&ring, Duration::from_secs(2)).expect("third");
    let jump = third.0.mono_start_ns - second.0.mono_start_ns;
    assert_eq!(
        jump, 80_000_000,
        "the lost packet must widen the timeline instead of being stitched out"
    );
    assert_eq!(third.0.capture_seq, 640 * 2);
    assert!(
        metrics
            .net_packets_lost
            .load(std::sync::atomic::Ordering::Relaxed)
            >= 1
    );
}

/// The node is on another host, so "what is it running" can only come from the
/// node. This is the wire path end to end: encode on the node side, decode and
/// store on the server side, no schema shared between them.
#[test]
fn a_node_self_report_is_stored_verbatim() {
    let port = free_port();
    let (_ring, metrics, _handle) = start_server(port, 150);
    let node = Node::new(format!("127.0.0.1:{port}"));

    let report = r#"{"argv":["/usr/local/bin/steno-node","--config","/etc/steno-node/node.toml"],"config":"/etc/steno-node/node.toml","version":"0.1.0","device_id":1,"server":"192.168.1.10:8091","rate_hz":16000,"channels":1,"reported_at_unix_s":1735689600}"#;
    let header = Header {
        kind: Kind::Info,
        device_id: 1,
        session_id: node.session_id,
        packet_seq: 0,
        first_frame: 0,
        t_mono_us: 0,
    };
    let mut packet = Vec::new();
    protocol::encode_info_packet(&header, report.as_bytes(), &mut packet);
    node.socket.send_to(&packet, &node.target).expect("send info");

    assert!(
        eventually(
            || metrics.node_report().is_some(),
            Duration::from_secs(2)
        ),
        "the report should be stored"
    );
    // Byte-for-byte: the API hands this to the Portal, so a re-serialisation
    // here would be a second source of truth for what the node said.
    assert_eq!(metrics.node_report().as_deref(), Some(report));
    assert!(metrics.net_node_report_unix_ms.load(Ordering::Relaxed) > 0);
}

/// The wire protocol carries a device id, not an address, so the socket is the
/// only place "which machine is sending" can come from. It is what the status
/// page names when a node goes quiet — "who was talking to us" has no other
/// answer.
#[test]
fn the_peer_address_of_the_node_is_recorded() {
    let port = free_port();
    let (_ring, metrics, _handle) = start_server(port, 150);
    let node = Node::new(format!("127.0.0.1:{port}"));
    let expected = node.socket.local_addr().expect("node local addr").to_string();

    assert!(metrics.net_peer().is_none(), "nothing has arrived yet");

    node.send_hello(RATE_HZ, CHANNELS);

    assert!(
        eventually(|| metrics.net_peer().is_some(), Duration::from_secs(2)),
        "the datagram's source address should be recorded"
    );
    // `ip:port` exactly as the socket reported it: the node's port is ephemeral
    // and changes when it restarts, and that change is itself a useful signal.
    assert_eq!(metrics.net_peer().as_deref(), Some(expected.as_str()));
}

/// A node that gates quiet audio announces the range it did not send. The server
/// must count it as *gated*, never as packet loss — otherwise a node behaving
/// exactly as configured would page the operator about a broken uplink.
#[test]
fn a_gate_announcement_is_counted_as_gated_not_lost() {
    let port = free_port();
    let (_ring, metrics, _handle) = start_server(port, 150);
    let node = Node::new(format!("127.0.0.1:{port}"));

    let header = Header {
        kind: Kind::Gate,
        device_id: 1,
        session_id: node.session_id,
        packet_seq: 0,
        first_frame: 0,
        t_mono_us: 0,
    };
    let mut packet = Vec::new();
    // 3 s at 16 kHz skipped.
    protocol::encode_gate_packet(&header, 16_000, 64_000, &mut packet);
    node.socket.send_to(&packet, &node.target).expect("send gate");

    assert!(
        eventually(
            || metrics.net_gated_frames.load(Ordering::Relaxed) == 48_000,
            Duration::from_secs(2)
        ),
        "the announced range must be counted"
    );
    assert_eq!(metrics.net_gate_announcements.load(Ordering::Relaxed), 1);
    assert_eq!(
        metrics.net_packets_lost.load(Ordering::Relaxed),
        0,
        "frames the node never sent are not losses"
    );
    assert_eq!(metrics.net_protocol_errors.load(Ordering::Relaxed), 0);
}

/// A reversed or empty range is refused rather than clamped: clamping would hide
/// audio the server believes it deliberately skipped.
#[test]
fn an_impossible_gate_range_is_a_protocol_error() {
    let port = free_port();
    let (_ring, metrics, _handle) = start_server(port, 150);
    let node = Node::new(format!("127.0.0.1:{port}"));
    let header = Header {
        kind: Kind::Gate,
        device_id: 1,
        session_id: node.session_id,
        packet_seq: 0,
        first_frame: 0,
        t_mono_us: 0,
    };
    let mut packet = Vec::new();
    protocol::encode_gate_packet(&header, 64_000, 16_000, &mut packet);
    node.socket.send_to(&packet, &node.target).expect("send gate");

    assert!(
        eventually(
            || metrics.net_protocol_errors.load(Ordering::Relaxed) > 0,
            Duration::from_secs(2)
        ),
        "a reversed range must be refused"
    );
    assert_eq!(metrics.net_gated_frames.load(Ordering::Relaxed), 0);
}

/// A malformed report is a protocol error, not a stored string: the status page
/// must never show half a JSON document.
#[test]
fn a_malformed_self_report_is_counted_and_dropped() {
    let port = free_port();
    let (_ring, metrics, _handle) = start_server(port, 150);
    let node = Node::new(format!("127.0.0.1:{port}"));
    let header = Header {
        kind: Kind::Info,
        device_id: 1,
        session_id: node.session_id,
        packet_seq: 0,
        first_frame: 0,
        t_mono_us: 0,
    };
    let mut packet = Vec::new();
    protocol::encode_info_packet(&header, b"not json at all", &mut packet);
    node.socket.send_to(&packet, &node.target).expect("send info");

    assert!(
        eventually(
            || metrics.net_protocol_errors.load(Ordering::Relaxed) > 0,
            Duration::from_secs(2)
        ),
        "an unparsable report must be counted"
    );
    assert!(metrics.node_report().is_none());
}

#[test]
fn a_node_that_comes_back_goes_offline_again_when_it_leaves() {
    use std::sync::atomic::Ordering;

    let port = free_port();
    let (_ring, metrics, _handle) = start_server(port, 150);
    let mut node = Node::new(format!("127.0.0.1:{port}"));
    let connected = || metrics.net_source_connected.load(Ordering::Relaxed);

    // The idle timeout in `start_server` is 2 s, so every silence below is
    // comfortably longer than that.
    let silence = Duration::from_millis(2_600);
    let settle = Duration::from_secs(1);

    node.send_hello(RATE_HZ, CHANNELS);
    node.send_pcm();
    assert!(eventually(connected, settle), "the node should come online");

    // First silence: the node is reported offline.
    assert!(
        eventually(|| !connected(), silence + settle),
        "a silent node must be reported offline"
    );

    // The node reconnects. Its HELLO repeats in the real world; one is enough
    // here because the session is still alive on the server side.
    node.send_hello(RATE_HZ, CHANNELS);
    node.send_pcm();
    assert!(
        eventually(connected, settle),
        "the node should be online again after reconnecting"
    );

    // Second silence: this is the regression the first version had — the idle
    // flag latched after the first silence and the node stayed "online"
    // forever after.
    assert!(
        eventually(|| !connected(), silence + settle),
        "a second silence must be reported offline too"
    );
}

#[test]
fn a_hello_with_a_different_format_is_refused() {
    let port = free_port();
    let (ring, metrics, _handle) = start_server(port, 150);
    let mut node = Node::new(format!("127.0.0.1:{port}"));

    // The node claims 48 kHz. There is nothing to "configure the server to
    // expect" any more — the wire format is a constant — so this is a node built
    // against a different definition of the wire, and its audio must not be
    // recorded.
    node.send_hello(48_000, CHANNELS);
    for _ in 0..4 {
        node.send_pcm();
    }

    assert!(
        pop_block(&ring, Duration::from_millis(500)).is_none(),
        "audio must not be recorded when the node does not speak the wire format"
    );
    let error = metrics.last_error().unwrap_or_default();
    assert!(
        error.contains("the wire format is"),
        "the refusal must name the wire format rather than a server setting: {error}"
    );
    assert!(
        metrics
            .net_protocol_errors
            .load(std::sync::atomic::Ordering::Relaxed)
            >= 1
    );
}

#[test]
fn packets_from_an_unexpected_device_are_ignored() {
    let port = free_port();
    let sink = Arc::new(CaptureRing::new(RATE_HZ, CHANNELS, 500, 20, 400));
    let metrics = Arc::new(Metrics::new());
    let _handle = net_capture::spawn(
        NetCaptureConfig {
            bind_address: "127.0.0.1".to_string(),
            port,
            device_id: 42,
            jitter_buffer_ms: 150,
            idle_timeout_ms: 2_000,
        },
        Arc::clone(&sink),
        Arc::clone(&metrics),
    )
    .expect("spawn");

    let mut node = Node::new(format!("127.0.0.1:{port}"));
    node.send_hello(RATE_HZ, CHANNELS);
    for _ in 0..3 {
        node.send_pcm();
    }

    assert!(pop_block(&sink, Duration::from_millis(400)).is_none());
    assert!(
        metrics
            .net_foreign_packets
            .load(std::sync::atomic::Ordering::Relaxed)
            >= 1
    );
}
