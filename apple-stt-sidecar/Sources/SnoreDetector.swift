// SnoreDetector: SoundAnalysis on the same audio the recogniser hears.
//
// `SNAudioStreamAnalyzer` classifies fixed windows of audio (Apple's built-in
// classifier: 3 s by default, adjustable between roughly 0.5 s and 14.5 s) and
// reports the confidence of every label it knows. Its `snoring` label is what
// this lane is for.
//
// ## Why windows are aggregated into episodes
//
// Windows overlap by 50 %, so three seconds of snoring arrives as two or three
// results that all say "snoring" about the same breath. Forwarding each result
// would put a row per 1.5 s into the database for one snore, which is not a
// recording of what happened — it is a recording of what the classifier did.
// So consecutive windows above the threshold are merged into one episode, and
// the episode is what gets reported, once, when it ends.
//
// ## What the episode is not
//
// It is not a diagnosis, and the confidence in it is not an accuracy claim.
// `docs/verification/` has no labelled-night measurement for this classifier, so
// the number that crosses the wire is exactly what the model said and nothing
// more. The row it becomes carries `detector_version = apple-soundanalysis-v1`
// so it can never be mistaken for the server's own `rule-v1` annotation of the
// same audio.
//
// ## Threading
//
// Analysis runs on its own serial queue. The buffer is read-only and shared with
// the recogniser, which is fine: neither consumer mutates it. If analysis ever
// falls behind, blocks are dropped and counted — never queued forever, and never
// dropped quietly.

import AVFoundation
import CoreML
import CoreMedia
import Foundation
import SoundAnalysis

struct SnoreConfig {
    /// Width of the hysteresis band below `threshold`.
    static let hysteresis: Float = 0.2

    /// The classifier label to watch. `snoring` is what Apple's classifier calls
    /// it; a custom model may use anything.
    var label = "snoring"
    /// A window at or above this confidence opens an episode.
    ///
    /// 0.5 is where the measurement put it: across the 40 ESC-50 "snoring"
    /// clips in `testdata/steno-eval`, the best window per clip ran
    /// 0.63 … 0.98 (median 0.87). A window often holds only part of a snore, so
    /// the per-window score sits well below the clip's peak — 0.7 would have
    /// missed 10 of those 40. This is a candidate threshold, not an accuracy
    /// claim: nothing here has been measured against a negative corpus.
    var threshold: Float = 0.5
    /// Consecutive windows below `threshold - hysteresis` that end an episode.
    /// Two, because one quiet window inside a snore is a breath, not the end.
    var releaseWindows = 2
    /// `nil` keeps the classifier's own default window.
    var windowSeconds: Double?
    /// A compiled CoreML model (`.mlmodelc`) or a source model (`.mlmodel`).
    var modelPath: String?
    /// A continuous snore is cut into episodes of at most this long, so one
    /// night of snoring cannot become a single eight-hour row.
    var maxSeconds: Double = 30
}

struct SnoreEpisode {
    let startMs: Int64
    let endMs: Int64
    let label: String
    let peak: Float
    let mean: Float
    let windows: Int
    let threshold: Float
    /// `"normal"` when the episode closed on quiet audio, `"shutdown"` when the
    /// session ended first.
    let endReason: String
}

enum SnoreError: Error, CustomStringConvertible {
    case modelUnreadable(String)
    case requestFailed(String)
    case labelMissing(String, [String])
    case formatMismatch(String)

    var description: String {
        switch self {
        case .modelUnreadable(let detail): return "cannot load the snore model: \(detail)"
        case .requestFailed(let detail): return "cannot build a sound classification request: \(detail)"
        case .labelMissing(let label, let near):
            let hint = near.isEmpty ? "no label resembles it" : "closest labels: \(near.joined(separator: ", "))"
            return "the classifier has no label \"\(label)\" (\(hint))"
        case .formatMismatch(let detail): return "snore detector format mismatch: \(detail)"
        }
    }
}

