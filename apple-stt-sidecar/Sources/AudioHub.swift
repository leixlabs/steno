// AudioHub: the distribution point. One canonical buffer in, N consumers out.
//
// Every input — a microphone, a framed socket, raw PCM on stdin, a finished file
// — narrows to the same thing before anything analyses it: 16 kHz, mono, signed
// 16-bit little-endian. Conversion happens exactly once, here, so the two
// consumers can never disagree about what they are looking at, and so a
// consumer's timestamp arithmetic is the same arithmetic for every source.
//
// ## Threading
//
// Producers (the socket reader thread, the AVAudioEngine tap, the file loop) call
// `ingest` and get control back immediately: the block is copied and handed to
// the hub's serial queue, where the timeline advances and the fan-out happens.
// Each consumer then takes over on its own queue. Nothing a consumer does can
// slow the producer, and a block is seen by every consumer even if one of them
// is busy.
//
// A capture tap's buffer is only valid for the duration of the callback, so the
// microphone path converts (which copies) before returning — the copy has to
// happen on the audio thread or not at all.

import AVFoundation
import Foundation

/// What a block is worth to a consumer: the buffer plus the frame range it
/// occupies on the shared timeline.
protocol AudioConsumer: AnyObject {
    var name: String { get }
    /// Called once, before any block. Throwing here aborts the session with the
    /// message the consumer supplied — a consumer that cannot start must not be
    /// silently skipped.
    func start(format: AVAudioFormat) throws
    /// Called once per block, in order, on a queue the consumer owns.
    func submit(_ buffer: AVAudioPCMBuffer, frames: Range<Int64>)
    /// Drains whatever is in flight. Called once, when the session ends.
    func finish() async
}

enum HubError: Error, CustomStringConvertible {
    case formatConversion(String)

    var description: String {
        switch self {
        case .formatConversion(let detail): return "cannot convert to the analysis format: \(detail)"
        }
    }
}

final class AudioHub: @unchecked Sendable {
    static let canonicalRate: Double = 16_000
    static let canonicalChannels: AVAudioChannelCount = 1

    /// 16 kHz mono Int16, de-interleaved. One channel makes interleaving
    /// meaningless in memory; the flag is spelled out because `AVAudioFormat`
    /// compares it and a mismatch would fail the buffer allocation later.
    static func canonicalFormat() -> AVAudioFormat {
        guard let format = AVAudioFormat(
            commonFormat: .pcmFormatInt16,
            sampleRate: canonicalRate,
            channels: canonicalChannels,
            interleaved: false
        ) else {
            fatalError("AVAudioFormat(int16, 16000, 1) is not constructible")
        }
        return format
    }

    let format: AVAudioFormat
    /// The one clock. Consumers are built against this exact instance — a second
    /// `Timeline` would mean the recogniser and the snore lane disagreeing about
    /// when the same block happened.
    let timeline: Timeline

    private let consumers: [AudioConsumer]
    private let queue = DispatchQueue(label: "dev.workbuddy.steno.hub", qos: .userInitiated)
    private let conversionLock = NSLock()
    private var converter: AVAudioConverter?
    private var converterSource: AVAudioFormat?
    private var started = false

    init(format: AVAudioFormat = AudioHub.canonicalFormat(), timeline: Timeline, consumers: [AudioConsumer]) {
        self.format = format
        self.timeline = timeline
        self.consumers = consumers
    }

    /// What each consumer will be told, for the `ready` line.
    var consumerNames: [String] { consumers.map(\.name) }

    func start() throws {
        try queue.sync {
            for consumer in consumers {
                try consumer.start(format: format)
            }
            started = true
        }
    }

    // MARK: - producers

    /// A block of canonical samples. `utcMs` is the wall clock of the first
    /// sample, as stamped by whoever captured it — never computed here.
    func ingest(samples: [Int16], utcMs: Int64) {
        guard !samples.isEmpty else { return }
        guard let buffer = AVAudioPCMBuffer(pcmFormat: format, frameCapacity: AVAudioFrameCount(samples.count)) else {
            log("hub: cannot allocate a \(samples.count)-frame buffer; dropping the block")
            return
        }
        buffer.frameLength = AVAudioFrameCount(samples.count)
        guard let channel = buffer.int16ChannelData?[0] else {
            log("hub: the canonical format has no int16 storage; dropping the block")
            return
        }
        samples.withUnsafeBufferPointer { source in
            guard let base = source.baseAddress else { return }
            channel.update(from: base, count: samples.count)
        }
        handOff(buffer, utcMs: utcMs)
    }

