// Transcriber: one `SpeechAnalyzer` session, driven by hand.
//
// This is the whole of the speech conversion. Audio goes in, text comes out, and
// nothing here decides *what* the audio means: no endpointing, no rotation, no
// re-decoding. `SpeechAnalyzer` finalises its own results as audio flows — when
// it says a phrase is final, that is the phrase, and it is forwarded unchanged.
//
// ## Why there is no stop condition
//
// The predecessor of this program owned three policies (rotate at 50 s, end the
// task after 1.2 s of quiet, re-recognise the task's audio when Apple returned
// nothing). All three existed because `SFSpeechRecognizer` only produces a final
// when the client ends the request. `SpeechAnalyzer` has neither problem:
// results are finalised continuously and there is no request to end. So the
// policies are gone, and with them the reason the transcript used to disagree
// with itself.
//
// ## Why the preset is always a time-indexed one
//
// `SpeechTranscriber.Preset` bundles reporting and *attribute* options, and only
// the time-indexed variants carry `.audioTimeRange`:
//
//     progressiveTranscription            fastResults, volatileResults
//     timeIndexedProgressiveTranscription fastResults, volatileResults, audioTimeRange
//
// Without the time range there is no way to say when a phrase was spoken, and a
// transcript a night long with no clock is not worth storing. The two presets
// the CLI accepts therefore both map onto time-indexed variants.

import AVFoundation
import CoreMedia
import Foundation
import Speech

enum TranscriptionPreset: String, CaseIterable {
    /// Live: volatile results as the phrase forms. The streaming default.
    case progressiveTranscription
    /// One pass, no volatile results. Cheaper for a finished file.
    case transcription

    var speechPreset: SpeechTranscriber.Preset {
        switch self {
        case .progressiveTranscription: return .timeIndexedProgressiveTranscription
        case .transcription: return .timeIndexedTranscriptionWithAlternatives
        }
    }

    static func parse(_ raw: String) -> TranscriptionPreset? {
        TranscriptionPreset(rawValue: raw)
    }

