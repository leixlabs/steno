//! The socket session: one connection to the server, one engine behind it.
//!
//! The contract is the same one `apple-stt-sidecar` speaks — the server binds,
//! launches this process with the socket path as its only argument, and both
//! ends exchange the framed protocol in [`crate::sidecar_wire`]. Nothing on the
//! wire is new, which is the point: the server cannot tell which engine is
//! behind the socket, and does not need to.
//!
//! ## Why the first frame decides when the engine starts
//!
//! The engine's language is the server's decision (`stt.locale`), and it arrives
//! in a `config` frame before any audio. Two sources for one value is how they
//! drift, so the language is read from the wire and nowhere else. The engine is
//! spawned when that frame arrives, which is also the earliest moment its ~2.5 s
//! model load can start without guessing.
//!
//! If the config never comes, or audio arrives first, the engine still starts —
//! on `auto`, which is the model's documented default — and the session reports
//! `degraded` with the reason. Falling back is allowed; falling back *silently*
//! is not.

use std::path::Path;
use std::time::Duration;

use tokio::net::UnixStream;
use tokio::sync::mpsc;

use crate::audiocpp::config::{self, SidecarConfig};
use crate::audiocpp::engine::Engine;
use crate::audiocpp::Outgoing;
use crate::sidecar_wire::{self as wire, Direction, MsgType};