    /// A buffer in any format, e.g. straight from an `AVAudioEngine` tap. The
    /// conversion happens on the calling thread because the caller's buffer is
    /// about to be recycled.
    func ingest(buffer: AVAudioPCMBuffer, utcMs: Int64) {
        guard buffer.frameLength > 0 else { return }
        do {
            let canonical = try canonicalCopy(of: buffer)
            handOff(canonical, utcMs: utcMs)
        } catch {
            log("hub: \(error)")
        }
    }

    /// Waits for in-flight blocks, then lets each consumer drain. Called once.
    func finish() async {
        await withCheckedContinuation { (continuation: CheckedContinuation<Void, Never>) in
            queue.async {
                self.started = false
                continuation.resume()
            }
        }
        for consumer in consumers {
            await consumer.finish()
        }
    }

    // MARK: - fan-out

    private func handOff(_ buffer: AVAudioPCMBuffer, utcMs: Int64) {
        queue.async { [weak self] in
            guard let self else { return }
            guard self.started else {
                // Only reachable if a producer outran `start()`; worth knowing,
                // because the block is lost.
                log("hub: block arrived before the consumers started; dropping \(buffer.frameLength) frames")
                return
            }
            let frames = self.timeline.advance(frames: Int(buffer.frameLength), utcMs: utcMs)
            for consumer in self.consumers {
                consumer.submit(buffer, frames: frames)
            }
        }
    }

    // MARK: - conversion

    private func canonicalCopy(of source: AVAudioPCMBuffer) throws -> AVAudioPCMBuffer {
        let sourceFormat = source.format
        // Identical layout: a straight copy. The tap's storage is recycled the
        // moment the callback returns, so the copy is not optional.
        if sourceFormat == format {
            guard let output = AVAudioPCMBuffer(pcmFormat: format, frameCapacity: source.frameLength),
                  let from = source.int16ChannelData?[0],
                  let to = output.int16ChannelData?[0]
            else {
                throw HubError.formatConversion("identical format but no int16 storage")
            }
            output.frameLength = source.frameLength
            to.update(from: from, count: Int(source.frameLength))
            return output
        }

        let converter = try converterFor(sourceFormat)
        let capacity = AVAudioFrameCount(
            (Double(source.frameLength) * format.sampleRate / sourceFormat.sampleRate).rounded(.up)
        ) + 1_024
        guard let output = AVAudioPCMBuffer(pcmFormat: format, frameCapacity: capacity) else {
            throw HubError.formatConversion("cannot allocate \(capacity) converted frames")
        }
        var supplied = false
        var conversionError: NSError?
        let input: AVAudioConverterInputBlock = { _, status in
            if supplied {
                // Not `.endOfStream`: the converter is reused for the next
                // block, and ending the stream would retire it.
                status.pointee = .noDataNow
                return nil
            }
            supplied = true
            status.pointee = .haveData
            return source
        }
        let result = converter.convert(to: output, error: &conversionError, withInputFrom: input)
        if let conversionError { throw HubError.formatConversion(conversionError.localizedDescription) }
        if result == .error { throw HubError.formatConversion("converter reported an error") }
        guard output.frameLength > 0 else { return try emptyCanonical() }
        return output
    }

    private func emptyCanonical() throws -> AVAudioPCMBuffer {
        guard let buffer = AVAudioPCMBuffer(pcmFormat: format, frameCapacity: 1) else {
            throw HubError.formatConversion("cannot allocate an empty buffer")
        }
        buffer.frameLength = 0
        return buffer
    }

    private func converterFor(_ source: AVAudioFormat) throws -> AVAudioConverter {
        conversionLock.lock()
        defer { conversionLock.unlock() }
        if let converter, converterSource == source { return converter }
        guard let fresh = AVAudioConverter(from: source, to: format) else {
            throw HubError.formatConversion("no converter from \(source)")
        }
        converter = fresh
        converterSource = source
        log("hub: converting \(source.sampleRate) Hz \(source.channelCount) ch → \(format.sampleRate) Hz mono")
        return fresh
    }
}
