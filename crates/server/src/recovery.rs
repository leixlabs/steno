//! Startup recovery scan (TECH_SPEC §4.2, task T3.6).
//!
//! Runs before the capture pipeline starts, so it is the one place where a
//! `.partial` file is known not to be receiving writes. Two things happen:
//!
//! 1. Every `.partial` left by a crash or a shutdown timeout is truncated to a
//!    whole frame, its header is patched to the real length, renamed to `.wav`,
//!    and described by one `recordings` row. There is no status column to mark:
//!    a file that made it to disk is simply a recording, and its own header says
//!    how much of it is real.
//! 2. A `.partial` whose header cannot be validated is left exactly as it is —
//!    it is never published — and reported for a human to look at.
//!
//! A light reconciliation follows: a recording row whose file is gone, and a WAV
//! file no row mentions, are both reported. Nothing is deleted here; deleting
//! audio is retention's job, and it only deletes rows it can identify.
//!
//! Capacity is checked by the caller afterwards (T3.6), once the scan has made
//! the on-disk inventory accurate.

use steno_lib::error::{Error, Result};
use crate::recorder::{recover_partial, validate_relative_path};
use crate::storage::{Database, RecordingDraft};
use chrono::{DateTime, NaiveDateTime, Utc};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

/// A `.partial` that could not be validated. It stays on disk, unpublished.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnverifiablePartial {
    pub path: PathBuf,
    pub reason: String,
}

/// Summary of one startup scan, suitable for logging and for the status page.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecoveryReport {
    /// `.partial` files repaired and published as recordings.
    pub recovered: Vec<String>,
    /// `.partial` files left untouched because they could not be validated.
    pub unverifiable: Vec<UnverifiablePartial>,
    /// WAV files with no matching row. They are reported, never deleted here.
    pub orphans: Vec<PathBuf>,
    /// Recording ids whose row promises audio that is no longer on disk.
    pub missing: Vec<String>,
}

impl RecoveryReport {
    pub fn is_clean(&self) -> bool {
        self.unverifiable.is_empty() && self.orphans.is_empty() && self.missing.is_empty()
    }
}

/// Scans `recordings_root`, repairs partials and reconciles them with `db`.
///
/// The scan is deliberately conservative: audio is only ever published when its
/// header and length agree, and nothing is deleted.
pub fn scan(recordings_root: &Path, db: &Database) -> Result<RecoveryReport> {
    if !recordings_root.is_absolute() {
        return Err(Error::Storage(format!(
            "recordings root must be absolute: {}",
            recordings_root.display()
        )));
    }
    let mut report = RecoveryReport::default();
    repair_partials(recordings_root, db, &mut report)?;
    reconcile(recordings_root, db, &mut report)?;
    Ok(report)
}

/// Step 1/2: repair every `.partial` below the root.
fn repair_partials(
    recordings_root: &Path,
    db: &Database,
    report: &mut RecoveryReport,
) -> Result<()> {
    for partial in collect_with_extension(recordings_root, "partial")? {
        // `name.wav.partial` belongs to `name.wav`; a partial with any other
        // shape is not ours to guess about.
        if partial.extension().is_none_or(|ext| ext != "partial") {
            continue;
        }
        let Some(stem_is_wav) = partial
            .file_stem()
            .and_then(|s| Path::new(s).extension())
            .map(|e| e == "wav")
        else {
            continue;
        };
        if !stem_is_wav {
            report.unverifiable.push(UnverifiablePartial {
                path: partial.clone(),
                reason: "partial does not follow the <name>.wav.partial convention".into(),
            });
            continue;
        }
        match recover_partial(&partial) {
            // `recover_partial` reads the 44-byte header, cuts a half-written
            // frame, patches both RIFF lengths and renames to `.wav`, so the row
            // below describes the file that is actually on disk.
            Ok((final_path, frames, size_bytes, format)) => {
                let started_at = match started_at_from_path(&partial) {
                    Some(started) => started,
                    None => {
                        // The file is now publishable, but we cannot name its
                        // start time without inventing one.
                        let id = recording_id_from_path(&partial);
                        report.orphans.push(final_path);
                        tracing::warn!(file = %partial.display(), %id, "recovered WAV has no parseable start timestamp; left for manual review");
                        continue;
                    }
                };
                let id = recording_id_from_path(&partial);
                let relative = relative_to(recordings_root, &final_path)?;
                let ended_at = started_at
                    + chrono::Duration::from_std(frame_duration(frames, format.sample_rate_hz))
                        .unwrap_or_else(|_| chrono::Duration::zero());
                db.insert_recording(&RecordingDraft {
                    id: id.clone(),
                    relative_path: relative,
                    started_at_utc: started_at,
                    ended_at_utc: ended_at,
                    capture_rate_hz: format.sample_rate_hz,
                    channels: format.channels,
                    frame_count: frames,
                    size_bytes,
                    // Nothing is known to have been dropped: the only bytes that
                    // never made it are the half frame just cut, and that is not
                    // a frame the timeline ever promised.
                    lost_frames: 0,
                })?;
                report.recovered.push(id);
            }
            Err(error) => {
                report.unverifiable.push(UnverifiablePartial {
                    path: partial.clone(),
                    reason: error.to_string(),
                });
            }
        }
    }
    Ok(())
}

