-- 0003 — streaming transcription produces phrases, not per-segment text.
--
-- One utterance can span a segment boundary, and a segment can hold several.
-- They are keyed by wall clock so the UI can lay them on the same timeline as
-- the events.

CREATE TABLE utterances (
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
