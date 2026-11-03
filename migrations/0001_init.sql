-- 0001 — three tables: what was kept, what was heard, what was said.
--
-- The history is collapsed into this one file on purpose. Earlier versions built
-- five tables (recording_segments / audio_gaps / snore_events /
-- segment_transcripts / utterances) and then migrated to these three across
-- 0005 and 0006; a fresh database should not create a schema in order to drop it.
-- There is no compatibility path: an old file is refused rather than upgraded,
-- and `var/db/app.db.before-3tables` is kept beside the database as the record of
-- what it looked like.
--
--   recordings    one row per WAV that exists. Written when the file is closed, so
--                 "a row exists" and "a file exists" say the same thing — there is
--                 no status column to disbelieve.
--   episodes      one row per episode a producer decided was snoring, with the
--                 wall clock it started and ended. Two producers write here and
--                 `source` says which: the rule detector answers "there was
--                 sound", the classifier answers "this is snoring".
--   transcripts   one row per recognised stretch of speech, with its own bounds.
--
-- Annotations are written the moment they are produced, which is usually before
-- the file they belong to closes, so `recording_id` starts NULL and the recorder
-- links it afterwards by wall clock. `ON DELETE SET NULL` lets a retention sweep
-- remove audio without losing the annotation.
--
-- What was given up, and why it is not coming back: per-gap rows (an xrun and a
-- queue overflow are now one `lost_frames` number), clock quality, channel
-- mapping, boot id and the segment lifecycle statuses. All of them were diagnostic
-- columns for a recorder that no longer has a lifecycle to report.

CREATE TABLE recordings (
    id              TEXT PRIMARY KEY,
    relative_path   TEXT NOT NULL UNIQUE,
    started_at_utc  TEXT NOT NULL,
    ended_at_utc    TEXT NOT NULL,
    capture_rate_hz INTEGER NOT NULL CHECK(capture_rate_hz > 0),
    channels        INTEGER NOT NULL CHECK(channels BETWEEN 1 AND 2),
    frame_count     INTEGER NOT NULL CHECK(frame_count >= 0),
    size_bytes      INTEGER NOT NULL CHECK(size_bytes >= 0),
    lost_frames     INTEGER NOT NULL DEFAULT 0 CHECK(lost_frames >= 0),
    created_at_utc  TEXT NOT NULL
);

CREATE INDEX idx_recordings_started_at ON recordings(started_at_utc);

CREATE TABLE episodes (
    id              TEXT PRIMARY KEY,
    recording_id    TEXT REFERENCES recordings(id) ON DELETE SET NULL,
    started_at_utc  TEXT NOT NULL,
    ended_at_utc    TEXT NOT NULL,
    source          TEXT NOT NULL CHECK(source IN ('rule-v1','apple-soundanalysis-v1')),
    label           TEXT,
    confidence      REAL CHECK(confidence IS NULL OR (confidence >= 0 AND confidence <= 1)),
    review_status   TEXT NOT NULL DEFAULT 'unreviewed'
                    CHECK(review_status IN ('snore','not_snore','unreviewed')),
    created_at_utc  TEXT NOT NULL
);

CREATE INDEX idx_episodes_started_at ON episodes(started_at_utc);
CREATE INDEX idx_episodes_recording ON episodes(recording_id);

CREATE TABLE transcripts (
    id              TEXT PRIMARY KEY,
    recording_id    TEXT REFERENCES recordings(id) ON DELETE SET NULL,
    started_at_utc  TEXT NOT NULL,
    ended_at_utc    TEXT NOT NULL,
    text            TEXT NOT NULL,
    engine          TEXT NOT NULL,
    language        TEXT,
    created_at_utc  TEXT NOT NULL
);

CREATE INDEX idx_transcripts_started_at ON transcripts(started_at_utc);
CREATE INDEX idx_transcripts_recording ON transcripts(recording_id);
