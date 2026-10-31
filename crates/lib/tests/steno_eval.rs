//! End-to-end scoring test for `steno-eval` (tasks.md T8.6).
//!
//! This drives the whole chain the tool uses — a WAV file through the real
//! [`DetectionPipeline`] via `detect_file`, into seconds, into [`score_clip`]. It
//! checks that a burst placed at a known time is reported as a hit at that time,
//! while a quiet control stays clean. The audio is synthetic and deterministic,
//! so per TECH_SPEC §5.5 this proves the wiring and the offset mapping, not
//! real-world accuracy; accuracy comes from the labelled corpus in
//! `testdata/steno-eval/`.
//!
//! [`DetectionPipeline`]: steno_lib::detector::DetectionPipeline

use steno_lib::config::DetectorConfig;
use steno_lib::detect_wav::detect_file;
use steno_lib::eval::{ClipLabels, Reference, Score, event_intervals, score_clip};
use std::f64::consts::PI;
use std::path::PathBuf;

/// Writes `seconds` of `signal` at `rate` Hz, 1 channel, S16, and returns the path.
fn write_wav(
    dir: &tempfile::TempDir,
    name: &str,
    rate: u32,
    seconds: f64,
    signal: impl Fn(f64) -> f32,
) -> PathBuf {
    let path = dir.path().join(name);
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: rate,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(&path, spec).unwrap();
    for n in 0..(seconds * rate as f64) as usize {
        let t = n as f64 / rate as f64;
        let value = (signal(t) * 32767.0).clamp(-32768.0, 32767.0) as i16;
        writer.write_sample(value).unwrap();
    }
    writer.finalize().unwrap();
    path
}

/// A snore-band burst (300 Hz, 0.4 amplitude) between `start_s` and `end_s`.
fn burst(start_s: f64, end_s: f64) -> impl Fn(f64) -> f32 {
    move |t: f64| {
        if (start_s..end_s).contains(&t) {
            (2.0 * PI * 300.0 * t).sin() as f32 * 0.4
        } else {
            0.0
        }
    }
}

/// Detector settings that reach detection quickly on a short clip: the live
/// service warms the noise floor for 10 s, which a 3 s fixture never survives.
fn short_clip_config() -> DetectorConfig {
    DetectorConfig {
        noise_floor_window_seconds: 1,
        noise_floor_min_seconds: 0,
        filter_warmup_ms: 0,
        ..DetectorConfig::default()
    }
}

fn labels(file: &str, snore: bool, intervals: &[[f64; 2]]) -> ClipLabels {
    ClipLabels {
        file: file.to_string(),
        snore,
        intervals_s: intervals.to_vec(),
        note: None,
    }
}

/// The product question, end to end: snoring at 1.0-1.5 s must be reported as an
/// event at roughly that time.
#[test]
fn a_labelled_burst_is_detected_at_the_marked_time() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_wav(&dir, "night.wav", 16_000, 3.0, burst(1.0, 1.5));

    let result = detect_file(&path, short_clip_config(), 0, chrono::Utc::now()).unwrap();
    let events = event_intervals(&result.events, result.capture_rate_hz);

    let clip = labels("night.wav", true, &[[1.0, 1.5]]);
    let score = score_clip(&clip, &events, 0.05);
    assert_eq!(clip.reference_kind(), Reference::Intervals);
    assert_eq!(score.matched, 1, "the marked snore must be detected");
    assert_eq!(score.missed, 0, "no marked snore may be missed");
    assert_eq!(
        score.spurious, 0,
        "a single burst must not yield extra events"
    );
    let onset = score.onset_error_ms[0];
    assert!(onset.abs() <= 50.0, "onset error {onset} ms exceeds 50 ms");
}

/// A quiet clip is a clean control: no events, so nothing is spurious.
#[test]
fn a_quiet_control_clip_produces_no_events() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_wav(&dir, "quiet.wav", 16_000, 3.0, |_| 0.0);

    let result = detect_file(&path, short_clip_config(), 0, chrono::Utc::now()).unwrap();
    let events = event_intervals(&result.events, result.capture_rate_hz);

    let clip = labels("quiet.wav", false, &[]);
    let score = score_clip(&clip, &events, 0.5);
    assert_eq!(clip.reference_kind(), Reference::None);
    assert_eq!(score.detected, 0, "silence must not produce events");
    assert_eq!(
        score.spurious, 0,
        "silence must not produce false positives"
    );
}

/// Aggregating over a positive and a control yields perfect precision and recall
/// when detection behaves; this is the shape of the corpus report.
#[test]
fn corpus_aggregate_reports_precision_and_recall() {
    let dir = tempfile::tempdir().unwrap();
    let snore = write_wav(&dir, "snore.wav", 16_000, 3.0, burst(1.0, 1.5));
    let quiet = write_wav(&dir, "quiet.wav", 16_000, 3.0, |_| 0.0);

    let mut scores = Vec::new();
    for (path, clip) in [
        (&snore, labels("snore.wav", true, &[[1.0, 1.5]])),
        (&quiet, labels("quiet.wav", false, &[])),
    ] {
        let result = detect_file(path, short_clip_config(), 0, chrono::Utc::now()).unwrap();
        let events = event_intervals(&result.events, result.capture_rate_hz);
        scores.push(score_clip(&clip, &events, 0.05));
    }

    let score = Score::aggregate(scores);
    assert_eq!(score.true_positives, 1);
    assert_eq!(score.false_positives, 0);
    assert_eq!(score.false_negatives, 0);
    assert_eq!(score.precision, Some(1.0));
    assert_eq!(score.recall, Some(1.0));
    assert_eq!(score.f1, Some(1.0));
}
