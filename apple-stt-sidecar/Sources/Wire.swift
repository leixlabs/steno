// Wire: the only two edges this program has.
//
//   in  : [u32 LE length][i64 LE utc_ms][s16le mono 16 kHz PCM]
//   out : one JSON object per line
//
// Both directions are byte-level concerns and nothing else: no policy, no
// buffering decisions, no retries. The framing is owned by the Rust side
// (`crates/server/src/stt_stream.rs`), so it is spelled out here once and never
// interpreted.

import Foundation

/// Where diagnostics go. LaunchServices swallows stderr, so a file copy is the
/// only way to see why a headless run did nothing.
let logPath: String =
    ProcessInfo.processInfo.environment["STENO_STT_LOG"] ?? "/tmp/steno-stt-sidecar.log"

private let logLock = NSLock()

func log(_ message: String) {
    let line = "steno-stt-sidecar[\(getpid())]: \(message)\n"
    guard let data = line.data(using: .utf8) else { return }
    logLock.lock()
    defer { logLock.unlock() }
    FileHandle.standardError.write(data)
    if let handle = FileHandle(forWritingAtPath: logPath) {
        handle.seekToEndOfFile()
        handle.write(data)
        try? handle.close()
    } else {
        try? data.write(to: URL(fileURLWithPath: logPath))
    }
}

/// One block of audio as it arrived: samples plus the wall clock of the first
/// one. The clock is the recorder's, not ours — this process never invents a
/// timestamp.
struct AudioFrame {
    let samples: [Int16]
    let utcMs: Int64
}

enum WireError: Error, CustomStringConvertible {
    case closed
    case malformed(String)

    var description: String {
        switch self {
        case .closed: return "socket closed"
        case .malformed(let detail): return "malformed frame: \(detail)"
        }
    }
}

/// Reads frames off a connected socket. Blocking by design: one dedicated
/// thread owns the read side, so a slow analyser can never stall the socket.
struct FrameReader {
    private let fd: Int32

    init(fd: Int32) { self.fd = fd }

    mutating func next() throws -> AudioFrame? {
        guard let header = readExactly(12) else { return nil }
        let length = header.withUnsafeBytes { $0.loadUnaligned(fromByteOffset: 0, as: UInt32.self) }
        let utcMs = header.withUnsafeBytes { $0.loadUnaligned(fromByteOffset: 4, as: Int64.self) }
        let bytes = Int(UInt32(littleEndian: length))
        guard bytes > 0, bytes < (1 << 20), bytes % 2 == 0 else {
            throw WireError.malformed("length \(bytes)")
        }
        guard let payload = readExactly(bytes) else { throw WireError.closed }
        var samples = [Int16](repeating: 0, count: bytes / 2)
        payload.withUnsafeBytes { raw in
            for index in 0..<samples.count {
                samples[index] = Int16(bitPattern: UInt16(
                    littleEndian: raw.loadUnaligned(fromByteOffset: index * 2, as: UInt16.self)))
            }
        }
        return AudioFrame(samples: samples, utcMs: Int64(littleEndian: utcMs))
    }

    /// Nil means the peer closed the connection mid-read: the session is over,
    /// not broken.
    private func readExactly(_ count: Int) -> [UInt8]? {
        var buffer = [UInt8](repeating: 0, count: count)
        var filled = 0
        while filled < count {
            let read = buffer.withUnsafeMutableBytes { pointer -> Int in
                guard let base = pointer.baseAddress else { return 0 }
                return Darwin.read(fd, base.advanced(by: filled), count - filled)
            }
            if read == 0 { return nil }
            if read < 0 {
                if errno == EINTR { continue }
                return nil
            }
            filled += read
        }
        return buffer
    }
}

/// Writes JSON lines. Serialised because the results task and the audio thread
/// can both have something to say.
final class EventWriter {
    private let fd: Int32
    private let lock = NSLock()
    private let encoder: JSONEncoder

    init(fd: Int32) {
        self.fd = fd
        let encoder = JSONEncoder()
        encoder.outputFormatting = [.withoutEscapingSlashes]
        self.encoder = encoder
    }

    /// Returns false when the peer is gone; the caller then stops the session
    /// rather than writing into a broken pipe for the rest of the night.
    @discardableResult
    func send(_ event: Event) -> Bool {
        guard let data = try? encoder.encode(event) else {
            log("cannot encode event \(event.type)")
            return true
        }
        var line = [UInt8](data)
        line.append(0x0a)
        lock.lock()
        defer { lock.unlock() }
        let ok = write(bytes: line)
        if !ok { log("peer is gone (server stopped); closing after \(event.type)") }
        return ok
    }

    private func write(bytes data: [UInt8]) -> Bool {
        var written = 0
        while written < data.count {
            let count = data.withUnsafeBytes { pointer -> Int in
                guard let base = pointer.baseAddress else { return 0 }
                return Darwin.write(fd, base.advanced(by: written), data.count - written)
            }
            if count <= 0 {
                if errno == EINTR { continue }
                return false
            }
            written += count
        }
        return true
    }
}

/// One line on the wire. Field names are the contract with `HelperEvent` in
/// `stt_stream.rs`; unknown fields there are ignored, missing ones become
/// `None`, which is exactly what a partial or a state line needs.
struct Event: Encodable {
    let type: String
    var text: String?
    var start_ms: Int?
    var end_ms: Int?
    var segments: [Segment]?
    var engine: String?
    var language: String?
    var message: String?
    /// Only on `ready`, so the server can log which locale is really running.
    var locale: String?

    static func ready(engine: String, locale: String) -> Event {
        Event(type: "ready", engine: engine, language: nil, locale: locale)
    }

    static func partial(_ text: String) -> Event {
        Event(type: "partial", text: text)
    }

    static func final(text: String, startMs: Int, endMs: Int, engine: String, language: String) -> Event {
        Event(
            type: "final",
            text: text,
            start_ms: startMs,
            end_ms: endMs,
            segments: [],
            engine: engine,
            language: language
        )
    }

    static func error(_ message: String) -> Event {
        Event(type: "error", message: message)
    }
}

/// Per-word timing. `SpeechAnalyzer` hands back an attributed string whose runs
/// carry `audioTimeRange`; when a caller asked for it we can fill this in, and
/// when it did not the array stays empty rather than guessed.
struct Segment: Encodable {
    let start_ms: Int
    let end_ms: Int
    let text: String
    let confidence: Float?
}
