//! The engine process: `audiocpp_cli`, its stdin, its stdout, and the four
//! things about it that are not obvious.
//!
//! ## Why a long-lived process
//!
//! A cold `audiocpp` spends about 2.5 s of its 5.8 s wall time loading the GGUF
//! and initialising Metal before it decodes the first word. Starting one per
//! window would make the load cost exceed the recognition cost, so one process
//! lasts the whole night and the windows come from the engine's own fixed
//! chunker.
//!
//! ## The four things the engine does that its help text does not say
//!
//! 1. **`partial_text=` is a final, not a partial.** One line per window, and the
//!    next line never revises it. It is forwarded as `kind: "final"`, which is
//!    why `capabilities.partials` is false: a few seconds with no text on screen
//!    is the expected behaviour, not a stall.
//! 2. **`text_output=` is the whole session concatenated.** Storing it would
//!    write the night a second time as one sentence. It is read and dropped.
//! 3. **A silent window prints nothing.** Line count is not window count, so a
//!    window boundary can only come from our own index — never from counting
//!    lines.
//! 4. **The tail only appears before EOF.** Closing stdin early looks identical
//!    and loses most of the audio: the first implementation of this backend sent
//!    a 17 s file and got one window back. `finish` therefore stops feeding,
//!    waits for the writer to drain, and only then lets stdin close.
//!
//! ## Where a window is placed
//!
//! Window `k` is `[k*N, (k+1)*N)` on the engine's own grid, and `k` is sound as
//! long as the lines arrive in order. What is not sound is *when* the engine
//! finished that window: text arrives roughly 1.3 s late, by which time the
//! writer has fed more audio. So `end` is the newest frame the engine's stdin had
//! accepted, and `start` is `k*N`. Nearby windows therefore overlap by about a
//! decode latency, and the interval **always contains** the speech it describes.
//! Tightening it would point a sentence at a patch of silence, which is worse
//! than a three-second precision cue that overshoots.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use steno_lib::config::WIRE_RATE_HZ;

use crate::audiocpp::config::{SidecarConfig, ENGINE_LABEL};
use crate::audiocpp::queue::{AudioPipe, Block};
use crate::audiocpp::Outgoing;
use crate::sidecar_wire::MsgType;

/// How much audio may wait for the engine before the log says so.
const BACKLOG_WARN_SECONDS: u32 = 60;
/// How long the writer thread gets to hand the backlog over after `finish`.
const WRITER_DRAIN_TIMEOUT: Duration = Duration::from_secs(120);
/// How long the engine gets to print its tail windows after stdin closes.
const READER_EOF_TIMEOUT: Duration = Duration::from_secs(60);

struct Placement {
    index: u64,
    start: u64,
    end: u64,
    start_ms: i64,
    end_ms: i64,
}

/// Maps the engine's window index onto frames and frames onto wall clock.
///
/// The base is the `utc_ms` prefix of the first audio block the server sent; the
/// sidecar never asks what time it is, it only counts frames from the moment the
/// server said the stream began.
struct WindowClock {
    chunk_frames: u64,
    base_utc_ms: Option<i64>,
    next_index: u64,
}

impl WindowClock {
    fn new(chunk_frames: u64) -> Self {
        WindowClock {
            chunk_frames: chunk_frames.max(1),
            base_utc_ms: None,
            next_index: 0,
        }
    }

    fn note_base(&mut self, utc_ms: i64) {
        if self.base_utc_ms.is_none() {
            self.base_utc_ms = Some(utc_ms);
        }
    }

    fn place(&mut self, delivered: u64) -> Option<Placement> {
        let base = self.base_utc_ms?;
        let index = self.next_index;
        let start = index.saturating_mul(self.chunk_frames);
        let end = if delivered > start {
            delivered
        } else {
            start.saturating_add(self.chunk_frames)
        };
        self.next_index = index.saturating_add(1);
        Some(Placement {
            index,
            start,
            end,
            start_ms: base.saturating_add(frames_to_ms(start)),
            end_ms: base.saturating_add(frames_to_ms(end)),
        })
    }
}

fn frames_to_ms(frames: u64) -> i64 {
    let rate = i64::from(WIRE_RATE_HZ.max(1));
    i64::try_from(frames)
        .unwrap_or(i64::MAX)
        .saturating_mul(1000)
        / rate
}