final class SnoreDetector: NSObject, AudioConsumer, @unchecked Sendable {
    let label: String

    private let analyzer: SNAudioStreamAnalyzer
    private let request: SNClassifySoundRequest
    private let timeline: Timeline
    private let emit: (SnoreEpisode) -> Void
    private let queue = DispatchQueue(label: "dev.workbuddy.steno.snore", qos: .userInitiated)
    private let expectedFormat: AVAudioFormat
    private let maxSeconds: Double
    /// Consecutive quiet windows that end an episode.
    private let releaseWindows: Int

    /// Serialises the tracker and the counters. The observer and `submit` run on
    /// different threads.
    private let lock = NSLock()
    private var thresholdValue: Float
    private var episode: ActiveEpisode?
    private var quietRun = 0
    private var droppedBlocks = 0
    private var inFlight = 0
    private var analysedBlocks = 0
    private var added = false

    var threshold: Float {
        lock.lock()
        defer { lock.unlock() }
        return thresholdValue
    }

    /// The bottom of the hysteresis band. A window at or above this keeps an
    /// episode open even though it is below the entry threshold.
    private var release: Float { max(0, thresholdValue - SnoreConfig.hysteresis) }

    /// A block that cannot be analysed is not queued. 32 blocks is 640 ms of
    /// backlog — far more than a live stream ever needs (the probe runs at real
    /// time with a handful of drops at startup, all of them while the classifier
    /// warms up), and small enough that a producer that outruns analysis cannot
    /// grow memory for a whole night.
    ///
    /// Past this point the lane is behind, and being behind is *reported*: a
    /// silent drop would make "no snore episodes" indistinguishable from "we
    /// stopped listening".
    private static let maxInFlight = 32

    var name: String { "snore(\(label)@\(String(format: "%.2f", threshold)))" }

    private struct ActiveEpisode {
        var startMs: Int64
        var endMs: Int64
        var peak: Float
        var sum: Double
        var windows: Int
    }

    private init(
        analyzer: SNAudioStreamAnalyzer,
        request: SNClassifySoundRequest,
        expectedFormat: AVAudioFormat,
        timeline: Timeline,
        config: SnoreConfig,
        emit: @escaping (SnoreEpisode) -> Void
    ) {
        self.analyzer = analyzer
        self.request = request
        self.expectedFormat = expectedFormat
        self.timeline = timeline
        self.emit = emit
        self.label = config.label
        self.thresholdValue = config.threshold
        self.maxSeconds = config.maxSeconds
        self.releaseWindows = max(1, config.releaseWindows)
        super.init()
    }

    /// Builds the request and the analyzer. Throws with a message that names
    /// what is missing — a lane that cannot work must say so at startup, not
    /// produce silence all night.
    static func make(
        config: SnoreConfig,
        format: AVAudioFormat,
        timeline: Timeline,
        emit: @escaping (SnoreEpisode) -> Void
    ) throws -> SnoreDetector {
        let request: SNClassifySoundRequest
        if let path = config.modelPath {
            request = try loadRequest(modelPath: path)
        } else {
            do {
                request = try SNClassifySoundRequest(classifierIdentifier: .version1)
            } catch {
                throw SnoreError.requestFailed(error.localizedDescription)
            }
        }
        if let seconds = config.windowSeconds {
            request.windowDuration = CMTime(seconds: seconds, preferredTimescale: 16_000)
        }
        let known = request.knownClassifications
        guard known.contains(config.label) else {
            let needle = config.label.lowercased()
            let near = known
                .filter { $0.lowercased().contains(needle.prefix(4)) || needle.contains($0.lowercased().prefix(4)) }
                .prefix(8)
                .map { $0 }
            throw SnoreError.labelMissing(config.label, near)
        }
        let analyzer = SNAudioStreamAnalyzer(format: format)
        let detector = SnoreDetector(
            analyzer: analyzer,
            request: request,
            expectedFormat: format,
            timeline: timeline,
            config: config,
            emit: emit
        )
        log("snore: \(detector.name) window \(String(format: "%.2f", request.windowDuration.seconds)) s, overlap \(request.overlapFactor)")
        return detector
    }

