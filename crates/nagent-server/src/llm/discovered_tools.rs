//! `llm::discovered_tools` — per-session tool-discovery store.
//!
//! Tracks the agent names the LLM has already invoked in the
//! current `chat_session_id` so the round-level
//! [`build_tools_for_round`] helper can ship their full JSON
//! Schemas again on the next round. The LLM model schema cache
//! on most upstreams silently forgets tools that disappear from
//! `tools=[]` between rounds, so any tool the model has
//! successfully called at least once must stay in the array
//! for as long as the conversation lasts.
//!
//! Same shape and lifetime as [`super::permission::PermissionStore`]:
//! in-memory `Arc<Mutex<HashMap<Uuid, HashSet<String>>>>`, cleared
//! on session mint. The two stores live next to each other on
//! `AppState`; the chat-completion route reads both to compose
//! its prompt and its tool-dispatch policy.
//!
//! [`build_tools_for_round`]: super::proxy::build_tools_for_round

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use uuid::Uuid;

/// Per-session tool-discovery store.
///
/// Cheap to clone (`Arc` inside) so it lives directly on
/// `AppState`. The mutex (sync) is enough — every operation is a
/// handful of `HashSet` insertions, microseconds; an async lock
/// would just add scheduling jitter. Mirrors
/// [`super::permission::PermissionStore`].
#[derive(Clone, Default)]
pub struct DiscoveredTools {
    inner: Arc<Mutex<HashMap<Uuid, HashSet<String>>>>,
}

impl std::fmt::Debug for DiscoveredTools {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let n = self.inner.lock().map(|m| m.len()).unwrap_or(0);
        f.debug_struct("DiscoveredTools")
            .field("sessions", &n)
            .finish()
    }
}

impl DiscoveredTools {
    /// Construct a fresh empty store. Cheap; call once at boot.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record that `tool` was successfully invoked (or surfaced
    /// via `search_tools`) during `session_id`'s conversation.
    /// Idempotent: re-adding the same name is a no-op so the
    /// tool loop can call this every round without bloating the
    /// set.
    pub fn add(&self, session: Uuid, tool: &str) {
        if tool.is_empty() {
            return;
        }
        let mut guard = self.inner.lock().expect("discovered_tools poisoned");
        guard.entry(session).or_default().insert(tool.to_string());
    }

    /// Snapshot the names discovered for `session_id`, in
    /// insertion order where `HashSet` preserves it. Empty when
    /// the session has not discovered anything yet.
    pub fn snapshot(&self, session: Uuid) -> Vec<String> {
        let guard = self.inner.lock().expect("discovered_tools poisoned");
        guard
            .get(&session)
            .map(|s| s.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Wipe every entry for `session_id`. Called on session
    /// mint and explicit reset, mirroring
    /// [`super::permission::PermissionStore::clear_session`].
    #[allow(dead_code)]
    pub fn clear_session(&self, session_id: Uuid) {
        let mut guard = self.inner.lock().expect("discovered_tools poisoned");
        guard.remove(&session_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_and_snapshot_round_trip() {
        let s = DiscoveredTools::new();
        let sid = Uuid::new_v4();
        s.add(sid, "get_weather");
        s.add(sid, "calculate");
        let mut snap = s.snapshot(sid);
        snap.sort();
        assert_eq!(
            snap,
            vec!["calculate".to_string(), "get_weather".to_string()]
        );
    }

    #[test]
    fn add_is_idempotent() {
        let s = DiscoveredTools::new();
        let sid = Uuid::new_v4();
        s.add(sid, "get_weather");
        s.add(sid, "get_weather");
        s.add(sid, "get_weather");
        assert_eq!(s.snapshot(sid).len(), 1);
    }

    #[test]
    fn empty_tool_name_is_ignored() {
        let s = DiscoveredTools::new();
        let sid = Uuid::new_v4();
        s.add(sid, "");
        assert!(s.snapshot(sid).is_empty());
    }

    #[test]
    fn sessions_are_isolated() {
        let s = DiscoveredTools::new();
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        s.add(a, "get_weather");
        assert!(s.snapshot(a).contains(&"get_weather".to_string()));
        assert!(s.snapshot(b).is_empty(), "session B must not see A's tools");
    }

    #[test]
    fn clear_session_wipes_entries() {
        let s = DiscoveredTools::new();
        let sid = Uuid::new_v4();
        s.add(sid, "get_weather");
        s.clear_session(sid);
        assert!(s.snapshot(sid).is_empty());
    }

    #[test]
    fn unknown_session_snapshot_is_empty() {
        let s = DiscoveredTools::new();
        assert!(s.snapshot(Uuid::new_v4()).is_empty());
    }
}
