//! Asynchronous single-owner SQLite writer (TECH_SPEC §4.3/§7, task T4.2).
//!
//! Capture, detection and transcription workers send commands; only this thread
//! owns `Database` and executes writes. There is no foreign-key retry loop any
//! more: an episode or a transcript may legitimately point at no recording (the
//! audio may have been rotated away, or the file may not have closed yet), so the
//! rows no longer depend on each other's arrival order.

use crate::storage::{Database, EpisodeRow, RecordingDraft};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};
use std::time::Duration;

#[derive(Debug)]
pub enum DbCommand {
    /// A finished file for the `recordings` table.
    Recording(RecordingDraft),
    /// A snore for the `episodes` table, from either detector.
    Episode(EpisodeRow),
    Flush(Sender<()>),
    /// How many annotations fall inside a window. Asked by the recorder when it
    /// closes a file, to decide whether the file is worth keeping.
    CountBetween {
        from: chrono::DateTime<chrono::Utc>,
        to: chrono::DateTime<chrono::Utc>,
        ack: Sender<u64>,
    },
    Shutdown,
}

#[derive(Clone, Debug)]
pub struct DbWriterHandle {
    tx: Sender<DbCommand>,
}

impl DbWriterHandle {
    pub fn send(&self, command: DbCommand) -> std::result::Result<(), String> {
        self.tx.send(command).map_err(|_| "database writer has stopped".to_string())
    }

    pub fn recording(&self, recording: RecordingDraft) -> std::result::Result<(), String> {
        self.send(DbCommand::Recording(recording))
    }

    pub fn episode(&self, episode: EpisodeRow) -> std::result::Result<(), String> {
        self.send(DbCommand::Episode(episode))
    }

    /// Counts the annotations inside a window, synchronously.
    pub fn count_between(
        &self,
        from: chrono::DateTime<chrono::Utc>,
        to: chrono::DateTime<chrono::Utc>,
        timeout: Duration,
    ) -> Option<u64> {
        let (ack, rx) = mpsc::channel();
        self.tx.send(DbCommand::CountBetween { from, to, ack }).ok()?;
        rx.recv_timeout(timeout).ok()
    }

    /// Waits until all commands enqueued before this call have been applied.
    pub fn flush(&self, timeout: Duration) -> bool {
        let (tx, rx) = mpsc::channel();
        if self.tx.send(DbCommand::Flush(tx)).is_err() {
            return false;
        }
        rx.recv_timeout(timeout).is_ok()
    }
}

pub struct DbWriter {
    handle: DbWriterHandle,
    join: Option<JoinHandle<()>>,
}

impl DbWriter {
    pub fn spawn(database: Database, on_error: impl Fn(String) + Send + 'static) -> Self {
        let (tx, rx) = mpsc::channel();
        let handle = DbWriterHandle { tx };
        let join = thread::Builder::new()
            .name("steno-db-writer".into())
            .spawn(move || run_writer(database, rx, on_error))
            .expect("failed to spawn DB writer thread");
        DbWriter { handle, join: Some(join) }
    }

    pub fn handle(&self) -> DbWriterHandle {
        self.handle.clone()
    }

    pub fn shutdown(mut self, timeout: Duration) -> bool {
        // Flush is queued before Shutdown, so its acknowledgement proves every
        // prior command has been applied before we join the worker.
        let drained = self.handle.flush(timeout);
        let _ = self.handle.send(DbCommand::Shutdown);
        let Some(join) = self.join.take() else { return true; };
        if drained {
            let _ = join.join();
        }
        drained
    }
}

fn run_writer(database: Database, rx: Receiver<DbCommand>, on_error: impl Fn(String)) {
    while let Ok(command) = rx.recv() {
        match command {
            DbCommand::Recording(recording) => {
                if let Err(error) = database.insert_recording(&recording) {
                    on_error(format!(
                        "recording write failed for {}: {error}",
                        recording.id
                    ));
                }
            }
            DbCommand::Episode(episode) => {
                if let Err(error) = database.insert_episode(&episode) {
                    on_error(format!("episode write failed for {}: {error}", episode.id));
                }
            }
            DbCommand::CountBetween { from, to, ack } => {
                let count = database.count_annotations(from, to).unwrap_or(0);
                let _ = ack.send(count);
            }
            DbCommand::Flush(ack) => {
                let _ = ack.send(());
            }
            DbCommand::Shutdown => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::{EpisodeRow, ReviewStatus};
    use chrono::Utc;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    fn recording(id: &str) -> RecordingDraft {
        RecordingDraft {
            id: id.into(),
            relative_path: PathBuf::from(format!("2025/01/01/{id}.wav")),
            started_at_utc: Utc::now(),
            ended_at_utc: Utc::now(),
            capture_rate_hz: 16_000,
            channels: 1,
            frame_count: 16_000,
            size_bytes: 32_044,
            lost_frames: 0,
        }
    }

    fn episode(id: &str) -> EpisodeRow {
        let now = Utc::now();
        EpisodeRow {
            id: id.into(),
            recording_id: None,
            started_at_utc: now,
            ended_at_utc: now,
            source: "rule-v1".into(),
            label: None,
            confidence: Some(0.8),
            review_status: ReviewStatus::Unreviewed,
            created_at_utc: now,
        }
    }

    #[test]
    fn writer_applies_recordings_and_episodes_and_flushes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app.db");
        // Two connections to one file, exactly as `main.rs` wires the writer and
        // the HTTP side.
        let writer_db = Database::open(&path).unwrap();
        let reader = Database::open(&path).unwrap();
        let errors = Arc::new(Mutex::new(Vec::new()));
        let errors_for_cb = Arc::clone(&errors);
        let writer = DbWriter::spawn(writer_db, move |error| errors_for_cb.lock().unwrap().push(error));
        let handle = writer.handle();
        // An episode may be written before the file that carries it closes.
        handle.episode(episode("evt")).unwrap();
        handle.recording(recording("rec")).unwrap();
        assert!(handle.flush(Duration::from_secs(2)));
        assert!(reader.recording("rec").unwrap().is_some(), "the flush must cover the recording");
        assert!(reader.episode("evt").unwrap().is_some(), "the flush must cover the episode");
        assert!(writer.shutdown(Duration::from_secs(2)));
        assert!(errors.lock().unwrap().is_empty(), "writer errors: {:?}", *errors.lock().unwrap());
    }

    #[test]
    fn a_failed_write_is_reported_instead_of_being_dropped() {
        let db = Database::in_memory().unwrap();
        let errors = Arc::new(Mutex::new(Vec::new()));
        let errors_for_cb = Arc::clone(&errors);
        let writer = DbWriter::spawn(db, move |error| errors_for_cb.lock().unwrap().push(error));
        let handle = writer.handle();
        // The schema refuses an unknown source; the writer must say so.
        let mut bad = episode("bad");
        bad.source = "unknown".into();
        handle.episode(bad).unwrap();
        assert!(handle.flush(Duration::from_secs(2)));
        assert!(writer.shutdown(Duration::from_secs(2)));
        let errors = errors.lock().unwrap();
        assert_eq!(errors.len(), 1, "one refusal, one report: {errors:?}");
        assert!(errors[0].contains("episode write failed for bad"), "{errors:?}");
    }
}
