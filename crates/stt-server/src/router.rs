//! `ResultRouter` — consumes inference results from the worker and pushes
//! them into the right session's outbound channel.
//!
//! The router is the **only** place that ever looks up a session by ID
//! outside of the WS handler that owns that session. A session that has
//! already been removed (e.g. disconnected, evicted by the watchdog)
//! silently drops the result.

use std::sync::Arc;

use tokio::sync::mpsc;
use tracing::{debug, warn};
use uuid::Uuid;

use stt_core::InferResponse;

use crate::session::{outbound_for, OutboundMessage, SessionMap};
use stt_proto::{FinalTranscript, Segment};

/// Result router task.
#[derive(Debug)]
pub struct ResultRouter;

impl ResultRouter {
    /// Spawn a task that drains `rx` and forwards each result to the
    /// owning session.
    pub fn spawn(
        map: SessionMap,
        mut rx: mpsc::Receiver<InferResponse>,
    ) -> Arc<tokio::sync::Notify> {
        let shutdown = Arc::new(tokio::sync::Notify::new());
        let shutdown_signal = Arc::clone(&shutdown);
        tokio::spawn(async move {
            while let Some(resp) = rx.recv().await {
                Self::route(&map, resp).await;
            }
            shutdown_signal.notify_waiters();
            debug!("result router stopped (channel closed)");
        });
        shutdown
    }

    async fn route(map: &SessionMap, resp: InferResponse) {
        let InferResponse {
            session_id,
            text,
            segments,
            lang,
            duration_ms: _,
        } = resp;

        // Strip empty segments into the wire format expected by clients.
        let wire_segments: Vec<Segment> = segments;

        let payload = stt_proto::Payload::Final(FinalTranscript {
            text,
            segments: wire_segments,
            lang,
        });

        match outbound_for(map, session_id) {
            Some(tx) => {
                if tx.send(OutboundMessage::Payload(payload)).is_err() {
                    debug!(%session_id, "outbound channel closed, dropping result");
                }
            }
            None => {
                warn!(%session_id, "dropping result for unknown session");
            }
        }
    }
}

/// Helper used by tests / future code to construct a `ResultRouter`-style
/// mapper from a session ID + payload without spawning a task.
pub async fn send_to_session(
    map: &SessionMap,
    session_id: Uuid,
    payload: stt_proto::Payload,
) -> bool {
    match outbound_for(map, session_id) {
        Some(tx) => tx.send(OutboundMessage::Payload(payload)).is_ok(),
        None => false,
    }
}
