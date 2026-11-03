//! Typed queries against the SQLite file (TECH_SPEC §7, task T4.1).
//!
//! The schema is not declared here. Every table lives in a versioned file under
//! `migrations/`, applied by [`crate::migrate`]; this module only reads and
//! writes rows in the shape those files define.
//!
//! Connections are opened per owning worker/thread. WAL and foreign keys are
//! enabled for each connection; audio callbacks never touch this module.

use steno_lib::error::{Error, Result};
use chrono::{DateTime, NaiveDate, Utc};
use rusqlite::{params, Connection, OptionalExtension, Row};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewStatus {
    Unreviewed,
    Snore,
    NotSnore,
}

impl ReviewStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            ReviewStatus::Unreviewed => "unreviewed",
            ReviewStatus::Snore => "snore",
            ReviewStatus::NotSnore => "not_snore",
        }
    }
}

impl TryFrom<&str> for ReviewStatus {
    type Error = String;
    fn try_from(value: &str) -> std::result::Result<Self, Self::Error> {
        match value {
            "unreviewed" => Ok(ReviewStatus::Unreviewed),
            "snore" => Ok(ReviewStatus::Snore),
            "not_snore" => Ok(ReviewStatus::NotSnore),
            _ => Err(format!("invalid review_status {value:?}")),
        }
    }
}

/// An annotation produced outside the server's own detector — today, the
/// sidecar's `SoundAnalysis` snore lane.
///
/// The range is wall-clock, which is the whole point: the analyser knows what it
/// heard and when, not which file and which frame.
#[derive(Debug, Clone)]
pub struct ClassifiedEvent {
    pub started_at_utc: DateTime<Utc>,
    pub ended_at_utc: DateTime<Utc>,
    /// Classifier label, e.g. `snoring`.
    pub label: String,
    /// Peak confidence across the windows that made up the episode.
    pub confidence: f32,
    /// The confidence needed to open the episode.
    pub threshold: f32,
    /// Written into `source`: for a classifier row it names the thing that
    /// judged the audio.
    pub source: String,
}

impl ClassifiedEvent {
    /// The same annotation in the stored shape: a classifier label with its peak
    /// confidence, and no rule score to invent.
    ///
    /// `recording_id` is supplied by the caller because the file is written when
    /// the recording closes, which can be after the analyser reported.
    pub fn episode_row(&self, recording_id: Option<String>) -> EpisodeRow {
        EpisodeRow {
            id: external_episode_id(),
            recording_id,
            started_at_utc: self.started_at_utc,
            ended_at_utc: self.ended_at_utc,
            source: self.source.clone(),
            label: Some(self.label.clone()),
            confidence: Some(self.confidence),
            review_status: ReviewStatus::Unreviewed,
            created_at_utc: Utc::now(),
        }
    }
}

#[derive(Debug)]
pub struct Database {
    conn: Connection,
}

/// Pool-level totals for the control centre: what is on disk and how many
/// recordings it is made of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PoolSummary {
    pub recordings: u64,
    pub bytes: u64,
}

