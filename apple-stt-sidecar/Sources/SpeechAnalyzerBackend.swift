// SpeechAnalyzerBackend: Apple's on-device recogniser, behind `TranscriptionBackend`.
//
// This is the whole of the speech conversion for `--backend apple`. Audio goes
// in, text comes out, and nothing here decides *what* the audio means: no
// endpointing, no rotation, no re-decoding. `SpeechAnalyzer` finalises its own
// results as audio flows — when it says a phrase is final, that is the phrase,
// and it is forwarded unchanged.
//
// Everything this file knows that a backend must not know — the clock, the
// socket, the snore lane — is above it: `AudioHub` feeds it, `TranscriptBridge`
// places its results. What is left is the Apple-specific part: locale
// resolution, the audio format the model wants, presets, assets.
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

/// Apple's knob, and only Apple's: `--preset` means nothing to another engine,
/// which is why `Options` refuses it for any other backend instead of ignoring
/// it.
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

    /// `.transcription` reports no volatile results, so the session should not
    /// promise partials it will never see.
    var emitsPartials: Bool {
        switch self {
        case .progressiveTranscription: return true
        case .transcription: return false
        }
    }

    static func parse(_ raw: String) -> TranscriptionPreset? {
        TranscriptionPreset(rawValue: raw)
    }

    static var known: String {
        allCases.map(\.rawValue).joined(separator: ", ")
    }
}

enum SpeechAnalyzerError: Error, CustomStringConvertible {
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

/// `@unchecked Sendable`: `submit` is called from the hub's one queue and
/// `handle` from the one results task, and the only state that crosses them is
/// an `AsyncStream.Continuation`, which is itself thread-safe.
final class SpeechAnalyzerBackend: TranscriptionBackend, @unchecked Sendable {
    let engine: String
    let language: String

    private let transcriber: SpeechTranscriber
    private let analyzer: SpeechAnalyzer
    private let input: AsyncStream<AnalyzerInput>
    private let continuation: AsyncStream<AnalyzerInput>.Continuation
    private let expectedFormat: AVAudioFormat
    private let target: TranscriptTarget
    private let preset: TranscriptionPreset

    private var resultsTask: Task<Void, Never>?

    var name: String { "transcriber(\(engine))" }

    /// The format the recogniser asked for. The hub adopts it, so every consumer
    /// shares one buffer layout instead of resampling independently.
    var audioFormat: AVAudioFormat { expectedFormat }

    /// Per phrase, always: `.audioTimeRange` is why the preset is time-indexed
    /// (see the header), so a result that arrives without a placeable range
    /// would be a contradiction, not a limitation.
    static func capabilities(preset: TranscriptionPreset) -> BackendCapabilities {
        BackendCapabilities(partials: preset.emitsPartials, timing: .phrase)
    }

    var capabilities: BackendCapabilities { Self.capabilities(preset: preset) }

    private init(
        transcriber: SpeechTranscriber,
        analyzer: SpeechAnalyzer,
        input: AsyncStream<AnalyzerInput>,
        continuation: AsyncStream<AnalyzerInput>.Continuation,
        expectedFormat: AVAudioFormat,
        preset: TranscriptionPreset,
        target: TranscriptTarget,
        engine: String,
        language: String
    ) {
        self.transcriber = transcriber
        self.analyzer = analyzer
        self.input = input
        self.continuation = continuation
        self.expectedFormat = expectedFormat
        self.preset = preset
        self.target = target
        self.engine = engine
        self.language = language
    }

