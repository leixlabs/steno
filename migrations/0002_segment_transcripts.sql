-- 0002 — per-segment transcription.
--
-- Transcription is an optional, additive stage: it never blocks recording and
-- it can be re-run, so it gets its own table rather than columns on
-- `recording_segments`.

CREATE TABLE segment_transcripts (
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
