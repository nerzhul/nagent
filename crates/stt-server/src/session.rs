//! Per-session state and the shared `SessionMap`.
//!
//! ## Isolation invariant
//!
//! - `session_id` is server-generated and never accepted from the client.
//! - Each `SessionState` owns an `outbound_tx` whose only receiver is the
//!   WS task of that session.
//! - `SessionMap` is the *only* shared mutable state across sessions; it
//!   is a `DashMap<Uuid, Arc<SessionState>>` and we never hand out
//!   references into other sessions' `SessionState`.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use dashmap::DashMap;
use tokio::sync::mpsc;
use uuid::Uuid;

use stt_proto::Payload;

/// Outbound message types that can be sent from the server to a client.
#[derive(Debug, Clone)]
pub enum OutboundMessage {
    Payload(Payload),
    Close,
}

/// Per-session state held in the [`SessionMap`].
///
/// Constructed by [`register`]; the only mutable fields are protected by
/// interior mutability so [`SessionState`] itself can be wrapped in `Arc`
/// without needing `&mut`.
#[derive(Debug)]
pub struct SessionState {
    /// Sender end of the per-session outbound channel. Owned by the WS task.
    pub outbound_tx: mpsc::UnboundedSender<OutboundMessage>,
    /// Language override (`None` = auto-detect). Uses a synchronous mutex
    /// because the WS handler is single-threaded per session and we must
    /// never hold an async lock across an `.await` while a DashMap shard
    /// read lock is also held.
    pub language: Mutex<Option<String>>,
    /// Translate-to-English flag.
    pub translate: AtomicBool,
    /// Last activity timestamp in milliseconds since the Unix epoch.
    pub last_activity_ms: AtomicU64,
}

impl SessionState {
    fn new(outbound_tx: mpsc::UnboundedSender<OutboundMessage>) -> Self {
        Self {
            outbound_tx,
            language: Mutex::new(None),
            translate: AtomicBool::new(false),
            last_activity_ms: AtomicU64::new(now_ms()),
        }
    }

    /// Refresh the last-activity timestamp to "now".
    pub fn touch(&self) {
        self.last_activity_ms.store(now_ms(), Ordering::Relaxed);
    }

    /// Returns true if this session has been idle longer than `timeout`.
    pub fn is_idle(&self, timeout_ms: u64) -> bool {
        let last = self.last_activity_ms.load(Ordering::Relaxed);
        let now = now_ms();
        now.saturating_sub(last) >= timeout_ms
    }

    /// Replace the current language override.
    pub fn set_language(&self, lang: Option<String>) {
        if let Ok(mut g) = self.language.lock() {
            *g = lang;
        }
    }

    /// Snapshot the current language override.
    pub fn language(&self) -> Option<String> {
        self.language.lock().ok().and_then(|g| g.clone())
    }

    /// Replace the translate flag.
    pub fn set_translate(&self, v: bool) {
        self.translate.store(v, Ordering::Relaxed);
    }

    /// Snapshot the translate flag.
    pub fn translate(&self) -> bool {
        self.translate.load(Ordering::Relaxed)
    }
}

/// Shared registry of active sessions.
pub type SessionMap = Arc<DashMap<Uuid, Arc<SessionState>>>;

/// Construct a fresh [`SessionMap`].
pub fn new_session_map() -> SessionMap {
    Arc::new(DashMap::new())
}

/// Register a new session and return its `(id, state)`.
///
/// `buffer` is the bounded size of the outbound mpsc channel; once full,
/// [`SessionState::outbound_tx`] returns `SendError`.
pub fn register(
    map: &SessionMap,
    _buffer: usize,
) -> (
    Uuid,
    Arc<SessionState>,
    mpsc::UnboundedReceiver<OutboundMessage>,
) {
    let id = Uuid::new_v4();
    let (tx, rx) = mpsc::unbounded_channel();
    let state = Arc::new(SessionState::new(tx));
    map.insert(id, Arc::clone(&state));
    (id, state, rx)
}

/// Remove a session from the map. Idempotent: missing keys are a no-op.
pub fn unregister(map: &SessionMap, id: Uuid) -> Option<Arc<SessionState>> {
    map.remove(&id).map(|(_, v)| v)
}

/// Look up the outbound sender for a session, if it still exists.
pub fn outbound_for(map: &SessionMap, id: Uuid) -> Option<mpsc::UnboundedSender<OutboundMessage>> {
    map.get(&id).map(|s| s.outbound_tx.clone())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn register_and_unregister() {
        let map = new_session_map();
        let (id, _state, _rx) = register(&map, 1);
        assert!(map.contains_key(&id));
        assert!(unregister(&map, id).is_some());
        assert!(!map.contains_key(&id));
        // Unregister of an unknown id is a no-op.
        assert!(unregister(&map, id).is_none());
    }

    #[tokio::test]
    async fn touch_updates_last_activity() {
        let map = new_session_map();
        let (_id, state, _rx) = register(&map, 1);
        let before = state.last_activity_ms.load(Ordering::Relaxed);
        // Sleep just enough to detect a clock delta on coarse systems.
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        state.touch();
        let after = state.last_activity_ms.load(Ordering::Relaxed);
        assert!(after >= before);
    }

    #[tokio::test]
    async fn language_round_trip() {
        let map = new_session_map();
        let (_id, state, _rx) = register(&map, 1);
        assert_eq!(state.language(), None);
        state.set_language(Some("fr".into()));
        assert_eq!(state.language().as_deref(), Some("fr"));
    }

    #[tokio::test]
    async fn outbound_for_unknown_returns_none() {
        let map = new_session_map();
        assert!(outbound_for(&map, Uuid::new_v4()).is_none());
    }
}
