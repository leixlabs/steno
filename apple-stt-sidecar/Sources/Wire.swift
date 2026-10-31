// Wire: the byte-level contract with the Rust side. No policy lives here.
//
// ```text
// offset size field
//      0    2 magic       0x5352 ("SR")
//      2    1 direction   0x00 Swift -> Rust, 0x01 Rust -> Swift
//      3    1 msg type
//      4    4 payload len  u32, big-endian
//      8    N payload
// ```
//
// The header is fixed at 8 bytes and the length is explicit, so a byte stream
// that arrives split across `read()` calls (half a packet) or glued together
// (three packets in one read) still decodes to exact frames: the reader asks for
// exactly 8 then exactly `len` bytes and can never consume the next frame's
// header as payload.
//
// The header has no room for the recorder's clock, so the *audio* payload
// carries it:
//
// ```text
// audio  payload = [i64 BE utc_ms of the first sample][s16le mono 16 kHz PCM]
// snore  payload = JSON  SnorePayload
// text   payload = JSON  TranscriptPayload      (final or partial)
// state  payload = JSON  SessionPayload
// R->S control   = JSON  ControlPayload
// R->S config    = JSON  ConfigPayload
// ```
//
// `utc_ms` is the reason this is a payload field rather than a side channel:
// timestamps are the recorder's, and this process never invents one.

import Foundation

/// Which way a frame is travelling. Checked on receipt: an uplink frame arriving
/// on the downlink is a protocol error, not something to shrug at.
enum Direction: UInt8 {
    case swiftToRust = 0x00
    case rustToSwift = 0x01
}

enum MsgType: UInt8 {
    case audio = 0x01
    case snore = 0x02
    case transcript = 0x03
    case session = 0x04
    case control = 0x81
    case config = 0x82
}

enum WireError: Error, CustomStringConvertible {
    case closed
    case badMagic(UInt16)
    case badDirection(UInt8)
    case unknownType(UInt8)
    case payloadTooLarge(UInt32)
    case truncated(needed: Int, got: Int)
    case emptyAudio

    var description: String {
        switch self {
        case .closed: return "peer closed the stream"
        case .badMagic(let value): return String(format: "bad magic 0x%04X, expected 0x%04X", value, Wire.magic)
        case .badDirection(let value): return String(format: "unexpected direction 0x%02X", value)
        case .unknownType(let value): return String(format: "unknown message type 0x%02X", value)
        case .payloadTooLarge(let value): return "payload of \(value) bytes exceeds \(Wire.maxPayload)"
        case .truncated(let needed, let got): return "truncated payload: need \(needed) bytes, got \(got)"
        case .emptyAudio: return "audio payload carries no samples"
        }
    }
}

enum Wire {
    static let magic: UInt16 = 0x5352
    static let headerBytes = 8
    /// 20 ms of 16 kHz mono s16 is 640 bytes; a whole second is 32 000. The cap
    /// is deliberately far above both so a legitimate burst is never refused,
    /// and low enough that a corrupt length cannot ask for a gigabyte.
    static let maxPayload: UInt32 = 1 << 20

    static func header(direction: Direction, type: MsgType, length: Int) -> [UInt8] {
        var bytes = [UInt8](repeating: 0, count: headerBytes)
        bytes[0] = UInt8(magic >> 8)
        bytes[1] = UInt8(magic & 0xff)
        bytes[2] = direction.rawValue
        bytes[3] = type.rawValue
        let value = UInt32(length)
        bytes[4] = UInt8((value >> 24) & 0xff)
        bytes[5] = UInt8((value >> 16) & 0xff)
        bytes[6] = UInt8((value >> 8) & 0xff)
        bytes[7] = UInt8(value & 0xff)
        return bytes
    }

