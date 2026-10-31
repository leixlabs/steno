//! Sidecar ↔ server framing, both directions.
//!
//! One connection to `steno-stt-sidecar`, carrying audio up and commands down.
//! The layout is fixed-width header plus explicit length, so a stream that has
//! been split by the kernel (half a frame) or coalesced (three frames in one
//! `read`) still decodes to exact frames — the reader asks for 8 bytes, then
//! exactly `len` more, and can never mistake the next frame's header for payload.
//!
//! ```text
//! offset size field
//!      0    2 magic      0x5352 ("SR")
//!      2    1 direction  0x00 sidecar -> server, 0x01 server -> sidecar
//!      3    1 msg type
//!      4    4 payload    u32, big-endian
//!      8    N payload
//! ```
//!
//! The header has no room for a clock, so the audio payload carries one:
//!
//! ```text
//! audio  = [i64 BE utc_ms of the first sample][s16le mono 16 kHz PCM]
//! snore  = JSON, a finished episode from the SoundAnalysis lane
//! text   = JSON, a final or a partial recogniser result
//! state  = JSON, ready / stopped / error / status / degraded
//! control (down) = JSON, {"action":"stop"|"flush"|"status"}
//! config  (down) = JSON, locale / preset / snore lane settings
//! ```
//!
//! `utc_ms` becomes a payload field rather than a separate channel because it is
//! the only thing that makes a result placeable: the recogniser and the
//! classifier both work on their own zero-based audio timeline, and the recorder
//! is the only party that knows what time it was. It is never derived here.
//!
//! ## Why the direction byte
//!
//! Both ends can write, so a frame that arrives pointing the wrong way is a
//! framing error (a desynchronised stream, or a version mismatch) rather than
//! something to guess at. Each side hands [`read_frame`] the direction it
//! expects and a mismatch ends the session with a named error.

use std::fmt;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// `"SR"`, big-endian.
pub const MAGIC: u16 = 0x5352;
pub const HEADER_LEN: usize = 8;
/// Audio payload prefix: the wall clock of the first sample.
pub const AUDIO_PREFIX_LEN: usize = 8;
/// 20 ms of 16 kHz mono s16 is 640 bytes. The cap is far above a legitimate
/// block and low enough that a corrupt length cannot ask for a gigabyte.
pub const MAX_PAYLOAD: usize = 1 << 20;

pub const TYPE_AUDIO: u8 = 0x01;
pub const TYPE_SNORE: u8 = 0x02;
pub const TYPE_TRANSCRIPT: u8 = 0x03;
pub const TYPE_SESSION: u8 = 0x04;
pub const TYPE_CONTROL: u8 = 0x81;
pub const TYPE_CONFIG: u8 = 0x82;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    SidecarToServer,
    ServerToSidecar,
}

impl Direction {
    fn to_u8(self) -> u8 {
        match self {
            Direction::SidecarToServer => 0x00,
            Direction::ServerToSidecar => 0x01,
        }
    }

    fn from_u8(value: u8) -> Result<Self, WireError> {
        match value {
            0x00 => Ok(Direction::SidecarToServer),
            0x01 => Ok(Direction::ServerToSidecar),
            other => Err(WireError::BadDirection(other)),
        }
    }
}

impl fmt::Display for Direction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Direction::SidecarToServer => f.write_str("sidecar→server"),
            Direction::ServerToSidecar => f.write_str("server→sidecar"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MsgType {
    Audio,
    Snore,
    Transcript,
    Session,
    Control,
    Config,
}

impl MsgType {
    fn to_u8(self) -> u8 {
        match self {
            MsgType::Audio => TYPE_AUDIO,
            MsgType::Snore => TYPE_SNORE,
            MsgType::Transcript => TYPE_TRANSCRIPT,
            MsgType::Session => TYPE_SESSION,
            MsgType::Control => TYPE_CONTROL,
            MsgType::Config => TYPE_CONFIG,
        }
    }

    fn from_u8(value: u8) -> Result<Self, WireError> {
        match value {
            TYPE_AUDIO => Ok(MsgType::Audio),
            TYPE_SNORE => Ok(MsgType::Snore),
            TYPE_TRANSCRIPT => Ok(MsgType::Transcript),
            TYPE_SESSION => Ok(MsgType::Session),
            TYPE_CONTROL => Ok(MsgType::Control),
            TYPE_CONFIG => Ok(MsgType::Config),
            other => Err(WireError::UnknownType(other)),
        }
    }
}

impl fmt::Display for MsgType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            MsgType::Audio => "audio",
            MsgType::Snore => "snore",
            MsgType::Transcript => "transcript",
            MsgType::Session => "session",
            MsgType::Control => "control",
            MsgType::Config => "config",
        };
        f.write_str(name)
    }
}

