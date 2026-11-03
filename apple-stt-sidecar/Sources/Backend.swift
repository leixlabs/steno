// Backend: what a transcriber has to be, so that there can be more than one.
//
// There is one implementation here: `SpeechAnalyzerBackend`, Apple's on-device
// recogniser. The audio.cpp / Qwen3-ASR engine used to be a second one
// (`--backend audiocpp`), but it imported no Apple framework and only drove a
// subprocess, so it lives with the server now as the Rust sidecar
// `steno-stt-sidecar-audiocpp` (`crates/server/src/audiocpp/`, decision record
// `docs/decisions/2026-11-03-audiocpp-sidecar-rust.md`). It speaks the same wire,
// so nothing below the socket changed.
//
// ## What a backend is not allowed to know
//
// Audio in, text out. A backend does not know the wall clock, the socket, the
// log, or that a snore lane exists. Its results leave as *frame ranges on the
// shared timeline* — the numbering `AudioHub` hands every consumer in `submit` —
// and `TranscriptBridge` is the single place that turns a frame into `utc_ms`.
// So a backend never has to be right about what time it is, only about which
// samples it just heard.
//
// ## The obligations, and why each one exists
//
// - `audioFormat` is asked before any audio arrives. The hub adopts it and the
//   snore lane is built against it, so every consumer sees one buffer layout;
//   the alternative is two resamplers with two opinions about "here".
// - `submit` is called on the hub's serial queue and must return promptly. A
//   backend that drives a helper process owns its own queue and its own
//   ordering; the hub will not wait for it, and a block it drops is a block
//   nobody else sees either.
// - `final` always carries a range. A phrase that cannot be placed in the night
//   cannot be stored, so a backend that can place nothing is not a backend this
//   program will run. Coarse is allowed as long as it is declared
//   (`Timing.window`) — then the reader knows the bounds are the edges of a
//   window, not of a syllable. Inventing a time is not allowed.
// - `flush` commits what is still volatile without ending the session: that is
//   what the `flush` control frame means. For a chunked engine it means "run the
//   tail you are holding now", not "stop".
// - `finish` returns only after the last result has reached the target, because
//   the process exits immediately behind it.

import AVFoundation
import Foundation

// MARK: - which backend

enum BackendID: String, CaseIterable {
    /// `SpeechTranscriber` + `SpeechAnalyzer`. On-device, per phrase, macOS 26+.
    case apple

    static var known: String { allCases.map(\.rawValue).joined(separator: ", ") }

    /// The `engine` label carried on `ready` and stored with every utterance,
    /// used when `--engine` is not given. Each backend derives its own rather
    /// than sharing a constant: the label is how a stored row says where the
    /// text came from, and one backend must never report another's name.
    var defaultEngine: String {
        switch self {
        case .apple: return "apple-speechanalyzer"
        }
    }
}

// MARK: - what it promises

struct BackendCapabilities {
    /// How precisely a result can be placed on the audio timeline.
    enum Timing {
        /// Every result is a phrase with its own bounds (`SpeechAnalyzer`).
        ///
        /// The coarser `window(seconds:)` case left with the audio.cpp backend:
        /// its results describe fixed decode windows, and that engine now reports
        /// them from the Rust sidecar with its own `capabilities` string.
        case phrase
    }

    /// Whether results arrive before they are final. Off means a watcher sees
    /// text only when a phrase is committed, which is not a defect — it is what
    /// to expect, said out loud.
    var partials: Bool
    var timing: Timing

    var summary: String {
        let timingText: String
        switch timing {
        case .phrase: timingText = "phrase-timed"
        }
        return "\(timingText), partials \(partials ? "on" : "off")"
    }
}

// MARK: - where results go

/// What a backend may say, and nothing more. Implemented once, by
/// `TranscriptBridge`, which is what adds the session's clock and the lane facts
/// the backend has no business knowing.
///
/// `ready` is guaranteed to arrive before any result: it is the backend saying
/// the model is loaded, and a result before it would be text from a session the
/// server does not know exists.
protocol TranscriptTarget: AnyObject {
    /// `engine` and `language` are the backend's own answer — the locale it
    /// actually resolved to, not the one it was asked for — because they travel
    /// to the server on the `ready` frame.
    func ready(engine: String, language: String)
    func partial(_ text: String)
    func final(text: String, frames: Range<Int64>)
    func failed(_ message: String)
}

// MARK: - the contract

/// An `AudioConsumer` that also describes itself. Everything a second
/// implementation has to supply is here; everything it must not care about is
/// in `AudioHub` and `TranscriptBridge`.
protocol TranscriptionBackend: AudioConsumer {
    /// The label for the wire (see `BackendID.defaultEngine`).
    var engine: String { get }
    /// The locale in use, after resolution. Asked for by the caller, answered by
    /// the backend.
    var language: String { get }
    /// The audio layout this backend needs. Read before `start`, because the hub
    /// and the snore lane adopt it.
    var audioFormat: AVAudioFormat { get }
    var capabilities: BackendCapabilities { get }
    /// Commit volatile results without ending the session (`control: flush`).
    /// A backend that cannot do it must say so rather than pretend: audio.cpp
    /// has no way to decode a half-filled window, and its `flush` logs that.
    func flush() async
}

// MARK: - selection

extension BackendID {
    /// Builds the backend and everything it needs to fail early: model, locale,
    /// assets. Throws rather than falling back — a backend that cannot run must
    /// stop the session, not hand it to a different engine, because the
    /// transcript would then be labelled with the wrong one all night.
    func make(
        locale: String,
        engine: String,
        preset: TranscriptionPreset,
        target: TranscriptTarget
    ) async throws -> TranscriptionBackend {
        switch self {
        case .apple:
            return try await SpeechAnalyzerBackend.make(
                localeIdentifier: locale,
                engine: engine,
                preset: preset,
                target: target
            )
        }
    }

    /// Device questions for `--check`, answered by the backend that would run.
    func probe(locale: String, preset: TranscriptionPreset) async -> [String: Any] {
        switch self {
        case .apple:
            return await SpeechAnalyzerBackend.probe(localeIdentifier: locale, preset: preset)
        }
    }

    /// Downloads whatever this backend needs on this device. Explicit, never
    /// implicit: a missing asset must be said out loud, not downloaded mid-night.
    func install(locale: String, preset: TranscriptionPreset) async throws -> String {
        switch self {
        case .apple:
            return try await SpeechAnalyzerBackend.installAssets(localeIdentifier: locale, preset: preset)
        }
    }
}
