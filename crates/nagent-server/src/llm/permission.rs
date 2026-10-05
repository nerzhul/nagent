//! `llm::permission` — chat-session permission store + decision parser.
//!
//! Bridges between the LLM tool loop's `Agent::requires_confirmation`
//! outcome and the chat UI's inline approval card. The flow is:
//!
//! 1. When a tool call needs user confirmation, the tool loop pushes a
//!    [`PendingApproval`] entry into [`PermissionStore`] keyed on the
//!    tool call id, and emits an enriched `tool_result` SSE frame with
//!    `needs_approval = true` so the client can render the approval
//!    card.
//! 2. The user clicks one of the three buttons
//!    (Autoriser / Toujours pour cette session / Refuser); `chat.js`
//!    sends a synthetic user message whose first token is a sentinel
//!    (`[APPROVE:tool_call_id]`, `[APPROVE_ALWAYS:tool_name]`, or
//!    `[DENY:tool_call_id]`).
//! 3. [`parse_decision_prefix`] recognises the sentinel on the next
//!    chat-completion request, BEFORE the upstream round begins. The
//!    chat route handler drives the appropriate path (re-invoke the
//!    tool with `requires_confirmation` bypassed, persist a
//!    session-wide override, or emit a synthetic `tool_result` denial)
//!    and then lets the regular tool loop continue so the LLM can
//!    summarise the action.
//!
//! In-memory only — the store vanishes on server restart, matching the
//! LLM-mediated path's own lack of state. Session mint clears the
//! store for that session so a fresh chat never inherits a previous
//! user's overrides.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::json;
use uuid::Uuid;

/// Hard ceiling on how long an approval entry survives in the store
/// before being silently dropped. The UI stops waiting after a few
/// seconds in practice; the TTL is a safety net for a user who abandoned
/// the chat mid-approval and came back later. Older entries are
/// evicted lazily on every store access.
const PENDING_TTL: Duration = Duration::from_secs(300); // 5 min

/// One outstanding approval request surfaced to the chat UI.
#[derive(Debug, Clone)]
pub struct PendingApproval {
    pub tool_name: String,
    pub args: serde_json::Value,
    pub created_at: Instant,
}

/// Per-session state: pending approvals + persistent overrides.
#[derive(Debug, Default)]
pub(crate) struct SessionPermissionState {
    pub pending: HashMap<String, PendingApproval>,
    pub session_overrides: HashSet<String>,
}

/// Decision parsed from the start of a user message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DecisionPrefix {
    /// Approve a single pending tool call.
    Approve { tool_call_id: String },
    /// Approve every future call of the named tool within this session.
    ApproveAlways { tool_name: String },
    /// Cancel a pending tool call; the LLM sees a denial tool_result.
    Deny { tool_call_id: String },
}

/// Recognise one of the three decision prefixes at the start of a user
/// message. The sentinel must appear at the start of the message
/// (allowing optional leading whitespace) and the rest of the message
/// is returned so the caller can splice it back as natural-language
/// carry-over. `None` means "not a decision — send to the LLM".
///
/// Format:
/// - `[APPROVE:<tool_call_id>]`
/// - `[APPROVE_ALWAYS:<tool_name>]`
/// - `[DENY:<tool_call_id>]`
///
/// `tool_call_id` is a server-issued UUID-shaped id (short ASCII
/// alphabet). `tool_name` is a snake_case agent name. Anything that
/// doesn't match the regex returns `None` — including mid-message
/// occurrences (e.g. a user typing "[APPROVE:abc]" inside a sentence)
/// — so an honest sentence never gets eaten by accident.
pub(crate) fn parse_decision_prefix(text: &str) -> Option<(DecisionPrefix, &str)> {
    let trimmed = text.trim_start();
    if let Some(rest) = trimmed.strip_prefix("[APPROVE:") {
        let close = rest.find(']')?;
        let id = &rest[..close];
        if !is_safe_id(id) {
            return None;
        }
        let remainder = &rest[close + 1..];
        return Some((
            DecisionPrefix::Approve {
                tool_call_id: id.to_string(),
            },
            remainder,
        ));
    }
    if let Some(rest) = trimmed.strip_prefix("[APPROVE_ALWAYS:") {
        let close = rest.find(']')?;
        let name = &rest[..close];
        if !is_safe_tool_name(name) {
            return None;
        }
        let remainder = &rest[close + 1..];
        return Some((
            DecisionPrefix::ApproveAlways {
                tool_name: name.to_string(),
            },
            remainder,
        ));
    }
    if let Some(rest) = trimmed.strip_prefix("[DENY:") {
        let close = rest.find(']')?;
        let id = &rest[..close];
        if !is_safe_id(id) {
            return None;
        }
        let remainder = &rest[close + 1..];
        return Some((
            DecisionPrefix::Deny {
                tool_call_id: id.to_string(),
            },
            remainder,
        ));
    }
    None
}