pub struct Engine {
    child: Child,
    pipe: Arc<AudioPipe>,
    clock: Arc<Mutex<WindowClock>>,
    writer_done: Receiver<()>,
    reader_done: Receiver<()>,
    writer: Option<JoinHandle<()>>,
    reader: Option<JoinHandle<()>>,
    /// Set by [`Engine::finish`], read by the stdout reader: an EOF that arrives
    /// *before* this is set means the engine died, which the server has to hear
    /// about — a dead recogniser and a quiet room look identical otherwise.
    finished: Arc<AtomicBool>,
}

impl Engine {
    /// Spawns the engine and returns as soon as it is launched. Blocks that
    /// arrive during the model load queue in the pipe, so the load costs those
    /// seconds of latency and nothing else.
    pub fn start(
        config: &SidecarConfig,
        language: &str,
        locale: &str,
        outbox: tokio::sync::mpsc::Sender<Outgoing>,
    ) -> std::io::Result<Engine> {
        let mut command = Command::new(&config.binary);
        command
            .arg("--task")
            .arg("asr")
            .arg("--family")
            .arg("qwen3_asr")
            .arg("--model")
            .arg(&config.model)
            .arg("--mode")
            .arg("streaming")
            .arg("--audio")
            .arg("-")
            .arg("--input-format")
            .arg("s16le")
            .arg("--input-rate")
            .arg(WIRE_RATE_HZ.to_string())
            .arg("--input-channels")
            .arg("1")
            // Fixed windows on purpose: `vad` chunking is refused by the engine in
            // streaming, and `auto` would let the window length differ per night.
            .arg("--audio-chunk-mode")
            .arg("fixed")
            .arg("--audio-chunk-seconds")
            .arg(config.chunk_argument())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // The engine's diagnostics belong in the server's journal, next to the
            // session they explain.
            .stderr(Stdio::inherit());
        if language != "auto" {
            // Omitting `--language` is what makes the model detect for itself.
            command.arg("--language").arg(language);
        }
        let mut child = command.spawn()?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| std::io::Error::other("engine stdin was not piped"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| std::io::Error::other("engine stdout was not piped"))?;

        let pipe = Arc::new(AudioPipe::new(BACKLOG_WARN_SECONDS));
        let clock = Arc::new(Mutex::new(WindowClock::new(config.chunk_frames())));

        let (writer_done_tx, writer_done) = mpsc::channel();
        let write_pipe = Arc::clone(&pipe);
        let writer = thread::Builder::new()
            .name("audiocpp-stdin".to_string())
            .spawn(move || {
                let mut stdin = stdin;
                let mut bytes: Vec<u8> = Vec::new();
                while let Some(block) = write_pipe.pop() {
                    bytes.clear();
                    bytes.reserve(block.samples.len() * 2);
                    for sample in &block.samples {
                        bytes.extend_from_slice(&sample.to_le_bytes());
                    }
                    if let Err(error) = stdin.write_all(&bytes) {
                        tracing::warn!(%error, "cannot write to the engine's stdin");
                        break;
                    }
                    write_pipe.note_delivered(block.end);
                }
                // Dropping stdin is what lets the engine see EOF and decode the
                // windows it was still holding.
                let _ = stdin.flush();
                drop(stdin);
                let _ = writer_done_tx.send(());
            })?;

        let (reader_done_tx, reader_done) = mpsc::channel();
        let finished = Arc::new(AtomicBool::new(false));
        let reader_finished = Arc::clone(&finished);
        let context = LineContext {
            outbox,
            pipe: Arc::clone(&pipe),
            clock: Arc::clone(&clock),
            language: language.to_string(),
            locale: locale.to_string(),
            ready_sent: AtomicBool::new(false),
        };
        let reader = thread::Builder::new()
            .name("audiocpp-stdout".to_string())
            .spawn(move || {
                let mut reader = BufReader::new(stdout);
                let mut buffer: Vec<u8> = Vec::new();
                loop {
                    buffer.clear();
                    match reader.read_until(b'\n', &mut buffer) {
                        // Accumulating bytes up to a newline and decoding then is
                        // what keeps a read that lands mid-character from turning
                        // a Chinese sentence into replacement garbage.
                        Ok(0) => break,
                        Ok(_) => {}
                        Err(error) => {
                            tracing::warn!(%error, "cannot read the engine's stdout");
                            break;
                        }
                    }
                    let line = String::from_utf8_lossy(&buffer);
                    context.handle(line.trim_end_matches(['\r', '\n']));
                }
                // EOF while the session is still running means the engine is gone.
                // Saying so is the difference between "the room was quiet" and "the
                // recogniser died at 02:14"; only the server can act on it.
                if !reader_finished.load(Ordering::SeqCst) {
                    tracing::warn!("the engine exited while the session was running");
                    context.send(
                        MsgType::Session,
                        serde_json::json!({
                            "state": "error",
                            "message": "the engine exited while the session was running; its last stderr line is in the server log",
                        }),
                    );
                }
                let _ = reader_done_tx.send(());
            })?;

        tracing::info!(
            binary = %config.binary.display(),
            chunk_seconds = config.chunk_seconds,
            %language,
            "audiocpp engine started"
        );

        Ok(Engine {
            child,
            pipe,
            clock,
            writer_done,
            reader_done,
            writer: Some(writer),
            reader: Some(reader),
            finished,
        })
    }

