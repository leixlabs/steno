// Session: the pipeline and the framed loop that drives it.
//
// The pipeline is source-agnostic: build the two consumers, fan audio into them,
// drain at the end. `FramedSession` adds the socket-shaped concerns on top —
// reading frames, honouring `control`, applying a mid-session `config` — and is
// the only place either direction of the wire is interpreted.

import AVFoundation
import Foundation

struct PipelineConfig {
    var locale: String
    var engine: String
    var preset: TranscriptionPreset
    /// `nil` disables the snore lane.
    var snore: SnoreConfig?
}

final class Pipeline {
    let hub: AudioHub
    let transcriber: Transcriber
    let snore: SnoreDetector?
    let timeline: Timeline

    private init(hub: AudioHub, transcriber: Transcriber, snore: SnoreDetector?, timeline: Timeline) {
        self.hub = hub
        self.transcriber = transcriber
        self.snore = snore
        self.timeline = timeline
    }

    /// Builds both consumers against one timeline and one format.
    ///
    /// The recogniser picks the audio format it wants; the hub adopts it and the
    /// snore detector is built from it too. Two consumers, one buffer format,
    /// one clock — the alternative is two resamplers disagreeing about what
    /// "here" means.
    static func make(config: PipelineConfig, sink: EventSink) async throws -> Pipeline {
        let timeline = Timeline()
        let transcriber = try await Transcriber.make(
            localeIdentifier: config.locale,
            engine: config.engine,
            preset: config.preset,
            timeline: timeline,
            sink: sink
        )
        let format = transcriber.analysisFormat
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
        consumers.append(transcriber)
        // Named before `start()`, which is what puts it on the `ready` frame.
        transcriber.describeSnoreLane(detector?.name)
        return Pipeline(
            hub: AudioHub(format: format, timeline: timeline, consumers: consumers),
            transcriber: transcriber,
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
        await transcriber.flush()
    }

    func finish() async {
        await hub.finish()
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
        log("session started: \(pipeline.transcriber.engine) \(pipeline.transcriber.language)")
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
                engine: pipeline.transcriber.engine,
                language: pipeline.transcriber.language,
                locale: pipeline.transcriber.language,
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
        if let locale = config.locale, locale != pipeline.transcriber.language {
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
