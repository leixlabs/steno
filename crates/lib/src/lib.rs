//! Shared domain logic for the snore monitor.
//!
//! Everything in this crate is platform-neutral and free of storage, HTTP or
//! process concerns, so both the server and the capture node compile against it:
//!
//! * [`config`] — the tuning contract; the single source of truth for formats
//! * [`dsp`], [`detector`], [`resampler`] — pure signal processing and the
//!   detection state machine
//! * [`timeline`] — time arithmetic, gaps, drift, quality estimation
//! * [`audio_capture`] — the capture ring every audio producer writes into,
//!   plus the ALSA/CPAL device selection used by the node and by the server's
//!   local-microphone mode
//! * [`protocol`] — the UDP wire format shared by node and server
//! * [`metrics`], [`error`], [`bounded_queue`] — plumbing used on both sides

pub mod audio_capture;
pub mod bounded_queue;
pub mod config;
pub mod detector;
pub mod dsp;
pub mod error;
pub mod metrics;
pub mod protocol;
pub mod resampler;
pub mod timeline;
pub mod vad;

// Offline analysis: reads WAV files from disk and never opens a microphone.
// Feature-gated because it is the only part of the library that needs a CLI
// parser and a WAV reader, and the capture node needs neither.
#[cfg(feature = "tools")]
pub mod detect_wav;
#[cfg(feature = "tools")]
pub mod eval;
