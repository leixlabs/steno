//! The backlog between the socket and the engine's stdin.
//!
//! ## Why a queue at all
//!
//! `audiocpp_cli` reads PCM from a pipe with a fixed kernel buffer. Writing from
//! the socket reader would block that reader the moment the model is slower than
//! realtime for a couple of seconds, and a blocked socket reader stops draining
//! the channel the server is writing to. So the reader only appends here, and a
//! thread of our own feeds the engine at whatever pace it can take.
//!
//! ## Why the backlog is unbounded
//!
//! A dropped block is a hole in the timeline that nobody is ever told about;
//! growing memory is a symptom someone can see in `ps`. The live path never
//! needs the headroom — the engine decodes a 5 s window in about 1.3 s, so the
//! queue stays at a few hundred milliseconds — but a backlog that is *chosen*
//! (a slow host, a shorter window, file mode) must not silently become loss. The
//! warn threshold is where latency becomes visible in the log, not a limit.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Condvar, Mutex};

use steno_lib::config::WIRE_RATE_HZ;

/// One block of audio on its way to the engine, with the frames it occupies on
/// the session timeline. The range is what a window is placed against later, so
/// it is carried with the samples instead of being recomputed from lengths.
#[derive(Debug)]
pub struct Block {
    pub samples: Vec<i16>,
    pub start: u64,
    pub end: u64,
}

struct State {
    pending: VecDeque<Block>,
    queued_bytes: usize,
    closed: bool,
    warned: bool,
}

pub struct AudioPipe {
    state: Mutex<State>,
    ready: Condvar,
    /// End of the last block the engine's stdin accepted. The upper bound of
    /// every window: the engine cannot have heard more than this.
    delivered: AtomicI64,
    warn_bytes: usize,
}

impl AudioPipe {
    pub fn new(warn_seconds: u32) -> Self {
        AudioPipe {
            state: Mutex::new(State {
                pending: VecDeque::new(),
                queued_bytes: 0,
                closed: false,
                warned: false,
            }),
            ready: Condvar::new(),
            delivered: AtomicI64::new(0),
            warn_bytes: (warn_seconds as usize)
                .saturating_mul(WIRE_RATE_HZ as usize)
                .saturating_mul(2),
        }
    }

    /// Called on the socket reader. Never blocks, never drops.
    ///
    /// Returns `false` when the pipe is already closed, which is the only case
    /// where audio is refused — and the caller says so in the log, because the
    /// tail past a `stop` is the one thing a finish can lose.
    pub fn push(&self, block: Block) -> bool {
        let behind_seconds = {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            if state.closed {
                return false;
            }
            state.queued_bytes = state.queued_bytes.saturating_add(block.samples.len() * 2);
            state.pending.push_back(block);
            if !state.warned && state.queued_bytes > self.warn_bytes {
                state.warned = true;
                Some(state.queued_bytes / (WIRE_RATE_HZ as usize * 2))
            } else {
                None
            }
        };
        self.ready.notify_one();
        if let Some(seconds) = behind_seconds {
            tracing::warn!(
                seconds,
                "the engine is behind; transcript latency is growing (nothing is dropped)"
            );
        }
        true
    }

    /// Blocking pop for the writer thread. `None` means closed and drained.
    pub fn pop(&self) -> Option<Block> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        loop {
            if let Some(block) = state.pending.pop_front() {
                state.queued_bytes = state.queued_bytes.saturating_sub(block.samples.len() * 2);
                return Some(block);
            }
            if state.closed {
                return None;
            }
            state = self.ready.wait(state).unwrap_or_else(|error| error.into_inner());
        }
    }

    pub fn note_delivered(&self, end: u64) {
        let end = i64::try_from(end).unwrap_or(i64::MAX);
        let mut current = self.delivered.load(Ordering::Relaxed);
        while end > current {
            match self.delivered.compare_exchange_weak(
                current,
                end,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(observed) => current = observed,
            }
        }
    }

    pub fn close(&self) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.closed = true;
        drop(state);
        self.ready.notify_all();
    }

    pub fn delivered_frames(&self) -> u64 {
        u64::try_from(self.delivered.load(Ordering::Relaxed)).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(start: u64, frames: usize) -> Block {
        Block {
            samples: vec![0; frames],
            start,
            end: start + frames as u64,
        }
    }

    #[test]
    fn blocks_come_out_in_the_order_they_went_in() {
        let pipe = AudioPipe::new(60);
        assert!(pipe.push(block(0, 4)));
        assert!(pipe.push(block(4, 4)));
        assert_eq!(pipe.pop().map(|b| b.start), Some(0));
        assert_eq!(pipe.pop().map(|b| b.start), Some(4));
    }

    #[test]
    fn closing_drains_what_is_queued_before_it_returns_none() {
        let pipe = AudioPipe::new(60);
        assert!(pipe.push(block(0, 4)));
        pipe.close();
        assert!(pipe.pop().is_some());
        assert!(pipe.pop().is_none());
    }

    #[test]
    fn audio_pushed_after_close_is_refused_rather_than_queued() {
        let pipe = AudioPipe::new(60);
        pipe.close();
        assert!(!pipe.push(block(0, 4)));
    }

    #[test]
    fn delivered_frames_only_move_forward() {
        let pipe = AudioPipe::new(60);
        pipe.note_delivered(100);
        pipe.note_delivered(64);
        assert_eq!(pipe.delivered_frames(), 100);
    }
}
