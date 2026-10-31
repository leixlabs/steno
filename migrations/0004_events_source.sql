-- 0004 — every event has to say which producer wrote it.
--
-- The sidecar's classifier is a second producer the original schema never
-- anticipated, and without a `source` column the two annotations of the same
-- second are indistinguishable after the fact.
--
-- The table is rebuilt rather than altered, so the new columns can sit in a
-- readable order and carry the CHECKs they need. Existing rows are carried over
-- with `source='rule-v1'`: before this version the rule detector was the table's
-- only writer, so that is a fact, not a guess. The classifier columns stay NULL
-- for them, because the DSP has no label and no model confidence to report.

CREATE TABLE snore_events_v4 (
    id TEXT PRIMARY KEY,
    segment_id TEXT NOT NULL REFERENCES recording_segments(id) ON DELETE CASCADE,
    start_offset_frames INTEGER NOT NULL CHECK(start_offset_frames >= 0),
    end_offset_frames INTEGER NOT NULL CHECK(end_offset_frames >= start_offset_frames),
    started_at_utc TEXT NOT NULL,
    ended_at_utc TEXT NOT NULL,
    duration_ms INTEGER NOT NULL CHECK(duration_ms >= 0),
    source TEXT NOT NULL CHECK(source IN ('rule-v1','apple-soundanalysis-v1')),
    rule_score REAL CHECK(rule_score IS NULL OR (rule_score >= 0 AND rule_score <= 1)),
    detector_version TEXT NOT NULL,
    classifier_label TEXT,
    classifier_confidence REAL CHECK(classifier_confidence IS NULL OR (classifier_confidence >= 0 AND classifier_confidence <= 1)),
    classifier_threshold REAL,
    peak_dbfs REAL,
    mean_level_dbfs REAL,
    mean_band_ratio REAL,
    noise_floor_dbfs REAL,
    end_reason TEXT NOT NULL DEFAULT 'normal' CHECK(end_reason IN ('normal','max_duration','segment_end','gap','detector_gap','shutdown')),
    review_status TEXT NOT NULL DEFAULT 'unreviewed' CHECK(review_status IN ('snore','not_snore','unreviewed')),
    continued INTEGER NOT NULL DEFAULT 0 CHECK(continued IN (0,1)),
    created_at_utc TEXT NOT NULL
);

INSERT INTO snore_events_v4 (
    id,segment_id,start_offset_frames,end_offset_frames,started_at_utc,
    ended_at_utc,duration_ms,source,rule_score,detector_version,
    classifier_label,classifier_confidence,classifier_threshold,
    peak_dbfs,mean_level_dbfs,mean_band_ratio,noise_floor_dbfs,
    end_reason,review_status,continued,created_at_utc
)
SELECT
    id,segment_id,start_offset_frames,end_offset_frames,started_at_utc,
    ended_at_utc,duration_ms,'rule-v1',rule_score,detector_version,
    NULL,NULL,NULL,
    peak_dbfs,mean_level_dbfs,mean_band_ratio,noise_floor_dbfs,
    end_reason,review_status,continued,created_at_utc
FROM snore_events;

DROP TABLE snore_events;
ALTER TABLE snore_events_v4 RENAME TO snore_events;
CREATE INDEX idx_events_started_at ON snore_events(started_at_utc);
CREATE INDEX idx_events_segment_offset ON snore_events(segment_id,start_offset_frames);