fn is_safe_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn is_safe_tool_name(s: &str) -> bool {
    !s.is_empty() && s.len() <= 64 && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Build the human-readable prompt the chat UI renders in the
/// approval card. Falls back to a generic "Autoriser cet outil ?"
/// for any agent that isn't in the known list — keeps the wire
/// shape consistent without leaking internal agent logic.
pub(crate) fn approval_prompt(tool_name: &str, args: &serde_json::Value) -> ApprovalPrompt {
    match tool_name {
        "caldav_create_event" => {
            let summary = args
                .get("summary")
                .and_then(|v| v.as_str())
                .unwrap_or("(sans titre)");
            let start = args.get("start").and_then(|v| v.as_str()).unwrap_or("");
            let body = if start.is_empty() {
                format!("L'assistant veut créer « {summary} » dans votre calendrier.")
            } else {
                format!("L'assistant veut créer « {summary} » le {start} dans votre calendrier.")
            };
            ApprovalPrompt {
                title: "Créer un événement dans votre calendrier ?".to_string(),
                body,
                danger: Danger::Medium,
            }
        }
        "caldav_list_events" => {
            let start = args.get("start").and_then(|v| v.as_str()).unwrap_or("");
            let end = args.get("end").and_then(|v| v.as_str()).unwrap_or("");
            let body = if start.is_empty() || end.is_empty() {
                "L'assistant veut lister les événements de votre calendrier.".to_string()
            } else {
                format!(
                    "L'assistant veut lister les événements de votre calendrier entre le {start} et le {end}."
                )
            };
            ApprovalPrompt {
                title: "Lire votre calendrier CalDAV ?".to_string(),
                body,
                danger: Danger::Medium,
            }
        }
        "caldav_get_event" => {
            let uid = args.get("uid").and_then(|v| v.as_str()).unwrap_or("");
            let body = if uid.is_empty() {
                "L'assistant veut lire un événement spécifique de votre calendrier.".to_string()
            } else {
                format!("L'assistant veut lire l'événement « {uid} » de votre calendrier.")
            };
            ApprovalPrompt {
                title: "Lire un événement CalDAV ?".to_string(),
                body,
                danger: Danger::Medium,
            }
        }
        "web_fetch" => {
            let url = args.get("url").and_then(|v| v.as_str()).unwrap_or("");
            ApprovalPrompt {
                title: "Visiter un site externe ?".to_string(),
                body: if url.is_empty() {
                    "L'assistant veut consulter une URL externe.".to_string()
                } else {
                    format!("L'assistant veut consulter {url}.")
                },
                danger: Danger::Low,
            }
        }
        _ => ApprovalPrompt {
            title: "Autoriser cet outil ?".to_string(),
            body: format!("L'assistant veut appeler l'outil « {tool_name} »."),
            danger: Danger::Low,
        },
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // High reserved for a future high-risk agent.
pub(crate) enum Danger {
    Low,
    Medium,
    High,
}

impl Danger {
    pub fn as_str(self) -> &'static str {
        match self {
            Danger::Low => "low",
            Danger::Medium => "medium",
            Danger::High => "high",
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ApprovalPrompt {
    pub title: String,
    pub body: String,
    pub danger: Danger,
}

/// `Arc<Mutex<HashMap<SessionId, …>>>` shared across all axum tasks.
/// Mutex (sync) is enough — every operation is a handful of
/// HashMap/HashSet insertions, microseconds; an async lock would just
/// add scheduling jitter.
#[derive(Clone, Default)]
pub struct PermissionStore {
    inner: Arc<Mutex<HashMap<Uuid, SessionPermissionState>>>,
}

impl std::fmt::Debug for PermissionStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let n = self.inner.lock().map(|m| m.len()).unwrap_or(0);
        f.debug_struct("PermissionStore")
            .field("sessions", &n)
            .finish()
    }
}

impl PermissionStore {
    /// Construct a fresh empty store. Cheap; call once at boot.
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert (or replace) a pending approval for the given session
    /// and tool call id. Lazy-evicts expired entries on every call.
    pub fn set_pending(
        &self,
        session_id: Uuid,
        tool_call_id: String,
        tool_name: String,
        args: serde_json::Value,
    ) {
        let mut guard = self.inner.lock().expect("permission store poisoned");
        let state = guard.entry(session_id).or_default();
        evict_expired(state, Instant::now());
        state.pending.insert(
            tool_call_id,
            PendingApproval {
                tool_name,
                args,
                created_at: Instant::now(),
            },
        );
    }

    /// Pop a pending entry by tool call id. Returns `None` if the
    /// entry is missing or expired.
    pub fn take_pending(&self, session_id: Uuid, tool_call_id: &str) -> Option<PendingApproval> {
        let mut guard = self.inner.lock().expect("permission store poisoned");
        let state = guard.entry(session_id).or_default();
        evict_expired(state, Instant::now());
        state.pending.remove(tool_call_id)
    }

    /// Peek at the args (without taking the entry) so the approval
    /// path can rebuild the prompt after a server restart that wiped
    /// the store. Currently only used in tests; the live path uses
    /// [`take_pending`] so the entry is consumed atomically with the
    /// decision.
    #[allow(dead_code)]
    pub fn peek_pending(&self, session_id: Uuid, tool_call_id: &str) -> Option<PendingApproval> {
        let guard = self.inner.lock().expect("permission store poisoned");
        guard
            .get(&session_id)
            .and_then(|s| s.pending.get(tool_call_id).cloned())
    }

    /// Record a session-wide override for the given tool name. Any
    /// future call to that tool in the same session will be allowed
    /// without an approval card (the tool loop's bypass path checks
    /// this set before consulting `Agent::requires_confirmation`).
    pub fn add_override(&self, session_id: Uuid, tool_name: String) {
        let mut guard = self.inner.lock().expect("permission store poisoned");
        let state = guard.entry(session_id).or_default();
        evict_expired(state, Instant::now());
        state.session_overrides.insert(tool_name);
    }

    /// `true` iff the session has a stored override for the given
    /// tool. Called from the tool loop's bypass path.
    pub fn has_override(&self, session_id: Uuid, tool_name: &str) -> bool {
        let guard = self.inner.lock().expect("permission store poisoned");
        guard
            .get(&session_id)
            .map(|s| s.session_overrides.contains(tool_name))
            .unwrap_or(false)
    }

    /// Wipe every entry (pending + overrides) for the session. Called
    /// on session mint and explicit reset.
    #[allow(dead_code)]
    pub fn clear_session(&self, session_id: Uuid) {
        let mut guard = self.inner.lock().expect("permission store poisoned");
        guard.remove(&session_id);
    }
}

fn evict_expired(state: &mut SessionPermissionState, now: Instant) {
    state
        .pending
        .retain(|_, p| now.duration_since(p.created_at) < PENDING_TTL);
}

/// Build the JSON payload describing an approval request, matching
/// the shape the chat UI expects in the `prompt` field of the
/// `tool_result` SSE event.
pub(crate) fn approval_prompt_json(p: &ApprovalPrompt) -> serde_json::Value {
    json!({
        "title": p.title,
        "body": p.body,
        "danger": p.danger.as_str(),
    })
}

/// Lightweight wrapper that drives a permission-decision prefix when
/// the latest user message carries one. Returns a fresh body with the
/// sentinel stripped (and a synthetic `tool` entry appended for the
/// APPROVE / DENY cases) when a prefix was matched; `None` when the
/// latest user message is a normal message.
///
/// Kept for unit-test coverage; the production path is
/// [`run_permission_intercept`] which is async (it invokes the
/// agent on APPROVE before returning).
#[allow(dead_code)]
pub(crate) fn shape_permission_body(
    body: &serde_json::Value,
    pending_lookup: impl Fn(&str) -> Option<PendingApproval>,
) -> Option<serde_json::Value> {
    let messages = body.get("messages")?.as_array()?;
    let last_user_idx = messages
        .iter()
        .rposition(|m| m.get("role").and_then(|r| r.as_str()) == Some("user"))?;
    let last_user = &messages[last_user_idx];
    let text = last_user.get("content").and_then(|v| v.as_str())?;
    let (decision, remainder) = parse_decision_prefix(text)?;

    let mut new_body = body.clone();
    let new_messages = new_body
        .as_object_mut()
        .and_then(|o| o.get_mut("messages"))
        .and_then(|m| m.as_array_mut())?;

    // Preserve any natural-language carry-over so the LLM still
    // sees the user's intent (e.g. "[APPROVE:abc] merci" → "merci").
    let trimmed_remainder = remainder.trim_start();
    if trimmed_remainder.is_empty() {
        new_messages.remove(last_user_idx);
    } else {
        new_messages[last_user_idx] = json!({
            "role": "user",
            "content": trimmed_remainder,
        });
    }

    match decision {
        DecisionPrefix::Approve { tool_call_id } => {
            if let Some(pending) = pending_lookup(&tool_call_id) {
                let args_str = serde_json::to_string(&pending.args).unwrap_or_else(|_| "{}".into());
                new_messages.push(json!({
                    "role": "assistant",
                    "content": "",
                    "tool_calls": [{
                        "id": tool_call_id,
                        "type": "function",
                        "function": {
                            "name": pending.tool_name,
                            "arguments": args_str,
                        }
                    }],
                }));
                // Tool result placeholder; the async caller
                // replaces it with the real result before
                // pushing the body to run_tool_loop.
                new_messages.push(json!({
                    "role": "tool",
                    "tool_call_id": tool_call_id,
                    "content": "",
                }));
            } else {
                new_messages.push(json!({
                    "role": "tool",
                    "tool_call_id": tool_call_id,
                    "content": "[error] approval expired or unknown; please ask the user again",
                }));
            }
            Some(new_body)
        }
        DecisionPrefix::ApproveAlways { tool_name: _ } => {
            // The caller has already persisted the session
            // override. The LLM just acks the decision.
            Some(new_body)
        }
        DecisionPrefix::Deny { tool_call_id } => {
            new_messages.push(json!({
                "role": "tool",
                "tool_call_id": tool_call_id,
                "content": "Utilisateur a refusé l'opération.",
            }));
            Some(new_body)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_approve() {
        let (d, rest) = parse_decision_prefix("[APPROVE:abc-123] merci").unwrap();
        assert_eq!(
            d,
            DecisionPrefix::Approve {
                tool_call_id: "abc-123".into()
            }
        );
        assert_eq!(rest, " merci");
    }

    #[test]
    fn parse_approve_no_rest() {
        let (d, rest) = parse_decision_prefix("[APPROVE:abc-123]").unwrap();
        assert_eq!(
            d,
            DecisionPrefix::Approve {
                tool_call_id: "abc-123".into()
            }
        );
        assert_eq!(rest, "");
    }

    #[test]
    fn parse_approve_always() {
        let (d, rest) = parse_decision_prefix("[APPROVE_ALWAYS:caldav_create_event]").unwrap();
        assert_eq!(
            d,
            DecisionPrefix::ApproveAlways {
                tool_name: "caldav_create_event".into()
            }
        );
        assert_eq!(rest, "");
    }

    #[test]
    fn parse_deny() {
        let (d, _rest) = parse_decision_prefix("[DENY:abc]").unwrap();
        assert_eq!(
            d,
            DecisionPrefix::Deny {
                tool_call_id: "abc".into()
            }
        );
    }

    #[test]
    fn parse_leading_whitespace_ok() {
        let (d, _rest) = parse_decision_prefix("  [APPROVE:abc]").unwrap();
        assert_eq!(
            d,
            DecisionPrefix::Approve {
                tool_call_id: "abc".into()
            }
        );
    }

    #[test]
    fn parse_mid_message_returns_none() {
        // Sentinel embedded mid-message is treated as plain text.
        assert!(parse_decision_prefix("hello [APPROVE:abc] world").is_none());
    }

    #[test]
    fn parse_malformed_brackets() {
        assert!(parse_decision_prefix("[APPROVE:abc").is_none());
        assert!(parse_decision_prefix("APPROVE:abc]").is_none());
        assert!(parse_decision_prefix("[approve:abc]").is_none());
    }

    #[test]
    fn parse_unsafe_id_rejected() {
        assert!(parse_decision_prefix("[APPROVE:abc def]").is_none());
        assert!(parse_decision_prefix("[APPROVE:abc/def]").is_none());
        assert!(parse_decision_prefix("[APPROVE:]").is_none());
    }

    #[test]
    fn parse_unsafe_tool_name_rejected() {
        assert!(parse_decision_prefix("[APPROVE_ALWAYS:has space]").is_none());
        assert!(parse_decision_prefix("[APPROVE_ALWAYS:]").is_none());
    }

    #[test]
    fn store_set_and_take_pending() {
        let s = PermissionStore::new();
        let sid = Uuid::new_v4();
        s.set_pending(
            sid,
            "abc".into(),
            "caldav_create_event".into(),
            json!({"summary": "test"}),
        );
        let p = s.take_pending(sid, "abc").unwrap();
        assert_eq!(p.tool_name, "caldav_create_event");
        assert!(s.take_pending(sid, "abc").is_none());
    }

    #[test]
    fn store_overrides() {
        let s = PermissionStore::new();
        let sid = Uuid::new_v4();
        assert!(!s.has_override(sid, "caldav_create_event"));
        s.add_override(sid, "caldav_create_event".into());
        assert!(s.has_override(sid, "caldav_create_event"));
        assert!(!s.has_override(sid, "web_fetch"));
    }

    #[test]
    fn store_clear_session() {
        let s = PermissionStore::new();
        let sid = Uuid::new_v4();
        s.set_pending(sid, "abc".into(), "x".into(), json!({}));
        s.add_override(sid, "x".into());
        s.clear_session(sid);
        assert!(s.take_pending(sid, "abc").is_none());
        assert!(!s.has_override(sid, "x"));
    }

    #[test]
    fn approval_prompt_caldav_create_event() {
        let p = approval_prompt(
            "caldav_create_event",
            &json!({"summary": "Dentiste", "start": "2026-10-08T10:00:00"}),
        );
        assert!(p.title.contains("calendrier"));
        assert!(p.body.contains("Dentiste"));
        assert_eq!(p.danger, Danger::Medium);
    }

    #[test]
    fn approval_prompt_web_fetch() {
        let p = approval_prompt("web_fetch", &json!({"url": "https://example.com"}));
        assert_eq!(p.danger, Danger::Low);
        assert!(p.body.contains("https://example.com"));
    }

    #[test]
    fn approval_prompt_fallback() {
        let p = approval_prompt("get_weather", &json!({}));
        assert_eq!(p.danger, Danger::Low);
        assert!(p.body.contains("get_weather"));
    }

    #[test]
    fn approval_prompt_caldav_list_events() {
        let p = approval_prompt(
            "caldav_list_events",
            &json!({"start": "2026-10-05T00:00:00Z", "end": "2026-10-19T00:00:00Z"}),
        );
        assert!(p.title.contains("calendrier"));
        assert!(p.body.contains("2026-10-05"));
        assert!(p.body.contains("2026-10-19"));
        // Calendar reads carry the medium-danger accent (the
        // border + ⚠ icon surface in the chat UI).
        assert_eq!(p.danger, Danger::Medium);
    }

    #[test]
    fn approval_prompt_caldav_list_events_missing_range_falls_back() {
        let p = approval_prompt("caldav_list_events", &json!({}));
        assert_eq!(p.danger, Danger::Medium);
        assert!(p.body.contains("lister"));
    }

    #[test]
    fn approval_prompt_caldav_get_event() {
        let p = approval_prompt("caldav_get_event", &json!({"uid": "evt-123@host"}));
        assert!(p.title.contains("événement"));
        assert!(p.body.contains("evt-123@host"));
        assert_eq!(p.danger, Danger::Medium);
    }

    #[test]
    fn shape_body_no_prefix_returns_none() {
        let body = json!({"messages": [{"role":"user","content":"hello"}]});
        assert!(shape_permission_body(&body, |_| None).is_none());
    }

    #[test]
    fn shape_body_approve_known_id() {
        let body = json!({"messages": [{"role":"user","content":"[APPROVE:abc]"}]});
        let lookup = |id: &str| {
            assert_eq!(id, "abc");
            Some(PendingApproval {
                tool_name: "caldav_create_event".into(),
                args: json!({"summary": "Dentiste"}),
                created_at: std::time::Instant::now(),
            })
        };
        let out = shape_permission_body(&body, lookup).unwrap();
        let msgs = out.get("messages").unwrap().as_array().unwrap();
        // The sentinel-only user message is dropped; only the
        // assistant + tool pair remains.
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].get("role").unwrap(), "assistant");
        assert_eq!(msgs[1].get("role").unwrap(), "tool");
        assert_eq!(msgs[1].get("content").unwrap(), "");
    }

    #[test]
    fn shape_body_deny() {
        let body = json!({"messages": [{"role":"user","content":"[DENY:abc]"}]});
        let out = shape_permission_body(&body, |_| None).unwrap();
        let msgs = out.get("messages").unwrap().as_array().unwrap();
        assert_eq!(msgs.len(), 1);
        assert!(msgs[0]
            .get("content")
            .unwrap()
            .as_str()
            .unwrap()
            .contains("refusé"));
    }
}

/// Drive the permission intercept on the chat-completion body.
///
/// Inspects the last user message for a decision sentinel; on hit:
/// 1. Emits the matching `tool_call` (if APPROVE) and `tool_result`
///    SSE frames into the response channel.
/// 2. Mutates the body so the synthetic tool round reaches the LLM
///    on the next round.
///
/// Returns `Some((new_body, pre_frames))` when a sentinel was found,
/// or `None` when the last user message was a regular message.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_permission_intercept(
    body: &serde_json::Value,
    agents: Option<&nagent_agents::AgentRegistry>,
    session_id: Option<uuid::Uuid>,
    user_id: uuid::Uuid,
    resolver: Option<std::sync::Arc<dyn nagent_agents::SecretSource>>,
    permission_store: PermissionStore,
) -> Option<(serde_json::Value, Vec<bytes::Bytes>)> {
    let messages = body.get("messages")?.as_array()?;
    let last_user_idx = messages
        .iter()
        .rposition(|m| m.get("role").and_then(|r| r.as_str()) == Some("user"))?;
    let last_user = &messages[last_user_idx];
    let text = last_user.get("content").and_then(|v| v.as_str())?;
    let (decision, remainder) = parse_decision_prefix(text)?;

    let mut frames: Vec<bytes::Bytes> = Vec::new();
    let mut new_body = body.clone();
    let new_messages = new_body
        .get_mut("messages")
        .and_then(|m| m.as_array_mut())?;

    let trimmed_remainder = remainder.trim_start();
    if trimmed_remainder.is_empty() {
        new_messages.remove(last_user_idx);
    } else {
        new_messages[last_user_idx] = json!({
            "role": "user",
            "content": trimmed_remainder,
        });
    }

    match decision {
        DecisionPrefix::Approve { tool_call_id } => {
            let pending = match session_id {
                Some(sid) => permission_store.take_pending(sid, &tool_call_id),
                None => None,
            };
            let pending = match pending {
                Some(p) => p,
                None => {
                    // Stale / unknown id: surface an error so the
                    // LLM can re-prompt the user.
                    new_messages.push(json!({
                        "role": "tool",
                        "tool_call_id": tool_call_id,
                        "content": "[error] approval expired or unknown; please ask the user again",
                    }));
                    return Some((new_body, frames));
                }
            };
            let agents = match agents {
                Some(a) => a,
                None => {
                    new_messages.push(json!({
                        "role": "tool",
                        "tool_call_id": tool_call_id,
                        "content": format!("[error] agents not registered; cannot run `{}`", pending.tool_name),
                    }));
                    return Some((new_body, frames));
                }
            };
            let agent = match agents.get(&pending.tool_name) {
                Some(a) => a,
                None => {
                    new_messages.push(json!({
                        "role": "tool",
                        "tool_call_id": tool_call_id,
                        "content": format!("[error] unknown agent: `{}`", pending.tool_name),
                    }));
                    return Some((new_body, frames));
                }
            };
            // Emit `tool_call` SSE so the chat UI updates the
            // footer pill to a "running" state for this entry.
            let args_str = serde_json::to_string(&pending.args).unwrap_or_else(|_| "{}".into());
            frames.push(bytes::Bytes::from(crate::llm::sse::sse_tool_call_event(
                &tool_call_id,
                &pending.tool_name,
                &args_str,
                1, // synthetic round index; LLM never sees this
            )));
            // Build a per-call `UserContext` exactly like the
            // tool loop does, then invoke the agent directly —
            // no `requires_confirmation` check (this IS the
            // approval).
            let services = nagent_agents::ServiceRegistry::empty().into_arc();
            let mut ctx = match session_id {
                Some(sid) => nagent_agents::UserContext::for_chat_session(
                    user_id,
                    services,
                    resolver.clone(),
                    None,
                    sid,
                ),
                None => nagent_agents::UserContext::for_tests(user_id, services),
            };
            ctx.record_invocation(&pending.tool_name);
            let (ok, payload) = match agent.invoke(&ctx, pending.args.clone()).await {
                Ok(s) => (true, s),
                Err(e) => (false, format!("[error] {e}")),
            };
            frames.push(bytes::Bytes::from(crate::llm::sse::sse_tool_result_event(
                &tool_call_id,
                &pending.tool_name,
                ok,
                &payload,
            )));
            new_messages.push(json!({
                "role": "assistant",
                "content": "",
                "tool_calls": [{
                    "id": tool_call_id,
                    "type": "function",
                    "function": {
                        "name": pending.tool_name,
                        "arguments": args_str,
                    }
                }],
            }));
            new_messages.push(json!({
                "role": "tool",
                "tool_call_id": tool_call_id,
                "content": payload,
            }));
            Some((new_body, frames))
        }
        DecisionPrefix::ApproveAlways { tool_name } => {
            if let Some(sid) = session_id {
                permission_store.add_override(sid, tool_name.clone());
            }
            // No synthetic tool round — the LLM just acks the
            // override on its next response.
            Some((new_body, frames))
        }
        DecisionPrefix::Deny { tool_call_id } => {
            // Drop the pending entry if present so a later
            // re-approval request must come from the LLM again.
            if let Some(sid) = session_id {
                let _ = permission_store.take_pending(sid, &tool_call_id);
            }
            let deny_msg = "Utilisateur a refusé l'opération.";
            frames.push(bytes::Bytes::from(crate::llm::sse::sse_tool_result_event(
                &tool_call_id,
                "<denied>",
                false,
                deny_msg,
            )));
            new_messages.push(json!({
                "role": "tool",
                "tool_call_id": tool_call_id,
                "content": deny_msg,
            }));
            Some((new_body, frames))
        }
    }
}