#[derive(Debug)]
pub enum WireError {
    /// The peer closed cleanly between frames. Not an error, but the caller has
    /// to stop reading.
    Closed,
    BadMagic(u16),
    BadDirection(u8),
    UnexpectedDirection {
        expected: Direction,
        got: Direction,
    },
    UnknownType(u8),
    PayloadTooLarge(usize),
    /// An audio payload without its 8-byte clock prefix, or one whose PCM is not
    /// a whole number of samples.
    MalformedAudio(String),
    Json(serde_json::Error),
    Io(std::io::Error),
}

impl fmt::Display for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WireError::Closed => f.write_str("sidecar closed the stream"),
            WireError::BadMagic(value) => {
                write!(f, "bad magic 0x{value:04X}, expected 0x{MAGIC:04X}")
            }
            WireError::BadDirection(value) => write!(f, "unknown direction 0x{value:02X}"),
            WireError::UnexpectedDirection { expected, got } => {
                write!(f, "frame is {got} but {expected} was expected")
            }
            WireError::UnknownType(value) => write!(f, "unknown message type 0x{value:02X}"),
            WireError::PayloadTooLarge(len) => {
                write!(f, "payload of {len} bytes exceeds the {MAX_PAYLOAD} byte limit")
            }
            WireError::MalformedAudio(detail) => write!(f, "malformed audio payload: {detail}"),
            WireError::Json(error) => write!(f, "unparsable payload: {error}"),
            WireError::Io(error) => write!(f, "socket error: {error}"),
        }
    }
}

impl std::error::Error for WireError {}

impl From<std::io::Error> for WireError {
    fn from(error: std::io::Error) -> Self {
        WireError::Io(error)
    }
}

impl From<serde_json::Error> for WireError {
    fn from(error: serde_json::Error) -> Self {
        WireError::Json(error)
    }
}

/// Header codec. Kept separate from the I/O so a test can assert the bytes
/// without a socket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub direction: Direction,
    pub kind: MsgType,
    pub length: usize,
}

impl Header {
    pub fn new(direction: Direction, kind: MsgType, length: usize) -> Self {
        Header {
            direction,
            kind,
            length,
        }
    }

    pub fn encode(&self) -> [u8; HEADER_LEN] {
        let mut buf = [0u8; HEADER_LEN];
        buf[0..2].copy_from_slice(&MAGIC.to_be_bytes());
        buf[2] = self.direction.to_u8();
        buf[3] = self.kind.to_u8();
        let len = u32::try_from(self.length).unwrap_or(u32::MAX);
        buf[4..8].copy_from_slice(&len.to_be_bytes());
        buf
    }

    pub fn decode(buf: &[u8]) -> Result<Self, WireError> {
        // Only reachable if a caller passes a short slice; `read_frame` always
        // hands over exactly `HEADER_LEN` bytes.
        let bytes: [u8; HEADER_LEN] = buf
            .try_into()
            .map_err(|_| WireError::MalformedAudio(format!("header is {} bytes", buf.len())))?;
        let magic = u16::from_be_bytes([bytes[0], bytes[1]]);
        if magic != MAGIC {
            return Err(WireError::BadMagic(magic));
        }
        let direction = Direction::from_u8(bytes[2])?;
        let kind = MsgType::from_u8(bytes[3])?;
        let length = u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]) as usize;
        if length > MAX_PAYLOAD {
            return Err(WireError::PayloadTooLarge(length));
        }
        Ok(Header {
            direction,
            kind,
            length,
        })
    }
}

/// One decoded frame, still as bytes: what a payload *means* is the caller's
/// business.
#[derive(Debug)]
pub struct Frame {
    pub kind: MsgType,
    pub payload: Vec<u8>,
}

