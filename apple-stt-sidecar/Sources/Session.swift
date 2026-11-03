// Session: the pipeline and the framed loop that drives it.
//
// The pipeline is source-agnostic: build the consumers, fan audio into them,
// drain at the end. `FramedSession` adds the socket-shaped concerns on top —
// reading frames, honouring `control`, applying a mid-session `config` — and is
// the only place either direction of the wire is interpreted.
//
// `TranscriptBridge` sits between the two: it is where a backend's frame numbers
// become the recorder's wall clock. It exists so that a backend never has to
// know what time it is, and so that both lanes and every backend place a block
// through the same `Timeline`.

import AVFoundation
import Foundation

struct PipelineConfig {
    var backend: BackendID
    var locale: String
    var engine: String
    var preset: TranscriptionPreset
    /// `nil` disables the snore lane.
    var snore: SnoreConfig?
}

final class Pipeline {
    let hub: AudioHub
    let backend: TranscriptionBackend
    let snore: SnoreDetector?
    let timeline: Timeline

    private init(hub: AudioHub, backend: TranscriptionBackend, snore: SnoreDetector?, timeline: Timeline) {
        self.hub = hub
        self.backend = backend
        self.snore = snore
        self.timeline = timeline
    }

    /// Builds every consumer against one timeline and one format.
    ///
    /// The backend picks the audio format it wants; the hub adopts it and the
    /// snore detector is built from it too. Two consumers, one buffer format,
    /// one clock — the alternative is two resamplers disagreeing about what
    /// "here" means.
    static func make(config: PipelineConfig, sink: EventSink) async throws -> Pipeline {
        let timeline = Timeline()
        // Built first: the backend is handed this as its way out, and it is the
        // only object that will know the clock when a result arrives.
        let bridge = TranscriptBridge(timeline: timeline, sink: sink, engine: config.engine, language: config.locale)
        let backend = try await config.backend.make(
            locale: config.locale,
            engine: config.engine,
            preset: config.preset,
            target: bridge
        )
        let format = backend.audioFormat
        var consumers: [AudioConsumer] = []
        var detector: SnoreDetector?
        if let snoreConfig = config.snore {
            let built = try SnoreDetector.make(
                config: snoreConfig,
                format: format,
                timeline: timeline,
                emit: { episode in sink.snore(episode) }
            )
            detector = built
            consumers.append(built)
        }
        consumers.append(backend)
        // Named before `start()`, which is what puts it on the `ready` frame: the
        // lane is the session's fact, not the backend's.
        bridge.snoreLane = detector?.name
        log("backend: \(config.backend.rawValue) → \(backend.engine), \(backend.capabilities.summary)")
        return Pipeline(
            hub: AudioHub(format: format, timeline: timeline, consumers: consumers),
            backend: backend,
            snore: detector,
            timeline: timeline
        )
    }

    func start() throws {
        try hub.start()
    }

    func ingest(samples: [Int16], utcMs: Int64) {
        hub.ingest(samples: samples, utcMs: utcMs)
    }

    func ingest(buffer: AVAudioPCMBuffer, utcMs: Int64) {
        hub.ingest(buffer: buffer, utcMs: utcMs)
    }

    /// Commits volatile results without ending the session (`control: flush`).
    func flush() async {
        await backend.flush()
    }

    func finish() async {
        await hub.finish()
    }
}

// MARK: - frames to wall clock

/// The seam between a backend's numbering and the recorder's clock.
///
/// A backend says "this phrase is frames 48000..<64000"; this says "that was
/// 21:34:56.120 to 21:34:57.120" — through the one `Timeline` the hub advanced,
/// so a backend and the snore lane cannot disagree about when the same block
/// happened.
///
/// It also owns the two pieces of wire hygiene that are nobody else's business:
/// an empty final is not a phrase, and a partial that only rewords the last one
/// every 50 ms would fill the server's broadcast channel with duplicates.
///
/// `@unchecked Sendable`: `final`/`partial` arrive on the backend's results
/// thread, `ready` too, and the throttle state is behind `lock`.
final class TranscriptBridge: TranscriptTarget, @unchecked Sendable {
    private let timeline: Timeline
    private let sink: EventSink

