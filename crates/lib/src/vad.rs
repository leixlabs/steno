//! A two-state activity gate for the capture node.
//!
//! ## The model
//!
//! ```text
//!            level >= floor + margin
//!   CLOSED ─────────────────────────────► OPEN
//!      ▲                                   │
//!      └───────────────────────────────────┘
//!            close_quiet_ms of continuous quiet
//! ```
//!
//! OPEN sends every block. CLOSED sends nothing and records the frame range it
//! withheld. So an utterance, a sentence, a conversation turn — whatever happens
//! before the room has been quiet for `close_quiet_ms` — leaves the device as one
//! contiguous run, and the server sees one file instead of a spray of fragments.
//!
//! ## The two edges are deliberately not symmetric
//!
//! * **Opening is cheap and jumpy.** Any block above the ambient opens the gate.
//!   A door, a cough, a false trigger — all of them only cost bandwidth, because
//!   opening can never hide audio. Being slow to open *would* hide audio (the
//!   attack of the very event you are trying to keep).
//! * **Closing is slow and reluctant.** It takes `close_quiet_ms` (20 s by
//!   default) of continuous quiet, and the quiet spent waiting is itself sent —
//!   it is attached to the run that just ended. That trailing silence is not
//!   waste: it is what keeps "嗯……" and the pause after it in the same file.
//!
//! ## Why the trailing quiet has to be sent, not gated
//!
//! Because a pause is not observable until it is over. If the gate withheld the
//! first second of a silence and then speech resumed, the two halves would arrive
//! as separate files with a hole between them — the pause would have been
//! manufactured by the gate, not measured. Waiting `close_quiet_ms` before
//! closing means every gap the server sees is at least that long, which is also
//! what lets a timeline distinguish "someone stopped talking" from "the node
//! dropped a packet".
//!
//! ## The one number that must stay below the detector's
//!
//! `margin_db` decides how far above ambient a block must sit to open. It has to
//! be **smaller** than the detector's `noise_margin_db` (8 dB by default), or the
//! gate could stay closed through audio the detector would have called an event.
//! The default is 6 dB for exactly that reason. This is a comparison of two
//! estimates, not a proof: the detector measures band-limited energy and this
//! gate measures wideband RMS, so the two floors are close but not identical.
//!
//! ## What this is not
//!
//! It is not a voice activity detector. A fan, a fridge and a conversation at the
//! same level are indistinguishable to it. That is acceptable *only* because the
//! error it makes is the safe one — it opens on anything louder than the room —
//! and because what it withholds is written to the local archive, never dropped.

/// Tuning for [`VadGate`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GateTuning {
    /// How far above the running ambient a block must sit to open the gate. Keep
    /// it below the detector's `noise_margin_db`, or the gate could stay closed
    /// through an event.
    pub margin_db: f32,
    /// How long the room must stay quiet before the gate closes. This is the
    /// unit-of-meaning knob, not a tail: 2 s treats every burst as its own thing,
    /// 20 s keeps a conversation turn whole.
    pub close_quiet_ms: u32,
    /// Audio always sent at the start of a session, whatever the level.
    pub warmup_ms: u32,
    /// Time constant of the ambient tracker, in milliseconds. Long on purpose:
    /// the ambient must not chase a passing snore upwards.
    pub floor_window_ms: u32,
}

impl Default for GateTuning {
    fn default() -> Self {
        GateTuning {
            margin_db: 6.0,
            // 2 s produced a 2.1 s median fragment on a real night — one sentence
            // cut into three. 20 s keeps a turn together and still closes during a
            // real lull.
            close_quiet_ms: 20_000,
            warmup_ms: 5_000,
            floor_window_ms: 30_000,
        }
    }
}

impl GateTuning {
    /// Rejects settings that would gate audio the recorder cannot get back.
    pub fn validate(&self) -> Result<(), String> {
        if !(2.0..=12.0).contains(&self.margin_db) {
            return Err(format!(
                "gate.margin_db: must be within 2..=12 dB, got {}",
                self.margin_db
            ));
        }
        if self.close_quiet_ms < 3_000 {
            return Err(format!(
                "gate.close_quiet_ms: must be at least 3000 ms, or every pause splits \
                 an utterance; got {}",
                self.close_quiet_ms
            ));
        }
        if self.warmup_ms < 1_000 {
            return Err(format!(
                "gate.warmup_ms: must be at least 1000 ms so the server can anchor, got {}",
                self.warmup_ms
            ));
        }
        if self.floor_window_ms < self.warmup_ms {
            return Err(format!(
                "gate.floor_window_ms: must be >= gate.warmup_ms ({}), got {}",
                self.warmup_ms, self.floor_window_ms
            ));
        }
        Ok(())
    }
}

