// Emitters: where events go once the analysers have produced them.
//
// Two implementations, because the two ways of running this program need
// different things. On the wire (`WireSink`) an event is a frame and its shape is
// the contract with the server. In a terminal (`TerminalSink`) an event is text,
// and the shape is whatever the caller asked for with `--format`.
//
// Nothing here interprets an event. A final is text with a time range; what that
// means is the reader's problem, not this file's.

import Foundation

protocol EventSink: AnyObject {
    /// `state` is `ready`, `stopped`, `error` or `degraded`. `snore` names the
    /// lane that is actually running, or nil when it is off.
    func session(state: String, message: String?, engine: String?, language: String?, locale: String?, snore: String?)
    func partial(_ text: String)
    func final(text: String, startMs: Int64, endMs: Int64, engine: String, language: String)
    func snore(_ episode: SnoreEpisode)
    func failed(_ message: String)
}

extension EventSink {
    func ready(engine: String, language: String, locale: String, snore: String?) {
        session(state: "ready", message: nil, engine: engine, language: language, locale: locale, snore: snore)
    }

}

// MARK: - the wire

/// One frame per event, on the socket or on stdout under `--stdio`.
final class WireSink: EventSink {
    private let channel: FrameChannel
    /// Called when the peer is gone. There is nobody left to tell, so the caller
    /// stops the session instead of writing into a dead pipe all night.
    private let onBroken: () -> Void

    init(channel: FrameChannel, onBroken: @escaping () -> Void) {
        self.channel = channel
        self.onBroken = onBroken
    }

    func session(state: String, message: String?, engine: String?, language: String?, locale: String?, snore: String?) {
        let payload = SessionPayload(
            state: state, engine: engine, language: language, locale: locale, snore: snore, message: message
        )
        send(type: .session, payload: payload)
    }

    func partial(_ text: String) {
        let payload = TranscriptPayload.partial(text)
        send(type: .transcript, payload: payload)
    }

    func final(text: String, startMs: Int64, endMs: Int64, engine: String, language: String) {
        let payload = TranscriptPayload.final(
            text: text, startMs: startMs, endMs: endMs, engine: engine, language: language
        )
        send(type: .transcript, payload: payload)
        // Opt-in: a transcript in the log is the one thing this project keeps out
        // of logs (see `logTranscripts`). When it is on, the line carries the
        // wall-clock bounds so it can be matched against `utterances` in the
        // database — a text line without a time is not diagnosable.
        if logTranscripts {
            log("text [\(startMs)..\(max(endMs, startMs))] \(text)")
        }
    }

    func snore(_ episode: SnoreEpisode) {
        let payload = SnorePayload(
            startMs: episode.startMs,
            endMs: episode.endMs,
            label: episode.label,
            peak: episode.peak,
            mean: episode.mean,
            windows: episode.windows,
            threshold: episode.threshold,
            endReason: episode.endReason
        )
        send(type: .snore, payload: payload)
        // Always logged, even without `--log-text`: an annotation is not the
        // transcript, and "did the classifier fire at all" is the first question
        // when a night comes back with no episodes.
        log(String(
            format: "snore: %@ %.3f→%.3f s peak %.2f mean %.2f over %d windows",
            episode.label,
            Double(episode.startMs) / 1000.0,
            Double(episode.endMs) / 1000.0,
            episode.peak,
            episode.mean,
            episode.windows
        ))
    }

    func failed(_ message: String) {
        log("error: \(message)")
        session(state: "error", message: message, engine: nil, language: nil, locale: nil, snore: nil)
    }


    private func send<T: Encodable>(type: MsgType, payload: T) {
        guard let bytes = JSONPayload.encode(payload) else {
            log("cannot encode a \(type) payload")
            return
        }
        if !channel.send(type: type, payload: bytes) { onBroken() }
    }
}

// MARK: - the terminal

enum OutputFormat: String, CaseIterable {
    case text
    case json
    case jsonl
    case srt

    static func parse(_ raw: String) -> OutputFormat? { OutputFormat(rawValue: raw) }
    static var known: String { allCases.map(\.rawValue).joined(separator: ", ") }
}

/// Collects finals and prints them in the requested shape.
///
/// `--out` exists because the bundle is launched by LaunchServices, which drops
/// stdout on the floor; the wrapper passes a file and echoes it back.
final class TerminalSink: EventSink {
    private let format: OutputFormat
    private let out: String?

    private(set) var text = ""
    private(set) var engine = ""
    private(set) var language: String?
    private var segments: [(startMs: Int64, endMs: Int64, text: String)] = []
    private var cues = 0
    private var originMs: Int64?
    private var finals = 0
    private var snores = 0

    init(format: OutputFormat, out: String?) {
        self.format = format
        self.out = out
    }

    func session(state: String, message: String?, engine: String?, language: String?, locale: String?, snore: String?) {
        if let engine, !engine.isEmpty { self.engine = engine }
        if let language { self.language = language }
        if format == .jsonl {
            var object: [String: Any] = ["type": "session", "state": state]
            if let engine { object["engine"] = engine }
            if let language { object["language"] = language }
            if let locale { object["locale"] = locale }
            if let snore { object["snore"] = snore }
            if let message { object["message"] = message }
            writeJSONLine(object)
            return
        }
        if state != "ready" {
            log("session \(state)\(message.map { ": \($0)" } ?? "")")
        }
    }

