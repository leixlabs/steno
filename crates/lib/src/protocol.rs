//! Capture-node ↔ server wire protocol (split deployment).
//!
//! A node owns the microphone and nothing else: it captures raw S16 PCM, chops
//! it into fixed-size blocks and ships them over UDP. The server owns recording,
//! detection, storage and the Portal. Both sides compile this module, so a
//! change to the layout fails `cargo build` on both ends instead of drifting.
//!
//! # Why UDP
//!
//! TCP would retransmit into a growing backlog whenever the WiFi link hiccups,
//! turning a network delay into a permanently shifted audio timeline, and it
//! would silently hide loss. This protocol is built to make loss *visible*: the
//! node stamps every packet with a session id, a packet sequence number and the
//! absolute frame index of its first sample, so the receiver can register an
//! honest gap instead of inventing continuous audio (TECH_SPEC §4.3).
//!
//! # Layout
//!
//! All integers are little-endian. The header is [`HEADER_LEN`] bytes:
//!
//! ```text
//! offset  size  field
//!      0     2  magic      0x534E ("SN")
//!      2     1  version
//!      3     1  kind       1 = HELLO, 2 = PCM, 3 = HELLO_ACK
//!      4     4  device_id  operator-assigned; 0 means "first node seen"
//!      8     4  session_id node boot counter; a new value restarts the stream
//!     12     4  packet_seq dense from 0 per session, for loss detection
//!     16     8  first_frame absolute frame index of the first sample
//!     24     8  t_mono_us  node monotonic microseconds (drift measurement only)
//! ```
//!
//! HELLO carries the negotiated capture format as a [`HELLO_PAYLOAD_LEN`] byte
//! payload. PCM carries raw interleaved S16 samples, at most
//! [`MAX_PCM_PAYLOAD_BYTES`] of them.
//!
//! `t_mono_us` is *never* used to place audio on the timeline. It exists only so
//! the server can measure the node's crystal drift; placing audio by arrival
//! time would let WiFi jitter masquerade as gaps.

use std::fmt;

/// `"SN"` in little-endian order.
pub const MAGIC: u16 = 0x534E;
/// Wire version. Bumped on any incompatible layout change.
pub const VERSION: u8 = 1;

pub const KIND_HELLO: u8 = 1;
pub const KIND_PCM: u8 = 2;
pub const KIND_HELLO_ACK: u8 = 3;
/// A node describing itself: its argv, config path, version and uptime.
///
/// Added after the first deployment, so an older server rejects the header and
/// counts one protocol error per report (the node repeats it on the HELLO
/// timer). Both ends are built from the same tree here; a mixed fleet would
/// need this behind a version check.
pub const KIND_INFO: u8 = 4;
/// "I deliberately sent nothing for these frames."
///
/// Without it a node that gated quiet audio would look exactly like a node whose
/// packets were lost, and the timeline would report a healthy night as a broken
/// uplink. The payload is two little-endian u64 frame indexes: the first frame
/// that was not sent and the frame that resumes transmission.
pub const KIND_GATE: u8 = 5;
/// Two u64 frame indexes.
pub const GATE_PAYLOAD_LEN: usize = 16;

/// Upper bound on a node's self-report. It is a few hundred bytes of JSON; the
/// cap keeps a node from pushing arbitrary volume into the status endpoint.
pub const MAX_INFO_PAYLOAD_BYTES: usize = 4096;

pub const HEADER_LEN: usize = 32;
pub const HELLO_PAYLOAD_LEN: usize = 40;
/// Maximum PCM bytes per packet: 40 ms of 16 kHz mono S16.
///
/// 1280 payload + 32 header = 1312 bytes, which stays under the 1472 byte IPv4
/// payload of a 1500 byte MTU. Larger blocks would trigger IP fragmentation and
/// fragment loss on WiFi costs several packets, not one.
pub const MAX_PCM_PAYLOAD_BYTES: usize = 1280;
/// Bytes reserved for the device name inside HELLO.
pub const DEVICE_NAME_LEN: usize = 32;
/// Only S16 little-endian is accepted on the wire (TECH_SPEC §5.1).
pub const SAMPLE_FORMAT_S16_LE: u8 = 1;

