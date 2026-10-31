-- 0001 — the recording spine: segments, gaps and candidate events.
--
-- Nothing here knows about the detector: a segment is cut by duration, and an
-- event is an annotation attached to a segment. `PRAGMA user_version` is set by
-- the applier (`crates/server/src/migrate.rs`), not by this file, so the same
-- text can also be run by `sqlx migrate run` on its own.

CREATE TABLE recording_segments (
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

-- Superseded by 0004, which rebuilds this table to add `source`; the definition
-- that counts is the one in that file. Keeping the original here is what makes
-- an existing v1 file a real starting point rather than a fiction.
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
