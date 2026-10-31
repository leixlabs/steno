//! Runtime counters and the volatile status snapshot (TECH_SPEC §12, task T1.3).
//!
//! Counters live in atomics because they are updated from the capture callback,
//! the dispatcher, the recorder, the detector and the DB writer. They are read by
//! the HTTP `/status` handler. Nothing here allocates on the audio path.

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicI64, AtomicU32, AtomicU64, Ordering};
use std::sync::Mutex;

/// Number of DSP frame timings kept for the p99 estimate.
const DSP_TIMING_WINDOW: usize = 1024;

/// Which component a status string describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Health {
    Ok,
    Degraded,
    Unknown,
    Silent,
    Disconnected,
}

impl Health {
    pub fn as_str(self) -> &'static str {
        match self {
            Health::Ok => "ok",
            Health::Degraded => "degraded",
            Health::Unknown => "unknown",
            Health::Silent => "silent",
            Health::Disconnected => "disconnected",
        }
    }
}

/// Detector state as reported by `/status` (TECH_SPEC §12).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetectorState {
    Unknown,
    WarmingUp,
    Idle,
    Candidate,
    Pending,
}

impl DetectorState {
    pub fn as_str(self) -> &'static str {
        match self {
            DetectorState::Unknown => "unknown",
            DetectorState::WarmingUp => "warming_up",
            DetectorState::Idle => "idle",
            DetectorState::Candidate => "candidate",
            DetectorState::Pending => "pending",
        }
    }
}

/// Fixed-window timing statistics for one repeated operation.
#[derive(Debug, Default)]
pub struct TimingStats {
    count: AtomicU64,
    sum_ns: AtomicU64,
    max_ns: AtomicU64,
    window: Mutex<TimingWindow>,
}

#[derive(Debug)]
struct TimingWindow {
    samples: Vec<u64>,
    next: usize,
    /// How many of `samples` have been written at least once.
    filled: usize,
}

impl Default for TimingWindow {
    fn default() -> Self {
        TimingWindow {
            samples: vec![0; DSP_TIMING_WINDOW],
            next: 0,
            filled: 0,
        }
    }
}

/// Summary of a [`TimingStats`] window.
#[derive(Debug, Clone, Copy, Default)]
pub struct TimingSummary {
    pub count: u64,
    pub avg_ns: u64,
    pub p99_ns: u64,
    pub max_ns: u64,
}

impl TimingStats {
    pub fn record(&self, elapsed_ns: u64) {
        self.count.fetch_add(1, Ordering::Relaxed);
        self.sum_ns.fetch_add(elapsed_ns, Ordering::Relaxed);
        self.max_ns.fetch_max(elapsed_ns, Ordering::Relaxed);
        if let Ok(mut window) = self.window.lock() {
            let slot = window.next;
            window.samples[slot] = elapsed_ns;
            window.next = (slot + 1) % window.samples.len();
            window.filled = (window.filled + 1).min(window.samples.len());
        }
    }

    pub fn summary(&self) -> TimingSummary {
        let count = self.count.load(Ordering::Relaxed);
        let sum = self.sum_ns.load(Ordering::Relaxed);
        let max = self.max_ns.load(Ordering::Relaxed);
        let p99 = self
            .window
            .lock()
            .ok()
            .map(|w| {
                if w.filled == 0 {
                    return 0;
                }
                // Only the populated part of the ring carries real timings; the
                // untouched tail is zero-filled and would drag the estimate down.
                let mut sorted = w.samples[..w.filled].to_vec();
                sorted.sort_unstable();
                let idx = ((sorted.len() as f64 * 0.99) as usize).min(sorted.len() - 1);
                sorted[idx]
            })
            .unwrap_or(0);
        TimingSummary {
            count,
            avg_ns: sum.checked_div(count).unwrap_or(0),
            p99_ns: p99,
            max_ns: max,
        }
    }
}

/// All counters and status flags shared across threads.
#[derive(Debug, Default)]
pub struct Metrics {
    // --- capture / timeline (TECH_SPEC §4.1, §4.3) ---
    pub capture_ring_overflow_frames: AtomicU64,
    pub capture_ring_high_water_frames: AtomicU64,
    pub xrun_count: AtomicU64,
    pub lost_frames_total: AtomicU64,
    pub stream_rebuild_count: AtomicU64,
    pub gap_detection_limited: AtomicBool,