/// Packet kind. Anything else is a protocol error, not a silently ignored packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Hello,
    Pcm,
    HelloAck,
    Info,
    Gate,
}

impl Kind {
    pub fn to_u8(self) -> u8 {
        match self {
            Kind::Hello => KIND_HELLO,
            Kind::Pcm => KIND_PCM,
            Kind::HelloAck => KIND_HELLO_ACK,
            Kind::Info => KIND_INFO,
            Kind::Gate => KIND_GATE,
        }
    }

    pub fn from_u8(value: u8) -> Result<Self, ProtocolError> {
        match value {
            KIND_HELLO => Ok(Kind::Hello),
            KIND_PCM => Ok(Kind::Pcm),
            KIND_HELLO_ACK => Ok(Kind::HelloAck),
            KIND_INFO => Ok(Kind::Info),
            KIND_GATE => Ok(Kind::Gate),
            other => Err(ProtocolError::UnknownKind(other)),
        }
    }
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Kind::Hello => "hello",
            Kind::Pcm => "pcm",
            Kind::HelloAck => "hello_ack",
            Kind::Info => "info",
            Kind::Gate => "gate",
        };
        f.write_str(name)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtocolError {
    ShortPacket { needed: usize, got: usize },
    BadMagic(u16),
    UnsupportedVersion(u8),
    UnknownKind(u8),
    PayloadTooLarge(usize),
    /// PCM byte length is not a whole number of frames for the announced layout.
    UnalignedPayload { bytes: usize, block_align: usize },
    UnsupportedSampleFormat(u8),
    ZeroRate,
    UnsupportedChannels(u16),
    /// A gate announcement whose range is empty or reversed. It is refused
    /// rather than clamped: a wrong range would hide audio the server believes
    /// it deliberately skipped.
    BadGateRange { from: u64, to: u64 },
    DeviceNameNotUtf8,
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProtocolError::ShortPacket { needed, got } => {
                write!(f, "packet too short: need {needed} bytes, got {got}")
            }
            ProtocolError::BadMagic(value) => {
                write!(f, "bad magic 0x{value:04X}, expected 0x{MAGIC:04X}")
            }
            ProtocolError::UnsupportedVersion(version) => {
                write!(f, "unsupported protocol version {version}, expected {VERSION}")
            }
            ProtocolError::UnknownKind(kind) => write!(f, "unknown packet kind {kind}"),
            ProtocolError::PayloadTooLarge(len) => write!(
                f,
                "pcm payload of {len} bytes exceeds the {MAX_PCM_PAYLOAD_BYTES} byte limit"
            ),
            ProtocolError::UnalignedPayload { bytes, block_align } => write!(
                f,
                "pcm payload of {bytes} bytes is not a multiple of the {block_align} byte block"
            ),
            ProtocolError::UnsupportedSampleFormat(format) => {
                write!(f, "unsupported sample format {format}, only S16_LE is supported")
            }
            ProtocolError::ZeroRate => f.write_str("capture rate must not be 0"),
            ProtocolError::UnsupportedChannels(channels) => {
                write!(f, "unsupported channel count {channels}, must be 1 or 2")
            }
            ProtocolError::BadGateRange { from, to } => write!(
                f,
                "gate range {from}..{to} is empty or reversed"
            ),
            ProtocolError::DeviceNameNotUtf8 => f.write_str("device name is not valid UTF-8"),
        }
    }
}

impl std::error::Error for ProtocolError {}

/// Fixed-size packet header, common to every kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub kind: Kind,
    pub device_id: u32,
    pub session_id: u32,
    pub packet_seq: u32,
    pub first_frame: u64,
    pub t_mono_us: u64,
}

impl Header {
    pub fn encode(&self) -> [u8; HEADER_LEN] {
        let mut buf = [0u8; HEADER_LEN];
        buf[0..2].copy_from_slice(&MAGIC.to_le_bytes());
        buf[2] = VERSION;
        buf[3] = self.kind.to_u8();
        buf[4..8].copy_from_slice(&self.device_id.to_le_bytes());
        buf[8..12].copy_from_slice(&self.session_id.to_le_bytes());
        buf[12..16].copy_from_slice(&self.packet_seq.to_le_bytes());
        buf[16..24].copy_from_slice(&self.first_frame.to_le_bytes());
        buf[24..32].copy_from_slice(&self.t_mono_us.to_le_bytes());
        buf
    }