    static func parseHeader(_ bytes: [UInt8]) throws -> (Direction, MsgType, Int) {
        guard bytes.count == headerBytes else {
            throw WireError.truncated(needed: headerBytes, got: bytes.count)
        }
        let magic = UInt16(bytes[0]) << 8 | UInt16(bytes[1])
        guard magic == Wire.magic else { throw WireError.badMagic(magic) }
        guard let direction = Direction(rawValue: bytes[2]) else {
            throw WireError.badDirection(bytes[2])
        }
        guard let type = MsgType(rawValue: bytes[3]) else {
            throw WireError.unknownType(bytes[3])
        }
        let length = UInt32(bytes[4]) << 24 | UInt32(bytes[5]) << 16 | UInt32(bytes[6]) << 8 | UInt32(bytes[7])
        guard length <= maxPayload else { throw WireError.payloadTooLarge(length) }
        return (direction, type, Int(length))
    }
}

/// One decoded frame, still as bytes: turning a payload into a message is the
/// caller's job, because what a payload means depends on who sent it.
struct Frame {
    let direction: Direction
    let type: MsgType
    let payload: [UInt8]
}

/// A full-duplex byte channel over two file descriptors (a connected Unix
/// socket, or stdin/stdout under `--stdio`).
///
/// Reads block: one dedicated thread owns the read direction, which is what
/// makes a slow analyser unable to stall the wire. Writes take a lock because
/// the results task and the audio thread can both have something to say.
final class FrameChannel {
    private let fdIn: Int32
    private let fdOut: Int32
    private let writeLock = NSLock()
    private(set) var bytesIn = 0
    private(set) var framesIn = 0

    init(fdIn: Int32, fdOut: Int32) {
        self.fdIn = fdIn
        self.fdOut = fdOut
    }

    /// Nil means the peer closed between frames: the session is over, not broken.
    func next() throws -> Frame? {
        guard let header = readExactly(Wire.headerBytes) else { return nil }
        let (direction, type, length) = try Wire.parseHeader(header)
        var payload: [UInt8] = []
        if length > 0 {
            guard let body = readExactly(length) else { throw WireError.closed }
            payload = body
        }
        bytesIn += Wire.headerBytes + length
        framesIn += 1
        return Frame(direction: direction, type: type, payload: payload)
    }

    /// Ends the read direction without touching writes.
    ///
    /// `control: stop` wants the session to drain and still be able to send its
    /// last finals, so the reader is unblocked (`read` returns 0) while the
    /// write half stays open.
    func shutdownRead() {
        _ = Darwin.shutdown(fdIn, SHUT_RD)
    }

    /// Returns false when the peer is gone; the caller then ends the session
    /// rather than writing into a dead pipe for the rest of the night.
    @discardableResult
    func send(type: MsgType, payload: [UInt8]) -> Bool {
        var bytes = Wire.header(direction: .swiftToRust, type: type, length: payload.count)
        bytes.append(contentsOf: payload)
        writeLock.lock()
        defer { writeLock.unlock() }
        return write(bytes: bytes)
    }

    private func write(bytes data: [UInt8]) -> Bool {
        var written = 0
        while written < data.count {
            let count = data.withUnsafeBytes { pointer -> Int in
                guard let base = pointer.baseAddress else { return 0 }
                return Darwin.write(fdOut, base.advanced(by: written), data.count - written)
            }
            if count <= 0 {
                if errno == EINTR { continue }
                return false
            }
            written += count
        }
        return true
    }

    private func readExactly(_ count: Int) -> [UInt8]? {
        var buffer = [UInt8](repeating: 0, count: count)
        var filled = 0
        while filled < count {
            let read = buffer.withUnsafeMutableBytes { pointer -> Int in
                guard let base = pointer.baseAddress else { return 0 }
                return Darwin.read(fdIn, base.advanced(by: filled), count - filled)
            }
            if read == 0 { return nil }  // clean EOF, even mid-frame
            if read < 0 {
                if errno == EINTR { continue }
                return nil
            }
            filled += read
        }
        return buffer
    }
}

// MARK: - payload codecs

enum AudioPayload {
    static let headerBytes = 8

    /// `[i64 BE utc_ms][s16le …]`. Big-endian to match the frame header: one
    /// endianness on the wire is one fewer thing to get wrong.
    static func encode(samples: [Int16], utcMs: Int64) -> [UInt8] {
        var bytes = [UInt8](repeating: 0, count: headerBytes + samples.count * 2)
        let value = UInt64(bitPattern: utcMs)
        for index in 0..<8 {
            bytes[index] = UInt8((value >> (56 - index * 8)) & 0xff)
        }
        var offset = headerBytes
        for sample in samples {
            let bits = UInt16(bitPattern: sample)
            bytes[offset] = UInt8(bits & 0xff)
            bytes[offset + 1] = UInt8(bits >> 8)
            offset += 2
        }
        return bytes
    }

