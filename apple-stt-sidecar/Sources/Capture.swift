// Capture: the three ways audio can enter this process.
//
// A microphone, a finished file, or a raw PCM pipe. All three end at
// `AudioHub.ingest`, which means the analysers cannot tell them apart — the same
// code path, the same conversion, the same timeline.
//
// ## What the clock means, per source
//
// Only the framed socket carries a recorder-stamped clock (`utc_ms` per block).
// Everything here has to answer the question itself, and answers it honestly
// rather than pretending to precision it does not have:
//
//   file      frame offset in the file, in ms from its first sample
//   stdin     arrival wall clock minus the block's duration
//   mic       capture wall clock minus the block's duration
//
// A file is not "now", and saying so with an arrival timestamp would put every
// phrase at 1970-adjacent noise instead of where it sits in the recording.

import AVFoundation
import Foundation

enum CaptureError: Error, CustomStringConvertible {
    case noSuchFile(String)
    case unreadable(String)
    case device(String)
    case microphoneDenied

    var description: String {
        switch self {
        case .noSuchFile(let path): return "no such file: \(path)"
        case .unreadable(let detail): return "cannot read the audio: \(detail)"
        case .device(let detail): return "no usable input device: \(detail)"
        case .microphoneDenied:
            return "microphone access was denied; grant it in System Settings → Privacy & Security → Microphone"
        }
    }
}

// MARK: - a finished file

enum FileSource {
    /// Feeds a file through the same hub the socket feeds, so a file transcript
    /// and a live one come out of identical code.
    static func run(path: String, hub: AudioHub, pace: Bool) throws {
        let url = URL(fileURLWithPath: path)
        guard FileManager.default.fileExists(atPath: url.path) else {
            throw CaptureError.noSuchFile(url.path)
        }
        let file: AVAudioFile
        do {
            file = try AVAudioFile(forReading: url, commonFormat: .pcmFormatInt16, interleaved: false)
        } catch {
            throw CaptureError.unreadable("\(url.lastPathComponent): \(error.localizedDescription)")
        }
        let format = file.processingFormat
        guard file.length > 0 else { throw CaptureError.unreadable("\(url.path) has no frames") }
        guard let buffer = AVAudioPCMBuffer(pcmFormat: format, frameCapacity: 4_096) else {
            throw CaptureError.unreadable("cannot allocate a read buffer for \(format)")
        }

        var cursor: AVAudioFramePosition = 0
        // Bounded by `file.length`: `read(into:)` past the end raises a generic
        // Objective-C error rather than returning zero frames, and a file that
        // ends is not a failure.
        while cursor < file.length {
            let remaining = AVAudioFrameCount(min(Int64(4_096), file.length - cursor))
            do {
                try file.read(into: buffer, frameCount: remaining)
            } catch {
                throw CaptureError.unreadable("read failed at frame \(cursor): \(error.localizedDescription)")
            }
            if buffer.frameLength == 0 { break }
            let utcMs = Int64((Double(cursor) / format.sampleRate * 1000).rounded())
            hub.ingest(buffer: buffer, utcMs: utcMs)
            cursor += AVAudioFramePosition(buffer.frameLength)
            // Faster than real time, but not flat out: the analyser is allowed
            // to be slower than a disk, and the queue behind it is unbounded by
            // design. 5 ms per 4096 frames is roughly 16× real time.
            if pace { Thread.sleep(forTimeInterval: 0.005) }
        }
        log("file: fed \(cursor) frames of \(format.sampleRate) Hz \(format.channelCount) ch from \(url.lastPathComponent)")
    }
}

// MARK: - raw PCM on stdin

