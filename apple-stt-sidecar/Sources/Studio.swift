// Studio: one `SpeechAnalyzer` session, driven by hand.
//
// This file is the whole of the "conversion function". Audio goes in, events go
// out, and nothing here decides *what* the audio means: no endpointing, no
// rotation, no re-decoding, no dropping of silence. `SpeechAnalyzer` finalises
// its own results as the audio flows — when it says a phrase is final, that is
// the phrase, and it is forwarded unchanged.
//
// ## Why there is no stop condition
//
// The predecessor of this program owned three policies (rotate at 50 s, end the
// task after 1.2 s of quiet, re-recognise the task's audio when Apple returned
// nothing). All three existed because `SFSpeechRecognizer` only produces a final
// when the client ends the request, and because a rotated request came back
// empty. `SpeechAnalyzer` has neither problem: results are finalised
// continuously and there is no request to end. So the policies are gone, and
// with them the reason the transcript used to disagree with itself.
//
// ## Timestamps
//
// `Result.range` is on the analyser's own audio timeline, which starts at zero
// with the first sample fed. The recorder's wall clock rides along in every
// frame header. `Anchors` is the one-line map between the two: normally a single
// anchor for the whole night, and a fresh one whenever the wire skipped audio
// (a dropped block, a node reconnect). Skipped audio is *not* padded with
// silence: the gap is recorded as a gap and the map re-anchors, so a timestamp
// is never invented for audio that never arrived.

import AVFoundation
import CoreMedia
import Foundation
import Speech

/// Where recognised text goes. Two implementations: one socket, one terminal.
protocol Sink: AnyObject {
    func ready(engine: String, language: String, locale: String)
    func partial(_ text: String)
    func final(text: String, startMs: Int, endMs: Int, engine: String, language: String)
    func failed(_ message: String)
}

/// Wall clock ↔ analyser audio time.
struct Anchors {
    private var points: [(audio: Double, utcMs: Int64)] = []

    /// The audio timeline starts with the first frame we ever fed.
    mutating func begin(utcMs: Int64) {
        points = [(audio: 0, utcMs: utcMs)]
    }

    /// True when the incoming frame does not continue the audio we already have:
    /// the wire lost something, so the old projection no longer holds.
    func isDiscontinuity(nextUtcMs: Int64, audioSeconds: Double) -> Bool {
        guard let last = points.last else { return false }
        let projected = last.utcMs + Int64((audioSeconds - last.audio) * 1000)
        return abs(Double(nextUtcMs - projected)) > 250
    }

    mutating func reanchor(utcMs: Int64, audioSeconds: Double) {
        points.append((audio: audioSeconds, utcMs: utcMs))
        if points.count > 64 {
            points.removeFirst(points.count - 64)
        }
    }

    func utcMs(forAudio seconds: Double) -> Int64 {
        guard let anchor = points.last(where: { $0.audio <= seconds + 1e-6 }) ?? points.first else {
            return 0
        }
        return anchor.utcMs + Int64(((seconds - anchor.audio) * 1000).rounded())
    }
}

enum StudioError: Error, CustomStringConvertible {
    case localeUnsupported(String, [String])
    case unavailable
    case audioFormat(String)

    var description: String {
        switch self {
        case .localeUnsupported(let asked, let supported):
            return "locale \(asked) is not supported by SpeechTranscriber; try one of \(supported.joined(separator: ", "))"
        case .unavailable:
            return "SpeechTranscriber.isAvailable is false on this machine"
        case .audioFormat(let detail):
            return "unexpected analyser audio format: \(detail)"
        }
    }
}

/// `@unchecked Sendable`: the only mutable state (`anchors`, the partial throttle)
/// sits behind `stateLock`, `feed` is called from the one socket thread and
/// `handle` from the one results task, and `AsyncStream.Continuation` is itself
/// thread-safe. The class is shared across those two, so it has to say so.
final class Studio: @unchecked Sendable {
    private let transcriber: SpeechTranscriber
    private let analyzer: SpeechAnalyzer
    private let input: AsyncStream<AnalyzerInput>
    private let continuation: AsyncStream<AnalyzerInput>.Continuation
    private let format: AVAudioFormat
    private let sink: Sink
    private let engine: String
    private let language: String

    private var anchors = Anchors()
    private var fedSeconds = 0.0
    private var resultsTask: Task<Void, Never>?
    private var lastPartialText = ""
    private var lastPartialAt = Date.distantPast
    private let stateLock = NSLock()

    private init(
        transcriber: SpeechTranscriber,
        analyzer: SpeechAnalyzer,
        input: AsyncStream<AnalyzerInput>,
        continuation: AsyncStream<AnalyzerInput>.Continuation,
        format: AVAudioFormat,
        sink: Sink,
        engine: String,
        language: String
    ) {
        self.transcriber = transcriber
        self.analyzer = analyzer
        self.input = input
        self.continuation = continuation
        self.format = format
        self.sink = sink
        self.engine = engine
        self.language = language
    }