    pub fn decode(buf: &[u8]) -> Result<Self, ProtocolError> {
        if buf.len() < HEADER_LEN {
            return Err(ProtocolError::ShortPacket {
                needed: HEADER_LEN,
                got: buf.len(),
            });
        }
        let magic = u16::from_le_bytes([buf[0], buf[1]]);
        if magic != MAGIC {
            return Err(ProtocolError::BadMagic(magic));
        }
        let version = buf[2];
        if version != VERSION {
            return Err(ProtocolError::UnsupportedVersion(version));
        }
        Ok(Header {
            kind: Kind::from_u8(buf[3])?,
            device_id: u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]),
            session_id: u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]),
            packet_seq: u32::from_le_bytes([buf[12], buf[13], buf[14], buf[15]]),
            first_frame: u64::from_le_bytes([
                buf[16], buf[17], buf[18], buf[19], buf[20], buf[21], buf[22], buf[23],
            ]),
            t_mono_us: u64::from_le_bytes([
                buf[24], buf[25], buf[26], buf[27], buf[28], buf[29], buf[30], buf[31],
            ]),
        })
    }
}

/// What the node announces once per session, before its first PCM packet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hello {
    pub rate_hz: u32,
    pub channels: u16,
    pub device_name: String,
}

impl Hello {
    pub fn encode_payload(&self) -> [u8; HELLO_PAYLOAD_LEN] {
        let mut buf = [0u8; HELLO_PAYLOAD_LEN];
        buf[0..4].copy_from_slice(&self.rate_hz.to_le_bytes());
        buf[4..6].copy_from_slice(&self.channels.to_le_bytes());
        buf[6] = SAMPLE_FORMAT_S16_LE;
        // buf[7] stays reserved/zero.
        let name = self.device_name.as_bytes();
        let take = name.len().min(DEVICE_NAME_LEN);
        buf[8..8 + take].copy_from_slice(&name[..take]);
        buf
    }

    pub fn decode_payload(payload: &[u8]) -> Result<Self, ProtocolError> {
        if payload.len() < HELLO_PAYLOAD_LEN {
            return Err(ProtocolError::ShortPacket {
                needed: HELLO_PAYLOAD_LEN,
                got: payload.len(),
            });
        }
        let rate_hz = u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]);
        if rate_hz == 0 {
            return Err(ProtocolError::ZeroRate);
        }
        let channels = u16::from_le_bytes([payload[4], payload[5]]);
        if !(1..=2).contains(&channels) {
            return Err(ProtocolError::UnsupportedChannels(channels));
        }
        if payload[6] != SAMPLE_FORMAT_S16_LE {
            return Err(ProtocolError::UnsupportedSampleFormat(payload[6]));
        }
        let raw = &payload[8..8 + DEVICE_NAME_LEN];
        let end = raw.iter().position(|byte| *byte == 0).unwrap_or(raw.len());
        let device_name = std::str::from_utf8(&raw[..end])
            .map_err(|_| ProtocolError::DeviceNameNotUtf8)?
            .to_string();
        Ok(Hello {
            rate_hz,
            channels,
            device_name,
        })
    }
}

/// Bytes per frame for a layout: interleaved S16 means 2 bytes per sample.
pub fn block_align(channels: u16) -> usize {
    usize::from(channels.max(1)) * 2
}

/// Validate a PCM payload against the announced layout and split it into
/// samples. Returns the number of frames on success.
pub fn validate_pcm_payload(payload: &[u8], channels: u16) -> Result<usize, ProtocolError> {
    if payload.is_empty() {
        return Ok(0);
    }
    if payload.len() > MAX_PCM_PAYLOAD_BYTES {
        return Err(ProtocolError::PayloadTooLarge(payload.len()));
    }
    let align = block_align(channels);
    if payload.len() % align != 0 {
        return Err(ProtocolError::UnalignedPayload {
            bytes: payload.len(),
            block_align: align,
        });
    }
    Ok(payload.len() / align)
}