/// Whether the gate is currently letting audio through.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateState {
    Open,
    Closed,
}

impl GateState {
    pub fn as_str(self) -> &'static str {
        match self {
            GateState::Open => "open",
            GateState::Closed => "closed",
        }
    }
}

/// What the caller should do with one block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateDecision {
    /// Transmit this block.
    Send,
    /// Do not transmit. The block still needs to reach local storage.
    Skip,
}

/// The decision plus the boundary information the sender needs to report it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GateVerdict {
    pub decision: GateDecision,
    /// Set on the first `Send` after a closed stretch: the half-open frame range
    /// that was not transmitted. Reporting it is what lets the server distinguish
    /// "the node chose not to send" from "packets were lost".
    pub skipped: Option<(u64, u64)>,
}

/// Adaptive activity gate. Feed it one block at a time, in order.
///
/// Frame indexes are **the caller's**, not counted internally: the gate is told
/// where each block sits, so a capture stream that restarts its own frame counter
/// cannot silently desynchronise the ranges it reports from the audio it sent.
#[derive(Debug, Clone)]
pub struct VadGate {
    tuning: GateTuning,
    /// Running ambient estimate in dBFS, or `None` before the first block.
    floor_dbfs: Option<f32>,
    warmup_ms_left: u32,
    /// Continuous quiet so far while open, in milliseconds.
    quiet_ms: u32,
    state: GateState,
    /// Start of the current closed stretch, in the caller's frame coordinates.
    skip_start_frame: Option<u64>,
}

impl VadGate {
    pub fn new(tuning: GateTuning) -> Self {
        VadGate {
            tuning,
            floor_dbfs: None,
            warmup_ms_left: tuning.warmup_ms,
            quiet_ms: 0,
            state: GateState::Open,
            skip_start_frame: None,
        }
    }

    pub fn state(&self) -> GateState {
        self.state
    }

    /// True while audio is being withheld.
    pub fn is_closed(&self) -> bool {
        self.state == GateState::Closed
    }

    /// Current ambient estimate, for the log line at startup.
    pub fn floor_dbfs(&self) -> Option<f32> {
        self.floor_dbfs
    }

    /// The level a block must exceed to open, given what the gate has learned.
    /// Exposed so an operator can see why a night was gated or not.
    pub fn threshold_dbfs(&self) -> Option<f32> {
        self.floor_dbfs.map(|floor| floor + self.tuning.margin_db)
    }

    /// One block of `frames` frames starting at `start_frame` (the caller's frame
    /// index, the same one that goes into the packet header).
    pub fn observe(
        &mut self,
        level_dbfs: f32,
        start_frame: u64,
        frames: u64,
        rate_hz: u32,
    ) -> GateVerdict {
        let block_ms = if rate_hz == 0 {
            0
        } else {
            (frames.saturating_mul(1000) / u64::from(rate_hz)) as u32
        };
        self.update_floor(level_dbfs, block_ms);

        // Warmup: the server needs the first seconds to anchor the timeline and
        // to measure the room for itself, so nothing is withheld yet.
        if self.warmup_ms_left > 0 {
            self.warmup_ms_left = self.warmup_ms_left.saturating_sub(block_ms);
            self.state = GateState::Open;
            self.quiet_ms = 0;
            return GateVerdict {
                decision: GateDecision::Send,
                skipped: self.close_skip(start_frame),
            };
        }

        let threshold = self.threshold_dbfs().unwrap_or(level_dbfs);

        match self.state {
            GateState::Closed => {
                if level_dbfs >= threshold {
                    // Onset: open on this very block. The attack is never the part
                    // that gets cut.
                    self.state = GateState::Open;
                    self.quiet_ms = 0;
                    GateVerdict {
                        decision: GateDecision::Send,
                        skipped: self.close_skip(start_frame),
                    }
                } else {
                    if self.skip_start_frame.is_none() {
                        self.skip_start_frame = Some(start_frame);
                    }
                    GateVerdict {
                        decision: GateDecision::Skip,
                        skipped: None,
                    }
                }
            }
            GateState::Open => {
                if level_dbfs >= threshold {
                    self.quiet_ms = 0;
                } else {
                    self.quiet_ms = self.quiet_ms.saturating_add(block_ms);
                    if self.quiet_ms >= self.tuning.close_quiet_ms {
                        // Close *after* this block. The quiet that got us here is
                        // already part of the run being kept, so the withheld range
                        // starts at the next block, never in the middle of this one.
                        self.state = GateState::Closed;
                        self.quiet_ms = 0;
                        self.skip_start_frame = None;
                    }
                }
                GateVerdict {
                    decision: GateDecision::Send,
                    skipped: None,
                }
            }
        }
    }