    /// Resolves the locale, prepares the model and builds a session. Everything
    /// that can fail before audio arrives fails here, with a message that names
    /// what is missing.
    static func make(
        localeIdentifier: String,
        engine: String,
        sink: Sink
    ) async throws -> Studio {
        guard SpeechTranscriber.isAvailable else { throw StudioError.unavailable }
        let asked = Locale(identifier: localeIdentifier)
        guard let locale = await SpeechTranscriber.supportedLocale(equivalentTo: asked) else {
            let supported = await SpeechTranscriber.supportedLocales.prefix(12).map(\.identifier)
            throw StudioError.localeUnsupported(localeIdentifier, Array(supported))
        }
        // `.fastResults` is what makes the intermediate results useful: without
        // it the analyser holds several seconds of audio before saying anything.
        let transcriber = SpeechTranscriber(
            locale: locale,
            transcriptionOptions: [],
            reportingOptions: [.volatileResults, .fastResults],
            attributeOptions: [.audioTimeRange]
        )
        guard let format = await SpeechAnalyzer.bestAvailableAudioFormat(compatibleWith: [transcriber]) else {
            throw StudioError.audioFormat("no compatible format")
        }
        // The wire carries 16 kHz mono Int16. Resampling here would be a second
        // opinion about the audio; refuse instead.
        guard format.sampleRate == 16_000, format.channelCount == 1 else {
            throw StudioError.audioFormat("\(format.sampleRate) Hz \(format.channelCount) ch")
        }
        let analyzer = SpeechAnalyzer(modules: [transcriber])
        // Unbounded on purpose: dropping a block would silently break the
        // audio-time ↔ wall-clock map that every timestamp depends on. If the
        // analyser ever fell behind for a whole night, growing memory is the
        // honest symptom; a gap in the timeline would not be.
        let (stream, continuation) = AsyncStream<AnalyzerInput>.makeStream(
            bufferingPolicy: .unbounded
        )
        return Studio(
            transcriber: transcriber,
            analyzer: analyzer,
            input: stream,
            continuation: continuation,
            format: format,
            sink: sink,
            engine: engine,
            language: locale.identifier
        )
    }

    /// Starts the analyser and the results pump. `ready` is emitted only after
    /// both exist, so the server never sees a session that cannot answer.
    func start() async throws {
        try await analyzer.start(inputSequence: input)
        resultsTask = Task { [weak self] in
            guard let self else { return }
            do {
                for try await result in self.transcriber.results {
                    self.handle(result)
                }
            } catch {
                self.sink.failed("results stream failed: \(error.localizedDescription)")
            }
        }
        let locale = language
        sink.ready(engine: engine, language: language, locale: locale)
    }

    /// Feeds one block. Callers pass the recorder's wall clock untouched.
    func feed(samples: [Int16], utcMs: Int64) {
        guard !samples.isEmpty else { return }
        stateLock.lock()
        if fedSeconds == 0 {
            anchors.begin(utcMs: utcMs)
        } else if anchors.isDiscontinuity(nextUtcMs: utcMs, audioSeconds: fedSeconds) {
            // A hole in the wire, not a hole in the night: re-anchor rather than
            // fabricate silence that was never recorded.
            anchors.reanchor(utcMs: utcMs, audioSeconds: fedSeconds)
        }
        fedSeconds += Double(samples.count) / 16_000.0
        stateLock.unlock()

        guard let buffer = AVAudioPCMBuffer(pcmFormat: format, frameCapacity: AVAudioFrameCount(samples.count)) else {
            sink.failed("cannot allocate a \(samples.count)-frame buffer")
            return
        }
        buffer.frameLength = AVAudioFrameCount(samples.count)
        guard let channel = buffer.int16ChannelData?[0] else {
            sink.failed("analysis format has no int16 storage")
            return
        }
        samples.withUnsafeBufferPointer { source in
            channel.update(from: source.baseAddress!, count: samples.count)
        }
        continuation.yield(AnalyzerInput(buffer: buffer))
    }

    /// Ends the session: commits whatever is still volatile and waits for the
    /// last results to land.
    func finish() async {
        continuation.finish()
        try? await analyzer.finalizeAndFinishThroughEndOfInput()
        guard let resultsTask else { return }
        await withTaskGroup(of: Void.self) { group in
            group.addTask { await resultsTask.value }
            group.addTask { try? await Task.sleep(nanoseconds: 10_000_000_000) }
            await group.next()
            group.cancelAll()
        }
    }

