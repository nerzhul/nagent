//! JSON projections from raw Shortcut API payloads to the shapes
//! consumed by the LLM chat tools (`project_story`, `label_names`,
//! …) and by the dashboard UI (`project_story_detail`,
//! `label_objects`, `project_comment`).
//!
//! The projections are deliberately lossy on purpose: they pick
//! the fields the consumers actually render and drop the rest, so
//! a change in the upstream schema does not ripple into every
//! caller.

use serde_json::{json, Value};

/// Project one Shortcut story object into the LLM-facing row.
/// Used by the list / search tools; `shortcut_get_story` adds the
/// capped description and comments, and the dashboard detail uses
/// [`project_story_detail`].
pub fn project_story(s: &Value) -> Value {
    json!({
        "id": s.get("id"),
        "name": s.get("name"),
        "story_type": s.get("story_type"),
        "workflow_state_id": s.get("workflow_state_id"),
        "project_id": s.get("project_id"),
        "epic_id": s.get("epic_id"),
        "iteration_id": s.get("iteration_id"),
        "estimate": s.get("estimate"),
        "labels": label_names(s),
        "owner_ids": s.get("owner_ids"),
        "requested_by_id": s.get("requested_by_id"),
        "deadline": s.get("deadline"),
        "archived": s.get("archived"),
        "created_at": s.get("created_at"),
        "updated_at": s.get("updated_at"),
        "app_url": s.get("app_url"),
    })
}

/// Project one Shortcut story object into the dashboard detail
/// row: everything [`project_story`] keeps, plus the **full**
/// description and the label objects (name + color) so the UI
/// can render badges.
pub fn project_story_detail(s: &Value) -> Value {
    let mut p = project_story(s);
    p["description"] = s.get("description").cloned().unwrap_or(Value::Null);
    p["labels"] = Value::Array(label_objects(s));
    p
}

/// Project one Shortcut story comment into the dashboard row.
/// The text is returned in full (the dashboard renders it as
/// markdown); the LLM tools cap it separately via
/// [`crate::truncate`].
pub fn project_comment(c: &Value) -> Value {
    json!({
        "id": c.get("id"),
        "author_id": c.get("author_id"),
        "text": c.get("text"),
        "created_at": c.get("created_at"),
        "updated_at": c.get("updated_at"),
    })
}

/// Extract the label names from a Shortcut `labels` array.
pub fn label_names(v: &Value) -> Vec<Value> {
    v.get("labels")
        .and_then(|l| l.as_array())
        .map(|arr| arr.iter().filter_map(|l| l.get("name").cloned()).collect())
        .unwrap_or_default()
}