enum RawStreamSource {
    /// Unframed PCM on stdin: the shape `ffmpeg … | steno-stt-sidecar --stream`
    /// produces. There is no header to steal a clock from, so each block is
    /// stamped when it arrives.
    static func run(
        fd: Int32,
        rate: Double,
        channels: AVAudioChannelCount,
        hub: AudioHub
    ) throws {
        guard rate > 0 else { throw CaptureError.device("sample rate must be positive") }
        guard let format = AVAudioFormat(
            commonFormat: .pcmFormatInt16,
            sampleRate: rate,
            channels: channels,
            interleaved: false
        ) else {
            throw CaptureError.device("cannot describe \(rate) Hz \(channels) ch s16")
        }
        // 20 ms, the same block the recorder sends.
        let frames = AVAudioFrameCount(rate / 50.0)
        let bytes = Int(frames) * Int(channels) * 2
        guard let buffer = AVAudioPCMBuffer(pcmFormat: format, frameCapacity: frames) else {
            throw CaptureError.device("cannot allocate a \(frames)-frame buffer")
        }
        var raw = [UInt8](repeating: 0, count: bytes)
        var total: Int64 = 0
        while true {
            let read = raw.withUnsafeMutableBytes { pointer -> Int in
                guard let base = pointer.baseAddress else { return -1 }
                return Darwin.read(fd, base, bytes)
            }
            if read == 0 { break }
            if read < 0 {
                if errno == EINTR { continue }
                throw CaptureError.unreadable("stdin read failed: \(String(cString: strerror(errno)))")
            }
            let wholeFrames = AVAudioFrameCount(read / (Int(channels) * 2))
            guard wholeFrames > 0, let channel = buffer.int16ChannelData?[0] else { continue }
            buffer.frameLength = wholeFrames
            raw.withUnsafeBytes { source in
                guard let base = source.baseAddress else { return }
                channel.withMemoryRebound(to: UInt8.self, capacity: Int(wholeFrames) * 2) { destination in
                    destination.update(from: base.assumingMemoryBound(to: UInt8.self), count: read)
                }
            }
            let nowMs = Int64((Date().timeIntervalSince1970 * 1000).rounded())
            let durationMs = Int64((Double(wholeFrames) / rate * 1000).rounded())
            hub.ingest(buffer: buffer, utcMs: nowMs - durationMs)
            total += Int64(wholeFrames)
        }
        log("stdin: \(total) frames of \(rate) Hz \(channels) ch")
    }
}

// MARK: - the microphone

/// Captures the default input device.
///
/// This is the one mode that needs a permission: `AVAudioEngine`'s input node is
/// a microphone, and macOS asks the user (and needs `NSMicrophoneUsageDescription`
/// in the bundle). `SpeechAnalyzer` never did, which is why the recorder path
/// runs without any prompt at all.
final class MicrophoneSource {
    private let hub: AudioHub
    private let engine = AVAudioEngine()

    init(hub: AudioHub) {
        self.hub = hub
    }

    /// Runs until `seconds` elapse (0 = until the process is interrupted).
    func run(seconds: Double) throws {
        let input = engine.inputNode
        let format = input.outputFormat(forBus: 0)
        guard format.sampleRate > 0, format.channelCount > 0 else {
            throw CaptureError.device("the default input reports \(format.sampleRate) Hz \(format.channelCount) ch")
        }
        log("mic: capturing \(format.sampleRate) Hz \(format.channelCount) ch → \(AudioHub.canonicalRate) Hz mono")

        input.installTap(onBus: 0, bufferSize: 4_096, format: format) { [hub] buffer, _ in
            // The tap's buffer is recycled when this returns, so the hub
            // converts (which copies) before we hand control back.
            let nowMs = Int64((Date().timeIntervalSince1970 * 1000).rounded())
            let durationMs = Int64((Double(buffer.frameLength) / format.sampleRate * 1000).rounded())
            hub.ingest(buffer: buffer, utcMs: nowMs - durationMs)
        }
        do {
            try engine.start()
        } catch {
            input.removeTap(onBus: 0)
            throw CaptureError.device(error.localizedDescription)
        }
        defer {
            engine.stop()
            input.removeTap(onBus: 0)
        }
        if seconds > 0 {
            Thread.sleep(forTimeInterval: seconds)
        } else {
            // Nothing else to do: the tap is the work. Sleeping here keeps the
            // call stack honest about who owns the engine.
            while true { Thread.sleep(forTimeInterval: 1) }
        }
    }
}