    private func handle(_ result: SpeechTranscriber.Result) {
        let text = String(result.text.characters)
        let startSeconds = result.range.start.seconds
        let endSeconds = startSeconds + result.range.duration.seconds

        stateLock.lock()
        let startMs = Int(anchors.utcMs(forAudio: startSeconds))
        let endMs = Int(anchors.utcMs(forAudio: endSeconds))
        stateLock.unlock()

        if result.isFinal {
            guard !text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else { return }
            sink.final(text: text, startMs: startMs, endMs: max(endMs, startMs), engine: engine, language: language)
            stateLock.lock()
            lastPartialText = ""
            stateLock.unlock()
            return
        }
        guard !text.isEmpty else { return }
        // Wire hygiene only: the portal fans every partial out over a broadcast
        // channel, and a per-character rewording would fill it with duplicates.
        stateLock.lock()
        let now = Date()
        let fresh = text != lastPartialText && now.timeIntervalSince(lastPartialAt) >= 0.2
        if fresh {
            lastPartialText = text
            lastPartialAt = now
        }
        stateLock.unlock()
        if fresh { sink.partial(text) }
    }

    /// Locale-independent report used by `--check`.
    static func probe(localeIdentifier: String) async -> [String: Any] {
        var report: [String: Any] = [
            "available": SpeechTranscriber.isAvailable,
            "locale_asked": localeIdentifier,
        ]
        let supported = await SpeechTranscriber.supportedLocales.map(\.identifier)
        report["supported_locales"] = supported.count
        report["installed_locales"] = await SpeechTranscriber.installedLocales.map(\.identifier)
        guard SpeechTranscriber.isAvailable,
              let locale = await SpeechTranscriber.supportedLocale(equivalentTo: Locale(identifier: localeIdentifier))
        else {
            report["locale_resolved"] = NSNull()
            return report
        }
        report["locale_resolved"] = locale.identifier
        let module = SpeechTranscriber(
            locale: locale,
            transcriptionOptions: [],
            reportingOptions: [.volatileResults, .fastResults],
            attributeOptions: [.audioTimeRange]
        )
        switch await AssetInventory.status(forModules: [module]) {
        case .unsupported: report["assets"] = "unsupported"
        case .supported: report["assets"] = "supported"
        case .downloading: report["assets"] = "downloading"
        case .installed: report["assets"] = "installed"
        @unknown default: report["assets"] = "unknown"
        }
        report["formats"] = await module.availableCompatibleAudioFormats.map(\.description)
        return report
    }

    /// Downloads the on-device model for a locale. Explicit, never implicit: a
    /// device that has no asset must say so rather than quietly start a
    /// multi-hundred-megabyte download mid-night.
    static func installAssets(localeIdentifier: String) async throws -> String {
        guard let locale = await SpeechTranscriber.supportedLocale(equivalentTo: Locale(identifier: localeIdentifier)) else {
            throw StudioError.localeUnsupported(localeIdentifier, [])
        }
        let module = SpeechTranscriber(
            locale: locale,
            transcriptionOptions: [],
            reportingOptions: [.volatileResults, .fastResults],
            attributeOptions: [.audioTimeRange]
        )
        guard let request = try await AssetInventory.assetInstallationRequest(supporting: [module]) else {
            return "already installed: \(locale.identifier)"
        }
        try await request.downloadAndInstall()
        return "installed: \(locale.identifier)"
    }
}

/// Streams events to the socket the server is listening on.
final class SocketSink: Sink {
    private let writer: EventWriter
    private let onBroken: () -> Void

    init(writer: EventWriter, onBroken: @escaping () -> Void) {
        self.writer = writer
        self.onBroken = onBroken
    }

    func ready(engine: String, language: String, locale: String) {
        if !writer.send(.ready(engine: engine, locale: locale)) { onBroken() }
    }

    func partial(_ text: String) {
        if !writer.send(.partial(text)) { onBroken() }
    }

    func final(text: String, startMs: Int, endMs: Int, engine: String, language: String) {
        if !writer.send(.final(text: text, startMs: startMs, endMs: endMs, engine: engine, language: language)) {
            onBroken()
        }
    }

    func failed(_ message: String) {
        log("error: \(message)")
        if !writer.send(.error(message)) { onBroken() }
    }
}

/// Collects finals for `--file`, which prints one `Transcript` object on stdout
/// (the shape `stt::parse_transcript` expects) and nothing else.
final class CollectingSink: Sink {
    private(set) var text: String = ""
    private(set) var segments: [Segment] = []
    private(set) var engine: String = ""
    private(set) var language: String?

    func ready(engine: String, language: String, locale: String) {
        self.engine = engine
        self.language = language
    }

    func partial(_ text: String) {}

    func final(text: String, startMs: Int, endMs: Int, engine: String, language: String) {
        self.text = joined(self.text, text)
        self.segments.append(Segment(start_ms: startMs, end_ms: endMs, text: text, confidence: nil))
        self.engine = engine
        self.language = language
    }

    func failed(_ message: String) {
        log("error: \(message)")
    }

    /// Two finalised blocks are separate sentences, not tokens: a space belongs
    /// between them only when both edges are ASCII words ("Hello" + "World"),
    /// and would be wrong between two CJK sentences.
    private func joined(_ previous: String, _ next: String) -> String {
        guard let last = previous.last, let first = next.first else { return previous + next }
        func word(_ character: Character) -> Bool {
            character.isASCII && (character.isLetter || character.isNumber)
        }
        return word(last) && word(first) ? previous + " " + next : previous + next
    }
}