    private static func loadRequest(modelPath: String) throws -> SNClassifySoundRequest {
        let url = URL(fileURLWithPath: modelPath)
        guard FileManager.default.fileExists(atPath: url.path) else {
            throw SnoreError.modelUnreadable("no such file \(url.path)")
        }
        do {
            let modelURL: URL
            if url.pathExtension == "mlmodel" {
                modelURL = try MLModel.compileModel(at: url)
            } else {
                modelURL = url
            }
            let model = try MLModel(contentsOf: modelURL)
            return try SNClassifySoundRequest(mlModel: model)
        } catch {
            throw SnoreError.modelUnreadable("\(url.lastPathComponent): \(error.localizedDescription)")
        }
    }

    // MARK: - AudioConsumer

    func start(format: AVAudioFormat) throws {
        guard format == expectedFormat else {
            throw SnoreError.formatMismatch("hub \(format) vs analyser \(expectedFormat)")
        }
        lock.lock()
        let already = added
        added = true
        lock.unlock()
        guard !already else { return }
        do {
            try analyzer.add(request, withObserver: self)
        } catch {
            throw SnoreError.requestFailed(error.localizedDescription)
        }
    }

    func submit(_ buffer: AVAudioPCMBuffer, frames: Range<Int64>) {
        lock.lock()
        if inFlight >= SnoreDetector.maxInFlight {
            droppedBlocks += 1
            let dropped = droppedBlocks
            lock.unlock()
            if dropped == 1 || dropped % 100 == 0 {
                log("snore: analysis is behind; \(dropped) blocks dropped")
            }
            return
        }
        inFlight += 1
        lock.unlock()

        // The explicit frame position is what makes a dropped block harmless to
        // everything after it: the analyser is told where this audio belongs
        // instead of assuming it follows the previous call.
        let position = AVAudioFramePosition(frames.lowerBound)
        queue.async { [weak self] in
            guard let self else { return }
            self.analyzer.analyze(buffer, atAudioFramePosition: position)
            self.lock.lock()
            self.inFlight -= 1
            self.analysedBlocks += 1
            self.lock.unlock()
        }
    }

    /// Closes an open episode when the session ends mid-snore. The audio is
    /// already on disk either way, so the episode is reported rather than
    /// thrown away — with an end reason that says why it stops where it does.
    func finish() async {
        await withCheckedContinuation { (continuation: CheckedContinuation<Void, Never>) in
            queue.async { [weak self] in
                guard let self else { return continuation.resume() }
                self.lock.lock()
                let pending = self.closeLocked(reason: "shutdown")
                let dropped = self.droppedBlocks
                let analysed = self.analysedBlocks
                self.lock.unlock()
                if let pending { self.emit(pending) }
                let total = analysed + dropped
                if dropped > 0, total > 0, dropped * 10 >= total {
                    log("snore: analysis could not keep up — \(dropped) of \(total) blocks were dropped, so the episode list for this session is incomplete")
                } else {
                    log("snore: \(analysed) blocks analysed, \(dropped) dropped")
                }
                self.analyzer.completeAnalysis()
                continuation.resume()
            }
        }
    }

    /// Live threshold change from a `config` frame. Applies to the next window;
    /// an episode already in flight keeps the threshold it opened with.
    func updateThreshold(_ value: Float) {
        lock.lock()
        thresholdValue = value
        lock.unlock()
        log("snore: threshold now \(value)")
    }

    // MARK: - results