/// Reads one frame, validating the direction.
///
/// `Ok(None)` means the peer closed between frames. The first byte is read on its
/// own so that a clean EOF is distinguishable from a frame cut in half: the
/// former is a session ending, the latter is a bug worth reporting.
pub async fn read_frame<R>(reader: &mut R, expect: Direction) -> Result<Option<Frame>, WireError>
where
    R: AsyncRead + Unpin,
{
    let mut header = [0u8; HEADER_LEN];
    let mut first = [0u8; 1];
    loop {
        match reader.read(&mut first).await {
            Ok(0) => return Ok(None),
            Ok(_) => break,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(WireError::Io(error)),
        }
    }
    header[0] = first[0];
    reader.read_exact(&mut header[1..]).await?;
    let header = Header::decode(&header)?;
    if header.direction != expect {
        return Err(WireError::UnexpectedDirection {
            expected: expect,
            got: header.direction,
        });
    }
    let mut payload = vec![0u8; header.length];
    if header.length > 0 {
        reader.read_exact(&mut payload).await?;
    }
    Ok(Some(Frame {
        kind: header.kind,
        payload,
    }))
}

/// Writes one frame. The caller owns any timeout: this is the bare write.
pub async fn write_frame<W>(
    writer: &mut W,
    kind: MsgType,
    payload: &[u8],
) -> Result<(), WireError>
where
    W: AsyncWrite + Unpin,
{
    if payload.len() > MAX_PAYLOAD {
        return Err(WireError::PayloadTooLarge(payload.len()));
    }
    let header = Header::new(Direction::ServerToSidecar, kind, payload.len());
    writer.write_all(&header.encode()).await?;
    writer.write_all(payload).await?;
    writer.flush().await?;
    Ok(())
}

// MARK: payload codecs

/// Splits an audio payload into its wall clock and its PCM bytes.
pub fn split_audio(payload: &[u8]) -> Result<(i64, &[u8]), WireError> {
    if payload.len() <= AUDIO_PREFIX_LEN {
        return Err(WireError::MalformedAudio(format!(
            "{} bytes is shorter than the {AUDIO_PREFIX_LEN}-byte clock prefix plus one sample",
            payload.len()
        )));
    }
    let pcm = &payload[AUDIO_PREFIX_LEN..];
    if !pcm.len().is_multiple_of(2) {
        return Err(WireError::MalformedAudio(format!(
            "{} PCM bytes is not a whole number of s16 samples",
            pcm.len()
        )));
    }
    let mut clock = [0u8; AUDIO_PREFIX_LEN];
    clock.copy_from_slice(&payload[..AUDIO_PREFIX_LEN]);
    Ok((i64::from_be_bytes(clock), pcm))
}

/// Decodes little-endian s16 PCM into `out`, replacing its contents.
pub fn decode_pcm(pcm: &[u8], out: &mut Vec<i16>) {
    out.clear();
    out.reserve(pcm.len() / 2);
    for pair in pcm.as_chunks::<2>().0 {
        out.push(i16::from_le_bytes(*pair));
    }
}

/// Encodes a block the way the sidecar expects it: clock first, then PCM.
pub fn encode_audio(utc_ms: i64, samples: &[i16], out: &mut Vec<u8>) {
    out.clear();
    out.reserve(AUDIO_PREFIX_LEN + samples.len() * 2);
    out.extend_from_slice(&utc_ms.to_be_bytes());
    for sample in samples {
        out.extend_from_slice(&sample.to_le_bytes());
    }
}

// MARK: messages

/// A finished snore episode from the sidecar's `SoundAnalysis` lane.
///
/// `windows` and `peak` are what the classifier said about the windows that made
/// up the episode; they are carried verbatim so the row can be read later
/// without re-deriving anything. `end_reason` distinguishes an episode that
/// closed on quiet audio from one the session cut short.
#[derive(Debug, Clone, Deserialize)]
pub struct SnoreEvent {
    pub start_ms: i64,
    pub end_ms: i64,
    pub label: String,
    pub peak: f32,
    pub mean: f32,
    pub windows: u32,
    pub threshold: f32,
    #[serde(default = "default_end_reason")]
    pub end_reason: String,
}

fn default_end_reason() -> String {
    "normal".to_string()
}

