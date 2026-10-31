//! `steno-eval`: score detected events against labelled reference audio (tasks.md T8.6).
//!
//! Where `detect-wav` reports what the service *would* store, this tool asks
//! whether that is right. It reads a corpus manifest — a list of WAV clips and
//! what a human heard in each — replays every clip through the *same*
//! [`steno_lib::detector::DetectionPipeline`] the service uses, and reports
//! true/false positives and negatives plus the onset timing error, so a change to
//! a threshold can be judged against labelled snoring instead of by ear alone.
//!
//! It needs no microphone, no database and no running service. The scoring rules
//! live in [`steno_lib::eval`]; this file only does I/O and presentation.
//!
//! See `docs/operations/eval-workflow.md` for the workflow and
//! `testdata/steno-eval/` for a ready-to-fetch corpus of real snoring.

use clap::Parser;
use serde::Serialize;
use steno_lib::config::{Config, DetectorConfig};
use steno_lib::detect_wav::{detect_file, file_anchor};
use steno_lib::eval::{ClipScore, Manifest, Score, event_intervals, score_clip};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

#[derive(Debug, Parser)]
#[command(
    name = "steno-eval",
    version,
    about = "Score snore-detection events against labelled reference audio.",
    long_about = "Replays every clip in a corpus manifest through the exact detection pipeline the service \
uses and compares the events against the snoring a human labelled, reporting true/false positives and \
negatives, precision/recall/F1 and the onset timing error. Needs no microphone, no database and no \
running service.\n\n\
The manifest is JSON: a `clips` array whose entries carry `file` and either `snore` (a whole-clip \
judgement) or `intervals_s` (marked snoring spans, in seconds). See testdata/steno-eval/labels.json.",
    after_help = "EXAMPLES:\n  \
steno-eval --manifest testdata/steno-eval/labels.json --noise-floor-min-seconds 0 --noise-floor-window-seconds 5\n  \
steno-eval --manifest labels.json --tolerance-ms 300 --verbose\n  \
steno-eval --manifest labels.json --config /etc/steno-monitor/config.toml --band-ratio-min 0.45 --json\n  \
steno-eval --manifest labels.json --json > report.json"
)]
struct Cli {
    /// Corpus manifest (JSON) listing clips and their ground truth.
    #[arg(long, value_name = "PATH")]
    manifest: PathBuf,

    /// Directory that relative `file` paths resolve against. Defaults to the
    /// manifest's own directory.
    #[arg(long, value_name = "DIR")]
    base_dir: Option<PathBuf>,

    /// Take detection defaults from a service configuration file; explicit flags still win.
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,

    /// How far a detected event may sit from a marked span and still count as a hit.
    #[arg(long, value_name = "MS", default_value_t = 500)]
    tolerance_ms: u64,

    /// Print one line per clip in addition to the summary.
    #[arg(long, action = clap::ArgAction::SetTrue)]
    verbose: bool,

    /// Emit the full report as JSON instead of a text table.
    #[arg(long, action = clap::ArgAction::SetTrue)]
    json: bool,

    /// Channel the detector listens to. Defaults to audio.mono_channel_index
    /// from --config, else 0.
    #[arg(long, value_name = "INDEX")]
    mono_channel: Option<u16>,

    #[command(flatten)]
    overrides: DetectionOverrides,
}

/// The subset of `[detector]` keys that matters when tuning against short
/// reference clips. Unset values come from `--config` or the built-in default.
#[derive(Debug, Default, clap::Args)]
struct DetectionOverrides {
    /// Override detector.absolute_floor_dbfs.
    #[arg(long, value_name = "DBFS")]
    absolute_floor_dbfs: Option<f32>,
    /// Override detector.noise_margin_db.
    #[arg(long, value_name = "DB")]
    noise_margin_db: Option<f32>,
    /// Override detector.band_margin_db.
    #[arg(long, value_name = "DB")]
    band_margin_db: Option<f32>,
    /// Override detector.band_ratio_min.
    #[arg(long, value_name = "RATIO")]
    band_ratio_min: Option<f32>,
    /// Override detector.min_event_ms.
    #[arg(long, value_name = "MS")]
    min_event_ms: Option<u32>,
    /// Override detector.hangover_frames.
    #[arg(long, value_name = "FRAMES")]
    hangover_frames: Option<u32>,
    /// Override detector.merge_gap_ms.
    #[arg(long, value_name = "MS")]
    merge_gap_ms: Option<u32>,
    /// Override detector.noise_floor_window_seconds. Short clips need a short window.
    #[arg(long, value_name = "SECONDS")]
    noise_floor_window_seconds: Option<u32>,
    /// Override detector.noise_floor_min_seconds. Short clips need this at 0 so
    /// the floor is trusted before the clip ends.
    #[arg(long, value_name = "SECONDS")]
    noise_floor_min_seconds: Option<u32>,
    /// Override detector.filter_warmup_ms.
    #[arg(long, value_name = "MS")]
    filter_warmup_ms: Option<u32>,
}