    // --- network capture (split deployment, source.kind = "net") ---
    /// True once a node's HELLO has been accepted in this process lifetime.
    pub net_source_connected: AtomicBool,
    pub net_packets_received: AtomicU64,
    /// Packets the sequence numbers proved missing (one per lost datagram).
    pub net_packets_lost: AtomicU64,
    pub net_packets_duplicate: AtomicU64,
    pub net_packets_reordered: AtomicU64,
    pub net_protocol_errors: AtomicU64,
    /// Bytes received from a `device_id` that did not match the configured one.
    pub net_foreign_packets: AtomicU64,
    /// Wall clock of the last packet from the node, in Unix milliseconds. `0`
    /// means "no packet yet in this process", which is not the same as
    /// "a packet arrived at the epoch".
    ///
    /// `net_source_connected` answers "is the node with us"; this answers "how
    /// stale is it", which is what an operator needs to tell a node that died
    /// from a node that is merely quiet.
    pub net_last_packet_unix_ms: AtomicI64,
    /// The node's own description of itself (argv, config path, version),
    /// verbatim as JSON. Kept as text so the API can hand it on without the
    /// server and the node sharing a schema.
    net_node_report: Mutex<Option<String>>,
    pub net_node_report_unix_ms: AtomicI64,
    /// `ip:port` the newest datagram came from. Behind a mutex because it is
    /// text; the receiver only writes it when the address changes, so this is
    /// one write per node session, not one per packet.
    net_peer: Mutex<Option<String>>,
    /// Frames the node announced it deliberately did not send, so the Portal can
    /// separate "the node gated this" from "the uplink lost this".
    pub net_gated_frames: AtomicU64,
    pub net_gate_announcements: AtomicU64,

    // --- queues (TECH_SPEC §4.3) ---
    pub dropped_detection_blocks: AtomicU64,
    /// Live level of the newest block, in milli-dBFS. `known` gates it: the
    /// atomic's default `0` would otherwise read as 0 dBFS, i.e. a perfectly
    /// clipped signal.
    pub live_peak_mdb: AtomicI32,
    pub live_rms_mdb: AtomicI32,
    pub live_level_known: AtomicBool,
    /// Audio blocks the streaming recogniser never got. Transcription is
    /// best-effort, so this is a counter rather than an error.
    pub dropped_stt_blocks: AtomicU64,
    pub recording_queue_high_water: AtomicU64,
    pub detection_queue_high_water: AtomicU64,
    pub recording_queue_control_slots: AtomicU64,
    pub detection_queue_control_slots: AtomicU64,
    pub queue_overflow_gaps: AtomicU64,

    // --- dispatcher / recording ---
    pub clipped_samples_total: AtomicU64,
    pub segments_completed: AtomicU64,
    pub degraded: AtomicBool,

    // --- detector (TECH_SPEC §5.3, §12) ---
    pub events_total: AtomicU64,
    pub events_discarded_short: AtomicU64,
    pub event_fk_retries: AtomicU64,
    pub detector_state: AtomicU32,
    pub noise_floor_mdb: AtomicU32,
    pub band_noise_floor_mdb: AtomicU32,
    /// Set while `noise_floor_mdb` holds a measured value. Without it, the
    /// atomic's default `0` would read back as a plausible 0 dBFS reading.
    pub noise_floor_known: AtomicBool,
    pub band_noise_floor_known: AtomicBool,
    pub signal_silent: AtomicBool,
    /// Audio arriving that is not digital silence but carries no usable signal:
    /// a muted capsule, a microphone unseated from it, or an input gain at zero.
    ///
    /// Separate from [`Self::signal_silent`] because the two failures are
    /// different and need different fixes. `signal_silent` means every sample is
    /// exactly `0` — no stream at all, the `null` plugin, or a decoder producing
    /// nothing. This one means samples arrive and carry only dither: a muted
    /// capsule reads around -78 dBFS with peaks of a few LSB, which is not
    /// "silence" by any byte-level test and used to be reported as `ok`.
    pub signal_dead: AtomicBool,

    // --- storage ---
    pub storage_pressure: AtomicBool,
    pub recording_stopped_low_disk: AtomicBool,
    pub deleted_segments: AtomicU64,
    pub missing_segments: AtomicU64,

    /// Whether the system wall clock is currently trusted. The Pi 3B has no RTC,
    /// so until NTP has settled `Utc::now()` is meaningless and segments recorded
    /// in that window must not claim `time_quality=synced` (§4.3). Defaults to
    /// `false`: an unprobed clock is reported as untrusted, never assumed good.
    pub time_synced: AtomicBool,

    // --- dsp cost ---
    pub dsp_block_time: TimingStats,

    // --- streaming speech-to-text, for the status page ---
    /// The recogniser process is attached and accepting audio.
    pub stt_connected: AtomicBool,
    /// Utterances stored since start.
    pub stt_finals: AtomicU64,
    /// Snore episodes the sidecar's classifier reported and this server stored.
    /// Counted apart from `events_total`, which is the rule-based detector's:
    /// the two annotate the same audio and must stay distinguishable.
    pub stt_snores: AtomicU64,
    stt_engine: Mutex<String>,
    /// How the recogniser is launched: the command and the arguments the server
    /// actually passes. Set once when the worker starts.
    stt_launch: Mutex<Option<(String, Vec<String>)>>,
    stt_partial: Mutex<Option<String>>,
    stt_error: Mutex<Option<String>>,