    /// Tracks the quietest thing seen recently. Falls instantly (the ambient is
    /// the quiet floor) and rises with a `floor_window_ms` time constant, so a
    /// loud stretch cannot drag the ambient up and gate the next onset.
    fn update_floor(&mut self, level_dbfs: f32, block_ms: u32) {
        self.floor_dbfs = Some(match self.floor_dbfs {
            None => level_dbfs,
            Some(floor) => {
                if level_dbfs < floor {
                    level_dbfs
                } else {
                    let alpha = (block_ms as f32 / self.tuning.floor_window_ms as f32).min(0.1);
                    floor + (level_dbfs - floor) * alpha
                }
            }
        });
    }

    /// Closes an open closed-stretch that ends where `end_frame` begins.
    fn close_skip(&mut self, end_frame: u64) -> Option<(u64, u64)> {
        match self.skip_start_frame.take() {
            Some(start) if end_frame > start => Some((start, end_frame)),
            // A stretch shorter than one block is not worth a control message.
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 16_000;
    /// 20 ms at 16 kHz — `period_ms` in `deployment/node.example.toml`.
    const FRAMES: u64 = 320;

    /// The gate plus a frame cursor, so tests feed it exactly the way the node's
    /// send loop does: contiguous blocks with the caller's own frame index.
    struct Feed {
        gate: VadGate,
        next: u64,
    }

    impl Feed {
        fn new() -> Self {
            Feed {
                gate: VadGate::new(GateTuning::default()),
                next: 0,
            }
        }

        fn block(&mut self, level: f32) -> GateVerdict {
            let start = self.next;
            self.next += FRAMES;
            self.gate.observe(level, start, FRAMES, RATE)
        }

        fn blocks(&mut self, count: usize, level: f32) -> Vec<GateVerdict> {
            (0..count).map(|_| self.block(level)).collect()
        }
    }

    #[test]
    fn the_warmup_is_always_sent() {
        let mut feed = Feed::new();
        // 5 s of warmup is 250 blocks of 20 ms.
        let verdicts = feed.blocks(240, -80.0);
        assert!(verdicts.iter().all(|v| v.decision == GateDecision::Send));
        assert_eq!(feed.gate.state(), GateState::Open);
    }

    /// The complaint that produced this state machine: a pause inside a
    /// conversation must not split it. 5 s of quiet — longer than the old
    /// `hangover_ms` of 2 s — has to stay inside the same open run.
    #[test]
    fn a_pause_inside_a_conversation_does_not_split_it() {
        let mut feed = Feed::new();
        feed.blocks(250, -70.0); // warmup in ambient

        for _ in 0..3 {
            let speech = feed.blocks(100, -25.0); // 2 s of speech
            assert!(speech.iter().all(|v| v.decision == GateDecision::Send));
            let pause = feed.blocks(250, -70.0); // 5 s of quiet
            assert!(
                pause.iter().all(|v| v.decision == GateDecision::Send),
                "a 5 s pause is inside the turn, not the end of it"
            );
            assert!(pause.iter().all(|v| v.skipped.is_none()));
        }
        assert_eq!(feed.gate.state(), GateState::Open);
    }

    #[test]
    fn a_long_silence_closes_and_reports_the_range_it_withheld() {
        let mut feed = Feed::new();
        feed.blocks(250, -70.0);
        assert_eq!(feed.block(-25.0).decision, GateDecision::Send);

        // 30 s of quiet: 20 s keeps sending, the last 10 s is withheld.
        let quiet = feed.blocks(1_500, -70.0);
        let skipped = quiet
            .iter()
            .filter(|v| v.decision == GateDecision::Skip)
            .count();
        assert_eq!(skipped, 500, "10 s of a 30 s silence is withheld");
        assert!(feed.gate.is_closed());

        // Speech returns: sent immediately, naming the withheld range.
        let resume = feed.next;
        let verdict = feed.block(-25.0);
        assert_eq!(verdict.decision, GateDecision::Send);
        let (from, to) = verdict.skipped.expect("the closed stretch is reported");
        assert_eq!(to, resume, "the withheld range ends where this block begins");
        assert_eq!(to - from, 500 * FRAMES, "exactly the blocks that were skipped");
    }

    /// Frame indexes belong to the caller: a capture stream that restarts its
    /// counter must not shift the ranges the gate reports.
    #[test]
    fn ranges_use_the_callers_frame_index() {
        let base = 9_876_543u64;
        let mut gate = VadGate::new(GateTuning::default());
        for index in 0..1_750u64 {
            gate.observe(-70.0, base + index * FRAMES, FRAMES, RATE);
        }
        assert!(gate.is_closed());
        let resume = base + 1_750 * FRAMES;
        let (from, to) = gate
            .observe(-25.0, resume, FRAMES, RATE)
            .skipped
            .expect("reported");
        assert_eq!(to, resume);
        assert_eq!(from, base + 1_250 * FRAMES);
    }

    /// A room measured at -39 dBFS is a fan or a badly gained microphone, not a
    /// quiet room — but it is still *ambient*, so it still closes eventually.
    #[test]
    fn ambient_in_a_loud_room_closes_once_it_is_steady() {
        let mut feed = Feed::new();
        feed.blocks(250, -39.0);
        let wobble: Vec<_> = (0..2_000)
            .map(|index| feed.block(if index % 2 == 0 { -41.0 } else { -37.5 }))
            .collect();
        assert!(
            wobble.iter().any(|v| v.decision == GateDecision::Skip),
            "ambient noise at a high level is still noise"
        );
        // 9 dB above the ambient: the detector would call this active.
        assert_eq!(feed.block(-30.0).decision, GateDecision::Send);
        assert_eq!(feed.gate.state(), GateState::Open);
    }

    /// The margin has to stay below the detector's, otherwise the gate can stay
    /// closed through audio the detector would have flagged.
    #[test]
    fn a_block_just_above_the_detector_margin_opens_at_once() {
        let mut feed = Feed::new();
        feed.blocks(250, -60.0);
        feed.blocks(2_000, -60.0); // closed by now
        assert!(feed.gate.is_closed());
        // 8 dB above ambient is exactly the detector's threshold.
        assert_eq!(feed.block(-52.0).decision, GateDecision::Send);
        assert_eq!(feed.gate.state(), GateState::Open);
    }

    /// Just under the close threshold: still the same run, so the quiet is still
    /// sent and no range is announced.
    #[test]
    fn a_quiet_stretch_below_the_close_threshold_stays_in_the_run() {
        let mut feed = Feed::new();
        feed.blocks(250, -70.0);
        assert_eq!(feed.block(-25.0).decision, GateDecision::Send);
        let tail = feed.blocks(990, -70.0); // 19.8 s
        assert!(tail.iter().all(|v| v.decision == GateDecision::Send));
        assert!(tail.iter().all(|v| v.skipped.is_none()));
        assert_eq!(feed.gate.state(), GateState::Open);
    }

    #[test]
    fn tuning_outside_the_safe_range_is_refused() {
        let narrow = GateTuning {
            margin_db: 1.0,
            ..GateTuning::default()
        };
        assert!(narrow.validate().is_err());

        let eager = GateTuning {
            close_quiet_ms: 1_000,
            ..GateTuning::default()
        };
        let error = eager.validate().expect_err("a 1 s close would split pauses");
        assert!(error.contains("close_quiet_ms"), "got: {error}");

        let cold = GateTuning {
            warmup_ms: 100,
            ..GateTuning::default()
        };
        assert!(cold.validate().is_err());
        assert!(GateTuning::default().validate().is_ok());
    }

    /// A rate of zero must not divide by zero; the block counts as zero-length.
    #[test]
    fn a_zero_rate_is_survivable() {
        let mut gate = VadGate::new(GateTuning::default());
        assert_eq!(gate.observe(-70.0, 0, 320, 0).decision, GateDecision::Send);
    }
}