    /// The wall clock of the stream's first frame. Only the first call counts:
    /// every later block is placed by counting frames from it.
    pub fn note_clock_base(&self, utc_ms: i64) {
        let mut clock = self.clock.lock().unwrap_or_else(|error| error.into_inner());
        clock.note_base(utc_ms);
    }

    pub fn submit(&self, samples: Vec<i16>, start: u64, end: u64) {
        if !self.pipe.push(Block {
            samples,
            start,
            end,
        }) {
            tracing::warn!("audio arrived after the pipe closed; that tail is not recognised");
        }
    }

    pub fn delivered_frames(&self) -> u64 {
        self.pipe.delivered_frames()
    }

    /// Commits what is still volatile and ends the process.
    ///
    /// The order is the whole of this method: **stop feeding → drain into the
    /// engine → close stdin**. Closing stdin first looks identical and drops
    /// every block the writer had not handed over yet, which is most of the tail.
    pub fn finish(&mut self) {
        if self.finished.load(Ordering::SeqCst) {
            return;
        }
        self.finished.store(true, Ordering::SeqCst);
        self.pipe.close();
        if self.writer_done.recv_timeout(WRITER_DRAIN_TIMEOUT).is_err() {
            tracing::warn!(
                timeout = ?WRITER_DRAIN_TIMEOUT,
                "the writer did not drain; terminating the engine rather than keeping the tail hostage"
            );
            let _ = self.child.kill();
        }
        if self.reader_done.recv_timeout(READER_EOF_TIMEOUT).is_err() {
            tracing::warn!(
                timeout = ?READER_EOF_TIMEOUT,
                "no EOF after stdin closed; terminating the engine"
            );
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
        if let Some(handle) = self.writer.take() {
            let _ = handle.join();
        }
        if let Some(handle) = self.reader.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        if self.finished.load(Ordering::SeqCst) {
            return;
        }
        // An engine outliving its session is a model-resident process nobody will
        // ever read from again.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct LineContext {
    outbox: tokio::sync::mpsc::Sender<Outgoing>,
    pipe: Arc<AudioPipe>,
    clock: Arc<Mutex<WindowClock>>,
    language: String,
    locale: String,
    ready_sent: AtomicBool,
}

impl LineContext {
    fn handle(&self, line: &str) {
        if let Some(raw) = line.strip_prefix("partial_text=") {
            self.report(raw);
        } else if line.starts_with("audio_input=") {
            self.announce_ready();
        } else if line.starts_with("text_output=") {
            tracing::debug!(
                chars = line.len(),
                "session text_output received; the per-window lines already carry every word"
            );
        } else if let Some(reason) = line.strip_prefix("audiocpp_cli failed:") {
            self.fail(reason);
        } else if line.starts_with("family=") || line.starts_with("task=") || line.starts_with("mode=")
        {
            tracing::debug!(line, "audiocpp");
        } else if !line.is_empty() {
            tracing::debug!(line, "unhandled audiocpp stdout line");
        }
    }

    fn report(&self, raw: &str) {
        let text = raw.trim();
        // A window of silence decodes to nothing and prints no line at all; an
        // empty line here would only become a row that cannot be stored.
        if text.is_empty() {
            return;
        }
        let delivered = self.pipe.delivered_frames();
        let placement = {
            let mut clock = self.clock.lock().unwrap_or_else(|error| error.into_inner());
            clock.place(delivered)
        };
        let Some(placement) = placement else {
            tracing::warn!(
                "a window was reported before any audio carried a clock; dropping it rather than inventing a time"
            );
            return;
        };
        if delivered <= placement.start {
            tracing::warn!(
                index = placement.index,
                delivered,
                "window reported before its audio was written; placing it on the grid, not on a guess"
            );
        }
        tracing::debug!(
            index = placement.index,
            start = placement.start,
            end = placement.end,
            chars = text.len(),
            "audiocpp window"
        );
        self.send(
            MsgType::Transcript,
            serde_json::json!({
                "kind": "final",
                "text": text,
                "start_ms": placement.start_ms,
                "end_ms": placement.end_ms,
                "engine": ENGINE_LABEL,
                "language": self.language,
            }),
        );
    }

    fn announce_ready(&self) {
        if self.ready_sent.swap(true, Ordering::SeqCst) {
            return;
        }
        // The model is loaded and the stream is open: the first moment a result
        // can follow, which is what `ready` promises the server.
        self.send(
            MsgType::Session,
            serde_json::json!({
                "state": "ready",
                "lanes": ["text"],
                "engine": ENGINE_LABEL,
                "language": self.language,
                "locale": self.locale,
            }),
        );
    }

    fn fail(&self, reason: &str) {
        let message = if reason.trim().is_empty() {
            "the engine reported a failure with no message".to_string()
        } else {
            reason.trim().to_string()
        };
        tracing::warn!(%message, "engine failure");
        self.send(
            MsgType::Session,
            serde_json::json!({ "state": "error", "message": message }),
        );
    }

    fn send(&self, kind: MsgType, value: serde_json::Value) {
        let payload = match serde_json::to_vec(&value) {
            Ok(payload) => payload,
            Err(error) => {
                tracing::warn!(%error, "cannot encode a frame");
                return;
            }
        };
        if self
            .outbox
            .blocking_send(Outgoing { kind, payload })
            .is_err()
        {
            tracing::warn!("the server is gone; the result was not delivered");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clock(chunk_seconds: f64) -> WindowClock {
        let frames = (chunk_seconds * f64::from(WIRE_RATE_HZ)).round() as u64;
        let mut clock = WindowClock::new(frames);
        clock.note_base(1_000_000);
        clock
    }

    #[test]
    fn the_first_window_starts_at_zero_and_ends_at_the_grid_edge() {
        let mut clock = clock(5.0);
        let placement = clock.place(0).expect("a base was set");
        assert_eq!(placement.start, 0);
        assert_eq!(placement.end, 80_000);
        assert_eq!(placement.start_ms, 1_000_000);
        assert_eq!(placement.end_ms, 1_005_000);
    }

    #[test]
    fn a_window_ends_at_the_frames_already_delivered_when_that_is_later() {
        let mut clock = clock(5.0);
        let placement = clock.place(96_000).expect("a base was set");
        assert_eq!(placement.end, 96_000);
        assert_eq!(placement.end_ms, 1_006_000);
    }

    #[test]
    fn indices_advance_one_per_reported_window() {
        let mut clock = clock(5.0);
        assert_eq!(clock.place(80_000).map(|p| p.index), Some(0));
        assert_eq!(clock.place(160_000).map(|p| p.index), Some(1));
        assert_eq!(clock.place(240_000).map(|p| p.start), Some(160_000));
    }

    #[test]
    fn a_window_reported_before_any_clock_base_is_refused() {
        let mut clock = WindowClock::new(80_000);
        assert!(clock.place(0).is_none());
    }

    #[test]
    fn the_clock_base_is_not_moved_by_a_later_block() {
        let mut clock = clock(5.0);
        clock.note_base(2_000_000);
        assert_eq!(
            clock.place(0).map(|p| p.start_ms),
            Some(1_000_000),
            "the base is the stream's first frame, not the newest one"
        );
    }
}