    /// Set before `start()`, read by `ready`. Nil when the lane is off.
    var snoreLane: String?

    private let lock = NSLock()
    /// What a final is attributed to. The configured label and locale until
    /// `ready` says what the backend actually resolved — a final can only
    /// precede `ready` if the backend broke its contract, and then the label
    /// that was asked for is still a true statement about the request.
    private var engine: String
    private var language: String
    private var lastPartialText = ""
    private var lastPartialAt = Date.distantPast

    init(timeline: Timeline, sink: EventSink, engine: String, language: String) {
        self.timeline = timeline
        self.sink = sink
        self.engine = engine
        self.language = language
    }

    // MARK: - TranscriptTarget

    func ready(engine: String, language: String) {
        lock.lock()
        self.engine = engine
        self.language = language
        lock.unlock()
        sink.ready(engine: engine, language: language, locale: language, snore: snoreLane)
    }

    func partial(_ text: String) {
        guard !text.isEmpty else { return }
        lock.lock()
        let now = Date()
        let fresh = text != lastPartialText && now.timeIntervalSince(lastPartialAt) >= 0.2
        if fresh {
            lastPartialText = text
            lastPartialAt = now
        }
        lock.unlock()
        if fresh { sink.partial(text) }
    }

    func final(text: String, frames: Range<Int64>) {
        guard !text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else { return }
        let startMs = timeline.utcMs(forFrame: frames.lowerBound)
        let endMs = timeline.utcMs(forFrame: max(frames.upperBound, frames.lowerBound))
        lock.lock()
        // Copied under the lock so a `ready` arriving mid-final cannot split the
        // attribution of one phrase across two engines.
        let attributedTo = (engine: engine, language: language)
        lastPartialText = ""
        lock.unlock()
        sink.final(
            text: text,
            startMs: startMs,
            endMs: max(endMs, startMs),
            engine: attributedTo.engine,
            language: attributedTo.language
        )
    }

    func failed(_ message: String) {
        sink.failed(message)
    }
}

// MARK: - the framed loop

/// One connection's worth of state: the wire, the pipeline, and the promise that
/// `finish` runs exactly once no matter which side ended.
///
/// `@unchecked Sendable`: the read direction runs on its own thread while
/// `finish` runs on the concurrency pool, and the only mutable state (`finishing`,
/// the counters) is behind `lock`.
final class FramedSession: @unchecked Sendable {
    let channel: FrameChannel
    let sink: WireSink
    let pipeline: Pipeline

    private let lock = NSLock()
    private var finishing = false
    private var audioBlocks = 0
    private var controlFrames = 0

    private init(channel: FrameChannel, sink: WireSink, pipeline: Pipeline) {
        self.channel = channel
        self.sink = sink
        self.pipeline = pipeline
    }

    static func make(config: Options, channel: FrameChannel, sink: WireSink) async throws -> FramedSession {
        let pipeline = try await Pipeline.make(
            config: PipelineConfig(
                backend: config.backend,
                locale: config.locale,
                engine: config.engine,
                preset: config.preset,
                snore: config.snore
            ),
            sink: sink
        )
        return FramedSession(channel: channel, sink: sink, pipeline: pipeline)
    }

    func start() throws {
        try pipeline.start()
        log("session started: \(pipeline.backend.engine) \(pipeline.backend.language)")
    }

    func ingest(samples: [Int16], utcMs: Int64) {
        pipeline.ingest(samples: samples, utcMs: utcMs)
    }