/// Step 3: report rows without files and files without rows.
fn reconcile(recordings_root: &Path, db: &Database, report: &mut RecoveryReport) -> Result<()> {
    // Keep both identities of each file: the absolute path (for reporting) and
    // the DB-relative path (for matching against rows).
    let mut files: Vec<(PathBuf, PathBuf)> = Vec::new();
    for path in collect_with_extension(recordings_root, "wav")? {
        let relative = relative_to(recordings_root, &path)?;
        files.push((path, relative));
    }
    let mut claimed: HashSet<PathBuf> = HashSet::new();
    for row in db.recordings_oldest_first()? {
        if files.iter().any(|(_, stored)| *stored == row.relative_path) {
            claimed.insert(row.relative_path);
        } else {
            report.missing.push(row.id);
        }
    }
    for (path, stored) in files {
        if !claimed.contains(&stored) {
            report.orphans.push(path);
        }
    }
    Ok(())
}

/// Collects every file below `root` with the given extension.
fn collect_with_extension(root: &Path, extension: &str) -> Result<Vec<PathBuf>> {
    let mut found = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            // A per-day directory that is already gone is not an error worth
            // failing startup over; the scan is a best-effort reconciliation.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(Error::io(&dir, error)),
        };
        for entry in entries {
            let entry = entry.map_err(|e| Error::io(&dir, e))?;
            let path = entry.path();
            let file_type = entry.file_type().map_err(|e| Error::io(&path, e))?;
            if file_type.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|ext| ext == extension) {
                found.push(path);
            }
        }
    }
    found.sort();
    Ok(found)
}

/// The DB-relative path of a file found under `recordings_root`.
///
/// `relative_path` in `recordings` is exactly `recording_dir`-relative — the
/// recorder writes `recording_dir.join(relative_path)`, and both playback
/// (`http_server::get_audio`) and retention do the same. This function must
/// mirror that one join, nothing more.
fn relative_to(root: &Path, path: &Path) -> Result<PathBuf> {
    let relative = path.strip_prefix(root).map_err(|_| {
        Error::Storage(format!(
            "{} is outside the recordings root {}",
            path.display(),
            root.display()
        ))
    })?;
    let stored = relative.to_path_buf();
    validate_relative_path(&stored)?;
    Ok(stored)
}

/// Parses the UTC start time embedded in `<YYYYMMDDTHHMMSS.mmmZ>_<id>.wav`.
fn started_at_from_path(path: &Path) -> Option<DateTime<Utc>> {
    let name = path.file_stem()?.to_str()?;
    let (timestamp, _) = name.split_once('_')?;
    let naive = NaiveDateTime::parse_from_str(timestamp, "%Y%m%dT%H%M%S%.3fZ").ok()?;
    Some(naive.and_utc())
}

