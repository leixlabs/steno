//! Continuous local archive for the capture node.
//!
//! ## Why the node writes 900 MB a night it may never send
//!
//! The uplink may drop quiet stretches (see [`steno_lib::vad`]), but the product
//! still promises a whole night. The only place that can hold the whole night
//! once the uplink gates is the node, so the node writes it here before the gate
//! is even consulted. Gating without this file would not be "saving bandwidth",
//! it would be discarding audio and calling it policy.
//!
//! ## Why plain WAV and no database
//!
//! The node must not link SQLite: a crash on a bedside device should cost at most
//! the current file, and a directory of timestamped WAVs is recoverable by hand.
//! Rotation is by duration, retention is oldest-first by file name.
//!
//! ## The power-loss objection, and what answers it
//!
//! `main.rs` used to say the node keeps **no** local storage, because buffering to
//! an SD card reintroduces the power-loss corruption risk the split deployment
//! exists to avoid. That objection is real, and it is why this writer patches the
//! header every `HEADER_FLUSH_SECONDS` instead of only on close: a power cut costs
//! at most that window of the current file, not the whole night, and every closed
//! file in the directory is already playable. The objection still stands against
//! *buffering for retransmission* — this file is an archive, not a queue: nothing
//! here is ever replayed to the server.

use std::fs::{self, File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Bytes per sample. The node only ever captures S16_LE.
const BYTES_PER_SAMPLE: u64 = 2;
const HEADER_BYTES: u64 = 44;
/// How much audio may be lost to a power cut, in seconds. Same rationale as the
/// server's `audio.header_flush_interval_seconds`.
const HEADER_FLUSH_SECONDS: u64 = 30;

/// One open `.wav` with a placeholder header until it is closed.
struct OpenSegment {
    path: PathBuf,
    file: File,
    frames: u64,
    rate_hz: u32,
}

/// Rotating WAV writer with oldest-first retention.
pub struct WavArchive {
    dir: PathBuf,
    segment_seconds: u64,
    max_bytes: u64,
    rate_hz: u32,
    current: Option<OpenSegment>,
    /// Counts the files this process has started. Two rotations inside the same
    /// second would otherwise get the same name and truncate each other.
    sequence: u64,
}

impl WavArchive {
    /// Opens (and creates) the archive directory. `max_bytes` is the ceiling for
    /// every file this writer owns, enforced on rotation.
    pub fn open(
        dir: impl Into<PathBuf>,
        segment_seconds: u64,
        max_bytes: u64,
        rate_hz: u32,
    ) -> std::io::Result<Self> {
        let dir = dir.into();
        fs::create_dir_all(&dir)?;
        Ok(WavArchive {
            dir,
            segment_seconds: segment_seconds.max(1),
            max_bytes: max_bytes.max(HEADER_BYTES * 2),
            rate_hz,
            current: None,
            sequence: 0,
        })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Appends one block of mono samples, rotating first if this block would
    /// cross the segment duration.
    pub fn push(&mut self, samples: &[i16]) -> std::io::Result<()> {
        if samples.is_empty() {
            return Ok(());
        }
        let frames = samples.len() as u64;
        let rotate_at = self.segment_seconds.saturating_mul(u64::from(self.rate_hz.max(1)));
        if let Some(current) = self.current.as_ref() {
            if current.frames + frames > rotate_at {
                self.rotate()?;
            }
        }
        if self.current.is_none() {
            self.start_segment()?;
        }
        let current = self.current.as_mut().expect("segment opened above");
        let mut bytes = Vec::with_capacity(samples.len() * BYTES_PER_SAMPLE as usize);
        for sample in samples {
            bytes.extend_from_slice(&sample.to_le_bytes());
        }
        current.file.write_all(&bytes)?;
        current.frames += frames;
        // Patch the header on a timer, so the file is playable after an
        // unplugged power supply instead of carrying a placeholder size.
        let flush_every = u64::from(self.rate_hz.max(1)) * HEADER_FLUSH_SECONDS;
        if current.frames >= flush_every {
            let data_bytes = current.frames * BYTES_PER_SAMPLE;
            let header = wav_header(current.rate_hz, 1, data_bytes);
            current.file.seek(SeekFrom::Start(0))?;
            current.file.write_all(&header)?;
            // Back to the end, or the next block would overwrite audio.
            current
                .file
                .seek(SeekFrom::Start(HEADER_BYTES + data_bytes))?;
        }
        Ok(())
    }

    /// Closes the open file with a correct header. Safe to call twice.
    pub fn finish(&mut self) -> std::io::Result<()> {
        if let Some(current) = self.current.take() {
            finalise(current)?;
        }
        Ok(())
    }

    fn rotate(&mut self) -> std::io::Result<()> {
        if let Some(current) = self.current.take() {
            finalise(current)?;
        }
        self.reap()?;
        Ok(())
    }

    fn start_segment(&mut self) -> std::io::Result<()> {
        let started = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_secs())
            .unwrap_or(0);
        // `started` alone is not unique: a short `segment_seconds` can rotate
        // twice within one second, and the second file would truncate the first.
        let path = self
            .dir
            .join(format!("node-{started:010}-{:04}.wav", self.sequence));
        self.sequence = self.sequence.wrapping_add(1);
        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&path)?;
        file.write_all(&wav_header(self.rate_hz, 1, 0))?;
        self.current = Some(OpenSegment {
            path,
            file,
            frames: 0,
            rate_hz: self.rate_hz,
        });
        Ok(())
    }

    /// Deletes the oldest finished files until the directory is back under the
    /// budget. The open file is never a candidate: it is not in the listing yet
    /// with its final size.
    fn reap(&mut self) -> std::io::Result<()> {
        let mut files: Vec<(u64, PathBuf)> = Vec::new();
        let mut total = 0u64;
        for entry in fs::read_dir(&self.dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().is_none_or(|ext| ext != "wav") {
                continue;
            }
            if let Some(open) = self.current.as_ref() {
                if open.path == path {
                    continue;
                }
            }
            let size = entry.metadata().map(|meta| meta.len()).unwrap_or(0);
            total += size;
            // File names embed the start second, so name order is time order.
            files.push((size, path));
        }
        // Reserve room for the file that is about to start, or the budget would
        // be exceeded again the moment it is opened — the check has to hold for
        // the steady state, not just for this instant.
        let reserve = HEADER_BYTES
            + self
                .segment_seconds
                .saturating_mul(u64::from(self.rate_hz.max(1)))
                .saturating_mul(BYTES_PER_SAMPLE);
        files.sort_by(|a, b| a.1.cmp(&b.1));
        for (size, path) in files {
            if total.saturating_add(reserve) <= self.max_bytes {
                break;
            }
            if fs::remove_file(&path).is_ok() {
                total = total.saturating_sub(size);
            }
        }
        Ok(())
    }
}

