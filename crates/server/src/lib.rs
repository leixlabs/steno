//! Server side of the split deployment: recording, detection, storage,
//! transcription and the HTTP/UI layer.
//!
//! The audio source is pluggable — [`net_capture`] receives PCM from capture
//! nodes over UDP, while the server can still open a local microphone through
//! [`steno_lib::audio_capture`]. Both feed the same [`dispatcher`], so nothing
//! downstream knows or cares where the samples came from.
//!
//! ```text
//! UDP / ALSA -> capture ring -> dispatcher -> { recorder worker, detector worker }
//!                                              |                    |
//!                                              +--> DB writer  <-----+
//!                                        HTTP server (Axum) reads the same DB
//! ```

pub mod control;
pub mod db_writer;
pub mod dispatcher;
pub mod http_server;
pub mod net_capture;
pub mod recorder;
pub mod recovery;
pub mod retention;
pub mod storage;
pub mod stt;
pub mod stt_stream;
pub mod transcript_hub;