    /// Resolves the locale and builds the session. Everything that can fail
    /// before audio arrives fails here, naming what is missing.
    static func make(
        localeIdentifier: String,
        engine: String,
        preset: TranscriptionPreset,
        target: TranscriptTarget
    ) async throws -> SpeechAnalyzerBackend {
        guard SpeechTranscriber.isAvailable else { throw SpeechAnalyzerError.unavailable }
        let asked = Locale(identifier: localeIdentifier)
        guard let locale = await SpeechTranscriber.supportedLocale(equivalentTo: asked) else {
            let supported = await SpeechTranscriber.supportedLocales.prefix(12).map(\.identifier)
            throw SpeechAnalyzerError.localeUnsupported(localeIdentifier, Array(supported))
        }
        let transcriber = SpeechTranscriber(locale: locale, preset: preset.speechPreset)
        guard let format = await SpeechAnalyzer.bestAvailableAudioFormat(compatibleWith: [transcriber]) else {
            throw SpeechAnalyzerError.audioFormat("no compatible format")
        }
        // The hub is 16 kHz mono Int16 by construction. Anything else means the
        // model wants audio the recorder never produces, and resampling twice
        // (once to the hub, once to the model) is a second opinion about the
        // audio that nothing here is entitled to.
        guard format.sampleRate == 16_000, format.channelCount == 1 else {
            throw SpeechAnalyzerError.audioFormat("\(format.sampleRate) Hz \(format.channelCount) ch")
        }
        guard format.commonFormat == .pcmFormatInt16 else {
            throw SpeechAnalyzerError.audioFormat("sample format \(format.commonFormat.rawValue), not int16")
        }
        let analyzer = SpeechAnalyzer(modules: [transcriber])
        // Unbounded on purpose: dropping a block would break the frame↔clock map
        // every timestamp depends on. If the analyser fell behind for a whole
        // night, growing memory is the honest symptom; a hole in the timeline
        // would not be.
        let (stream, continuation) = AsyncStream<AnalyzerInput>.makeStream(bufferingPolicy: .unbounded)
        return SpeechAnalyzerBackend(
            transcriber: transcriber,
            analyzer: analyzer,
            input: stream,
            continuation: continuation,
            expectedFormat: format,
            preset: preset,
            target: target,
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
            throw SpeechAnalyzerError.audioFormat("hub format \(format) but the model wants \(expectedFormat)")
        }
        let analyzer = self.analyzer
        let input = self.input
        resultsTask = Task { [weak self] in
            do {
                try await analyzer.start(inputSequence: input)
            } catch {
                self?.target.failed("analyser failed to start: \(error.localizedDescription)")
                return
            }
            guard let self else { return }
            self.target.ready(engine: self.engine, language: self.language)
            do {
                for try await result in self.transcriber.results {
                    self.handle(result)
                }
            } catch {
                self.target.failed("results stream failed: \(error.localizedDescription)")
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
            target.failed("flush failed: \(error.localizedDescription)")
        }
    }

    // MARK: - results

    /// Frames, not milliseconds: `SpeechAnalyzer` timestamps run on its own
    /// audio timeline, which starts at the first sample the hub fed it — the
    /// same origin as the frame numbering `submit` was given. Turning that into
    /// a wall clock is `TranscriptBridge`'s job.
    private func handle(_ result: SpeechTranscriber.Result) {
        let text = String(result.text.characters)
        logDebug("result final=\(result.isFinal) start=\(result.range.start.seconds) dur=\(result.range.duration.seconds) chars=\(text.count)")
        let start = frame(for: result.range.start)
        let end = start + Int64((result.range.duration.seconds * Double(Timeline.rate)).rounded())
        let frames = start..<max(end, start)
        if result.isFinal {
            target.final(text: text, frames: frames)
        } else {
            target.partial(text)
        }
    }

    private func frame(for time: CMTime) -> Int64 {
        Int64((time.seconds * Double(Timeline.rate)).rounded())
    }

    // MARK: - device questions

    /// Locale-independent report used by `--check`.
    static func probe(localeIdentifier: String, preset: TranscriptionPreset) async -> [String: Any] {
        var report: [String: Any] = [
            "available": SpeechTranscriber.isAvailable,
            "locale_asked": localeIdentifier,
            "preset": preset.rawValue,
            // What a transcript from this configuration promises: phrase bounds,
            // and partials only when the preset reports volatile results.
            "capabilities": Self.capabilities(preset: preset).summary,
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
        let module = SpeechTranscriber(locale: locale, preset: preset.speechPreset)
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
    static func installAssets(localeIdentifier: String, preset: TranscriptionPreset) async throws -> String {
        guard let locale = await SpeechTranscriber.supportedLocale(equivalentTo: Locale(identifier: localeIdentifier)) else {
            throw SpeechAnalyzerError.localeUnsupported(localeIdentifier, [])
        }
        let module = SpeechTranscriber(locale: locale, preset: preset.speechPreset)
        guard let request = try await AssetInventory.assetInstallationRequest(supporting: [module]) else {
            return "already installed: \(locale.identifier)"
        }
        try await request.downloadAndInstall()
        return "installed: \(locale.identifier)"
    }
}