/// Writes the real sizes into the placeholder header and closes the file.
fn finalise(mut current: OpenSegment) -> std::io::Result<()> {
    let data_bytes = current.frames * BYTES_PER_SAMPLE;
    let header = wav_header(current.rate_hz, 1, data_bytes);
    current.file.seek(SeekFrom::Start(0))?;
    current.file.write_all(&header)?;
    current.file.flush()?;
    Ok(())
}

/// Canonical 44-byte little-endian PCM header.
fn wav_header(rate_hz: u32, channels: u16, data_bytes: u64) -> [u8; 44] {
    let mut header = [0u8; 44];
    let riff = 36u32.saturating_add(data_bytes.min(u64::from(u32::MAX - 36)) as u32);
    header[0..4].copy_from_slice(b"RIFF");
    header[4..8].copy_from_slice(&riff.to_le_bytes());
    header[8..12].copy_from_slice(b"WAVE");
    header[12..16].copy_from_slice(b"fmt ");
    header[16..20].copy_from_slice(&16u32.to_le_bytes());
    header[20..22].copy_from_slice(&1u16.to_le_bytes()); // PCM
    header[22..24].copy_from_slice(&channels.to_le_bytes());
    header[24..28].copy_from_slice(&rate_hz.to_le_bytes());
    let block_align = channels.saturating_mul(BYTES_PER_SAMPLE as u16);
    header[28..32].copy_from_slice(&(rate_hz.saturating_mul(u32::from(block_align))).to_le_bytes());
    header[32..34].copy_from_slice(&block_align.to_le_bytes());
    header[34..36].copy_from_slice(&16u16.to_le_bytes());
    header[36..40].copy_from_slice(b"data");
    header[40..44].copy_from_slice(&(data_bytes.min(u64::from(u32::MAX)) as u32).to_le_bytes());
    header
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    fn read_wav(path: &Path) -> (u32, u32, u32) {
        let mut file = File::open(path).expect("open wav");
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).expect("read wav");
        assert_eq!(&bytes[0..4], b"RIFF");
        assert_eq!(&bytes[8..12], b"WAVE");
        let rate = u32::from_le_bytes(bytes[24..28].try_into().unwrap());
        let data = u32::from_le_bytes(bytes[40..44].try_into().unwrap());
        let file_len = bytes.len() as u32;
        (rate, data, file_len)
    }

    #[test]
    fn a_closed_file_declares_exactly_what_it_holds() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut archive =
            WavArchive::open(dir.path(), 60, 1 << 20, 16_000).expect("archive");
        archive.push(&vec![7i16; 320]).expect("push");
        archive.push(&vec![7i16; 320]).expect("push");
        archive.finish().expect("finish");

        let files: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .collect();
        assert_eq!(files.len(), 1);
        let (rate, data, file_len) = read_wav(&files[0]);
        assert_eq!(rate, 16_000);
        assert_eq!(data, 640 * 2);
        assert_eq!(file_len, 44 + 640 * 2);
    }

    #[test]
    fn the_duration_wins_over_a_single_block_and_rotates() {
        let dir = tempfile::tempdir().expect("tempdir");
        // 0.04 s per 640-frame block: two blocks fill a 0.05 s segment.
        let mut archive =
            WavArchive::open(dir.path(), 1, 1 << 20, 16_000).expect("archive");
        for _ in 0..3 {
            archive.push(&vec![0i16; 16_000]).expect("push a full second");
        }
        archive.finish().expect("finish");
        let count = fs::read_dir(dir.path()).unwrap().count();
        assert_eq!(count, 3, "each second is its own file");
    }

    #[test]
    fn retention_reaps_the_oldest_and_keeps_the_budget() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Roughly one second per file, budget for about two.
        let mut archive = WavArchive::open(dir.path(), 1, 70_000, 16_000).expect("archive");
        for _ in 0..5 {
            archive.push(&vec![0i16; 16_000]).expect("push a full second");
        }
        archive.finish().expect("finish");
        let mut total = 0u64;
        for entry in fs::read_dir(dir.path()).unwrap() {
            total += entry.unwrap().metadata().unwrap().len();
        }
        assert!(
            total <= 70_000,
            "retention must keep the directory under the budget, got {total}"
        );
    }
}