    /// Folds one classified window into the episode state machine. Emits at most
    /// one episode, and does it outside the lock so a slow consumer cannot hold
    /// up the analyser.
    private func ingest(confidence: Float, windowStartMs: Int64, windowEndMs: Int64) {
        var closed: SnoreEpisode?

        lock.lock()
        if var active = episode {
            if confidence >= release {
                quietRun = 0
                active.endMs = max(active.endMs, windowEndMs)
                active.peak = max(active.peak, confidence)
                active.sum += Double(confidence)
                active.windows += 1
                episode = active
                if Double(active.endMs - active.startMs) / 1000.0 >= maxSeconds {
                    // A continuous snore: close at the episode limit and open the
                    // next one where this ended, so a night of snoring becomes
                    // many rows instead of one impossible one.
                    closed = closeLocked(reason: "normal")
                    episode = ActiveEpisode(
                        startMs: windowEndMs, endMs: windowEndMs, peak: confidence,
                        sum: Double(confidence), windows: 1
                    )
                }
            } else {
                quietRun += 1
                if quietRun >= releaseWindows {
                    closed = closeLocked(reason: "normal")
                }
            }
        } else if confidence >= thresholdValue {
            episode = ActiveEpisode(
                startMs: windowStartMs, endMs: windowEndMs, peak: confidence,
                sum: Double(confidence), windows: 1
            )
            quietRun = 0
        }
        lock.unlock()

        if let closed {
            emit(closed)
        }
    }
    /// Closes the open episode, if any. Caller holds the lock.
    private func closeLocked(reason: String) -> SnoreEpisode? {
        guard let active = episode, active.windows > 0 else {
            episode = nil
            quietRun = 0
            return nil
        }
        episode = nil
        quietRun = 0
        return SnoreEpisode(
            startMs: active.startMs,
            endMs: max(active.endMs, active.startMs),
            label: label,
            peak: active.peak,
            mean: Float(active.sum / Double(active.windows)),
            windows: active.windows,
            threshold: thresholdValue,
            endReason: reason
        )
    }

    // MARK: - device questions

    /// What this machine can actually do, for `--check`. Never throws: a machine
    /// that cannot classify is a fact to report, not a failure to hide.
    static func probe(config: SnoreConfig) -> [String: Any] {
        var report: [String: Any] = ["enabled": true, "label": config.label, "threshold": config.threshold]
        let request: SNClassifySoundRequest
        do {
            if let path = config.modelPath {
                request = try loadRequest(modelPath: path)
                report["model"] = path
            } else {
                request = try SNClassifySoundRequest(classifierIdentifier: .version1)
                report["model"] = "apple-classifier-v1"
            }
        } catch {
            report["available"] = false
            report["error"] = "\(error)"
            return report
        }
        let known = request.knownClassifications
        report["available"] = known.contains(config.label)
        report["labels_total"] = known.count
        report["label_present"] = known.contains(config.label)
        report["snore_like_labels"] = known.filter { $0.lowercased().contains("snor") }
        report["default_window_seconds"] = request.windowDuration.seconds
        report["window_constraint"] = "\(request.windowDurationConstraint)"
        return report
    }
}

// MARK: - observer

extension SnoreDetector: SNResultsObserving {
    func request(_ request: SNRequest, didProduce result: SNResult) {
        guard let classification = result as? SNClassificationResult,
              let confidence = classification.classification(forIdentifier: label)?.confidence
        else { return }
        logDebug("snore: window confidence \(String(format: "%.3f", confidence))")
        let range = classification.timeRange
        let startFrame = Int64((range.start.seconds * Double(Timeline.rate)).rounded())
        let endFrame = startFrame + Int64((range.duration.seconds * Double(Timeline.rate)).rounded())
        ingest(
            confidence: Float(confidence),
            windowStartMs: timeline.utcMs(forFrame: startFrame),
            windowEndMs: timeline.utcMs(forFrame: max(endFrame, startFrame))
        )
    }

    func request(_ request: SNRequest, didFailWithError error: Error) {
        // The recogniser is unaffected: this lane stops annotating and says so.
        // It must never look like the transcription side died, because it did
        // not, and a night of silent text would then be misdiagnosed.
        log("snore: analysis failed, lane is idle: \(error.localizedDescription)")
    }

    func requestDidComplete(_ request: SNRequest) {
        log("snore: analysis completed")
    }
}
