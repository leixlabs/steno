// Options: the command line, parsed once, refused loudly when wrong.
//
// Two ways to talk to this program, and the flags say which:
//
//   --mode socket --path /tmp/stt.sock   framed duplex over a Unix socket
//   --mode stdio                         framed duplex over stdin/stdout
//   --mode mic                           the default input device
//   --mode file <audio>                  one finished file
//   --mode stdin                         raw PCM, no framing (`--stream`)
//   --mode check | install               what this machine can do
//
// And one choice that is not a mode, because it applies to all of them:
// `--backend apple|audiocpp` picks the recogniser. Everything else here is an
// obligation of the whole program, so it stays common; a flag only one backend
// understands (`--preset`) is refused for the others rather than ignored.
//
// An unknown flag or an unknown value is an error, never a hint. A typo that
// silently turns into a different night of recording is the one failure mode
// this file exists to prevent.

import AVFoundation
import Foundation

struct Options {
    enum Mode {
        case socket(path: String)
        case stdio
        case file(path: String)
        /// Raw interleaved PCM on stdin, no framing.
        case rawStdin
        case microphone
        case check
        case installAssets
    }

    var mode: Mode
    var backend: BackendID = .apple
    var locale = "zh-CN"
    var engine = BackendID.apple.defaultEngine
    var preset: TranscriptionPreset = .progressiveTranscription
    var format: OutputFormat = .text
    var out: String?
    var sampleRate: Double = 16_000
    var channels: AVAudioChannelCount = 1
    /// `nil` turns the snore lane off entirely.
    var snore: SnoreConfig? = SnoreConfig()
    /// Only for `--mode mic`; 0 runs until the process is stopped.
    var seconds: Double = 0
    /// Write recognised text into the log. Off by default: the log is in /tmp and
    /// a transcript does not belong there (see `Log.swift`).
    var logText = false
    var verbose = ProcessInfo.processInfo.environment["STENO_STT_VERBOSE"] == "1"

    static let usage = """
    usage: steno-stt-sidecar --mode socket --path <socket>     framed duplex, connects out
           steno-stt-sidecar --mode stdio                       framed duplex, stdin/stdout
           steno-stt-sidecar --mode mic [--seconds N]           default input device
           steno-stt-sidecar --mode file <audio> [--out <json>] [--format text|json|jsonl|srt]
           steno-stt-sidecar --stream [--sample-rate 16000] [--channels 1]
           steno-stt-sidecar --check | --install-assets

           [--backend apple] [--engine apple-speechanalyzer]
           [--locale zh-CN]
           [--preset progressiveTranscription|transcription]
           [--snore | --no-snore] [--snore-threshold 0.7] [--snore-window 3.0]
           [--snore-model <mlmodel|mlmodelc>] [--snore-label snoring]
           [--log-text] [--verbose]

    Shorthands kept for the wrappers: `--socket <path>` = --mode socket, `--file <audio>` = --mode file.
    """