    // --- last error, for the status page ---
    last_error: Mutex<Option<String>>,
}

impl Metrics {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_last_error(&self, msg: impl Into<String>) {
        if let Ok(mut slot) = self.last_error.lock() {
            *slot = Some(msg.into());
        }
    }

    pub fn clear_last_error(&self) {
        if let Ok(mut slot) = self.last_error.lock() {
            *slot = None;
        }
    }

    pub fn last_error(&self) -> Option<String> {
        self.last_error.lock().ok().and_then(|slot| slot.clone())
    }

    /// Records the recogniser command line so the control centre can show what
    /// is running rather than what the config file says should be running.
    pub fn set_stt_launch(&self, command: impl Into<String>, args: Vec<String>) {
        if let Ok(mut slot) = self.stt_launch.lock() {
            *slot = Some((command.into(), args));
        }
    }

    pub fn stt_launch(&self) -> Option<(String, Vec<String>)> {
        self.stt_launch.lock().ok().and_then(|slot| slot.clone())
    }

    pub fn set_stt_engine(&self, engine: impl Into<String>) {
        if let Ok(mut slot) = self.stt_engine.lock() {
            *slot = engine.into();
        }
    }

    pub fn stt_engine(&self) -> String {
        self.stt_engine
            .lock()
            .map(|slot| slot.clone())
            .unwrap_or_default()
    }

    /// The phrase the recogniser is currently forming, if any. Display only:
    /// it is replaced by the final text and never stored.
    pub fn set_stt_partial(&self, text: Option<String>) {
        if let Ok(mut slot) = self.stt_partial.lock() {
            *slot = text;
        }
    }

    pub fn stt_partial(&self) -> Option<String> {
        self.stt_partial.lock().ok().and_then(|slot| slot.clone())
    }

    pub fn set_stt_error(&self, message: Option<String>) {
        if let Ok(mut slot) = self.stt_error.lock() {
            *slot = message;
        }
    }

    pub fn stt_error(&self) -> Option<String> {
        self.stt_error.lock().ok().and_then(|slot| slot.clone())
    }

    /// Records the level of the block just dispatched. Display only.
    pub fn set_live_level(&self, peak_dbfs: f32, rms_dbfs: f32) {
        self.live_peak_mdb
            .store((peak_dbfs * 1000.0).round() as i32, Ordering::Relaxed);
        self.live_rms_mdb
            .store((rms_dbfs * 1000.0).round() as i32, Ordering::Relaxed);
        self.live_level_known.store(true, Ordering::Relaxed);
    }

    /// `None` until a block has been measured — never a fabricated 0 dBFS.
    pub fn live_level(&self) -> Option<(f32, f32)> {
        if !self.live_level_known.load(Ordering::Relaxed) {
            return None;
        }
        Some((
            self.live_peak_mdb.load(Ordering::Relaxed) as f32 / 1000.0,
            self.live_rms_mdb.load(Ordering::Relaxed) as f32 / 1000.0,
        ))
    }

    pub fn set_time_synced(&self, synced: bool) {
        self.time_synced.store(synced, Ordering::Relaxed);
    }

    pub fn time_synced(&self) -> bool {
        self.time_synced.load(Ordering::Relaxed)
    }

    /// Probes whether the host wall clock is trustworthy right now (§4.3).
    ///
    /// On Linux this asks the kernel via `adjtimex` whether the clock is
    /// synchronised. Anywhere else, and on any probe failure, the answer is
    /// `false`: a clock we cannot vouch for is reported as untrusted rather than
    /// optimistically assumed correct. Callers store the result with
    /// [`Metrics::set_time_synced`].
    pub fn probe_time_synced() -> bool {
        #[cfg(target_os = "linux")]
        {
            const STA_UNSYNC: libc::c_int = 0x0040;
            // `timex.modes` is `c_uint` on every Linux target, while `status` is
            // `c_int`; keeping the two constants at their field's type avoids a
            // 32-bit-only mismatch (armv7 rejected `modes = c_int`).
            const STA_NANO: libc::c_uint = 0x2000;
            let mut buf: libc::timex = unsafe { std::mem::zeroed() };
            buf.modes = STA_NANO;
            // SAFETY: `adjtimex` only reads/writes the `timex` we pass by pointer.
            let rc = unsafe { libc::adjtimex(&mut buf) };
            if rc < 0 {
                return false;
            }
            // `STA_UNSYNC` set means the kernel is not disciplined to a reference.
            buf.status & STA_UNSYNC == 0
        }
        #[cfg(not(target_os = "linux"))]
        {
            // No equivalent portable probe; report untrusted so nothing claims a
            // calibrated timestamp on a platform we have not validated.
            false
        }
    }

