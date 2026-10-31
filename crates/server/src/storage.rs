//! SQLite schema, migrations and typed queries (TECH_SPEC §7, task T4.1).
//!
//! Connections are opened per owning worker/thread. WAL and foreign keys are
//! enabled for each connection; audio callbacks never touch this module.

use steno_lib::detector::EventDraft;
use steno_lib::error::{Error, Result};
use crate::recorder::SegmentFile;
use steno_lib::timeline::{EstimateSource, GapKind, GapRecord, TimeQuality};
use chrono::{DateTime, NaiveDate, Utc};
use rusqlite::{params, Connection, OptionalExtension, Row};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::time::Duration;

const SCHEMA_VERSION: i32 = 3;

#[derive(Debug, Clone)]
pub struct SegmentStarted {
    pub id: String,
    pub relative_path: PathBuf,
    pub started_at_utc: DateTime<Utc>,
    pub capture_rate_hz: u32,
    pub channels: u16,
    pub sample_format: String,
    pub channel_mapping: String,
    pub time_quality: TimeQuality,
    pub boot_id: String,
}

#[derive(Debug, Clone)]
pub struct SegmentCompleted {
    pub id: String,
    pub ended_at_utc: DateTime<Utc>,
    pub frame_count: u64,
    pub size_bytes: u64,
    pub end_reason: String,
    pub clock_drift_ppm: Option<f64>,
    pub xrun_count: u64,
    pub gap_count: u64,
    pub lost_frames_total: u64,
    pub status: SegmentStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SegmentStatus {
    Recording,
    Complete,
    Interrupted,
    Deleted,
    Missing,
}

impl SegmentStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            SegmentStatus::Recording => "recording",
            SegmentStatus::Complete => "complete",
            SegmentStatus::Interrupted => "interrupted",
            SegmentStatus::Deleted => "deleted",
            SegmentStatus::Missing => "missing",
        }
    }
}

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

#[derive(Debug, Clone, Serialize)]
pub struct SegmentRow {
    pub id: String,
    pub relative_path: String,
    pub started_at_utc: DateTime<Utc>,
    pub ended_at_utc: Option<DateTime<Utc>>,
    pub capture_rate_hz: u32,
    pub channels: u16,
    pub sample_format: String,
    pub channel_mapping: String,
    pub frame_count: u64,
    pub size_bytes: u64,
    pub xrun_count: u64,
    pub gap_count: u64,
    pub lost_frames_total: u64,
    pub time_quality: String,
    pub boot_id: String,
    pub clock_drift_ppm: Option<f64>,
    pub end_reason: Option<String>,
    pub status: String,
    pub created_at_utc: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize)]
pub struct EventRow {
    pub id: String,
    pub segment_id: String,
    pub start_offset_frames: u64,
    pub end_offset_frames: u64,
    pub started_at_utc: DateTime<Utc>,
    pub ended_at_utc: DateTime<Utc>,
    pub duration_ms: u32,
    pub rule_score: Option<f32>,
    pub detector_version: String,
    pub peak_dbfs: Option<f32>,
    pub mean_level_dbfs: Option<f32>,
    pub mean_band_ratio: Option<f32>,
    pub noise_floor_dbfs: Option<f32>,
    pub end_reason: String,
    pub review_status: String,
    pub continued: bool,
    pub created_at_utc: DateTime<Utc>,
}

/// One segment that still needs a transcript.
#[derive(Debug, Clone)]
pub struct PendingTranscription {
    pub segment_id: String,
    pub relative_path: PathBuf,
    pub audio_seconds: f64,
}

/// The stored result of transcribing one segment.
#[derive(Debug, Clone, Serialize)]
pub struct TranscriptRow {
    pub segment_id: String,
    pub engine: String,
    pub language: Option<String>,
    pub text: String,
    /// The phrase timings as produced by the recogniser, kept verbatim so the
    /// UI can highlight a phrase without re-parsing the text.
    pub segments_json: String,
    pub status: String,
    pub error: Option<String>,
    pub audio_seconds: Option<f64>,
    pub created_at_utc: DateTime<Utc>,
}

#[derive(Debug)]
pub struct Database {
    conn: Connection,
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

