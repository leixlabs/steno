//! Scoring of detection results against labelled reference audio (tasks.md T8.6).
//!
//! `detect-wav` answers "what would the service store for this recording?". This
//! module answers the next question — "and is that right?" — by matching the
//! events a recording produced against the snoring a human labelled in it, and
//! reporting true/false positives and negatives plus the onset timing error.
//!
//! Nothing here re-runs detection: it consumes the [`EventDraft`]s that the real
//! [`crate::detector::DetectionPipeline`] produced, so the numbers describe the
//! same code path the live service uses.
//!
//! # What is counted
//!
//! * A **reference** is one thing a human marked: a snoring *interval*, a whole
//!   clip *known to contain* snoring, or a *control* clip known to be quiet.
//! * A reference is **matched** when at least one event overlaps it within the
//!   tolerance. A reference with no overlapping event is a **false negative**
//!   (a miss).
//! * A detected event that overlaps no reference is a **false positive**
//!   (a spurious event).
//!
//! This is the event-level convention, not sample coverage, because it answers
//! the product question directly: "snoring happened here — did an event fire?".
//! A single long event covering three snore bursts scores one hit, not three, so
//! recall is not inflated by a detector that never closes a candidate.
//!
//! # Honest limits
//!
//! TECH_SPEC §5.5 is explicit that synthetic fixtures prove implementation, not
//! accuracy, and that the rule detector is a *candidate* generator rather than a
//! classifier: it cannot separate every snore from every other sound. Precision
//! measured over a mixed corpus therefore describes the corpus, not the bedroom.
//! Recall on labelled snoring and the timing error are the numbers to watch while
//! calibrating; precision becomes meaningful once the negatives are the target
//! room's own quiet recordings.

use crate::detector::EventDraft;
use serde::{Deserialize, Serialize};

/// A time span in seconds, `start <= end`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Interval {
    pub start_s: f64,
    pub end_s: f64,
}

impl Interval {
    pub fn new(start_s: f64, end_s: f64) -> Self {
        Interval {
            start_s: start_s.min(end_s),
            end_s: start_s.max(end_s),
        }
    }

    /// True when the two spans overlap once each is widened by `tolerance_s`.
    ///
    /// Widening *both* sides means a single tolerance describes the acceptable
    /// onset jitter in either direction, which is what a reviewer wants: "the
    /// event fired within N ms of where I heard the snore".
    pub fn overlaps(&self, other: &Interval, tolerance_s: f64) -> bool {
        self.end_s >= other.start_s - tolerance_s && self.start_s <= other.end_s + tolerance_s
    }

    pub fn duration_s(&self) -> f64 {
        (self.end_s - self.start_s).max(0.0)
    }
}

/// Converts finalized events into seconds, using the clip's capture rate.
pub fn event_intervals(events: &[EventDraft], capture_rate_hz: u32) -> Vec<Interval> {
    let rate = f64::from(capture_rate_hz.max(1));
    events
        .iter()
        .map(|event| {
            Interval::new(
                event.start_offset_frames as f64 / rate,
                event.end_offset_frames as f64 / rate,
            )
        })
        .collect()
}

/// The ground truth for one clip.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum Reference {
    /// Snoring is expected somewhere in the clip, but it was not marked precisely.
    /// Any single event satisfies it; no event is a miss.
    Any,
    /// A control clip expected to be quiet: every event is a false positive.
    None,
    /// Explicitly marked snoring intervals, in seconds.
    Intervals,
}

/// One entry of a corpus manifest: a clip and what a human heard in it.
///
/// ```json
/// {
///   "file": "clips/1-20545-A-28.wav",
///   "snore": true,
///   "note": "ESC-50 snoring"
/// }
/// ```
///
/// A clip with `intervals_s` is scored interval-by-interval; otherwise `snore`
/// decides between a whole-clip positive and a control.
#[derive(Debug, Clone, Deserialize)]
pub struct ClipLabels {
    /// Path to the WAV, resolved relative to the manifest unless absolute.
    pub file: String,
    /// Whether the clip contains snoring (used when `intervals_s` is empty).
    #[serde(default)]
    pub snore: bool,
    /// Marked snoring intervals `[start_s, end_s]`; empty means clip-level.
    #[serde(default)]
    pub intervals_s: Vec<[f64; 2]>,
    /// Free-form note echoed into the report (source, environment, labeler).
    #[serde(default)]
    pub note: Option<String>,
}