    /// Reads until the peer closes or asks us to stop. Runs on its own thread:
    /// blocking here is the point, since nothing the analysers do may stall the
    /// socket.
    func readLoop() {
        while true {
            let frame: Frame?
            do {
                frame = try channel.next()
            } catch {
                log("read error: \(error)")
                return
            }
            guard let frame else { return }
            guard frame.direction == .rustToSwift else {
                log("protocol: uplink frame arrived on the downlink (\(frame.type)); closing")
                return
            }
            switch frame.type {
            case .audio:
                do {
                    let block = try AudioPayload.decode(frame.payload)
                    pipeline.ingest(samples: block.samples, utcMs: block.utcMs)
                    audioBlocks += 1
                } catch {
                    log("protocol: \(error)")
                    return
                }
            case .control:
                controlFrames += 1
                handleControl(frame.payload)
            case .config:
                handleConfig(frame.payload)
            case .snore, .transcript, .session:
                log("protocol: \(frame.type) is not something we accept; ignoring")
            }
        }
    }

    private func handleControl(_ payload: [UInt8]) {
        let control: ControlPayload
        do {
            control = try JSONPayload.decode(ControlPayload.self, from: payload)
        } catch {
            log("protocol: unreadable control payload: \(error)")
            return
        }
        switch control.action {
        case "stop":
            log("control: stop")
            // Ends the read direction only: the finals this triggers still have
            // to go out on the same socket.
            channel.shutdownRead()
        case "flush":
            log("control: flush")
            Task { await self.pipeline.flush() }
        case "status":
            lock.lock()
            let blocks = audioBlocks
            let controls = controlFrames
            lock.unlock()
            sink.session(
                state: "status",
                message: "\(blocks) audio blocks, \(controls) control frames, \(pipeline.timeline.audioFrames) frames fed",
                engine: pipeline.backend.engine,
                language: pipeline.backend.language,
                locale: pipeline.backend.language,
                snore: pipeline.snore?.name
            )
        default:
            log("control: unknown action \"\(control.action)\"; expected stop, flush or status")
        }
    }

    /// A config frame after the session started. The threshold is live; the
    /// things that would mean rebuilding the analyser are refused out loud
    /// rather than accepted and ignored.
    private func handleConfig(_ payload: [UInt8]) {
        let config: ConfigPayload
        do {
            config = try JSONPayload.decode(ConfigPayload.self, from: payload)
        } catch {
            log("protocol: unreadable config payload: \(error)")
            return
        }
        if let threshold = config.snoreThreshold {
            if let snore = pipeline.snore {
                snore.updateThreshold(threshold)
            } else {
                log("config: snore threshold \(threshold) ignored; the lane is off")
            }
        }
        if config.snore == false, pipeline.snore != nil {
            log("config: snore=false ignored mid-session; restart the sidecar to disable the lane")
        }
        if let locale = config.locale, locale != pipeline.backend.language {
            log("config: locale \(locale) ignored mid-session; restart the sidecar to switch languages")
        }
        if let preset = config.preset {
            log("config: preset \(preset) ignored mid-session")
        }
        if let window = config.snoreWindow {
            log("config: snore window \(window) ignored mid-session")
        }
    }

    /// Drains the pipeline and reports why. Safe to call from both the control
    /// path and the end of the read loop; only the first call does anything.
    func finish(reason: String) async {
        let (shouldFinish, blocks) = beginFinish()
        guard shouldFinish else { return }
        await pipeline.finish()
        log("session finished after \(blocks) audio blocks (\(reason)); \(pipeline.timeline.audioFrames) frames, \(pipeline.timeline.discontinuities) discontinuities")
    }

    /// Synchronous on purpose: taking a lock inside an `async` function is a
    /// hazard the compiler rightly complains about, so the state transition
    /// happens here and the waiting happens in `finish`.
    private func beginFinish() -> (Bool, Int) {
        lock.lock()
        defer { lock.unlock() }
        guard !finishing else { return (false, 0) }
        finishing = true
        return (true, audioBlocks)
    }
}