    pub fn set_detector_state(&self, state: DetectorState) {
        self.detector_state.store(state as u32, Ordering::Relaxed);
    }

    pub fn detector_state(&self) -> DetectorState {
        match self.detector_state.load(Ordering::Relaxed) {
            1 => DetectorState::WarmingUp,
            2 => DetectorState::Idle,
            3 => DetectorState::Candidate,
            4 => DetectorState::Pending,
            _ => DetectorState::Unknown,
        }
    }

    /// Stores a dBFS value as milli-dBFS in an atomic (f32 has no atomic form).
    fn store_dbfs(value: &AtomicU32, known: &AtomicBool, dbfs: Option<f32>) {
        match dbfs {
            Some(v) => {
                let mdb = (v.clamp(-200.0, 100.0) * 1000.0).round() as i32;
                value.store(mdb as u32, Ordering::Relaxed);
                known.store(true, Ordering::Relaxed);
            }
            None => known.store(false, Ordering::Relaxed),
        }
    }

    fn load_dbfs(value: &AtomicU32, known: &AtomicBool) -> Option<f32> {
        if known.load(Ordering::Relaxed) {
            Some(value.load(Ordering::Relaxed) as i32 as f32 / 1000.0)
        } else {
            None
        }
    }

    /// Stores the node's latest self-report.
    pub fn set_node_report(&self, report: impl Into<String>, unix_ms: i64) {
        if let Ok(mut slot) = self.net_node_report.lock() {
            *slot = Some(report.into());
        }
        self.net_node_report_unix_ms
            .store(unix_ms, Ordering::Relaxed);
    }

    pub fn node_report(&self) -> Option<String> {
        self.net_node_report.lock().ok().and_then(|slot| slot.clone())
    }

    /// Records the address the newest datagram came from, as reported by the
    /// socket: `ip:port`.
    ///
    /// This is the only place the server learns where the node is — the protocol
    /// carries a device id, not an address — so it is what an operator needs to
    /// answer "which machine is on the wire", and the only way to tell a node
    /// that moved from one that never attached.
    pub fn set_net_peer(&self, peer: impl Into<String>) {
        if let Ok(mut slot) = self.net_peer.lock() {
            *slot = Some(peer.into());
        }
    }

    pub fn net_peer(&self) -> Option<String> {
        self.net_peer.lock().ok().and_then(|slot| slot.clone())
    }

    /// Records the wall clock of the newest packet from the capture node.
    pub fn note_net_packet(&self, unix_ms: i64) {
        self.net_last_packet_unix_ms.store(unix_ms, Ordering::Relaxed);
    }

    /// Wall clock of the newest packet, or `None` before the first one.
    pub fn net_last_packet_unix_ms(&self) -> Option<i64> {
        match self.net_last_packet_unix_ms.load(Ordering::Relaxed) {
            0 => None,
            value => Some(value),
        }
    }

    pub fn set_noise_floor_dbfs(&self, dbfs: Option<f32>) {
        Self::store_dbfs(&self.noise_floor_mdb, &self.noise_floor_known, dbfs);
    }

    pub fn noise_floor_dbfs(&self) -> Option<f32> {
        Self::load_dbfs(&self.noise_floor_mdb, &self.noise_floor_known)
    }

    pub fn set_band_noise_floor_dbfs(&self, dbfs: Option<f32>) {
        Self::store_dbfs(&self.band_noise_floor_mdb, &self.band_noise_floor_known, dbfs);
    }

    pub fn band_noise_floor_dbfs(&self) -> Option<f32> {
        Self::load_dbfs(&self.band_noise_floor_mdb, &self.band_noise_floor_known)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noise_floor_roundtrip_handles_none() {
        let m = Metrics::new();
        assert_eq!(m.noise_floor_dbfs(), None);
        m.set_noise_floor_dbfs(Some(-63.25));
        assert_eq!(m.noise_floor_dbfs(), Some(-63.25));
        m.set_noise_floor_dbfs(None);
        assert_eq!(m.noise_floor_dbfs(), None);
    }

    #[test]
    fn timing_summary_reports_max_and_avg() {
        let t = TimingStats::default();
        t.record(100);
        t.record(300);
        let s = t.summary();
        assert_eq!(s.count, 2);
        assert_eq!(s.avg_ns, 200);
        assert_eq!(s.max_ns, 300);
        assert_eq!(s.p99_ns, 300);
    }
}