impl ClipLabels {
    /// The ground-truth kind this entry describes.
    pub fn reference_kind(&self) -> Reference {
        if !self.intervals_s.is_empty() {
            Reference::Intervals
        } else if self.snore {
            Reference::Any
        } else {
            Reference::None
        }
    }

    /// The marked intervals, empty for clip-level entries.
    pub fn intervals(&self) -> Vec<Interval> {
        self.intervals_s
            .iter()
            .map(|pair| Interval::new(pair[0], pair[1]))
            .collect()
    }
}

/// A corpus manifest: the list of clips to score, in file order.
#[derive(Debug, Clone, Deserialize)]
pub struct Manifest {
    /// Optional human description echoed into the report.
    #[serde(default)]
    pub description: Option<String>,
    pub clips: Vec<ClipLabels>,
}

/// Per-clip scoring result.
#[derive(Debug, Clone, Serialize)]
pub struct ClipScore {
    pub file: String,
    pub reference_kind: Reference,
    /// Number of marked references (1 for a whole-clip positive, 0 for a control).
    pub references: usize,
    /// Number of detected events.
    pub detected: usize,
    /// References with at least one overlapping event (true positives).
    pub matched: usize,
    /// References with no overlapping event (false negatives).
    pub missed: usize,
    /// Events overlapping no reference (false positives).
    pub spurious: usize,
    /// Signed onset error in ms for each matched reference (event start − label
    /// start); empty for clip-level and control entries.
    pub onset_error_ms: Vec<f64>,
    /// Set when the clip could not be analysed (missing/unsupported file).
    pub error: Option<String>,
    pub note: Option<String>,
}

impl ClipScore {
    /// A clip that could not be read at all; counted separately from a miss.
    pub fn failed(file: &str, error: String, note: Option<String>) -> Self {
        ClipScore {
            file: file.to_string(),
            reference_kind: Reference::None,
            references: 0,
            detected: 0,
            matched: 0,
            missed: 0,
            spurious: 0,
            onset_error_ms: Vec::new(),
            error: Some(error),
            note,
        }
    }
}

/// Scores one clip's events against its ground truth.
///
/// `tolerance_s` widens every reference on both sides (see [`Interval::overlaps`]).
pub fn score_clip(labels: &ClipLabels, events: &[Interval], tolerance_s: f64) -> ClipScore {
    let kind = labels.reference_kind();
    let mut score = ClipScore {
        file: labels.file.clone(),
        reference_kind: kind.clone(),
        references: 0,
        detected: events.len(),
        matched: 0,
        missed: 0,
        spurious: 0,
        onset_error_ms: Vec::new(),
        error: None,
        note: labels.note.clone(),
    };

    match kind {
        Reference::None => {
            // A control: nothing is expected, so every event is spurious.
            score.spurious = events.len();
        }
        Reference::Any => {
            score.references = 1;
            if events.is_empty() {
                score.missed = 1;
            } else {
                score.matched = 1;
            }
        }
        Reference::Intervals => {
            let references = labels.intervals();
            score.references = references.len();
            for reference in &references {
                let mut best: Option<&Interval> = None;
                for event in events {
                    if event.overlaps(reference, tolerance_s) {
                        let closer = best.is_none_or(|current| {
                            (event.start_s - reference.start_s).abs()
                                < (current.start_s - reference.start_s).abs()
                        });
                        if closer {
                            best = Some(event);
                        }
                    }
                }
                match best {
                    Some(event) => {
                        score.matched += 1;
                        score
                            .onset_error_ms
                            .push((event.start_s - reference.start_s) * 1000.0);
                    }
                    None => score.missed += 1,
                }
            }
            let matched_events = events
                .iter()
                .filter(|event| {
                    references
                        .iter()
                        .any(|reference| event.overlaps(reference, tolerance_s))
                })
                .count();
            score.spurious = events.len() - matched_events;
        }
    }

    score
}