/// Extract the full label objects (`name`, `color`) from a
/// Shortcut `labels` array, for UI badges.
pub fn label_objects(v: &Value) -> Vec<Value> {
    v.get("labels")
        .and_then(|l| l.as_array())
        .map(|arr| {
            arr.iter()
                .map(|l| {
                    json!({
                        "id": l.get("id"),
                        "name": l.get("name"),
                        "color": l.get("color"),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Normalise a handle for comparison: trim, strip a leading `@`,
/// lowercase.
fn normalize_handle(s: &str) -> String {
    s.trim().trim_start_matches('@').to_lowercase()
}

/// Pick the token's member out of a `GET /members` response. The v3
/// API has no `/members/me` route — that path falls through to
/// `/members/{member-public-id}`, whose spec requires a UUID — so the
/// list route plus a handle match is the supported resolution. With a
/// `handle`, matches `profile.mention_name` first, then
/// `profile.name`, then `profile.email_address` (case-insensitive,
/// `@`-tolerant). Without a handle, only a single-member list is
/// unambiguous. Accepts either a member array or a single member
/// object so callers survive minor API drift. Shared by the
/// `shortcut_me` agent and the server-side setup probe.
pub fn find_member<'a>(parsed: &'a Value, handle: Option<&str>) -> Option<&'a Value> {
    let members: Vec<&Value> = match parsed.as_array() {
        Some(arr) => arr.iter().collect(),
        None => vec![parsed],
    };
    match handle.map(normalize_handle).filter(|h| !h.is_empty()) {
        Some(h) => members.iter().copied().find(|m| {
            let profile = m.get("profile").unwrap_or(*m);
            ["mention_name", "name", "email_address"].iter().any(|f| {
                profile
                    .get(*f)
                    .and_then(|v| v.as_str())
                    .is_some_and(|v| normalize_handle(v) == h)
            })
        }),
        None => {
            if members.len() == 1 {
                members.first().copied()
            } else {
                None
            }
        }
    }
}

/// Extract `mention_name` from a Shortcut member object. The v3
/// member nests it under `profile`; a top-level field is accepted
/// as a fallback so the projection survives minor API drift.
pub fn mention_name_of(v: &Value) -> Value {
    v.get("profile")
        .and_then(|p| p.get("mention_name"))
        .or_else(|| v.get("mention_name"))
        .cloned()
        .unwrap_or(Value::Null)
}

/// Extract `name` from a Shortcut member object (same nesting
/// as [`mention_name_of`]).
pub fn name_of_member(v: &Value) -> Value {
    v.get("profile")
        .and_then(|p| p.get("name"))
        .or_else(|| v.get("name"))
        .cloned()
        .unwrap_or(Value::Null)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn project_story_picks_llm_fields() {
        let raw = json!({
            "id": 123,
            "name": "Fix login",
            "story_type": "bug",
            "workflow_state_id": 500000001,
            "epic_id": 7,
            "iteration_id": null,
            "estimate": 3,
            "labels": [{"name": "backend"}],
            "description": "long body — dropped from the list projection",
            "app_url": "https://app.shortcut.com/ws/story/123"
        });
        let p = project_story(&raw);
        assert_eq!(p["id"], 123);
        assert_eq!(p["story_type"], "bug");
        assert_eq!(p["labels"], json!(["backend"]));
        assert!(p.get("description").is_none());
    }

    #[test]
    fn project_story_detail_keeps_description_and_label_objects() {
        let raw = json!({
            "id": 123,
            "name": "Fix login",
            "labels": [{"id": 9, "name": "backend", "color": "#ff0000"}],
            "description": "full **markdown** body",
        });
        let p = project_story_detail(&raw);
        assert_eq!(p["description"], json!("full **markdown** body"));
        assert_eq!(p["labels"][0]["name"], json!("backend"));
        assert_eq!(p["labels"][0]["color"], json!("#ff0000"));
    }

    #[test]
    fn project_comment_keeps_full_text() {
        let c =
            json!({"id": 5, "author_id": "uuid", "text": "looks good", "created_at": "2026-01-01"});
        let p = project_comment(&c);
        assert_eq!(p["text"], json!("looks good"));
    }

    #[test]
    fn mention_name_reads_profile_with_fallback() {
        assert_eq!(
            mention_name_of(&json!({"profile": {"mention_name": "jane"}})),
            json!("jane")
        );
        assert_eq!(
            mention_name_of(&json!({"mention_name": "jane"})),
            json!("jane")
        );
        assert_eq!(mention_name_of(&json!({})), Value::Null);
    }

    #[test]
    fn find_member_matches_mention_name_case_insensitive() {
        let members = json!([
            {"profile": {"mention_name": "jane", "name": "Jane Doe"}},
            {"profile": {"mention_name": "john", "name": "John Roe"}},
        ]);
        let m = find_member(&members, Some("@JANE")).unwrap();
        assert_eq!(mention_name_of(m), json!("jane"));
        assert!(find_member(&members, Some("nobody")).is_none());
        // No handle + multi-member list is ambiguous.
        assert!(find_member(&members, None).is_none());
        let single = json!([{"profile": {"mention_name": "solo"}}]);
        assert!(find_member(&single, None).is_some());
    }
}