/// Recovers the recording id embedded in the file name. The file name is
/// `<UTC-start>_<id>.wav.partial`, so the id is everything after the first
/// underscore with the WAV extensions stripped. Falls back to the bare stem so a
/// repaired file still gets a stable identity.
fn recording_id_from_path(path: &Path) -> String {
    let stem = path
        .file_stem()
        .and_then(|s| Path::new(s).file_stem())
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    match stem.split_once('_') {
        Some((_, id)) if !id.is_empty() => id.to_string(),
        _ => stem,
    }
}

fn frame_duration(frames: u64, rate_hz: u32) -> std::time::Duration {
    if rate_hz == 0 {
        return std::time::Duration::ZERO;
    }
    std::time::Duration::from_secs_f64(frames as f64 / f64::from(rate_hz))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recorder::{WavFormat, read_header, write_header};
    use chrono::{NaiveDate, NaiveTime, TimeZone};
    use tempfile::tempdir;

    /// Writes a `.partial` holding `frames` whole mono S16 frames followed by a
    /// partial frame of `tail_bytes` bytes, with a header that declares the full
    /// (untruncated) length — exactly the state a crash leaves behind.
    fn write_partial(root: &Path, relative: &Path, frames: u64, tail_bytes: u64) -> PathBuf {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let data_bytes = frames * 2 + tail_bytes;
        let bytes = vec![0u8; (44 + data_bytes) as usize];
        fs::write(&path, &bytes).unwrap();
        let mut file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        write_header(&mut file, &WavFormat::new(16_000, 1).unwrap(), data_bytes).unwrap();
        drop(file);
        path
    }

    #[test]
    fn truncated_partial_is_repaired_and_published_as_a_recording() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("recordings");
        fs::create_dir_all(&root).unwrap();
        let db = Database::in_memory().unwrap();
        // 2 whole mono S16 frames plus a trailing partial frame that must be cut.
        // The recorder writes `recording_dir.join(relative_path)` and the stored
        // path already starts with `recordings/`, so the file sits one level
        // deeper than the directory name suggests.
        write_partial(
            &root,
            Path::new("recordings/2025/01/02/20250102T010203.000Z_rec1.wav.partial"),
            2,
            1,
        );

        let report = scan(&root, &db).unwrap();

        assert_eq!(report.recovered, vec!["rec1".to_string()]);
        assert!(report.is_clean(), "a repaired file needs no human: {report:?}");
        assert!(
            !root
                .join("recordings/2025/01/02/20250102T010203.000Z_rec1.wav.partial")
                .exists()
        );
        let recovered = root.join("recordings/2025/01/02/20250102T010203.000Z_rec1.wav");
        assert!(recovered.exists());
        // The partial frame was cut, so the header now declares exactly 2 frames.
        let (format, declared) = read_header(&recovered).unwrap();
        assert_eq!(format.sample_rate_hz, 16_000);
        assert_eq!(declared, 4);
        let row = db.recording("rec1").unwrap().unwrap();
        assert_eq!(row.frame_count, 2);
        assert_eq!(row.size_bytes, 48);
        assert_eq!(row.capture_rate_hz, 16_000);
        assert_eq!(row.channels, 1);
        assert_eq!(row.lost_frames, 0);
        assert_eq!(
            row.started_at_utc,
            Utc.with_ymd_and_hms(2025, 1, 2, 1, 2, 3).unwrap()
        );
        // 2 frames at 16 kHz is 125 µs.
        assert_eq!(
            (row.ended_at_utc - row.started_at_utc).num_microseconds(),
            Some(125)
        );
        // The stored path stays relative and keeps the recordings-root prefix.
        assert_eq!(
            row.relative_path,
            PathBuf::from("recordings/2025/01/02/20250102T010203.000Z_rec1.wav")
        );
    }

    #[test]
    fn unreadable_partial_is_left_in_place_and_reported() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("recordings");
        fs::create_dir_all(root.join("recordings/2025/01/02")).unwrap();
        let db = Database::in_memory().unwrap();
        let partial = root.join("recordings/2025/01/02/20250102T010203.000Z_rec2.wav.partial");
        fs::write(
            &partial,
            b"not a wav header at all, but long enough to read 44 bytes!!",
        )
        .unwrap();

        let report = scan(&root, &db).unwrap();

        assert!(report.recovered.is_empty());
        assert_eq!(report.unverifiable.len(), 1);
        assert_eq!(report.unverifiable[0].path, partial);
        assert!(
            partial.exists(),
            "an unverifiable partial must never be renamed away"
        );
        assert!(
            db.recording("rec2").unwrap().is_none(),
            "nothing may be published for it"
        );
        assert!(!report.is_clean(), "an unverifiable partial needs a human");
    }

    #[test]
    fn orphan_wav_and_missing_row_are_both_reported() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("recordings");
        fs::create_dir_all(root.join("recordings/2025/01/02")).unwrap();
        let db = Database::in_memory().unwrap();
        // A row whose file never existed.
        let started = DateTime::from_naive_utc_and_offset(
            NaiveDateTime::new(
                NaiveDate::from_ymd_opt(2025, 1, 2).unwrap(),
                NaiveTime::from_hms_opt(1, 2, 3).unwrap(),
            ),
            Utc,
        );
        db.insert_recording(&RecordingDraft {
            id: "gone".into(),
            relative_path: PathBuf::from("recordings/2025/01/02/20250102T010203.000Z_gone.wav"),
            started_at_utc: started,
            ended_at_utc: started,
            capture_rate_hz: 16_000,
            channels: 1,
            frame_count: 10,
            size_bytes: 64,
            lost_frames: 0,
        })
        .unwrap();
        // A file no row mentions.
        let orphan = root.join("recordings/2025/01/02/20250102T040506.000Z_orphan.wav");
        fs::write(&orphan, b"whatever").unwrap();

        let report = scan(&root, &db).unwrap();

        assert_eq!(report.missing, vec!["gone".to_string()]);
        assert_eq!(report.orphans, vec![orphan]);
        assert!(!report.is_clean());
    }

    /// Writes a finished WAV at `root.join(relative)`: a 44-byte header that
    /// declares exactly the data that follows it.
    fn write_finished(root: &Path, relative: &Path, declared_bytes: u64) -> PathBuf {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, vec![0u8; (44 + declared_bytes) as usize]).unwrap();
        let mut file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        write_header(&mut file, &WavFormat::new(16_000, 1).unwrap(), declared_bytes).unwrap();
        path
    }

    /// The regression that matters: recovery and the recorder must agree on what
    /// `recording_dir` means. When they did not, every row failed to match its own
    /// file after a restart — rows looked missing and files looked orphaned.
    #[test]
    fn a_file_the_recorder_wrote_is_matched_to_its_row() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("recordings");
        fs::create_dir_all(&root).unwrap();
        let db = Database::in_memory().unwrap();

        let started = Utc.with_ymd_and_hms(2025, 1, 2, 3, 4, 0).unwrap();
        let relative = crate::recorder::recording_relative_path(started, "rec-recorder");
        write_finished(&root, &relative, 8);
        db.insert_recording(&RecordingDraft {
            id: "rec-recorder".into(),
            relative_path: relative,
            started_at_utc: started,
            ended_at_utc: started + chrono::Duration::milliseconds(2),
            capture_rate_hz: 16_000,
            channels: 1,
            frame_count: 4,
            size_bytes: 52,
            lost_frames: 0,
        })
        .unwrap();

        let report = scan(&root, &db).unwrap();

        assert!(
            report.orphans.is_empty(),
            "the recorder's own file must be recognised: {:?}",
            report.orphans
        );
        assert!(report.missing.is_empty(), "the row still owns its file");
        assert!(report.is_clean());
    }

    #[test]
    fn scan_is_clean_when_nothing_needs_repair() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("recordings");
        fs::create_dir_all(&root).unwrap();
        let db = Database::in_memory().unwrap();
        let report = scan(&root, &db).unwrap();
        assert!(report.is_clean());
    }
}