/// Aggregate scoring over a whole corpus.
#[derive(Debug, Clone, Serialize)]
pub struct Score {
    pub clips: Vec<ClipScore>,
    pub clips_scored: usize,
    pub clips_failed: usize,
    /// Sum of `matched` over clips (reference-based true positives).
    pub true_positives: usize,
    /// Sum of `spurious` over clips (event-based false positives).
    pub false_positives: usize,
    /// Sum of `missed` over clips (reference-based false negatives).
    pub false_negatives: usize,
    /// `None` when there is nothing to divide by (no events and no references).
    pub precision: Option<f64>,
    pub recall: Option<f64>,
    pub f1: Option<f64>,
    /// Mean signed onset error over matched references, in ms.
    pub mean_onset_error_ms: Option<f64>,
    pub median_onset_error_ms: Option<f64>,
    pub max_abs_onset_error_ms: Option<f64>,
}

impl Score {
    /// Folds per-clip scores into corpus totals and derived metrics.
    pub fn aggregate(clips: Vec<ClipScore>) -> Score {
        let clips_scored = clips.iter().filter(|clip| clip.error.is_none()).count();
        let clips_failed = clips.len() - clips_scored;
        let true_positives: usize = clips.iter().map(|clip| clip.matched).sum();
        let false_positives: usize = clips.iter().map(|clip| clip.spurious).sum();
        let false_negatives: usize = clips.iter().map(|clip| clip.missed).sum();

        let ratio = |numerator: usize, denominator: usize| {
            (denominator > 0).then(|| numerator as f64 / denominator as f64)
        };
        let precision = ratio(true_positives, true_positives + false_positives);
        let recall = ratio(true_positives, true_positives + false_negatives);
        let f1 = match (precision, recall) {
            (Some(p), Some(r)) if p + r > 0.0 => Some(2.0 * p * r / (p + r)),
            _ => None,
        };

        let mut errors: Vec<f64> = clips
            .iter()
            .flat_map(|clip| clip.onset_error_ms.iter().copied())
            .collect();
        let (mean_onset_error_ms, median_onset_error_ms, max_abs_onset_error_ms) =
            if errors.is_empty() {
                (None, None, None)
            } else {
                errors.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
                let mean = errors.iter().sum::<f64>() / errors.len() as f64;
                let median = if errors.len() % 2 == 1 {
                    errors[errors.len() / 2]
                } else {
                    (errors[errors.len() / 2 - 1] + errors[errors.len() / 2]) / 2.0
                };
                let max_abs = errors.iter().fold(0.0f64, |acc, e| acc.max(e.abs()));
                (Some(mean), Some(median), Some(max_abs))
            };

        Score {
            clips,
            clips_scored,
            clips_failed,
            true_positives,
            false_positives,
            false_negatives,
            precision,
            recall,
            f1,
            mean_onset_error_ms,
            median_onset_error_ms,
            max_abs_onset_error_ms,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(file: &str, snore: bool, intervals: &[[f64; 2]]) -> ClipLabels {
        ClipLabels {
            file: file.to_string(),
            snore,
            intervals_s: intervals.to_vec(),
            note: None,
        }
    }

    #[test]
    fn interval_reference_counts_hits_misses_and_spurious() {
        // Two marked snores; one event hits each, plus one extra event.
        let clip = labels("a.wav", true, &[[1.0, 2.0], [5.0, 6.0]]);
        let events = [
            Interval::new(1.1, 1.6),
            Interval::new(5.2, 5.8),
            Interval::new(9.0, 9.5),
        ];
        let score = score_clip(&clip, &events, 0.5);
        assert_eq!(score.references, 2);
        assert_eq!(score.matched, 2);
        assert_eq!(score.missed, 0);
        assert_eq!(score.spurious, 1);
        assert_eq!(score.onset_error_ms.len(), 2);
        assert!((score.onset_error_ms[0] - 100.0).abs() < 1e-9);
    }

    #[test]
    fn a_missed_reference_is_a_false_negative() {
        let clip = labels("a.wav", true, &[[1.0, 2.0], [5.0, 6.0]]);
        let events = [Interval::new(1.0, 1.5)];
        let score = score_clip(&clip, &events, 0.2);
        assert_eq!(score.matched, 1);
        assert_eq!(score.missed, 1);
        assert_eq!(score.spurious, 0);
    }

    #[test]
    fn tolerance_decides_the_boundary() {
        let clip = labels("a.wav", true, &[[1.0, 2.0]]);
        // Event starts 300 ms after the marked interval ends.
        let events = [Interval::new(2.3, 2.6)];
        assert_eq!(score_clip(&clip, &events, 0.5).matched, 1);
        assert_eq!(score_clip(&clip, &events, 0.1).matched, 0);
    }

    #[test]
    fn whole_clip_positive_needs_a_single_event() {
        let clip = labels("a.wav", true, &[]);
        assert_eq!(clip.reference_kind(), Reference::Any);
        assert_eq!(
            score_clip(&clip, &[Interval::new(3.0, 3.4)], 0.5).matched,
            1
        );
        let empty = score_clip(&clip, &[], 0.5);
        assert_eq!(empty.missed, 1);
        assert_eq!(empty.spurious, 0);
    }

    #[test]
    fn a_control_clip_makes_every_event_spurious() {
        let clip = labels("quiet.wav", false, &[]);
        assert_eq!(clip.reference_kind(), Reference::None);
        let score = score_clip(&clip, &[Interval::new(1.0, 1.2)], 0.5);
        assert_eq!(score.references, 0);
        assert_eq!(score.spurious, 1);
        assert_eq!(score.missed, 0);
    }

    #[test]
    fn aggregate_computes_precision_recall_and_f1() {
        // 2 TP, 1 FP, 1 FN  =>  precision 2/3, recall 2/3, F1 2/3.
        let hit = score_clip(
            &labels("a", true, &[[1.0, 2.0]]),
            &[Interval::new(1.0, 1.5)],
            0.1,
        );
        let miss = score_clip(&labels("b", true, &[[1.0, 2.0]]), &[], 0.1);
        let control = score_clip(&labels("c", false, &[]), &[Interval::new(0.0, 0.1)], 0.1);
        let score = Score::aggregate(vec![hit, miss, control]);
        assert_eq!(score.true_positives, 1);
        assert_eq!(score.false_negatives, 1);
        assert_eq!(score.false_positives, 1);
        assert!((score.precision.unwrap() - 0.5).abs() < 1e-9);
        assert!((score.recall.unwrap() - 0.5).abs() < 1e-9);
        assert!((score.f1.unwrap() - 0.5).abs() < 1e-9);
    }

    #[test]
    fn failed_clips_are_counted_separately_from_misses() {
        let failed = ClipScore::failed("broken.wav", "unsupported rate".into(), None);
        let score = Score::aggregate(vec![failed]);
        assert_eq!(score.clips_failed, 1);
        assert_eq!(score.clips_scored, 0);
        assert_eq!(score.false_negatives, 0);
    }

    #[test]
    fn manifest_parses_clip_level_and_interval_entries() {
        let text = r#"{
            "description": "smoke",
            "clips": [
                {"file": "a.wav", "snore": true},
                {"file": "b.wav", "snore": true, "intervals_s": [[0.5, 1.5], [3.0, 4.0]]},
                {"file": "c.wav", "snore": false, "note": "quiet room"}
            ]
        }"#;
        let manifest: Manifest = serde_json::from_str(text).unwrap();
        assert_eq!(manifest.clips.len(), 3);
        assert_eq!(manifest.clips[0].reference_kind(), Reference::Any);
        assert_eq!(manifest.clips[1].reference_kind(), Reference::Intervals);
        assert_eq!(manifest.clips[1].intervals().len(), 2);
        assert_eq!(manifest.clips[2].reference_kind(), Reference::None);
    }
}