    static func parse(_ arguments: [String]) -> Options? {
        var mode: Mode?
        var backend: BackendID = .apple
        var locale = "zh-CN"
        // `nil` until the flag or the end of parsing decides: the default is the
        // selected backend's own label, so `--backend` alone cannot leave the
        // wire claiming Apple produced an audio.cpp transcript.
        var engine: String?
        var preset: TranscriptionPreset = .progressiveTranscription
        var presetGiven = false
        var format: OutputFormat?
        var out: String?
        var rate: Double = 16_000
        var channels: AVAudioChannelCount = 1
        var snore: SnoreConfig? = SnoreConfig()
        var seconds: Double = 0
        var logText = false
        var positionals: [String] = []
        var verbose = ProcessInfo.processInfo.environment["STENO_STT_VERBOSE"] == "1"

        func fail(_ message: String) -> Options? {
            log("\(message)\n\(usage)")
            return nil
        }

        var index = 1
        func next(_ flag: String) -> String? {
            index += 1
            guard index < arguments.count else {
                log("\(flag) needs a value")
                return nil
            }
            return arguments[index]
        }

        while index < arguments.count {
            let argument = arguments[index]
            switch argument {
            case "--mode":
                guard let raw = next("--mode") else { return nil }
                switch raw {
                case "socket": mode = .socket(path: "")
                case "stdio": mode = .stdio
                case "file": mode = .file(path: "")
                case "stdin", "stream": mode = .rawStdin
                case "mic": mode = .microphone
                case "check": mode = .check
                case "install", "install-assets": mode = .installAssets
                default:
                    return fail("unknown --mode \(raw); expected socket, stdio, file, stdin, mic, check or install")
                }
            case "--socket", "--path":
                guard let value = next(argument) else { return nil }
                mode = .socket(path: value)
            case "--stdio":
                mode = .stdio
            case "--file":
                guard let value = next("--file") else { return nil }
                mode = .file(path: value)
            case "--stream":
                mode = .rawStdin
            case "--mic":
                mode = .microphone
            case "--check":
                mode = .check
            case "--install-assets":
                mode = .installAssets
            case "--out":
                guard let value = next("--out") else { return nil }
                out = value
            case "--format", "-f":
                guard let value = next("--format") else { return nil }
                guard let parsed = OutputFormat.parse(value) else {
                    return fail("unknown --format \(value); expected \(OutputFormat.known)")
                }
                format = parsed
            case "--locale", "-l":
                guard let value = next("--locale") else { return nil }
                locale = value
            case "--backend":
                guard let value = next("--backend") else { return nil }
                guard let parsed = BackendID(rawValue: value) else {
                    return fail("unknown --backend \(value); expected \(BackendID.known)")
                }
                backend = parsed
            case "--engine":
                guard let value = next("--engine") else { return nil }
                engine = value
            case "--preset":
                guard let value = next("--preset") else { return nil }
                guard let parsed = TranscriptionPreset.parse(value) else {
                    return fail("unknown --preset \(value); expected \(TranscriptionPreset.known)")
                }
                preset = parsed
                presetGiven = true
            case "--sample-rate":
                guard let value = next("--sample-rate"), let parsed = Double(value), parsed > 0 else {
                    return fail("--sample-rate needs a positive number")
                }
                rate = parsed
            case "--channels":
                guard let value = next("--channels"), let parsed = UInt32(value), parsed >= 1, parsed <= 2 else {
                    return fail("--channels must be 1 or 2")
                }
                channels = AVAudioChannelCount(parsed)
            case "--seconds":
                guard let value = next("--seconds"), let parsed = Double(value), parsed >= 0 else {
                    return fail("--seconds needs a non-negative number")
                }
                seconds = parsed
            case "--snore":
                if snore == nil { snore = SnoreConfig() }
            case "--no-snore":
                snore = nil
            case "--snore-threshold":
                guard let value = next("--snore-threshold"), let parsed = Float(value),
                      parsed > 0, parsed <= 1 else {
                    return fail("--snore-threshold must be between 0 and 1")
                }
                snore?.threshold = parsed
            case "--snore-label":
                guard let value = next("--snore-label") else { return nil }
                snore?.label = value
            case "--snore-window":
                guard let value = next("--snore-window"), let parsed = Double(value), parsed > 0 else {
                    return fail("--snore-window needs a positive number of seconds")
                }
                snore?.windowSeconds = parsed
            case "--log-text":
                logText = true
            case "--snore-model":
                guard let value = next("--snore-model") else { return nil }
                snore?.modelPath = value
            case "--verbose":
                verbose = true
            default:
                // Unknown arguments are a mistake, not a hint: refusing here is
                // the only way a typo does not become a silently different run.
                guard !argument.hasPrefix("-") else {
                    return fail("unknown argument: \(argument)")
                }
                // A bare word is the path `--mode file` / `--mode socket` asked
                // for; anything else about it is checked below.
                positionals.append(argument)
            }
            index += 1
        }

        guard var resolved = mode else { return fail("no mode given") }
        // `--mode file` / `--mode socket` take their path as the next word, so a
        // position is only legitimate when exactly one is missing and exactly
        // one was given.
        if !positionals.isEmpty {
            switch resolved {
            case .file(let path) where path.isEmpty, .socket(let path) where path.isEmpty:
                guard positionals.count == 1 else {
                    return fail("expected one path, got \(positionals.count)")
                }
                if case .file = resolved {
                    resolved = .file(path: positionals[0])
                } else {
                    resolved = .socket(path: positionals[0])
                }
            default:
                return fail("unexpected argument: \(positionals[0])")
            }
        }
        switch resolved {
        case .socket(let path) where path.isEmpty:
            return fail("--mode socket needs --path <socket>")
        case .file(let path) where path.isEmpty:
            return fail("--mode file needs a file path")
        default:
            break
        }

        // A file is asked for one JSON object; a stream is read as text.
        var resolvedFormat = format ?? .text
        if case .file = resolved, format == nil { resolvedFormat = .json }

        // A preset is `SpeechTranscriber`'s reporting style. No other backend has
        // one, and accepting the flag would leave the operator believing it took
        // effect on a transcript that is formatted by somebody else's engine.
        if presetGiven, backend != .apple {
            return fail("--preset only applies to --backend apple; \(backend.rawValue) has no presets")
        }

        return Options(
            mode: resolved,
            backend: backend,
            locale: locale,
            engine: engine ?? backend.defaultEngine,
            preset: preset,
            format: resolvedFormat,
            out: out,
            sampleRate: rate,
            channels: channels,
            snore: snore,
            seconds: seconds,
            logText: logText,
            verbose: verbose
        )
    }
}