impl Database {
    /// Opens a connection, enables foreign keys/WAL/busy timeout and migrates.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
        }
        let conn = Connection::open(path)?;
        let db = Database { conn };
        db.configure(Duration::from_secs(5))?;
        db.migrate()?;
        Ok(db)
    }

    /// Opens an in-memory DB for deterministic tests.
    pub fn in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        let db = Database { conn };
        db.configure(Duration::from_secs(5))?;
        db.migrate()?;
        Ok(db)
    }

    fn configure(&self, busy_timeout: Duration) -> Result<()> {
        self.conn.busy_timeout(busy_timeout)?;
        self.conn.execute_batch("PRAGMA foreign_keys = ON; PRAGMA journal_mode = WAL;")?;
        Ok(())
    }

    /// Brings the file up to the schema this binary ships.
    ///
    /// The DDL is not here: every statement lives in `migrations/*.sql` and is
    /// applied by [`crate::migrate`], which also records what it ran in the
    /// `_sqlx_migrations` table `sqlx migrate info` reads. A database newer than
    /// the binary is refused, not downgraded.
    pub fn migrate(&self) -> Result<()> {
        crate::migrate::apply(&self.conn)
    }

    pub fn connection(&self) -> &Connection {
        &self.conn
    }

    pub fn insert_recording(&self, recording: &RecordingDraft) -> Result<()> {
        self.conn.execute(
            "INSERT INTO recordings (
                id, relative_path, started_at_utc, ended_at_utc, capture_rate_hz,
                channels, frame_count, size_bytes, lost_frames, created_at_utc
             ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
            params![
                recording.id,
                recording.relative_path.to_string_lossy(),
                recording.started_at_utc.to_rfc3339(),
                recording.ended_at_utc.to_rfc3339(),
                recording.capture_rate_hz,
                recording.channels,
                i64::try_from(recording.frame_count).unwrap_or(i64::MAX),
                i64::try_from(recording.size_bytes).unwrap_or(i64::MAX),
                i64::try_from(recording.lost_frames).unwrap_or(i64::MAX),
                Utc::now().to_rfc3339(),
            ],
        )?;
        // Annotations are written the moment they are produced, which is before
        // the file they belong to closes; this is the only place that attaches
        // them. It runs inside the same call as the insert so a recording row can
        // never exist while its annotations still point at nothing.
        self.link_annotations(
            &recording.id,
            recording.started_at_utc,
            recording.ended_at_utc,
        )?;
        Ok(())
    }

    /// Attaches unlinked annotations to a recording by wall clock.
    ///
    /// Only rows whose *start* falls inside the file: an episode that straddles the
    /// boundary belongs to the file it began in, which is the one a reader will
    /// play.
    fn link_annotations(
        &self,
        recording_id: &str,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<usize> {
        let mut linked = self.conn.execute(
            "UPDATE episodes SET recording_id = ?1
              WHERE recording_id IS NULL AND started_at_utc >= ?2 AND started_at_utc < ?3",
            params![recording_id, from.to_rfc3339(), to.to_rfc3339()],
        )?;
        linked += self.conn.execute(
            "UPDATE transcripts SET recording_id = ?1
              WHERE recording_id IS NULL AND started_at_utc >= ?2 AND started_at_utc < ?3",
            params![recording_id, from.to_rfc3339(), to.to_rfc3339()],
        )?;
        Ok(linked)
    }

    pub fn insert_episode(&self, episode: &EpisodeRow) -> Result<()> {
        self.conn.execute(
            "INSERT INTO episodes (
                id, recording_id, started_at_utc, ended_at_utc, source, label,
                confidence, review_status, created_at_utc
             ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![
                episode.id,
                episode.recording_id,
                episode.started_at_utc.to_rfc3339(),
                episode.ended_at_utc.to_rfc3339(),
                episode.source,
                episode.label,
                episode.confidence.map(f64::from),
                episode.review_status.as_str(),
                episode.created_at_utc.to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    pub fn insert_transcript(&self, transcript: &TranscriptRow) -> Result<()> {
        self.conn.execute(
            "INSERT INTO transcripts (
                id, recording_id, started_at_utc, ended_at_utc, text, engine,
                language, created_at_utc
             ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
            params![
                transcript.id,
                transcript.recording_id,
                transcript.started_at_utc.to_rfc3339(),
                transcript.ended_at_utc.to_rfc3339(),
                transcript.text,
                transcript.engine,
                transcript.language,
                transcript.created_at_utc.to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    /// Recordings that start inside the window, oldest first.
    pub fn recordings_between(
        &self,
        start_utc: DateTime<Utc>,
        end_utc: DateTime<Utc>,
    ) -> Result<Vec<RecordingRow>> {
        let mut statement = self.conn.prepare(
            "SELECT id, relative_path, started_at_utc, ended_at_utc, capture_rate_hz,
                    channels, frame_count, size_bytes, lost_frames, created_at_utc
             FROM recordings
             WHERE started_at_utc >= ?1 AND started_at_utc < ?2
             ORDER BY started_at_utc, id",
        )?;
        let rows = statement
            .query_map(params![start_utc.to_rfc3339(), end_utc.to_rfc3339()], map_recording)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn recording(&self, id: &str) -> Result<Option<RecordingRow>> {
        self.conn
            .query_row(
                "SELECT id, relative_path, started_at_utc, ended_at_utc, capture_rate_hz,
                        channels, frame_count, size_bytes, lost_frames, created_at_utc
                 FROM recordings WHERE id=?1",
                [id],
                map_recording,
            )
            .optional()
            .map_err(Error::from)
    }

    /// Every recording in start order. Retention deletes from the front, so this
    /// is its candidate list.
    pub fn recordings_oldest_first(&self) -> Result<Vec<RecordingRow>> {
        let mut statement = self
            .conn
            .prepare("SELECT * FROM recordings ORDER BY started_at_utc, id")?;
        let rows = statement.query_map([], map_recording)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Error::from)
    }

    /// Removes the row. `episodes.recording_id` / `transcripts.recording_id` are
    /// `ON DELETE SET NULL`, so an annotation outlives the audio it described.
    pub fn delete_recording(&self, id: &str) -> Result<bool> {
        let changed = self
            .conn
            .execute("DELETE FROM recordings WHERE id=?1", [id])?;
        Ok(changed > 0)
    }

    pub fn episode(&self, id: &str) -> Result<Option<EpisodeRow>> {
        self.conn
            .query_row("SELECT * FROM episodes WHERE id=?1", [id], map_episode)
            .optional()
            .map_err(Error::from)
    }

    /// Episodes that start inside the window, oldest first.
    pub fn episodes_between(
        &self,
        start_utc: DateTime<Utc>,
        end_utc: DateTime<Utc>,
    ) -> Result<Vec<EpisodeRow>> {
        let mut statement = self.conn.prepare(
            "SELECT * FROM episodes
             WHERE started_at_utc >= ?1 AND started_at_utc < ?2
             ORDER BY started_at_utc, id",
        )?;
        let rows = statement
            .query_map(params![start_utc.to_rfc3339(), end_utc.to_rfc3339()], map_episode)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn set_episode_review(&self, id: &str, status: ReviewStatus) -> Result<bool> {
        let changed = self.conn.execute(
            "UPDATE episodes SET review_status=?2 WHERE id=?1",
            params![id, status.as_str()],
        )?;
        Ok(changed > 0)
    }

    /// Phrases that start inside the window, oldest first.
    /// How many annotations fall inside a window: episodes plus transcripts.
    ///
    /// The recorder asks this when it closes a file, to decide whether the file is
    /// worth keeping. Zero means nothing was said and nothing sounded.
    pub fn count_annotations(&self, from: DateTime<Utc>, to: DateTime<Utc>) -> Result<u64> {
        let episodes: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM episodes
              WHERE started_at_utc >= ?1 AND started_at_utc < ?2",
            params![from.to_rfc3339(), to.to_rfc3339()],
            |row| row.get(0),
        )?;
        let transcripts: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM transcripts
              WHERE started_at_utc >= ?1 AND started_at_utc < ?2",
            params![from.to_rfc3339(), to.to_rfc3339()],
            |row| row.get(0),
        )?;
        Ok((episodes + transcripts) as u64)
    }

    pub fn transcripts_between(
        &self,
        start_utc: DateTime<Utc>,
        end_utc: DateTime<Utc>,
    ) -> Result<Vec<TranscriptRow>> {
        let mut statement = self.conn.prepare(
            "SELECT id, recording_id, started_at_utc, ended_at_utc, text, engine,
                    language, created_at_utc
             FROM transcripts
             WHERE started_at_utc >= ?1 AND started_at_utc < ?2
             ORDER BY started_at_utc, id",
        )?;
        let rows = statement
            .query_map(
                params![start_utc.to_rfc3339(), end_utc.to_rfc3339()],
                map_transcript,
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// One query for the control centre: how many recordings and how many bytes
    /// they hold.
    pub fn pool_summary(&self) -> Result<PoolSummary> {
        let (recordings, bytes): (i64, i64) = self.conn.query_row(
            "SELECT COUNT(*), COALESCE(SUM(size_bytes),0) FROM recordings",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        Ok(PoolSummary {
            recordings: recordings.max(0) as u64,
            bytes: bytes.max(0) as u64,
        })
    }
}

/// One written WAV file: a stretch of audio and the facts it was recorded with.
#[derive(Debug, Clone, Serialize)]
pub struct RecordingRow {
    pub id: String,
    pub relative_path: PathBuf,
    pub started_at_utc: DateTime<Utc>,
    pub ended_at_utc: DateTime<Utc>,
    pub capture_rate_hz: u32,
    pub channels: u16,
    pub frame_count: u64,
    pub size_bytes: u64,
    pub lost_frames: u64,
    pub created_at_utc: DateTime<Utc>,
}

/// A finished recording as the recorder hands it to the writer.
#[derive(Debug, Clone)]
pub struct RecordingDraft {
    pub id: String,
    pub relative_path: PathBuf,
    pub started_at_utc: DateTime<Utc>,
    pub ended_at_utc: DateTime<Utc>,
    pub capture_rate_hz: u32,
    pub channels: u16,
    pub frame_count: u64,
    pub size_bytes: u64,
    pub lost_frames: u64,
}

/// One snore, from either detector.
#[derive(Debug, Clone, Serialize)]
pub struct EpisodeRow {
    pub id: String,
    pub recording_id: Option<String>,
    pub started_at_utc: DateTime<Utc>,
    pub ended_at_utc: DateTime<Utc>,
    /// `rule-v1` or `apple-soundanalysis-v1`.
    pub source: String,
    pub label: Option<String>,
    pub confidence: Option<f32>,
    pub review_status: ReviewStatus,
    pub created_at_utc: DateTime<Utc>,
}

/// One recognised phrase.
#[derive(Debug, Clone, Serialize)]
pub struct TranscriptRow {
    pub id: String,
    pub recording_id: Option<String>,
    pub started_at_utc: DateTime<Utc>,
    pub ended_at_utc: DateTime<Utc>,
    pub text: String,
    pub engine: String,
    pub language: Option<String>,
    pub created_at_utc: DateTime<Utc>,
}

fn map_recording(row: &Row<'_>) -> rusqlite::Result<RecordingRow> {
    Ok(RecordingRow {
        id: row.get(0)?,
        relative_path: PathBuf::from(row.get::<_, String>(1)?),
        started_at_utc: parse_utc(&row.get::<_, String>(2)?),
        ended_at_utc: parse_utc(&row.get::<_, String>(3)?),
        capture_rate_hz: u32::try_from(row.get::<_, i64>(4)?.max(0)).unwrap_or(0),
        channels: u16::try_from(row.get::<_, i64>(5)?.max(0)).unwrap_or(0),
        frame_count: row.get::<_, i64>(6)?.max(0) as u64,
        size_bytes: row.get::<_, i64>(7)?.max(0) as u64,
        lost_frames: row.get::<_, i64>(8)?.max(0) as u64,
        created_at_utc: parse_utc(&row.get::<_, String>(9)?),
    })
}

fn map_episode(row: &Row<'_>) -> rusqlite::Result<EpisodeRow> {
    let status: String = row.get(7)?;
    Ok(EpisodeRow {
        id: row.get(0)?,
        recording_id: row.get(1)?,
        started_at_utc: parse_utc(&row.get::<_, String>(2)?),
        ended_at_utc: parse_utc(&row.get::<_, String>(3)?),
        source: row.get(4)?,
        label: row.get(5)?,
        confidence: row.get::<_, Option<f64>>(6)?.map(|value| value as f32),
        // The column is constrained to the three labels; an unparseable value
        // can only mean a database edited by hand, and `unreviewed` is the one
        // answer that does not invent a judgement.
        review_status: ReviewStatus::try_from(status.as_str()).unwrap_or(ReviewStatus::Unreviewed),
        created_at_utc: parse_utc(&row.get::<_, String>(8)?),
    })
}

fn map_transcript(row: &Row<'_>) -> rusqlite::Result<TranscriptRow> {
    Ok(TranscriptRow {
        id: row.get(0)?,
        recording_id: row.get(1)?,
        started_at_utc: parse_utc(&row.get::<_, String>(2)?),
        ended_at_utc: parse_utc(&row.get::<_, String>(3)?),
        text: row.get(4)?,
        engine: row.get(5)?,
        language: row.get(6)?,
        created_at_utc: parse_utc(&row.get::<_, String>(7)?),
    })
}

fn parse_utc(value: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(value).map(|v| v.to_utc()).unwrap_or(DateTime::<Utc>::UNIX_EPOCH)
}

/// Strict ISO calendar date parsing for API query parameters.
pub fn parse_date(value: &str) -> Result<NaiveDate> {
    NaiveDate::parse_from_str(value, "%Y-%m-%d")
        .map_err(|e| Error::Storage(format!("invalid date {value:?}: {e}")))
}

/// Converts one user's local calendar day to the half-open UTC interval `[start,end)`.
/// An empty configured timezone means use the host's local timezone.
pub fn local_day_range(date: NaiveDate, timezone: Option<chrono_tz::Tz>) -> (DateTime<Utc>, DateTime<Utc>) {
    let next = date.succ_opt().unwrap_or(date);
    match timezone {
        Some(tz) => {
            let start = date.and_hms_opt(0, 0, 0).unwrap().and_local_timezone(tz).earliest().unwrap().to_utc();
            let end = next.and_hms_opt(0, 0, 0).unwrap().and_local_timezone(tz).earliest().unwrap().to_utc();
            (start, end)
        }
        None => {
            // `chrono::Local` obtains the host zone; use the UTC offset at each
            // boundary separately so DST days may be 23 or 25 hours long.
            let start = date.and_hms_opt(0, 0, 0).unwrap().and_local_timezone(chrono::Local).earliest().unwrap().to_utc();
            let end = next.and_hms_opt(0, 0, 0).unwrap().and_local_timezone(chrono::Local).earliest().unwrap().to_utc();
            (start, end)
        }
    }
}

/// Ids only need to be unique within one database.
fn external_episode_id() -> String {
    use std::sync::atomic::AtomicU64;
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("evt{:013x}{:04x}", Utc::now().timestamp_millis(), n & 0xffff)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    /// The newest shipped migration, so adding one does not break assertions about
    /// "fully migrated" that were only ever asking "at the end".
    fn newest_version() -> i32 {
        crate::migrate::migration_files()
            .expect("migrations")
            .last()
            .expect("at least one migration")
            .version as i32
    }

    fn recording_draft(id: &str, start: DateTime<Utc>) -> RecordingDraft {
        RecordingDraft {
            id: id.into(),
            relative_path: PathBuf::from(format!("2025/01/01/{id}.wav")),
            started_at_utc: start,
            ended_at_utc: start + chrono::Duration::hours(1),
            capture_rate_hz: 16_000,
            channels: 1,
            frame_count: 57_600_000,
            size_bytes: 115_200_044,
            lost_frames: 0,
        }
    }

    fn episode(id: &str, recording_id: Option<&str>, start: DateTime<Utc>) -> EpisodeRow {
        EpisodeRow {
            id: id.into(),
            recording_id: recording_id.map(str::to_string),
            started_at_utc: start,
            ended_at_utc: start + chrono::Duration::seconds(2),
            source: "rule-v1".into(),
            label: None,
            confidence: Some(0.7),
            review_status: ReviewStatus::Unreviewed,
            created_at_utc: start,
        }
    }

    #[test]
    fn a_recording_round_trips_through_the_schema() {
        let db = Database::in_memory().unwrap();
        let start = Utc.timestamp_opt(1_700_000_000, 0).single().unwrap();
        db.insert_recording(&recording_draft("rec", start)).unwrap();

        let row = db.recording("rec").unwrap().expect("recording row");
        assert_eq!(row.relative_path, PathBuf::from("2025/01/01/rec.wav"));
        assert_eq!(row.started_at_utc, start);
        assert_eq!(row.ended_at_utc, start + chrono::Duration::hours(1));
        assert_eq!(row.capture_rate_hz, 16_000);
        assert_eq!(row.channels, 1);
        assert_eq!(row.frame_count, 57_600_000);
        assert_eq!(row.size_bytes, 115_200_044);
        assert_eq!(row.lost_frames, 0);

        let between = db
            .recordings_between(start, start + chrono::Duration::hours(2))
            .unwrap();
        assert_eq!(between.len(), 1);
        assert_eq!(between[0].id, "rec");
        assert_eq!(
            db.pool_summary().unwrap(),
            PoolSummary { recordings: 1, bytes: 115_200_044 }
        );
    }

    #[test]
    fn recordings_are_listed_oldest_first_and_deleting_one_keeps_its_annotations() {
        let db = Database::in_memory().unwrap();
        let start = Utc.timestamp_opt(1_700_000_000, 0).single().unwrap();
        db.insert_recording(&recording_draft("new", start + chrono::Duration::hours(2)))
            .unwrap();
        db.insert_recording(&recording_draft("old", start)).unwrap();
        db.insert_episode(&episode("ep", Some("old"), start)).unwrap();

        let ordered = db.recordings_oldest_first().unwrap();
        assert_eq!(ordered.len(), 2);
        assert_eq!(ordered[0].id, "old");
        assert_eq!(ordered[1].id, "new");

        assert!(db.delete_recording("old").unwrap());
        assert!(!db.delete_recording("old").unwrap());
        // The snore outlives the audio: the row stays, its pointer is cleared.
        let kept = db.episode("ep").unwrap().expect("episode stays");
        assert_eq!(kept.recording_id, None);
        assert_eq!(db.pool_summary().unwrap().recordings, 1);
    }

    #[test]
    fn review_status_is_strict_and_updates_are_idempotent() {
        assert_eq!(ReviewStatus::try_from("snore").unwrap(), ReviewStatus::Snore);
        assert!(ReviewStatus::try_from("maybe").is_err());
        let db = Database::in_memory().unwrap();
        let start = Utc.timestamp_opt(1_700_000_000, 0).single().unwrap();
        db.insert_episode(&episode("ep", None, start)).unwrap();

        assert!(db.set_episode_review("ep", ReviewStatus::NotSnore).unwrap());
        assert!(!db.set_episode_review("absent", ReviewStatus::Snore).unwrap());
        assert_eq!(
            db.episode("ep").unwrap().unwrap().review_status,
            ReviewStatus::NotSnore
        );
        assert_eq!(
            db.episodes_between(start, start + chrono::Duration::hours(1))
                .unwrap()[0]
                .review_status,
            ReviewStatus::NotSnore
        );
    }

    #[test]
    fn episodes_and_transcripts_are_found_by_time_range() {
        let db = Database::in_memory().unwrap();
        let start = Utc.timestamp_opt(1_700_000_000, 0).single().unwrap();
        db.insert_recording(&recording_draft("rec", start)).unwrap();

        // One rule episode and one classifier episode, both unattached: the
        // recording row is written when the file closes, which can be after the
        // event.
        db.insert_episode(&episode("ep-rule", None, start + chrono::Duration::seconds(10)))
            .unwrap();
        db.insert_episode(
            &ClassifiedEvent {
                started_at_utc: start + chrono::Duration::seconds(30),
                ended_at_utc: start + chrono::Duration::seconds(33),
                label: "snoring".into(),
                confidence: 0.9,
                threshold: 0.5,
                source: "apple-soundanalysis-v1".into(),
            }
            .episode_row(None),
        )
        .unwrap();
        db.insert_transcript(&TranscriptRow {
            id: "utt-1".into(),
            recording_id: None,
            started_at_utc: start + chrono::Duration::seconds(20),
            ended_at_utc: start + chrono::Duration::seconds(21),
            text: "good night".into(),
            engine: "apple-speech".into(),
            language: Some("en-US".into()),
            created_at_utc: start,
        })
        .unwrap();

        let early = db
            .episodes_between(start, start + chrono::Duration::seconds(25))
            .unwrap();
        assert_eq!(early.len(), 1);
        assert_eq!(early[0].id, "ep-rule");
        assert_eq!(early[0].label, None);
        assert_eq!(early[0].confidence, Some(0.7));

        let all = db
            .episodes_between(start, start + chrono::Duration::hours(1))
            .unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[1].source, "apple-soundanalysis-v1");
        assert_eq!(all[1].label.as_deref(), Some("snoring"));

        let quiet = db
            .transcripts_between(start, start + chrono::Duration::seconds(5))
            .unwrap();
        assert!(quiet.is_empty());
        let spoken = db
            .transcripts_between(start, start + chrono::Duration::hours(1))
            .unwrap();
        assert_eq!(spoken.len(), 1);
        assert_eq!(spoken[0].text, "good night");
        assert_eq!(spoken[0].recording_id, None);
    }

    #[test]
    fn an_episode_and_a_transcript_without_a_recording_are_accepted() {
        let db = Database::in_memory().unwrap();
        let start = Utc.timestamp_opt(1_700_000_000, 0).single().unwrap();
        // No `recordings` row at all: both must still insert, because the audio
        // that carried them may have been rotated away already.
        db.insert_episode(&episode("ep", None, start)).unwrap();
        db.insert_transcript(&TranscriptRow {
            id: "t".into(),
            recording_id: None,
            started_at_utc: start,
            ended_at_utc: start + chrono::Duration::seconds(1),
            text: "hi".into(),
            engine: "apple-speech".into(),
            language: None,
            created_at_utc: start,
        })
        .unwrap();
        assert_eq!(
            db.episodes_between(start, start + chrono::Duration::hours(1))
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            db.transcripts_between(start, start + chrono::Duration::hours(1))
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn migrations_are_idempotent_and_leave_only_the_three_tables() {
        let db = Database::in_memory().unwrap();
        db.migrate().unwrap();
        let version: i32 = db.conn.pragma_query_value(None, "user_version", |r| r.get(0)).unwrap();
        assert_eq!(version, newest_version());

        let mut tables: Vec<String> = db
            .conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        tables.retain(|name| !name.starts_with("_sqlx"));
        tables.retain(|name| !name.starts_with("sqlite_"));
        assert_eq!(tables, vec!["episodes", "recordings", "transcripts"]);

        let foreign_keys: i32 = db.conn.pragma_query_value(None, "foreign_keys", |r| r.get(0)).unwrap();
        assert_eq!(foreign_keys, 1);
    }

    #[test]
    fn date_parser_and_timezone_ranges_handle_calendar_boundaries() {
        let date = parse_date("2025-03-09").unwrap();
        assert!(parse_date("2025-02-30").is_err());
        let (start, end) = local_day_range(date, Some("America/New_York".parse().unwrap()));
        assert_eq!((end - start).num_hours(), 23); // DST spring-forward day
    }

    #[test]
    fn annotations_written_before_the_file_are_linked_when_it_closes() {
        let db = Database::in_memory().unwrap();
        let started = Utc::now();
        let ended = started + chrono::Duration::seconds(60);
        db.insert_episode(&EpisodeRow {
            id: "e1".into(),
            recording_id: None,
            started_at_utc: started + chrono::Duration::seconds(10),
            ended_at_utc: started + chrono::Duration::seconds(15),
            source: "rule-v1".into(),
            label: None,
            confidence: Some(0.8),
            review_status: ReviewStatus::Unreviewed,
            created_at_utc: started,
        })
        .unwrap();
        db.insert_recording(&RecordingDraft {
            id: "r1".into(),
            relative_path: PathBuf::from("recordings/r1.wav"),
            started_at_utc: started,
            ended_at_utc: ended,
            capture_rate_hz: 16_000,
            channels: 1,
            frame_count: 960_000,
            size_bytes: 1_920_000,
            lost_frames: 0,
        })
        .unwrap();
        let episode = db.episode("e1").unwrap().expect("episode");
        assert_eq!(
            episode.recording_id.as_deref(),
            Some("r1"),
            "an annotation produced before its file closed must be linked by wall clock"
        );
    }
}