impl SnoreEvent {
    /// A row needs a positive span; anything else is a protocol error, not a
    /// one-millisecond event.
    pub fn is_usable(&self) -> bool {
        self.end_ms > self.start_ms && !self.label.trim().is_empty()
    }
}

/// A recogniser result: a final (with wall-clock bounds) or a volatile partial.
#[derive(Debug, Clone, Deserialize)]
pub struct TranscriptEvent {
    pub kind: String,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub start_ms: Option<i64>,
    #[serde(default)]
    pub end_ms: Option<i64>,
    #[serde(default)]
    pub engine: Option<String>,
    #[serde(default)]
    pub language: Option<String>,
    #[serde(default)]
    pub segments: Option<serde_json::Value>,
}

/// A lifecycle message: `ready`, `stopped`, `error`, `status` or `degraded`.
#[derive(Debug, Clone, Deserialize)]
pub struct SessionEvent {
    pub state: String,
    #[serde(default)]
    pub engine: Option<String>,
    #[serde(default)]
    pub language: Option<String>,
    #[serde(default)]
    pub locale: Option<String>,
    /// Which snore lane is running, or absent when it is off.
    #[serde(default)]
    pub snore: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
}

/// Downlink command.
#[derive(Debug, Clone, Serialize)]
pub struct Control {
    pub action: String,
}

impl Control {
    pub fn stop() -> Self {
        Control {
            action: "stop".to_string(),
        }
    }
}

/// Downlink session configuration, sent once before the first audio block.
///
/// Everything is optional so a field the server has no opinion about is simply
/// absent, and the sidecar keeps whatever its own defaults were — that is how a
/// local fact like a CoreML model path survives, since the server has no
/// business knowing it.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Config {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub locale: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preset: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snore: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snore_threshold: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snore_window: Option<f64>,
}