impl DetectionOverrides {
    fn apply(&self, config: &mut DetectorConfig) -> Vec<String> {
        let mut applied = Vec::new();
        macro_rules! apply {
            ($field:ident, $target:ident) => {
                if let Some(value) = self.$field {
                    config.$target = value;
                    applied.push(format!("{}={}", stringify!($target), value));
                }
            };
        }
        apply!(absolute_floor_dbfs, absolute_floor_dbfs);
        apply!(noise_margin_db, noise_margin_db);
        apply!(band_margin_db, band_margin_db);
        apply!(band_ratio_min, band_ratio_min);
        apply!(min_event_ms, min_event_ms);
        apply!(hangover_frames, hangover_frames);
        apply!(merge_gap_ms, merge_gap_ms);
        apply!(noise_floor_window_seconds, noise_floor_window_seconds);
        apply!(noise_floor_min_seconds, noise_floor_min_seconds);
        apply!(filter_warmup_ms, filter_warmup_ms);
        applied
    }
}

#[derive(Debug)]
enum ToolError {
    Message(String),
}

impl std::fmt::Display for ToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ToolError::Message(text) => f.write_str(text),
        }
    }
}

impl std::error::Error for ToolError {}

type ToolResult<T> = Result<T, ToolError>;

fn fail<T>(message: impl Into<String>) -> ToolResult<T> {
    Err(ToolError::Message(message.into()))
}