    static func decode(_ payload: [UInt8]) throws -> (samples: [Int16], utcMs: Int64) {
        guard payload.count > headerBytes else {
            throw WireError.truncated(needed: headerBytes + 2, got: payload.count)
        }
        var value: UInt64 = 0
        for index in 0..<8 {
            value = value << 8 | UInt64(payload[index])
        }
        let body = payload[headerBytes...]
        guard body.count % 2 == 0 else {
            throw WireError.truncated(needed: body.count + 1, got: body.count)
        }
        var samples = [Int16](repeating: 0, count: body.count / 2)
        var offset = body.startIndex
        for index in 0..<samples.count {
            samples[index] = Int16(bitPattern: UInt16(body[offset]) | UInt16(body[offset + 1]) << 8)
            offset += 2
        }
        guard !samples.isEmpty else { throw WireError.emptyAudio }
        return (samples, Int64(bitPattern: value))
    }
}

// MARK: - messages
//
// Key names are the contract with `crates/server/src/sidecar_wire.rs`. Both
// sides spell them in snake_case on the wire.

struct SnorePayload: Encodable {
    let startMs: Int64
    let endMs: Int64
    let label: String
    let peak: Float
    let mean: Float
    let windows: Int
    let threshold: Float
    /// `"normal"` when the episode ended on its own, `"shutdown"` when the
    /// session stopped mid-episode — the audio exists either way, so the row is
    /// written either way, but the two must be distinguishable.
    let endReason: String
}

struct TranscriptPayload: Encodable {
    /// `"final"` or `"partial"`.
    let kind: String
    let text: String
    let startMs: Int64?
    let endMs: Int64?
    let engine: String?
    let language: String?
    let segments: [SegmentPayload]?

    static func final(text: String, startMs: Int64, endMs: Int64, engine: String, language: String) -> TranscriptPayload {
        TranscriptPayload(
            kind: "final", text: text, startMs: startMs, endMs: max(endMs, startMs),
            engine: engine, language: language, segments: []
        )
    }

    static func partial(_ text: String) -> TranscriptPayload {
        TranscriptPayload(
            kind: "partial", text: text, startMs: nil, endMs: nil,
            engine: nil, language: nil, segments: nil
        )
    }
}

struct SegmentPayload: Encodable {
    let startMs: Int64
    let endMs: Int64
    let text: String
}

struct SessionPayload: Encodable {
    /// `"ready"`, `"error"` or `"stopped"`.
    let state: String
    let engine: String?
    let language: String?
    let locale: String?
    /// What the snore lane is actually doing: `"apple-soundanalysis-v1"`,
    /// `"model:<name>"` or `null` when it is off. A string, not a bool, so the
    /// transcript history can say which detector produced an annotation.
    let snore: String?
    let message: String?
}

struct ConfigPayload: Decodable {
    let locale: String?
    let preset: String?
    let snore: Bool?
    let snoreThreshold: Float?
    let snoreWindow: Double?
}

struct ControlPayload: Decodable {
    /// `"stop"`, `"flush"` or `"status"`.
    let action: String
}

enum JSONPayload {
    static let encoder: JSONEncoder = {
        let encoder = JSONEncoder()
        encoder.outputFormatting = [.withoutEscapingSlashes, .sortedKeys]
        encoder.keyEncodingStrategy = .convertToSnakeCase
        return encoder
    }()

    static let decoder: JSONDecoder = {
        let decoder = JSONDecoder()
        decoder.keyDecodingStrategy = .convertFromSnakeCase
        return decoder
    }()

    static func encode<T: Encodable>(_ value: T) -> [UInt8]? {
        guard let data = try? encoder.encode(value) else { return nil }
        return [UInt8](data)
    }

    static func decode<T: Decodable>(_ type: T.Type, from payload: [UInt8]) throws -> T {
        try decoder.decode(type, from: Data(payload))
    }
}