/// Decode an interleaved S16_LE payload into `out`, replacing its contents.
pub fn decode_pcm(payload: &[u8], out: &mut Vec<i16>) -> Result<usize, ProtocolError> {
    if payload.len() % 2 != 0 {
        return Err(ProtocolError::UnalignedPayload {
            bytes: payload.len(),
            block_align: 2,
        });
    }
    out.clear();
    out.reserve(payload.len() / 2);
    for pair in payload.chunks_exact(2) {
        out.push(i16::from_le_bytes([pair[0], pair[1]]));
    }
    Ok(out.len())
}

/// Serialise a PCM packet (header + payload) into `out`, replacing its contents.
pub fn encode_pcm_packet(header: &Header, samples: &[i16], out: &mut Vec<u8>) {
    out.clear();
    out.reserve(HEADER_LEN + samples.len() * 2);
    out.extend_from_slice(&header.encode());
    for sample in samples {
        out.extend_from_slice(&sample.to_le_bytes());
    }
}

/// Serialise a HELLO packet (header + format payload) into `out`.
pub fn encode_hello_packet(header: &Header, hello: &Hello, out: &mut Vec<u8>) {
    out.clear();
    out.reserve(HEADER_LEN + HELLO_PAYLOAD_LEN);
    out.extend_from_slice(&header.encode());
    out.extend_from_slice(&hello.encode_payload());
}

/// Encodes a gate announcement: `[u64 from][u64 to]` in frames.
///
/// `from` is the first frame that was *not* transmitted, `to` is the frame where
/// transmission resumes, so `to - from` is exactly the audio the server did not
/// receive. Anything else (a zero-width range, `to < from`) is refused by the
/// decoder rather than being interpreted.
pub fn encode_gate_packet(
    header: &Header,
    from_frame: u64,
    to_frame: u64,
    out: &mut Vec<u8>,
) {
    out.clear();
    out.reserve(HEADER_LEN + GATE_PAYLOAD_LEN);
    out.extend_from_slice(&header.encode());
    out.extend_from_slice(&from_frame.to_le_bytes());
    out.extend_from_slice(&to_frame.to_le_bytes());
}

/// Decodes a gate announcement's payload.
pub fn decode_gate_payload(payload: &[u8]) -> Result<(u64, u64), ProtocolError> {
    if payload.len() < GATE_PAYLOAD_LEN {
        return Err(ProtocolError::ShortPacket {
            needed: HEADER_LEN + GATE_PAYLOAD_LEN,
            got: HEADER_LEN + payload.len(),
        });
    }
    let from = u64::from_le_bytes(payload[0..8].try_into().expect("8 bytes"));
    let to = u64::from_le_bytes(payload[8..16].try_into().expect("8 bytes"));
    if to <= from {
        return Err(ProtocolError::BadGateRange { from, to });
    }
    Ok((from, to))
}

