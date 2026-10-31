// Timeline: the one map between audio position and the recorder's clock.
//
// Consumers see two different clocks. `SNAudioStreamAnalyzer` reports a
// `CMTimeRange` on its own audio timeline and `SpeechTranscriber` reports a
// `CMTimeRange` on its — both start at zero with the first sample and neither
// knows what time it is. Every frame from the wire carries the wall clock of its
// first sample, stamped by the recorder. This is the map between them.
//
// Frames, not seconds, are the unit: a block is a whole number of samples, the
// map is `utc_ms + (frame_delta * 1000 / 16000)`, and the result is exact at the
// sample the recorder wrote. Seconds-as-Double would accumulate rounding over a
// nine-hour night.
//
// ## Gaps
//
// A dropped block or a node reconnect leaves a hole in the wire. Padding it with
// silence would fabricate audio that was never recorded, and the whole timeline
// after the hole would be wrong. Instead the map re-anchors at the next frame
// that does arrive: before the hole audio time runs one way, after it another,
// and the hole stays a hole. Consumers are unaffected — they only ever ask
// "which wall clock is frame N", and they always get a truthful answer.

import Foundation

final class Timeline: @unchecked Sendable {
    /// The rate every consumer is fed at. Fixed by `AudioHub`'s format.
    static let rate: Int64 = 16_000

    /// Points are trimmed to this many; a night with a flapping node would
    /// otherwise grow the array for hours.
    private static let maxPoints = 64
    /// A frame whose wall clock disagrees with the projection by more than this
    /// is a discontinuity, not jitter. Recorder timestamps are exact (they come
    /// from the capture clock), so anything above a quarter second is a hole.
    static let discontinuityMs: Int64 = 250

    private let lock = NSLock()
    private var points: [(frame: Int64, utcMs: Int64)] = []
    private var framesFed: Int64 = 0
    private(set) var discontinuities = 0

    /// Records a block and returns the frame range it occupies. Called exactly
    /// once per block, by the hub, before any consumer sees it.
    func advance(frames count: Int, utcMs: Int64) -> Range<Int64> {
        guard count > 0 else { return 0..<0 }
        lock.lock()
        defer { lock.unlock() }
        let start = framesFed
        let end = start + Int64(count)
        if let last = points.last {
            let projected = project(last, to: start)
            let drift = utcMs - projected
            if abs(drift) > Timeline.discontinuityMs {
                discontinuities += 1
                logDebug("timeline: \(drift) ms discontinuity at frame \(start); re-anchoring")
                append((frame: start, utcMs: utcMs))
            }
        } else {
            append((frame: start, utcMs: utcMs))
        }
        framesFed = end
        return start..<end
    }

    /// Wall clock of the first sample of `frame`. Before the first anchor the
    /// earliest known point is used, which is what makes a result that starts a
    /// few milliseconds before the first block still resolve instead of
    /// returning zero.
    func utcMs(forFrame frame: Int64) -> Int64 {
        lock.lock()
        defer { lock.unlock() }
        return projectLocked(frame)
    }

    func utcMs(forSeconds seconds: Double) -> Int64 {
        utcMs(forFrame: Int64((seconds * Double(Timeline.rate)).rounded()))
    }

    var audioFrames: Int64 {
        lock.lock()
        defer { lock.unlock() }
        return framesFed
    }

    private func projectLocked(_ frame: Int64) -> Int64 {
        guard let anchor = points.last(where: { $0.frame <= frame }) ?? points.first else {
            return 0
        }
        return project(anchor, to: frame)
    }

    private func project(_ anchor: (frame: Int64, utcMs: Int64), to frame: Int64) -> Int64 {
        let delta = frame - anchor.frame
        return anchor.utcMs + (delta * 1000) / Timeline.rate
    }

    private func append(_ point: (frame: Int64, utcMs: Int64)) {
        points.append(point)
        if points.count > Timeline.maxPoints {
            points.removeFirst(points.count - Timeline.maxPoints)
        }
    }
}