fn main() -> ExitCode {
    match run(&Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(ToolError::Message(message)) => {
            eprintln!("steno-eval: error: {message}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: &Cli) -> ToolResult<()> {
    let manifest = load_manifest(&cli.manifest)?;
    if manifest.clips.is_empty() {
        return fail(format!("{} lists no clips", cli.manifest.display()));
    }
    let base_dir = cli
        .base_dir
        .clone()
        .or_else(|| cli.manifest.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."));

    let mut detector = DetectorConfig::default();
    let mut configured_channel = 0u16;
    if let Some(path) = &cli.config {
        let config = Config::load(path).map_err(|error| {
            ToolError::Message(format!("cannot use {}: {error}", path.display()))
        })?;
        configured_channel = config.audio.mono_channel_index;
        detector = config.detector.clone();
    }
    let overridden = cli.overrides.apply(&mut detector);
    detector.validate_standalone().map_err(ToolError::Message)?;
    let channel = cli.mono_channel.unwrap_or(configured_channel);
    let tolerance_s = cli.tolerance_ms as f64 / 1000.0;

    let mut clip_scores = Vec::with_capacity(manifest.clips.len());
    for labels in &manifest.clips {
        let path = resolve(&base_dir, &labels.file);
        match detect_file(&path, detector.clone(), channel, file_anchor(&path)) {
            Ok(result) => {
                let events = event_intervals(&result.events, result.capture_rate_hz);
                clip_scores.push(score_clip(labels, &events, tolerance_s));
            }
            Err(error) => {
                clip_scores.push(ClipScore::failed(
                    &labels.file,
                    error.to_string(),
                    labels.note.clone(),
                ));
            }
        }
    }

    let score = Score::aggregate(clip_scores);
    if cli.json {
        print_json(cli, &manifest, &score, &overridden)?;
    } else {
        print_text(cli, &manifest, &score, &overridden);
    }

    // A failed clip is an infrastructure problem (missing/unsupported file), not
    // a detection miss, so it makes the run unsuccessful rather than the score bad.
    if score.clips_failed > 0 {
        return fail(format!(
            "{} of {} clip(s) could not be analysed",
            score.clips_failed,
            score.clips.len()
        ));
    }
    Ok(())
}

/// Resolves a manifest `file` against `base_dir`, leaving absolute paths alone.
fn resolve(base_dir: &Path, file: &str) -> PathBuf {
    let path = Path::new(file);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base_dir.join(path)
    }
}

fn load_manifest(path: &Path) -> ToolResult<Manifest> {
    let text = std::fs::read_to_string(path)
        .map_err(|error| ToolError::Message(format!("cannot read {}: {error}", path.display())))?;
    serde_json::from_str(&text).map_err(|error| {
        ToolError::Message(format!(
            "{} is not a valid manifest: {error}",
            path.display()
        ))
    })
}

/// Counts how many clips are positives and how many are controls, for the header.
fn clip_tally(manifest: &Manifest) -> (usize, usize) {
    let positives = manifest
        .clips
        .iter()
        .filter(|clip| !clip.intervals_s.is_empty() || clip.snore)
        .count();
    (positives, manifest.clips.len() - positives)
}

fn print_text(cli: &Cli, manifest: &Manifest, score: &Score, overridden: &[String]) {
    let (positives, controls) = clip_tally(manifest);
    if let Some(description) = &manifest.description {
        println!("corpus: {description}");
    }
    println!(
        "steno-eval: {} clip(s) ({} snore, {} control), tolerance +/-{} ms",
        score.clips.len(),
        positives,
        controls,
        cli.tolerance_ms
    );
    if !overridden.is_empty() {
        println!("overrides: {}", overridden.join(" "));
    }

    if cli.verbose {
        println!();
        println!(
            "{:<44} {:>9} {:>4} {:>4} {:>4} {:>4}  {:>12}",
            "file", "kind", "ref", "det", "hit", "fp", "onset_ms"
        );
        for clip in &score.clips {
            if let Some(error) = &clip.error {
                println!("{:<44} {:>9}  {}", truncate(&clip.file, 44), "ERROR", error);
                continue;
            }
            let onset = match clip.onset_error_ms.len() {
                0 => "-".to_string(),
                1 => format!("{:.0}", clip.onset_error_ms[0]),
                n => format!(
                    "{:.0} (n={n})",
                    clip.onset_error_ms.iter().sum::<f64>() / n as f64
                ),
            };
            println!(
                "{:<44} {:>9} {:>4} {:>4} {:>4} {:>4}  {:>12}",
                truncate(&clip.file, 44),
                kind_name(clip),
                clip.references,
                clip.detected,
                clip.matched,
                clip.spurious,
                onset
            );
        }
    }

    println!();
    println!(
        "scored {} clip(s), {} failed",
        score.clips_scored, score.clips_failed
    );
    println!(
        "true positives={}  false positives={}  false negatives={}",
        score.true_positives, score.false_positives, score.false_negatives
    );
    println!(
        "precision={}  recall={}  f1={}",
        fmt_metric(score.precision),
        fmt_metric(score.recall),
        fmt_metric(score.f1)
    );
    match (
        score.mean_onset_error_ms,
        score.median_onset_error_ms,
        score.max_abs_onset_error_ms,
    ) {
        (Some(mean), Some(median), Some(max_abs)) => {
            println!("onset error (ms): mean={mean:.0} median={median:.0} max|.|={max_abs:.0}")
        }
        _ => println!("onset error (ms): n/a (no matched interval-level references)"),
    }
}

fn kind_name(clip: &ClipScore) -> &'static str {
    use steno_lib::eval::Reference;
    match clip.reference_kind {
        Reference::Any => "clip",
        Reference::None => "control",
        Reference::Intervals => "interval",
    }
}

fn truncate(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        text.to_string()
    } else {
        let keep = width.saturating_sub(1);
        let mut out: String = text.chars().take(keep).collect();
        out.push('…');
        out
    }
}

fn fmt_metric(value: Option<f64>) -> String {
    value.map_or_else(|| "n/a".to_string(), |v| format!("{v:.3}"))
}

#[derive(Serialize)]
struct JsonReport<'a> {
    tool: &'static str,
    manifest: String,
    tolerance_ms: u64,
    description: Option<&'a str>,
    overrides: &'a [String],
    score: &'a Score,
}

fn print_json(
    cli: &Cli,
    manifest: &Manifest,
    score: &Score,
    overridden: &[String],
) -> ToolResult<()> {
    let report = JsonReport {
        tool: "steno-eval",
        manifest: cli.manifest.display().to_string(),
        tolerance_ms: cli.tolerance_ms,
        description: manifest.description.as_deref(),
        overrides: overridden,
        score,
    };
    let text = serde_json::to_string_pretty(&report)
        .map_err(|error| ToolError::Message(format!("cannot encode report: {error}")))?;
    println!("{text}");
    Ok(())
}