/// Encodes a self-report datagram: header, then raw UTF-8 JSON.
///
/// The payload is not length-prefixed because UDP already delivers one datagram
/// as one unit; the receiver reads whatever follows the header.
pub fn encode_info_packet(header: &Header, payload: &[u8], out: &mut Vec<u8>) {
    let take = payload.len().min(MAX_INFO_PAYLOAD_BYTES);
    out.clear();
    out.reserve(HEADER_LEN + take);
    out.extend_from_slice(&header.encode());
    out.extend_from_slice(&payload[..take]);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header() -> Header {
        Header {
            kind: Kind::Pcm,
            device_id: 7,
            session_id: 3,
            packet_seq: 42,
            first_frame: 1_234_567,
            t_mono_us: 9_876_543_210,
        }
    }

    #[test]
    fn header_round_trips() {
        let original = header();
        let decoded = Header::decode(&original.encode()).expect("decode");
        assert_eq!(decoded, original);
    }

    #[test]
    fn header_rejects_bad_magic() {
        let mut bytes = header().encode();
        bytes[0] = 0;
        assert!(matches!(
            Header::decode(&bytes),
            Err(ProtocolError::BadMagic(_))
        ));
    }

    #[test]
    fn header_rejects_wrong_version() {
        let mut bytes = header().encode();
        bytes[2] = VERSION + 1;
        assert!(matches!(
            Header::decode(&bytes),
            Err(ProtocolError::UnsupportedVersion(_))
        ));
    }

    #[test]
    fn header_rejects_unknown_kind() {
        let mut bytes = header().encode();
        bytes[3] = 99;
        assert!(matches!(
            Header::decode(&bytes),
            Err(ProtocolError::UnknownKind(99))
        ));
    }

    #[test]
    fn header_rejects_short_buffer() {
        let bytes = [0u8; HEADER_LEN - 1];
        match Header::decode(&bytes) {
            Err(ProtocolError::ShortPacket { needed, got }) => {
                assert_eq!(needed, HEADER_LEN);
                assert_eq!(got, HEADER_LEN - 1);
            }
            other => panic!("expected ShortPacket, got {other:?}"),
        }
    }

    #[test]
    fn hello_round_trips() {
        let hello = Hello {
            rate_hz: 16_000,
            channels: 1,
            device_name: "bedroom".to_string(),
        };
        let decoded = Hello::decode_payload(&hello.encode_payload()).expect("decode");
        assert_eq!(decoded, hello);
    }

    #[test]
    fn hello_truncates_a_long_device_name_without_panicking() {
        let hello = Hello {
            rate_hz: 48_000,
            channels: 2,
            device_name: "x".repeat(DEVICE_NAME_LEN * 3),
        };
        let decoded = Hello::decode_payload(&hello.encode_payload()).expect("decode");
        assert_eq!(decoded.device_name.len(), DEVICE_NAME_LEN);
        assert_eq!(decoded.channels, 2);
    }

    #[test]
    fn hello_rejects_zero_rate_and_bad_channels() {
        let mut payload = Hello {
            rate_hz: 16_000,
            channels: 1,
            device_name: "n".into(),
        }
        .encode_payload();

        let mut zero_rate = payload;
        zero_rate[0..4].copy_from_slice(&0u32.to_le_bytes());
        assert_eq!(
            Hello::decode_payload(&zero_rate),
            Err(ProtocolError::ZeroRate)
        );

        payload[4..6].copy_from_slice(&3u16.to_le_bytes());
        assert_eq!(
            Hello::decode_payload(&payload),
            Err(ProtocolError::UnsupportedChannels(3))
        );
    }

    #[test]
    fn hello_rejects_non_utf8_name() {
        let mut payload = Hello {
            rate_hz: 16_000,
            channels: 1,
            device_name: "n".into(),
        }
        .encode_payload();
        payload[8] = 0xFF;
        assert_eq!(
            Hello::decode_payload(&payload),
            Err(ProtocolError::DeviceNameNotUtf8)
        );
    }

    #[test]
    fn pcm_packet_round_trips_at_full_size() {
        let samples: Vec<i16> = (0..640).map(|i| (i as i16) - 320).collect();
        let mut buffer = Vec::new();
        encode_pcm_packet(&header(), &samples, &mut buffer);
        assert_eq!(buffer.len(), HEADER_LEN + MAX_PCM_PAYLOAD_BYTES);

        let decoded = Header::decode(&buffer).expect("header");
        assert_eq!(decoded.kind, Kind::Pcm);
        let mut out = Vec::new();
        let frames = validate_pcm_payload(&buffer[HEADER_LEN..], 1).expect("validate");
        assert_eq!(frames, 640);
        decode_pcm(&buffer[HEADER_LEN..], &mut out).expect("decode");
        assert_eq!(out, samples);
    }

    #[test]
    fn pcm_payload_over_the_limit_is_rejected() {
        let payload = vec![0u8; MAX_PCM_PAYLOAD_BYTES + 2];
        assert!(matches!(
            validate_pcm_payload(&payload, 1),
            Err(ProtocolError::PayloadTooLarge(_))
        ));
    }

    #[test]
    fn pcm_payload_must_be_whole_frames() {
        let payload = vec![0u8; 6];
        assert_eq!(
            validate_pcm_payload(&payload, 2),
            Err(ProtocolError::UnalignedPayload {
                bytes: 6,
                block_align: 4
            })
        );
    }

    #[test]
    fn an_empty_pcm_payload_is_harmless() {
        assert_eq!(validate_pcm_payload(&[], 1), Ok(0));
    }
}