    /// Idempotent migration managed by `PRAGMA user_version`.
    pub fn migrate(&self) -> Result<()> {
        let mut version: i32 = self.conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version > SCHEMA_VERSION {
            return Err(Error::Storage(format!(
                "database schema version {version} is newer than supported {SCHEMA_VERSION}"
            )));
        }
        if version == 0 {
            let tx = self.conn.unchecked_transaction()?;
            tx.execute_batch(
                "CREATE TABLE recording_segments (
                    id TEXT PRIMARY KEY,
                    relative_path TEXT NOT NULL UNIQUE,
                    started_at_utc TEXT NOT NULL,
                    ended_at_utc TEXT,
                    capture_rate_hz INTEGER NOT NULL CHECK(capture_rate_hz > 0),
                    channels INTEGER NOT NULL CHECK(channels BETWEEN 1 AND 2),
                    sample_format TEXT NOT NULL CHECK(sample_format = 'S16_LE'),
                    channel_mapping TEXT NOT NULL,
                    frame_count INTEGER NOT NULL DEFAULT 0 CHECK(frame_count >= 0),
                    size_bytes INTEGER NOT NULL DEFAULT 0 CHECK(size_bytes >= 0),
                    xrun_count INTEGER NOT NULL DEFAULT 0 CHECK(xrun_count >= 0),
                    gap_count INTEGER NOT NULL DEFAULT 0 CHECK(gap_count >= 0),
                    lost_frames_total INTEGER NOT NULL DEFAULT 0 CHECK(lost_frames_total >= 0),
                    time_quality TEXT NOT NULL CHECK(time_quality IN ('synced','unsynced','corrected')),
                    boot_id TEXT NOT NULL,
                    clock_drift_ppm REAL,
                    end_reason TEXT CHECK(end_reason IS NULL OR end_reason IN ('duration','gap','time_step','format_change','size_limit','shutdown','error')),
                    status TEXT NOT NULL CHECK(status IN ('recording','complete','interrupted','deleted','missing')),
                    created_at_utc TEXT NOT NULL
                );
                CREATE TABLE audio_gaps (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    segment_id TEXT NOT NULL REFERENCES recording_segments(id) ON DELETE CASCADE,
                    offset_frames INTEGER NOT NULL CHECK(offset_frames >= 0),
                    lost_frames INTEGER NOT NULL CHECK(lost_frames >= 0),
                    kind TEXT NOT NULL CHECK(kind IN ('xrun','stream_error','capture_ring_overflow','queue_overflow','stream_rebuild')),
                    estimate_source TEXT NOT NULL CHECK(estimate_source IN ('timestamp','counter','unknown')),
                    at_utc TEXT NOT NULL
                );
                CREATE TABLE snore_events (
                    id TEXT PRIMARY KEY,
                    segment_id TEXT NOT NULL REFERENCES recording_segments(id) ON DELETE CASCADE,
                    start_offset_frames INTEGER NOT NULL CHECK(start_offset_frames >= 0),
                    end_offset_frames INTEGER NOT NULL CHECK(end_offset_frames >= start_offset_frames),
                    started_at_utc TEXT NOT NULL,
                    ended_at_utc TEXT NOT NULL,
                    duration_ms INTEGER NOT NULL CHECK(duration_ms >= 0),
                    rule_score REAL CHECK(rule_score IS NULL OR (rule_score >= 0 AND rule_score <= 1)),
                    detector_version TEXT NOT NULL,
                    peak_dbfs REAL,
                    mean_level_dbfs REAL,
                    mean_band_ratio REAL,
                    noise_floor_dbfs REAL,
                    end_reason TEXT NOT NULL DEFAULT 'normal' CHECK(end_reason IN ('normal','max_duration','segment_end','gap','detector_gap','shutdown')),
                    review_status TEXT NOT NULL DEFAULT 'unreviewed' CHECK(review_status IN ('snore','not_snore','unreviewed')),
                    continued INTEGER NOT NULL DEFAULT 0 CHECK(continued IN (0,1)),
                    created_at_utc TEXT NOT NULL
                );
                CREATE INDEX idx_events_started_at ON snore_events(started_at_utc);
                CREATE INDEX idx_events_segment_offset ON snore_events(segment_id,start_offset_frames);
                CREATE INDEX idx_segments_started_at ON recording_segments(started_at_utc);
                CREATE INDEX idx_gaps_segment_offset ON audio_gaps(segment_id,offset_frames);
                PRAGMA user_version = 1;",
            )?;
            tx.commit()?;
            version = 1;
        }
        if version < 2 {
            // Transcription is an optional, additive stage: it never blocks
            // recording and it can be re-run, so it gets its own table rather
            // than columns on `recording_segments`.
            let tx = self.conn.unchecked_transaction()?;
            tx.execute_batch(
                "CREATE TABLE segment_transcripts (
                    segment_id TEXT PRIMARY KEY REFERENCES recording_segments(id) ON DELETE CASCADE,
                    engine TEXT NOT NULL,
                    language TEXT,
                    text TEXT NOT NULL DEFAULT '',
                    segments_json TEXT NOT NULL DEFAULT '[]',
                    status TEXT NOT NULL CHECK(status IN ('ok','failed','skipped')),
                    error TEXT,
                    audio_seconds REAL,
                    created_at_utc TEXT NOT NULL
                );
                CREATE INDEX idx_transcripts_created ON segment_transcripts(created_at_utc);
                CREATE INDEX idx_transcripts_status ON segment_transcripts(status);
                PRAGMA user_version = 2;",
            )?;
            tx.commit()?;
            version = 2;
        }
        if version < 3 {
            // Streaming transcription produces phrases, not per-segment text:
            // one utterance can span a segment boundary, and a segment can hold
            // several. They are keyed by wall clock so the UI can lay them on
            // the same timeline as the events.
            let tx = self.conn.unchecked_transaction()?;
            tx.execute_batch(
                "CREATE TABLE utterances (
                    id TEXT PRIMARY KEY,
                    started_at_utc TEXT NOT NULL,
                    ended_at_utc TEXT NOT NULL,
                    engine TEXT NOT NULL,
                    language TEXT,
                    text TEXT NOT NULL DEFAULT '',
                    segments_json TEXT NOT NULL DEFAULT '[]',
                    created_at_utc TEXT NOT NULL
                );
                CREATE INDEX idx_utterances_started_at ON utterances(started_at_utc);
                PRAGMA user_version = 3;",
            )?;
            tx.commit()?;
            version = 3;
        }
        let _ = version;
        Ok(())
    }

    pub fn connection(&self) -> &Connection {
        &self.conn
    }

    pub fn insert_segment_started(&self, segment: &SegmentStarted) -> Result<()> {
        self.conn.execute(
            "INSERT INTO recording_segments (
                id, relative_path, started_at_utc, capture_rate_hz, channels,
                sample_format, channel_mapping, time_quality, boot_id, status,
                created_at_utc
             ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,'recording',?3)",
            params![
                segment.id,
                segment.relative_path.to_string_lossy(),
                segment.started_at_utc.to_rfc3339(),
                segment.capture_rate_hz,
                segment.channels,
                segment.sample_format,
                segment.channel_mapping,
                segment.time_quality.as_str(),
                segment.boot_id,
            ],
        )?;
        Ok(())
    }

    pub fn complete_segment(&self, completed: &SegmentCompleted) -> Result<()> {
        self.conn.execute(
            "UPDATE recording_segments SET ended_at_utc=?2, frame_count=?3,
                size_bytes=?4, end_reason=?5, clock_drift_ppm=?6, xrun_count=?7,
                gap_count=?8, lost_frames_total=?9, status=?10 WHERE id=?1",
            params![
                completed.id,
                completed.ended_at_utc.to_rfc3339(),
                i64::try_from(completed.frame_count).unwrap_or(i64::MAX),
                i64::try_from(completed.size_bytes).unwrap_or(i64::MAX),
                completed.end_reason,
                completed.clock_drift_ppm,
                i64::try_from(completed.xrun_count).unwrap_or(i64::MAX),
                i64::try_from(completed.gap_count).unwrap_or(i64::MAX),
                i64::try_from(completed.lost_frames_total).unwrap_or(i64::MAX),
                completed.status.as_str(),
            ],
        )?;
        Ok(())
    }

    pub fn insert_gap(&self, segment_id: &str, gap: &GapRecord) -> Result<()> {
        self.conn.execute(
            "INSERT INTO audio_gaps (segment_id,offset_frames,lost_frames,kind,estimate_source,at_utc)
             VALUES (?1,?2,?3,?4,?5,?6)",
            params![
                segment_id,
                i64::try_from(gap.offset_frames).unwrap_or(i64::MAX),
                i64::try_from(gap.lost_frames).unwrap_or(i64::MAX),
                gap.kind.as_str(),
                gap.estimate_source.as_str(),
                gap.at_utc.to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    pub fn insert_event(&self, event: &EventDraft) -> Result<()> {
        self.conn.execute(
            "INSERT INTO snore_events (
                id,segment_id,start_offset_frames,end_offset_frames,started_at_utc,
                ended_at_utc,duration_ms,rule_score,detector_version,peak_dbfs,
                mean_level_dbfs,mean_band_ratio,noise_floor_dbfs,end_reason,
                review_status,continued,created_at_utc
             ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,'unreviewed',?15,?16)",
            params![
                event.id,
                event.segment_id,
                i64::try_from(event.start_offset_frames).unwrap_or(i64::MAX),
                i64::try_from(event.end_offset_frames).unwrap_or(i64::MAX),
                event.started_at_utc.to_rfc3339(),
                event.ended_at_utc.to_rfc3339(),
                event.duration_ms,
                event.rule_score,
                event.detector_version,
                event.peak_dbfs,
                event.mean_level_dbfs,
                event.mean_band_ratio,
                event.noise_floor_dbfs,
                event.end_reason.as_str(),
                event.continued,
                Utc::now().to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    pub fn set_review_status(&self, event_id: &str, status: ReviewStatus) -> Result<bool> {
        let changed = self.conn.execute(
            "UPDATE snore_events SET review_status=?2 WHERE id=?1",
            params![event_id, status.as_str()],
        )?;
        Ok(changed > 0)
    }

    pub fn event(&self, id: &str) -> Result<Option<EventRow>> {
        self.conn
            .query_row("SELECT * FROM snore_events WHERE id=?1", [id], map_event)
            .optional()
            .map_err(Error::from)
    }

    pub fn segment(&self, id: &str) -> Result<Option<SegmentRow>> {
        self.conn
            .query_row("SELECT * FROM recording_segments WHERE id=?1", [id], map_segment)
            .optional()
            .map_err(Error::from)
    }

    pub fn event_with_segment(&self, event_id: &str) -> Result<Option<(EventRow, SegmentRow)>> {
        let Some(event) = self.event(event_id)? else { return Ok(None); };
        let Some(segment) = self.segment(&event.segment_id)? else { return Ok(None); };
        Ok(Some((event, segment)))
    }

    pub fn gaps_for_segment(&self, segment_id: &str) -> Result<Vec<GapRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT offset_frames,lost_frames,kind,estimate_source,at_utc FROM audio_gaps
             WHERE segment_id=?1 ORDER BY offset_frames,id",
        )?;
        let rows = stmt.query_map([segment_id], |row| {
            Ok(GapRecord {
                offset_frames: u64::try_from(row.get::<_, i64>(0)?.max(0)).unwrap_or(0),
                lost_frames: u64::try_from(row.get::<_, i64>(1)?.max(0)).unwrap_or(0),
                kind: parse_gap_kind(&row.get::<_, String>(2)?),
                estimate_source: parse_estimate_source(&row.get::<_, String>(3)?),
                at_utc: parse_utc(&row.get::<_, String>(4)?),
            })
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>().map_err(Error::from)
    }

    pub fn segments_between(&self, start_utc: DateTime<Utc>, end_utc: DateTime<Utc>) -> Result<Vec<SegmentRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT * FROM recording_segments
             WHERE started_at_utc < ?2 AND COALESCE(ended_at_utc,started_at_utc) >= ?1
             ORDER BY started_at_utc,id",
        )?;
        let rows = stmt.query_map(params![start_utc.to_rfc3339(), end_utc.to_rfc3339()], map_segment)?;
        rows.collect::<std::result::Result<Vec<_>, _>>().map_err(Error::from)
    }

    pub fn events_between(&self, start_utc: DateTime<Utc>, end_utc: DateTime<Utc>) -> Result<Vec<EventRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT * FROM snore_events WHERE started_at_utc >= ?1 AND started_at_utc < ?2
             ORDER BY started_at_utc,id",
        )?;
        let rows = stmt.query_map(params![start_utc.to_rfc3339(), end_utc.to_rfc3339()], map_event)?;
        rows.collect::<std::result::Result<Vec<_>, _>>().map_err(Error::from)
    }

    pub fn days_between(&self, start_utc: DateTime<Utc>, end_utc: DateTime<Utc>) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT substr(started_at_utc,1,10) FROM recording_segments
             WHERE started_at_utc < ?2 AND COALESCE(ended_at_utc,started_at_utc) >= ?1
             ORDER BY 1",
        )?;
        let rows = stmt.query_map(params![start_utc.to_rfc3339(), end_utc.to_rfc3339()], |row| row.get(0))?;
        rows.collect::<std::result::Result<Vec<_>, _>>().map_err(Error::from)
    }

    pub fn list_segments_for_retention(&self) -> Result<Vec<SegmentRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT * FROM recording_segments WHERE status='complete' ORDER BY started_at_utc,id",
        )?;
        let rows = stmt.query_map([], map_segment)?;
        rows.collect::<std::result::Result<Vec<_>, _>>().map_err(Error::from)
    }

    /// Every segment row, in start order. Used by the startup recovery scan to
    /// reconcile the database against the files actually on disk; it deliberately
    /// is not filtered by status, since a crashed run can leave any status behind.
    pub fn all_segments(&self) -> Result<Vec<SegmentRow>> {
        let mut stmt = self
            .conn
            .prepare("SELECT * FROM recording_segments ORDER BY started_at_utc,id")?;
        let rows = stmt.query_map([], map_segment)?;
        rows.collect::<std::result::Result<Vec<_>, _>>().map_err(Error::from)
    }

    pub fn mark_segment_status(&self, id: &str, status: SegmentStatus) -> Result<bool> {
        let changed = self.conn.execute(
            "UPDATE recording_segments SET status=?2 WHERE id=?1",
            params![id, status.as_str()],
        )?;
        Ok(changed > 0)
    }

    pub fn update_segment_path_and_time(&self, id: &str, path: &Path, start: DateTime<Utc>, end: DateTime<Utc>, status: SegmentStatus) -> Result<()> {
        self.conn.execute(
            "UPDATE recording_segments SET relative_path=?2,started_at_utc=?3,ended_at_utc=?4,status=?5 WHERE id=?1",
            params![id, path.to_string_lossy(), start.to_rfc3339(), end.to_rfc3339(), status.as_str()],
        )?;
        Ok(())
    }

    pub fn insert_interrupted_recovery(&self, segment: &SegmentStarted, frame_count: u64, size_bytes: u64, ended_at: DateTime<Utc>) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "INSERT OR IGNORE INTO recording_segments (
                id,relative_path,started_at_utc,ended_at_utc,capture_rate_hz,channels,
                sample_format,channel_mapping,frame_count,size_bytes,time_quality,
                boot_id,end_reason,status,created_at_utc
             ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,'error','interrupted',?3)",
            params![
                segment.id, segment.relative_path.to_string_lossy(), segment.started_at_utc.to_rfc3339(),
                ended_at.to_rfc3339(), segment.capture_rate_hz, segment.channels, segment.sample_format,
                segment.channel_mapping, i64::try_from(frame_count).unwrap_or(i64::MAX), i64::try_from(size_bytes).unwrap_or(i64::MAX), segment.time_quality.as_str(), segment.boot_id,
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn database_path_is_recording(&self, id: &str, relative_path: &str) -> Result<bool> {
        let status: Option<String> = self.conn.query_row(
            "SELECT status FROM recording_segments WHERE id=?1 AND relative_path=?2",
            params![id, relative_path],
            |r| r.get(0),
        ).optional()?;
        Ok(status.as_deref() == Some("recording"))
    }

    pub fn bytes_of_recordings(&self) -> Result<u64> {
        let total: i64 = self.conn.query_row(
            "SELECT COALESCE(SUM(size_bytes),0) FROM recording_segments WHERE status IN ('complete','interrupted')",
            [], |row| row.get(0),
        )?;
        Ok(total.max(0) as u64)
    }

    /// Pool-level aggregates for the control centre: what is on disk, how many
    /// segments it is made of, and how far back it reaches.
    ///
    /// One query, and the same `status IN (...)` filter as `bytes_of_recordings`
    /// so the byte total here can never disagree with the one the retention pass
    /// reclaims against.
    pub fn pool_summary(&self) -> Result<PoolSummary> {
        let mut statement = self.conn.prepare(
            "SELECT COALESCE(SUM(size_bytes),0),
                    COUNT(*),
                    MIN(started_at_utc),
                    MAX(COALESCE(ended_at_utc, started_at_utc))
             FROM recording_segments
             WHERE status IN ('complete','interrupted')",
        )?;
        let summary = statement.query_row([], |row| {
            let used: i64 = row.get(0)?;
            let count: i64 = row.get(1)?;
            let oldest: Option<String> = row.get(2)?;
            let newest: Option<String> = row.get(3)?;
            Ok((used, count, oldest, newest))
        })?;
        let (used, count, oldest, newest) = summary;
        // Timestamps are stored as RFC 3339 in UTC. A row we cannot parse is
        // reported as "no known bound" rather than as the epoch, which would
        // read as a recording from 1970.
        Ok(PoolSummary {
            used_bytes: used.max(0) as u64,
            segment_count: count.max(0) as u64,
            oldest_started_at_utc: oldest
                .and_then(|value| DateTime::parse_from_rfc3339(&value).ok())
                .map(|value| value.with_timezone(&Utc)),
            newest_ended_at_utc: newest
                .and_then(|value| DateTime::parse_from_rfc3339(&value).ok())
                .map(|value| value.with_timezone(&Utc)),
        })
    }

    pub fn event_count(&self) -> Result<u64> {
        let n: i64 = self.conn.query_row("SELECT COUNT(*) FROM snore_events", [], |r| r.get(0))?;
        Ok(n.max(0) as u64)
    }

    /// Segments that finished recording but have no transcript row yet.
    ///
    /// The database is the transcription queue on purpose: there is no in-memory
    /// queue to lose, a crash simply resumes on the next pass, and a segment can
    /// never be transcribed twice because the insert is keyed by `segment_id`.
    pub fn segments_awaiting_transcript(
        &self,
        min_seconds: f64,
        limit: usize,
    ) -> Result<Vec<PendingTranscription>> {
        let mut statement = self.conn.prepare(
            "SELECT s.id, s.relative_path,
                    CAST(s.frame_count AS REAL) / s.capture_rate_hz
             FROM recording_segments s
             LEFT JOIN segment_transcripts t ON t.segment_id = s.id
             WHERE s.status = 'complete'
               AND t.segment_id IS NULL
               AND s.capture_rate_hz > 0
               AND CAST(s.frame_count AS REAL) / s.capture_rate_hz >= ?1
             ORDER BY s.started_at_utc ASC
             LIMIT ?2",
        )?;
        let rows = statement
            .query_map(params![min_seconds, limit as i64], |row| {
                Ok(PendingTranscription {
                    segment_id: row.get(0)?,
                    relative_path: PathBuf::from(row.get::<_, String>(1)?),
                    audio_seconds: row.get(2)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Record the outcome for a segment. Replaces any previous row so a manual
    /// re-run after fixing the recogniser is possible.
    pub fn save_transcript(&self, transcript: &TranscriptRow) -> Result<()> {
        self.conn.execute(
            "INSERT INTO segment_transcripts
                (segment_id, engine, language, text, segments_json, status, error, audio_seconds, created_at_utc)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(segment_id) DO UPDATE SET
                engine = excluded.engine,
                language = excluded.language,
                text = excluded.text,
                segments_json = excluded.segments_json,
                status = excluded.status,
                error = excluded.error,
                audio_seconds = excluded.audio_seconds,
                created_at_utc = excluded.created_at_utc",
            params![
                transcript.segment_id,
                transcript.engine,
                transcript.language,
                transcript.text,
                transcript.segments_json,
                transcript.status,
                transcript.error,
                transcript.audio_seconds,
                transcript.created_at_utc.to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    pub fn transcript_for_segment(&self, segment_id: &str) -> Result<Option<TranscriptRow>> {
        let mut statement = self.conn.prepare(
            "SELECT segment_id, engine, language, text, segments_json, status, error, audio_seconds, created_at_utc
             FROM segment_transcripts WHERE segment_id = ?1",
        )?;
        let mut rows = statement.query_map(params![segment_id], transcript_from_row)?;
        match rows.next() {
            Some(row) => Ok(Some(row?)),
            None => Ok(None),
        }
    }

    pub fn save_utterance(&self, utterance: &UtteranceRow) -> Result<()> {
        self.conn.execute(
            "INSERT INTO utterances
                (id, started_at_utc, ended_at_utc, engine, language, text, segments_json, created_at_utc)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(id) DO NOTHING",
            params![
                utterance.id,
                utterance.started_at_utc.to_rfc3339(),
                utterance.ended_at_utc.to_rfc3339(),
                utterance.engine,
                utterance.language,
                utterance.text,
                utterance.segments_json,
                utterance.created_at_utc.to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    /// Utterances that start inside the window, oldest first: a transcript
    /// reads forwards even though the event list reads backwards.
    pub fn utterances_between(
        &self,
        start_utc: DateTime<Utc>,
        end_utc: DateTime<Utc>,
    ) -> Result<Vec<UtteranceRow>> {
        let mut statement = self.conn.prepare(
            "SELECT id, started_at_utc, ended_at_utc, engine, language, text, segments_json, created_at_utc
             FROM utterances
             WHERE started_at_utc >= ?1 AND started_at_utc < ?2
             ORDER BY started_at_utc ASC",
        )?;
        let rows = statement
            .query_map(params![start_utc.to_rfc3339(), end_utc.to_rfc3339()], utterance_from_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Transcripts whose segment started inside the window, newest first.
    pub fn transcripts_between(
        &self,
        start_utc: DateTime<Utc>,
        end_utc: DateTime<Utc>,
    ) -> Result<Vec<TranscriptRow>> {
        let mut statement = self.conn.prepare(
            "SELECT t.segment_id, t.engine, t.language, t.text, t.segments_json, t.status,
                    t.error, t.audio_seconds, t.created_at_utc
             FROM segment_transcripts t
             JOIN recording_segments s ON s.id = t.segment_id
             WHERE s.started_at_utc >= ?1 AND s.started_at_utc < ?2
             ORDER BY s.started_at_utc DESC",
        )?;
        let rows = statement
            .query_map(params![start_utc.to_rfc3339(), end_utc.to_rfc3339()], transcript_from_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }
}

/// Everything the control centre needs to describe the recording pool.
///
/// `oldest_started_at_utc` / `newest_ended_at_utc` are `None` for an empty pool
/// (and for a row whose timestamp cannot be parsed) rather than being filled in
/// with "now" or the epoch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolSummary {
    pub used_bytes: u64,
    pub segment_count: u64,
    pub oldest_started_at_utc: Option<DateTime<Utc>>,
    pub newest_ended_at_utc: Option<DateTime<Utc>>,
}

/// One recognised phrase from the streaming recogniser.
#[derive(Debug, Clone)]
pub struct UtteranceRow {
    pub id: String,
    pub started_at_utc: DateTime<Utc>,
    pub ended_at_utc: DateTime<Utc>,
    pub engine: String,
    pub language: Option<String>,
    pub text: String,
    pub segments_json: String,
    pub created_at_utc: DateTime<Utc>,
}

fn utterance_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<UtteranceRow> {
    let started: String = row.get(1)?;
    let ended: String = row.get(2)?;
    let created: String = row.get(7)?;
    Ok(UtteranceRow {
        id: row.get(0)?,
        started_at_utc: DateTime::parse_from_rfc3339(&started)
            .map(|value| value.with_timezone(&Utc))
            .unwrap_or_else(|_| Utc::now()),
        ended_at_utc: DateTime::parse_from_rfc3339(&ended)
            .map(|value| value.with_timezone(&Utc))
            .unwrap_or_else(|_| Utc::now()),
        engine: row.get(3)?,
        language: row.get(4)?,
        text: row.get(5)?,
        segments_json: row.get(6)?,
        created_at_utc: DateTime::parse_from_rfc3339(&created)
            .map(|value| value.with_timezone(&Utc))
            .unwrap_or_else(|_| Utc::now()),
    })
}

fn transcript_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<TranscriptRow> {
    let created: String = row.get(8)?;
    Ok(TranscriptRow {
        segment_id: row.get(0)?,
        engine: row.get(1)?,
        language: row.get(2)?,
        text: row.get(3)?,
        segments_json: row.get(4)?,
        status: row.get(5)?,
        error: row.get(6)?,
        audio_seconds: row.get(7)?,
        created_at_utc: DateTime::parse_from_rfc3339(&created)
            .map(|value| value.with_timezone(&Utc))
            .unwrap_or_else(|_| Utc::now()),
    })
}

impl SegmentCompleted {
    pub fn from_file(id: impl Into<String>, _file: &SegmentFile, ended_at_utc: DateTime<Utc>, frame_count: u64, size_bytes: u64, end_reason: impl Into<String>) -> Self {
        SegmentCompleted {
            id: id.into(), ended_at_utc, frame_count, size_bytes, end_reason: end_reason.into(),
            clock_drift_ppm: None, xrun_count: 0, gap_count: 0, lost_frames_total: 0,
            status: SegmentStatus::Complete,
        }
    }
}

fn map_segment(row: &Row<'_>) -> rusqlite::Result<SegmentRow> {
    Ok(SegmentRow {
        id: row.get("id")?,
        relative_path: row.get("relative_path")?,
        started_at_utc: parse_utc(&row.get::<_, String>("started_at_utc")?),
        ended_at_utc: row.get::<_, Option<String>>("ended_at_utc")?.map(|s| parse_utc(&s)),
        capture_rate_hz: row.get("capture_rate_hz")?,
        channels: row.get("channels")?,
        sample_format: row.get("sample_format")?,
        channel_mapping: row.get("channel_mapping")?,
        frame_count: u64::try_from(row.get::<_, i64>("frame_count")?.max(0)).unwrap_or(0),
        size_bytes: u64::try_from(row.get::<_, i64>("size_bytes")?.max(0)).unwrap_or(0),
        xrun_count: u64::try_from(row.get::<_, i64>("xrun_count")?.max(0)).unwrap_or(0),
        gap_count: u64::try_from(row.get::<_, i64>("gap_count")?.max(0)).unwrap_or(0),
        lost_frames_total: u64::try_from(row.get::<_, i64>("lost_frames_total")?.max(0)).unwrap_or(0),
        time_quality: row.get("time_quality")?,
        boot_id: row.get("boot_id")?,
        clock_drift_ppm: row.get("clock_drift_ppm")?,
        end_reason: row.get("end_reason")?,
        status: row.get("status")?,
        created_at_utc: parse_utc(&row.get::<_, String>("created_at_utc")?),
    })
}

fn map_event(row: &Row<'_>) -> rusqlite::Result<EventRow> {
    Ok(EventRow {
        id: row.get("id")?,
        segment_id: row.get("segment_id")?,
        start_offset_frames: u64::try_from(row.get::<_, i64>("start_offset_frames")?.max(0)).unwrap_or(0),
        end_offset_frames: u64::try_from(row.get::<_, i64>("end_offset_frames")?.max(0)).unwrap_or(0),
        started_at_utc: parse_utc(&row.get::<_, String>("started_at_utc")?),
        ended_at_utc: parse_utc(&row.get::<_, String>("ended_at_utc")?),
        duration_ms: row.get("duration_ms")?,
        rule_score: row.get("rule_score")?,
        detector_version: row.get("detector_version")?,
        peak_dbfs: row.get("peak_dbfs")?,
        mean_level_dbfs: row.get("mean_level_dbfs")?,
        mean_band_ratio: row.get("mean_band_ratio")?,
        noise_floor_dbfs: row.get("noise_floor_dbfs")?,
        end_reason: row.get("end_reason")?,
        review_status: row.get("review_status")?,
        continued: row.get::<_, i64>("continued")? != 0,
        created_at_utc: parse_utc(&row.get::<_, String>("created_at_utc")?),
    })
}

fn parse_utc(value: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(value).map(|v| v.to_utc()).unwrap_or(DateTime::<Utc>::UNIX_EPOCH)
}

fn parse_gap_kind(value: &str) -> GapKind {
    match value {
        "xrun" => GapKind::Xrun,
        "stream_error" => GapKind::StreamError,
        "capture_ring_overflow" => GapKind::CaptureRingOverflow,
        "queue_overflow" => GapKind::QueueOverflow,
        _ => GapKind::StreamRebuild,
    }
}

fn parse_estimate_source(value: &str) -> EstimateSource {
    match value {
        "timestamp" => EstimateSource::Timestamp,
        "counter" => EstimateSource::Counter,
        _ => EstimateSource::Unknown,
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn segment(id: &str, started: DateTime<Utc>, relative: &str) -> SegmentStarted {
        SegmentStarted {
            id: id.into(), relative_path: PathBuf::from(relative), started_at_utc: started,
            capture_rate_hz: 16_000, channels: 1, sample_format: "S16_LE".into(),
            channel_mapping: "mono:0".into(), time_quality: TimeQuality::Synced, boot_id: "boot".into(),
        }
    }

    fn event(id: &str, segment_id: &str, start: DateTime<Utc>) -> EventDraft {
        EventDraft {
            id: id.into(), segment_id: segment_id.into(), start_offset_frames: 0, end_offset_frames: 3_200,
            started_at_utc: start, ended_at_utc: start + chrono::Duration::milliseconds(200), duration_ms: 200,
            rule_score: None, detector_version: "rule-v1".into(), peak_dbfs: Some(-10.0),
            mean_level_dbfs: Some(-30.0), mean_band_ratio: Some(0.5), noise_floor_dbfs: Some(-60.0),
            end_reason: steno_lib::detector::EndReason::Normal, continued: false,
        }
    }

    #[test]
    fn migrations_are_idempotent_and_enable_foreign_keys() {
        let db = Database::in_memory().unwrap();
        db.migrate().unwrap();
        let version: i32 = db.conn.pragma_query_value(None, "user_version", |r| r.get(0)).unwrap();
        assert_eq!(version, 3);
        // Each version is additive and both must be present after an in-place
        // upgrade of an older database.
        let transcripts: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM segment_transcripts", [], |r| r.get(0))
            .unwrap();
        assert_eq!(transcripts, 0);
        let utterances: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM utterances", [], |r| r.get(0))
            .unwrap();
        assert_eq!(utterances, 0);
        let foreign_keys: i32 = db.conn.pragma_query_value(None, "foreign_keys", |r| r.get(0)).unwrap();
        assert_eq!(foreign_keys, 1);
    }

    #[test]
    fn segment_event_and_gap_constraints_and_queries_work() {
        let db = Database::in_memory().unwrap();
        let start = Utc.timestamp_opt(1_700_000_000, 0).single().unwrap();
        db.insert_segment_started(&segment("seg", start, "2025/01/01/test.wav")).unwrap();
        let completed = SegmentCompleted {
            id: "seg".into(), ended_at_utc: start + chrono::Duration::hours(1), frame_count: 57_600_000,
            size_bytes: 115_200_044, end_reason: "duration".into(), clock_drift_ppm: Some(4.2),
            xrun_count: 1, gap_count: 1, lost_frames_total: 80, status: SegmentStatus::Complete,
        };
        db.complete_segment(&completed).unwrap();
        let gap = GapRecord {
            offset_frames: 16_000, lost_frames: 80, kind: GapKind::Xrun,
            estimate_source: EstimateSource::Timestamp, at_utc: start + chrono::Duration::seconds(1),
        };
        db.insert_gap("seg", &gap).unwrap();
        db.insert_event(&event("evt", "seg", start + chrono::Duration::seconds(2))).unwrap();
        let rows = db.events_between(start, start + chrono::Duration::hours(1)).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].review_status, "unreviewed");
        assert_eq!(db.gaps_for_segment("seg").unwrap().len(), 1);
        assert_eq!(db.segment("seg").unwrap().unwrap().status, "complete");
        assert_eq!(db.event_count().unwrap(), 1);
    }

    #[test]
    fn foreign_keys_reject_unknown_segment_events() {
        let db = Database::in_memory().unwrap();
        let t = Utc.timestamp_opt(1_700_000_000, 0).single().unwrap();
        assert!(db.insert_event(&event("evt", "missing", t)).is_err());
    }

    #[test]
    fn review_status_is_strict_and_updates_are_idempotent() {
        assert_eq!(ReviewStatus::try_from("snore").unwrap(), ReviewStatus::Snore);
        assert!(ReviewStatus::try_from("maybe").is_err());
        let db = Database::in_memory().unwrap();
        let t = Utc.timestamp_opt(1_700_000_000, 0).single().unwrap();
        db.insert_segment_started(&segment("seg", t, "a.wav")).unwrap();
        db.insert_event(&event("evt", "seg", t)).unwrap();
        assert!(db.set_review_status("evt", ReviewStatus::NotSnore).unwrap());
        assert!(!db.set_review_status("absent", ReviewStatus::Snore).unwrap());
        assert_eq!(db.event("evt").unwrap().unwrap().review_status, "not_snore");
    }

    #[test]
    fn date_parser_and_timezone_ranges_handle_calendar_boundaries() {
        let date = parse_date("2025-03-09").unwrap();
        assert!(parse_date("2025-02-30").is_err());
        let (start, end) = local_day_range(date, Some("America/New_York".parse().unwrap()));
        assert_eq!((end - start).num_hours(), 23); // DST spring-forward day
    }

    #[test]
    fn overlapping_segments_are_returned_for_a_local_day_range() {
        let db = Database::in_memory().unwrap();
        let t = Utc.timestamp_opt(1_700_000_000, 0).single().unwrap();
        db.insert_segment_started(&segment("seg", t, "a.wav")).unwrap();
        db.complete_segment(&SegmentCompleted {
            id: "seg".into(), ended_at_utc: t + chrono::Duration::hours(2), frame_count: 0, size_bytes: 44,
            end_reason: "duration".into(), clock_drift_ppm: None, xrun_count: 0, gap_count: 0,
            lost_frames_total: 0, status: SegmentStatus::Complete,
        }).unwrap();
        assert_eq!(db.segments_between(t + chrono::Duration::hours(1), t + chrono::Duration::hours(3)).unwrap().len(), 1);
    }
}