    static var known: String {
        allCases.map(\.rawValue).joined(separator: ", ")
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

/// `@unchecked Sendable`: mutable state sits behind `stateLock`, `submit` is
/// called from the hub's one queue and `handle` from the one results task, and
/// `AsyncStream.Continuation` is itself thread-safe.
final class Transcriber: AudioConsumer, @unchecked Sendable {
    let engine: String
    let language: String

    private let transcriber: SpeechTranscriber
    private let analyzer: SpeechAnalyzer
    private let input: AsyncStream<AnalyzerInput>
    private let continuation: AsyncStream<AnalyzerInput>.Continuation
    private let expectedFormat: AVAudioFormat
    private let timeline: Timeline
    private let sink: EventSink
    /// Named in `ready` so the server can record which lane produced a snore
    /// annotation, or that none did. Set by `Pipeline.make` before `start()`.
    private var snoreLane: String?

    private var resultsTask: Task<Void, Never>?
    private let stateLock = NSLock()
    private var lastPartialText = ""
    private var lastPartialAt = Date.distantPast

    var name: String { "transcriber(\(engine))" }

    /// The format the recogniser asked for. The hub adopts it, so the two
    /// consumers share one buffer layout instead of resampling independently.
    var analysisFormat: AVAudioFormat { expectedFormat }

    /// Set before `start()`; travels on the `ready` frame so a stored
    /// annotation can always name the lane that produced it.
    func describeSnoreLane(_ name: String?) {
        stateLock.lock()
        snoreLane = name
        stateLock.unlock()
    }

    private init(
        transcriber: SpeechTranscriber,
        analyzer: SpeechAnalyzer,
        input: AsyncStream<AnalyzerInput>,
        continuation: AsyncStream<AnalyzerInput>.Continuation,
        expectedFormat: AVAudioFormat,
        timeline: Timeline,
        sink: EventSink,
        engine: String,
        language: String
    ) {
        self.transcriber = transcriber
        self.analyzer = analyzer
        self.input = input
        self.continuation = continuation
        self.expectedFormat = expectedFormat
        self.timeline = timeline
        self.sink = sink
        self.engine = engine
        self.language = language
    }

    /// Resolves the locale and builds the session. Everything that can fail
    /// before audio arrives fails here, naming what is missing.
    static func make(
        localeIdentifier: String,
        engine: String,
        preset: TranscriptionPreset,
        timeline: Timeline,
        sink: EventSink
    ) async throws -> Transcriber {
        guard SpeechTranscriber.isAvailable else { throw StudioError.unavailable }
        let asked = Locale(identifier: localeIdentifier)
        guard let locale = await SpeechTranscriber.supportedLocale(equivalentTo: asked) else {
            let supported = await SpeechTranscriber.supportedLocales.prefix(12).map(\.identifier)
            throw StudioError.localeUnsupported(localeIdentifier, Array(supported))
        }
        let transcriber = SpeechTranscriber(locale: locale, preset: preset.speechPreset)
        guard let format = await SpeechAnalyzer.bestAvailableAudioFormat(compatibleWith: [transcriber]) else {
            throw StudioError.audioFormat("no compatible format")
        }
        // The hub is 16 kHz mono Int16 by construction. Anything else means the
        // model wants audio the recorder never produces, and resampling twice
        // (once to the hub, once to the model) is a second opinion about the
        // audio that nothing here is entitled to.
        guard format.sampleRate == 16_000, format.channelCount == 1 else {
            throw StudioError.audioFormat("\(format.sampleRate) Hz \(format.channelCount) ch")
        }
        guard format.commonFormat == .pcmFormatInt16 else {
            throw StudioError.audioFormat("sample format \(format.commonFormat.rawValue), not int16")
        }
        let analyzer = SpeechAnalyzer(modules: [transcriber])
        // Unbounded on purpose: dropping a block would break the frame↔clock map
        // every timestamp depends on. If the analyser fell behind for a whole
        // night, growing memory is the honest symptom; a hole in the timeline
        // would not be.
        let (stream, continuation) = AsyncStream<AnalyzerInput>.makeStream(bufferingPolicy: .unbounded)
        return Transcriber(
            transcriber: transcriber,
            analyzer: analyzer,
            input: stream,
            continuation: continuation,
            expectedFormat: format,
            timeline: timeline,
            sink: sink,
            engine: engine,
            language: locale.identifier
        )
    }

    // MARK: - AudioConsumer

    /// Starts the analyser and the results pump. Returns immediately: the first
    /// blocks land in the unbounded input stream while the model is still
    /// loading, so nothing is lost to a slow start.
    func start(format: AVAudioFormat) throws {
        guard format == expectedFormat else {
            throw StudioError.audioFormat("hub format \(format) but the model wants \(expectedFormat)")
        }
        let analyzer = self.analyzer
        let input = self.input
        resultsTask = Task { [weak self] in
            do {
                try await analyzer.start(inputSequence: input)
            } catch {
                self?.sink.failed("analyser failed to start: \(error.localizedDescription)")
                return
            }
            guard let self else { return }
            self.sink.ready(
                engine: self.engine,
                language: self.language,
                locale: self.language,
                snore: self.snoreLane
            )
            do {
                for try await result in self.transcriber.results {
                    self.handle(result)
                }
            } catch {
                self.sink.failed("results stream failed: \(error.localizedDescription)")
            }
        }
    }

    func submit(_ buffer: AVAudioPCMBuffer, frames: Range<Int64>) {
        continuation.yield(AnalyzerInput(buffer: buffer))
    }

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

    /// Commits whatever is still volatile without ending the session: the
    /// `flush` control, and the point of it is that a caller who asks "what has
    /// been said so far" gets an answer instead of waiting for a pause.
    func flush() async {
        do {
            try await analyzer.finalize(through: nil)
        } catch {
            sink.failed("flush failed: \(error.localizedDescription)")
        }
    }

    // MARK: - results

    private func handle(_ result: SpeechTranscriber.Result) {
        let text = String(result.text.characters)
        logDebug("result final=\(result.isFinal) start=\(result.range.start.seconds) dur=\(result.range.duration.seconds) chars=\(text.count)")
        let startFrame = frame(for: result.range.start)
        let endFrame = frame(for: result.range.start) + Int64((result.range.duration.seconds * Double(Timeline.rate)).rounded())
        let startMs = timeline.utcMs(forFrame: startFrame)
        let endMs = timeline.utcMs(forFrame: max(endFrame, startFrame))

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

    private func frame(for time: CMTime) -> Int64 {
        Int64((time.seconds * Double(Timeline.rate)).rounded())
    }

    // MARK: - device questions

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
        let module = SpeechTranscriber(locale: locale, preset: .timeIndexedProgressiveTranscription)
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
    /// device with no asset must say so rather than quietly start a
    /// multi-hundred-megabyte download mid-night.
    static func installAssets(localeIdentifier: String) async throws -> String {
        guard let locale = await SpeechTranscriber.supportedLocale(equivalentTo: Locale(identifier: localeIdentifier)) else {
            throw StudioError.localeUnsupported(localeIdentifier, [])
        }
        let module = SpeechTranscriber(locale: locale, preset: .timeIndexedProgressiveTranscription)
        guard let request = try await AssetInventory.assetInstallationRequest(supporting: [module]) else {
            return "already installed: \(locale.identifier)"
        }
        try await request.downloadAndInstall()
        return "installed: \(locale.identifier)"
    }
}