    func partial(_ text: String) {
        // `text` has no use for a rewording every 200 ms — the final carries the
        // same sentence, finished — but a watcher does: a partial is the only
        // evidence that the recogniser is alive before the first phrase closes.
        guard format == .jsonl else { return }
        writeJSONLine(["type": "partial", "text": text])
    }

    func final(text: String, startMs: Int64, endMs: Int64, engine: String, language: String) {
        self.engine = engine
        self.language = language
        self.text = joined(self.text, text)
        segments.append((startMs: startMs, endMs: max(endMs, startMs), text: text))
        if originMs == nil { originMs = startMs }
        if logTranscripts, format == .json || format == .text {
            log("text [\(startMs)..\(max(endMs, startMs))] \(text)")
        }

        switch format {
        case .text:
            write("\(text)\n", append: true)
        case .jsonl:
            finals += 1
            write(jsonLine(text: text, startMs: startMs, endMs: max(endMs, startMs)) + "\n", append: true)
        case .srt:
            cues += 1
            let from = srtTime(startMs - (originMs ?? startMs))
            let to = srtTime(max(endMs, startMs) - (originMs ?? startMs))
            write("\(cues)\n\(from) --> \(to)\n\(text)\n\n", append: true)
        case .json:
            break  // one object at the end
        }
    }

    func snore(_ episode: SnoreEpisode) {
        if format == .jsonl {
            snores += 1
            writeJSONLine([
                "type": "snore",
                "start_ms": episode.startMs,
                "end_ms": episode.endMs,
                "label": episode.label,
                "peak": Double(episode.peak),
                "mean": Double(episode.mean),
                "windows": episode.windows,
                "threshold": Double(episode.threshold),
            ])
        }
        // Snore annotations always go to the log too: a caller parsing
        // `--format text` must not find machine lines in the middle of prose, and
        // the log is where a night's episodes can be read back afterwards.
        log(String(
            format: "snore: %@ %.2f→%.2f s peak %.2f mean %.2f over %d windows",
            episode.label,
            Double(episode.startMs) / 1000.0,
            Double(episode.endMs) / 1000.0,
            episode.peak,
            episode.mean,
            episode.windows
        ))
    }

    func failed(_ message: String) {
        log("error: \(message)")
    }

    private func writeJSONLine(_ object: [String: Any]) {
        guard let data = try? JSONSerialization.data(
            withJSONObject: object, options: [.sortedKeys, .withoutEscapingSlashes]
        ) else { return }
        write(String(decoding: data, as: UTF8.self) + "\n", append: true)
    }

    /// Flushes whatever the format holds back. Idempotent enough to call once.
    func finish() {
        switch format {
        case .json:
            var object: [String: Any] = [
                "text": text,
                "engine": engine,
                "segments": segments.map { segment in
                    ["start_ms": segment.startMs, "end_ms": segment.endMs, "text": segment.text] as [String: Any]
                },
            ]
            if let language { object["language"] = language }
            writeJSON(object)
        case .text, .jsonl, .srt:
            break  // already streamed
        }
    }

    // MARK: - output

    private func jsonLine(text: String, startMs: Int64, endMs: Int64) -> String {
        var object: [String: Any] = [
            "text": text,
            "start_ms": startMs,
            "end_ms": endMs,
            "engine": engine.isEmpty ? NSNull() : engine,
            "language": language ?? NSNull(),
        ]
        object["type"] = "final"
        guard let data = try? JSONSerialization.data(withJSONObject: object, options: [.sortedKeys, .withoutEscapingSlashes])
        else { return "{}" }
        return String(decoding: data, as: UTF8.self)
    }

    private func writeJSON(_ object: [String: Any]) {
        guard let data = try? JSONSerialization.data(withJSONObject: object, options: [.sortedKeys, .withoutEscapingSlashes])
        else {
            log("cannot encode JSON")
            return
        }
        write(String(decoding: data, as: UTF8.self) + "\n", append: false)
    }

    /// `--out` holds the whole document, so it is written once at the end;
    /// stdout is a stream and is appended to as the night goes on.
    private func write(_ string: String, append: Bool) {
        guard let out else {
            FileHandle.standardOutput.write(Data(string.utf8))
            return
        }
        if append, let handle = FileHandle(forWritingAtPath: out) {
            handle.seekToEndOfFile()
            handle.write(Data(string.utf8))
            try? handle.close()
            return
        }
        try? Data(string.utf8).write(to: URL(fileURLWithPath: out))
    }

    private func srtTime(_ ms: Int64) -> String {
        let clamped = max(0, ms)
        let hours = clamped / 3_600_000
        let minutes = (clamped % 3_600_000) / 60_000
        let seconds = (clamped % 60_000) / 1000
        let millis = clamped % 1000
        return String(format: "%02lld:%02lld:%02lld,%03lld", hours, minutes, seconds, millis)
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

/// Used when the caller wants nothing but a return code: `--mode mic` in a
/// terminal that nobody is watching, or a bench run that only cares about the
/// log.
final class NullSink: EventSink {
    func session(state: String, message: String?, engine: String?, language: String?, locale: String?, snore: String?) {
        log("session \(state)\(message.map { ": \($0)" } ?? "")")
    }
    func partial(_ text: String) {}
    func final(text: String, startMs: Int64, endMs: Int64, engine: String, language: String) {
        log("final \(startMs)..\(max(endMs, startMs)): \(text)")
    }
    func snore(_ episode: SnoreEpisode) {
        log("snore \(episode.startMs)..\(episode.endMs) peak \(episode.peak)")
    }
    func failed(_ message: String) { log("error: \(message)") }
}