/// The server binds before it launches, so the first attempt normally succeeds;
/// the retries cover a launcher that got ahead of the listener.
const CONNECT_ATTEMPTS: u32 = 40;
const CONNECT_RETRY: Duration = Duration::from_millis(100);
/// How long the first frame may take before the engine starts anyway.
const CONFIG_WAIT: Duration = Duration::from_secs(10);
/// Frames in flight towards the socket. Transcripts are small and rare; this only
/// has to be deeper than one window's worth of results.
const OUTBOUND_DEPTH: usize = 256;
const WRITER_GRACE: Duration = Duration::from_secs(5);

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("socket io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("wire error: {0}")]
    Wire(#[from] wire::WireError),
}

#[derive(Debug, serde::Deserialize)]
struct DownConfig {
    #[serde(default)]
    locale: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
struct DownControl {
    #[serde(default)]
    action: String,
}

pub fn run(config: SidecarConfig) -> Result<(), SessionError> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(serve(config))
}

async fn serve(config: SidecarConfig) -> Result<(), SessionError> {
    let stream = connect(&config.socket).await?;
    tracing::info!(socket = %config.socket.display(), "connected to the server");
    let (mut reader, mut writer) = stream.into_split();

    // One writer owns the socket. Two tasks writing frames would interleave a
    // header with someone else's payload.
    let (outbox, mut outbox_rx) = mpsc::channel::<Outgoing>(OUTBOUND_DEPTH);
    let writer_task = tokio::spawn(async move {
        while let Some(message) = outbox_rx.recv().await {
            if let Err(error) = wire::write_frame_as(
                &mut writer,
                Direction::SidecarToServer,
                message.kind,
                &message.payload,
            )
            .await
            {
                tracing::warn!(%error, "cannot write to the server");
                break;
            }
        }
    });

    let mut engine: Option<Engine> = None;
    let mut total_frames: u64 = 0;
    let mut base_set = false;
    let mut first = true;
    let mut flush_announced = false;

    loop {
        let frame = if first {
            first = false;
            match tokio::time::timeout(
                CONFIG_WAIT,
                wire::read_frame(&mut reader, Direction::ServerToSidecar),
            )
            .await
            {
                Ok(result) => result,
                Err(_) => {
                    notice(
                        &outbox,
                        "degraded",
                        Some(format!(
                            "no frame within {CONFIG_WAIT:?}; the engine starts with language auto"
                        )),
                    )
                    .await;
                    match Engine::start(&config, "auto", "auto", outbox.clone()) {
                        Ok(started) => engine = Some(started),
                        Err(error) => {
                            notice(
                                &outbox,
                                "error",
                                Some(format!("cannot start audiocpp: {error}")),
                            )
                            .await;
                            break;
                        }
                    }
                    continue;
                }
            }
        } else {
            wire::read_frame(&mut reader, Direction::ServerToSidecar).await
        };

        let frame = match frame {
            Ok(Some(frame)) => frame,
            Ok(None) => {
                tracing::info!(frames = total_frames, "the server closed the stream");
                break;
            }
            Err(error) => {
                tracing::warn!(%error, "framing error; ending the session");
                break;
            }
        };

        match frame.kind {
            MsgType::Config => {
                let locale = locale_of(&frame.payload);
                if engine.is_some() {
                    tracing::debug!(
                        %locale,
                        "a late config frame is ignored: the engine is already running"
                    );
                    continue;
                }
                let language = match config::resolve_language(&locale) {
                    Ok(language) => language,
                    Err(error) => {
                        // The engine accepts any string and still produces a
                        // transcript — just not in that language. Refusing is the
                        // only way a typo becomes visible.
                        notice(&outbox, "error", Some(error.to_string())).await;
                        break;
                    }
                };
                match Engine::start(&config, &language, &locale, outbox.clone()) {
                    Ok(started) => engine = Some(started),
                    Err(error) => {
                        notice(
                            &outbox,
                            "error",
                            Some(format!("cannot start audiocpp: {error}")),
                        )
                        .await;
                        break;
                    }
                }
            }
            MsgType::Audio => {
                if engine.is_none() {
                    match Engine::start(&config, "auto", "auto", outbox.clone()) {
                        Ok(started) => {
                            engine = Some(started);
                            notice(
                                &outbox,
                                "degraded",
                                Some(
                                    "audio arrived before any config frame; language left on auto"
                                        .to_string(),
                                ),
                            )
                            .await;
                        }
                        Err(error) => {
                            notice(
                                &outbox,
                                "error",
                                Some(format!("cannot start audiocpp: {error}")),
                            )
                            .await;
                            break;
                        }
                    }
                }
                let (utc_ms, pcm) = match wire::split_audio(&frame.payload) {
                    Ok(split) => split,
                    Err(error) => {
                        tracing::warn!(%error, "malformed audio frame");
                        continue;
                    }
                };
                let mut samples: Vec<i16> = Vec::new();
                wire::decode_pcm(pcm, &mut samples);
                let frames = samples.len() as u64;
                let start = total_frames;
                total_frames = total_frames.saturating_add(frames);
                if let Some(engine) = engine.as_ref() {
                    if !base_set {
                        engine.note_clock_base(utc_ms);
                        base_set = true;
                    }
                    engine.submit(samples, start, total_frames);
                }
            }
            MsgType::Control => {
                let action = action_of(&frame.payload);
                match action.as_str() {
                    "stop" => {
                        tracing::info!(frames = total_frames, "the server asked for a stop");
                        break;
                    }
                    "flush" => {
                        // Said once per session instead of silently pretending:
                        // a window is decoded when its audio is complete, and the
                        // engine has no "run the half-filled window" interface.
                        if !flush_announced {
                            flush_announced = true;
                            tracing::info!(
                                "flush ignored: text is committed per window, and this engine cannot decode a half-filled window early"
                            );
                        }
                    }
                    other => tracing::debug!(
                        action = other,
                        "ignoring a control the sidecar has no use for"
                    ),
                }
            }
            other => tracing::debug!(
                kind = %other,
                "ignoring a downlink frame the sidecar has no use for"
            ),
        }
    }

    if let Some(engine) = engine.take() {
        // Finishing on the blocking pool on purpose: the engine's writer and
        // reader threads need the socket writer task to keep draining, and this
        // runtime has exactly one thread to give.
        let _ = tokio::task::spawn_blocking(move || {
            let mut engine = engine;
            engine.finish();
        })
        .await;
    }
    notice(&outbox, "stopped", None).await;
    drop(outbox);
    let _ = tokio::time::timeout(WRITER_GRACE, writer_task).await;
    tracing::info!(
        frames = total_frames,
        seconds = total_frames as f64 / f64::from(steno_lib::config::WIRE_RATE_HZ),
        "session over"
    );
    Ok(())
}

async fn connect(path: &Path) -> Result<UnixStream, SessionError> {
    let mut last: Option<std::io::Error> = None;
    for _ in 0..CONNECT_ATTEMPTS {
        match UnixStream::connect(path).await {
            Ok(stream) => return Ok(stream),
            Err(error) => {
                last = Some(error);
                tokio::time::sleep(CONNECT_RETRY).await;
            }
        }
    }
    Err(SessionError::Io(last.unwrap_or_else(|| {
        std::io::Error::other("could not connect to the server socket")
    })))
}

async fn notice(outbox: &mpsc::Sender<Outgoing>, state: &str, message: Option<String>) {
    let mut value = serde_json::json!({ "state": state });
    if let (Some(message), Some(object)) = (message, value.as_object_mut()) {
        object.insert("message".to_string(), serde_json::Value::String(message));
    }
    let payload = serde_json::to_vec(&value).unwrap_or_default();
    let _ = outbox
        .send(Outgoing {
            kind: MsgType::Session,
            payload,
        })
        .await;
}

fn locale_of(payload: &[u8]) -> String {
    serde_json::from_slice::<DownConfig>(payload)
        .ok()
        .and_then(|config| config.locale)
        .map(|locale| locale.trim().to_string())
        .filter(|locale| !locale.is_empty())
        .unwrap_or_else(|| "auto".to_string())
}

fn action_of(payload: &[u8]) -> String {
    serde_json::from_slice::<DownControl>(payload)
        .map(|control| control.action)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_config_frame_without_a_locale_falls_back_to_auto() {
        assert_eq!(locale_of(b"{}"), "auto");
        assert_eq!(locale_of(b"{\"locale\":\"  \"}"), "auto");
        assert_eq!(locale_of(b"not json"), "auto");
    }

    #[test]
    fn a_config_frame_carries_the_locale_the_server_chose() {
        assert_eq!(locale_of(b"{\"locale\":\"zh-CN\"}"), "zh-CN");
    }

    #[test]
    fn a_control_frame_without_an_action_is_not_a_stop() {
        assert_eq!(action_of(b"{}"), "");
        assert_eq!(action_of(b"{\"action\":\"stop\"}"), "stop");
    }
}