impl Config {
    /// The configuration this server actually knows how to state.
    pub fn for_stream(locale: &str) -> Self {
        Config {
            locale: Some(locale.to_string()),
            preset: None,
            snore: None,
            snore_threshold: None,
            snore_window: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::duplex;

    #[test]
    fn header_round_trips_field_by_field() {
        let header = Header::new(Direction::ServerToSidecar, MsgType::Config, 1234);
        let bytes = header.encode();
        assert_eq!(&bytes[0..2], &[0x53, 0x52]);
        assert_eq!(bytes[2], 0x01);
        assert_eq!(bytes[3], 0x82);
        assert_eq!(u32::from_be_bytes(bytes[4..8].try_into().unwrap()), 1234);
        assert_eq!(Header::decode(&bytes).expect("decode"), header);
    }

    #[test]
    fn header_rejects_a_bad_magic_version_or_type() {
        let mut bytes = Header::new(Direction::SidecarToServer, MsgType::Audio, 0).encode();
        bytes[0] = 0x00;
        assert!(matches!(Header::decode(&bytes), Err(WireError::BadMagic(_))));

        let mut bytes = Header::new(Direction::SidecarToServer, MsgType::Audio, 0).encode();
        bytes[3] = 0x42;
        assert!(matches!(
            Header::decode(&bytes),
            Err(WireError::UnknownType(0x42))
        ));

        let mut bytes = Header::new(Direction::SidecarToServer, MsgType::Audio, 0).encode();
        bytes[2] = 0x02;
        assert!(matches!(
            Header::decode(&bytes),
            Err(WireError::BadDirection(0x02))
        ));
    }

    #[test]
    fn a_payload_over_the_limit_is_refused_before_reading_it() {
        let mut bytes = Header::new(Direction::SidecarToServer, MsgType::Audio, 0).encode();
        bytes[4..8].copy_from_slice(&(MAX_PAYLOAD as u32 + 1).to_be_bytes());
        assert!(matches!(
            Header::decode(&bytes),
            Err(WireError::PayloadTooLarge(_))
        ));
    }

    #[tokio::test]
    async fn frames_survive_being_split_and_coalesced() {
        // One writer, one reader, and a duplex buffer small enough to force the
        // kernel-equivalent to deliver partial frames.
        let (mut client, mut server) = duplex(16);
        let audio = vec![7i16; 320];
        let mut payload = Vec::new();
        encode_audio(1_700_000_000_000, &audio, &mut payload);
        let expected = payload.clone();

        let writer = tokio::spawn(async move {
            for _ in 0..3 {
                let header = Header::new(Direction::SidecarToServer, MsgType::Audio, payload.len());
                client.write_all(&header.encode()).await.unwrap();
                client.write_all(&payload).await.unwrap();
            }
            client.shutdown().await.unwrap();
        });

        let mut decoded = Vec::new();
        while let Some(frame) = read_frame(&mut server, Direction::SidecarToServer)
            .await
            .expect("frame")
        {
            assert_eq!(frame.kind, MsgType::Audio);
            assert_eq!(frame.payload, expected);
            decoded.push(frame);
        }
        writer.await.unwrap();
        assert_eq!(decoded.len(), 3);
    }

    #[tokio::test]
    async fn a_clean_close_is_none_and_a_half_frame_is_an_error() {
        let (mut client, mut server) = duplex(64);
        let header = Header::new(Direction::SidecarToServer, MsgType::Session, 4).encode();
        client.write_all(&header[..5]).await.unwrap();
        drop(client);
        assert!(read_frame(&mut server, Direction::SidecarToServer).await.is_err());

        let (mut client, mut server) = duplex(64);
        client.shutdown().await.unwrap();
        assert!(
            read_frame(&mut server, Direction::SidecarToServer)
                .await
                .expect("clean close")
                .is_none()
        );
    }

    #[tokio::test]
    async fn a_frame_pointing_the_wrong_way_ends_the_session() {
        let (mut client, mut server) = duplex(64);
        let header = Header::new(Direction::ServerToSidecar, MsgType::Config, 0).encode();
        client.write_all(&header).await.unwrap();
        match read_frame(&mut server, Direction::SidecarToServer).await {
            Err(WireError::UnexpectedDirection { .. }) => {}
            other => panic!("expected a direction error, got {other:?}"),
        }
    }

    #[test]
    fn audio_payload_carries_the_clock_big_endian() {
        let mut payload = Vec::new();
        encode_audio(1_700_000_000_123, &[1, -2, 3], &mut payload);
        assert_eq!(payload.len(), AUDIO_PREFIX_LEN + 6);
        let (utc_ms, pcm) = split_audio(&payload).expect("split");
        assert_eq!(utc_ms, 1_700_000_000_123);
        let mut samples = Vec::new();
        decode_pcm(pcm, &mut samples);
        assert_eq!(samples, vec![1, -2, 3]);
    }

    #[test]
    fn a_short_or_odd_audio_payload_is_refused() {
        assert!(matches!(
            split_audio(&[0u8; 4]),
            Err(WireError::MalformedAudio(_))
        ));
        let mut payload = vec![0u8; AUDIO_PREFIX_LEN];
        payload.push(0);
        assert!(matches!(
            split_audio(&payload),
            Err(WireError::MalformedAudio(_))
        ));
    }

    #[test]
    fn a_snore_episode_without_a_span_is_not_usable() {
        let event: SnoreEvent = serde_json::from_str(
            r#"{"start_ms":1000,"end_ms":1000,"label":"snoring","peak":0.8,"mean":0.7,"windows":2,"threshold":0.5}"#,
        )
        .expect("json");
        assert!(!event.is_usable());
        assert_eq!(event.end_reason, "normal");

        let event: SnoreEvent = serde_json::from_str(
            r#"{"start_ms":1000,"end_ms":4200,"label":"snoring","peak":0.8,"mean":0.7,"windows":3,"threshold":0.5,"end_reason":"shutdown"}"#,
        )
        .expect("json");
        assert!(event.is_usable());
        assert_eq!(event.end_reason, "shutdown");
    }

    #[test]
    fn a_partial_transcript_needs_no_timestamps() {
        let event: TranscriptEvent =
            serde_json::from_str(r#"{"kind":"partial","text":"打呼"}"#).expect("json");
        assert_eq!(event.kind, "partial");
        assert!(event.start_ms.is_none());
        assert!(event.text == "打呼");
    }

    #[test]
    fn config_omits_what_the_server_has_no_opinion_about() {
        let json = serde_json::to_string(&Config::for_stream("zh-CN")).expect("json");
        assert_eq!(json, r#"{"locale":"zh-CN"}"#);
    }
}
