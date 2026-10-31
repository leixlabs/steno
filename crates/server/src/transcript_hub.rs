//! Live transcript fan-out.
//!
//! The streaming worker is the only producer of recognised text, and it runs on
//! its own thread with its own database connection. The Portal needs that text
//! *as it happens* — a 4 s poll of `/status` meant a phrase could appear, be
//! finalised and be superseded before the page ever saw it, and the partial was
//! the only thing that ever showed up live.
//!
//! So the worker publishes here and the HTTP layer fans out over WebSocket.
//!
//! ## Why a broadcast channel and not a queue per client
//!
//! A slow or suspended browser tab must never hold the recorder back. `broadcast`
//! drops the oldest events for a lagging receiver instead of blocking the sender,
//! and the receiver is told it lagged — which is the honest outcome: the page can
//! say "missed N events" rather than pretending its transcript is complete.
//!
//! ## What is *not* published
//!
//! Only what the server itself decided to keep. A `final` is published after the
//! row is written, so a client never shows a phrase that the database rejected.
//! That ordering is the whole reason this lives next to `save_utterance` rather
//! than inside the parser.

use std::sync::Arc;

use serde::Serialize;
use tokio::sync::broadcast;

use crate::storage::UtteranceRow;

/// How many events a client may fall behind before it starts losing them.
///
/// A partial is superseded within a second and a phrase every few seconds, so a
/// few hundred is already several minutes of slack; beyond that the page is not
/// watching and dropping is better than growing memory per connection.
const CHANNEL_CAPACITY: usize = 256;

/// One thing that happened to the transcript.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TranscriptEvent {
    /// The half-sentence the recogniser is forming. Replaces the previous one.
    Partial { text: String },
    /// A phrase the server stored. `id` is the row, so a client can dedupe
    /// against a page it loaded over HTTP.
    Final {
        id: String,
        started_at_utc: String,
        ended_at_utc: String,
        text: String,
        engine: String,
        language: Option<String>,
    },
    /// The recogniser is (re)connecting, or has stopped.
    State { state: String },
    /// The recogniser reported a failure. The page shows it instead of guessing.
    Error { message: String },
    /// Sent once when a client connects, so it knows the stream is live and what
    /// it missed before it arrived is not coming.
    Ready {
        engine: String,
        /// Phrase count at the moment of connecting.
        finals_total: u64,
    },
}

/// Publish side. Cloned into the streaming worker.
#[derive(Clone)]
pub struct TranscriptHub {
    sender: broadcast::Sender<TranscriptEvent>,
}

impl Default for TranscriptHub {
    fn default() -> Self {
        Self::new()
    }
}

impl TranscriptHub {
    pub fn new() -> Self {
        let (sender, _receiver) = broadcast::channel(CHANNEL_CAPACITY);
        TranscriptHub { sender }
    }

    /// Publishes an event. Returns false when nobody is listening — which is the
    /// normal case (no Portal open) and must not be logged as an error.
    pub fn publish(&self, event: TranscriptEvent) -> bool {
        self.sender.send(event).is_ok()
    }

    pub fn subscribe(&self) -> broadcast::Receiver<TranscriptEvent> {
        self.sender.subscribe()
    }

    /// Number of connected subscribers, for the status line.
    pub fn subscribers(&self) -> usize {
        self.sender.receiver_count()
    }
}

impl TranscriptEvent {
    /// Builds a `Final` from the row that was just stored, so the wire format and
    /// the database cannot disagree about a phrase.
    pub fn final_from_row(row: &UtteranceRow) -> Self {
        TranscriptEvent::Final {
            id: row.id.clone(),
            started_at_utc: row.started_at_utc.to_rfc3339(),
            ended_at_utc: row.ended_at_utc.to_rfc3339(),
            text: row.text.clone(),
            engine: row.engine.clone(),
            language: row.language.clone(),
        }
    }
}

/// A hub that nobody listens to, for tests and for a server built without
/// transcription.
pub fn shared() -> Arc<TranscriptHub> {
    Arc::new(TranscriptHub::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_published_event_reaches_a_subscriber() {
        let hub = TranscriptHub::new();
        let mut receiver = hub.subscribe();
        assert!(hub.publish(TranscriptEvent::Partial { text: "你".into() }));
        let event = receiver.try_recv().expect("delivered");
        match event {
            TranscriptEvent::Partial { text } => assert_eq!(text, "你"),
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn publishing_with_no_listener_is_not_an_error() {
        let hub = TranscriptHub::new();
        // No subscriber: `false`, not a panic and not a block.
        assert!(!hub.publish(TranscriptEvent::Partial { text: "x".into() }));
        assert_eq!(hub.subscribers(), 0);
    }

    /// The contract that matters for a slow tab: the sender never waits, it
    /// drops, and the receiver is told.
    #[test]
    fn a_lagging_subscriber_loses_events_instead_of_blocking_the_sender() {
        let hub = TranscriptHub::new();
        let mut receiver = hub.subscribe();
        for index in 0..(CHANNEL_CAPACITY + 10) {
            assert!(hub.publish(TranscriptEvent::Partial {
                text: index.to_string()
            }));
        }
        // The oldest ones are gone; what remains is the newest slice.
        match receiver.try_recv() {
            Err(broadcast::error::TryRecvError::Lagged(count)) => {
                assert!(count >= 10, "expected to be told how much was lost, got {count}")
            }
            other => panic!("expected Lagged, got {other:?}"),
        }
    }

    #[test]
    fn the_wire_format_names_the_event_type() {
        let json = serde_json::to_string(&TranscriptEvent::Partial { text: "好".into() }).unwrap();
        assert!(json.contains(r#""type":"partial""#), "got {json}");
        let json = serde_json::to_string(&TranscriptEvent::Error {
            message: "boom".into(),
        })
        .unwrap();
        assert!(json.contains(r#""type":"error""#), "got {json}");
    }
}
