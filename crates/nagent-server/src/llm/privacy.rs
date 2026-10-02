//! `llm::privacy` — admin kill-switches for browser-injected blocks.
//!
//! The browser prepends two ephemeral system messages when the user
//! has opted in:
//!
//! - `User's approximate location: …` ([`USER_LOCATION_MARKER`])
//! - `The user's local timezone is …` ([`USER_TIMEZONE_MARKER`])
//!
//! The proxy itself prepends one more when the user has set a
//! non-default reply language on the `user_preferences` row:
//!
//! - `The user's preferred reply language is …`
//!   ([`USER_REPLY_LANGUAGE_MARKER`])
//!
//! An operator can turn each one off independently via
//! `LLM_ALLOW_USER_LOCATION=false` / `LLM_ALLOW_USER_TIMEZONE=false`
//! / `LLM_ALLOW_USER_REPLY_LANGUAGE=false`. The helpers below are
//! defence-in-depth filters that run AFTER the admin system prompt
//! has been injected, so the admin block always survives.
//!
//! Re-exported from the parent module as
//! `strip_user_location_if_disabled` / `strip_user_timezone_if_disabled`
//! for backward-compat with the pre-split callers.

use serde_json::Value;

use crate::llm::prompt::{USER_LOCATION_MARKER, USER_REPLY_LANGUAGE_MARKER, USER_TIMEZONE_MARKER};

/// Drop the ephemeral `User's approximate location:` system message
/// the browser prepends when the admin has switched the feature off.
///
/// The browser only injects the block when the user has granted
/// consent, but the admin may still want to forbid the upstream model
/// from ever seeing it (compliance, sensitive deployment, …). The flag
/// defaults to `true` so the UI is the primary gate; this helper is a
/// defence-in-depth filter that runs after
/// [`crate::llm::prompt::inject_default_system_prompt`] so the admin's
/// prompt block always survives.
///
/// Matching is strict on the marker prefix to avoid clobbering an
/// unrelated system message the user happened to type. A no-op when
/// the flag is `true` or the body carries no location block.
pub fn strip_user_location_if_disabled(forward_body: &mut Value, allow: bool) {
    if allow {
        return;
    }
    let Some(messages) = forward_body
        .as_object_mut()
        .and_then(|o| o.get_mut("messages"))
        .and_then(|m| m.as_array_mut())
    else {
        return;
    };
    messages.retain(|m| {
        let Some(role) = m.get("role").and_then(|v| v.as_str()) else {
            return true;
        };
        if role != "system" {
            return true;
        }
        // `content` may be a string OR a list of `{type, text}` parts
        // per the OpenAI multimodal schema. We only support the string
        // form (the browser always emits it); anything else is left
        // alone so a future multimodal prompt isn't accidentally
        // dropped by the kill-switch.
        match m.get("content").and_then(|v| v.as_str()) {
            Some(text) => !text.starts_with(USER_LOCATION_MARKER),
            None => true,
        }
    });
}

/// Drop the ephemeral `The user's local timezone is` system message
/// the browser prepends when the admin has switched the feature off.
///
/// Behaviour mirrors [`strip_user_location_if_disabled`]: defaults to
/// `true`, runs after [`crate::llm::prompt::inject_default_system_prompt`],
/// strict prefix match, and only inspects string-typed `content` so a
/// future multimodal prompt isn't accidentally dropped. Splitting the
/// two strip helpers keeps the kill-switches independent — an admin
/// who wants location but not timezone (or vice versa) gets exactly
/// that.
pub fn strip_user_timezone_if_disabled(forward_body: &mut Value, allow: bool) {
    if allow {
        return;
    }
    let Some(messages) = forward_body
        .as_object_mut()
        .and_then(|o| o.get_mut("messages"))
        .and_then(|m| m.as_array_mut())
    else {
        return;
    };
    messages.retain(|m| {
        let Some(role) = m.get("role").and_then(|v| v.as_str()) else {
            return true;
        };
        if role != "system" {
            return true;
        }
        match m.get("content").and_then(|v| v.as_str()) {
            Some(text) => !text.starts_with(USER_TIMEZONE_MARKER),
            None => true,
        }
    });
}

/// Drop the ephemeral `The user's preferred reply language is`
/// system message the server prepends when the admin has switched the
/// feature off.
///
/// Behaviour mirrors [`strip_user_location_if_disabled`]: defaults
/// to `true`, runs after [`crate::llm::prompt::inject_default_system_prompt`],
/// strict prefix match, and only inspects string-typed `content`
/// so a future multimodal prompt isn't accidentally dropped. The
/// three strip helpers stay independent — an admin who wants
/// location but not timezone (or the reply language) gets exactly
/// that.
pub fn strip_user_reply_language_if_disabled(forward_body: &mut Value, allow: bool) {
    if allow {
        return;
    }
    let Some(messages) = forward_body
        .as_object_mut()
        .and_then(|o| o.get_mut("messages"))
        .and_then(|m| m.as_array_mut())
    else {
        return;
    };
    messages.retain(|m| {
        let Some(role) = m.get("role").and_then(|v| v.as_str()) else {
            return true;
        };
        if role != "system" {
            return true;
        }
        match m.get("content").and_then(|v| v.as_str()) {
            Some(text) => !text.starts_with(USER_REPLY_LANGUAGE_MARKER),
            None => true,
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn body_with_loc_marker() -> Value {
        // Hand-built payload matching what `chat.js` would send when
        // the user has shared their location: admin system prompt
        // (already prepended by `inject_default_system_prompt`),
        // followed by the browser-injected location block, then the
        // user's actual turn.
        json!({
            "messages": [
                { "role": "system", "content": "admin prompt" },
                {
                    "role": "system",
                    "content": format!("{USER_LOCATION_MARKER} lat=48.85, lon=2.35 (±65 m, captured 2026-09-26T14:05Z).")
                },
                { "role": "user", "content": "what's the weather?" },
            ]
        })
    }

    #[test]
    fn strip_user_location_keeps_block_when_allowed() {
        // Default-on path: the kill-switch flag is `true`, the
        // browser-injected block survives so the LLM can use it for
        // location-relative queries.
        let mut body = body_with_loc_marker();
        let snapshot = body.clone();
        strip_user_location_if_disabled(&mut body, true);
        assert_eq!(body, snapshot, "allow=true must be a pure no-op");
    }

    #[test]
    fn strip_user_location_drops_only_the_marker_block_when_disabled() {
        // Kill-switch path: the marker-prefixed system message is
        // dropped, but the admin prompt (different prefix) and the
        // user turn both survive untouched.
        let mut body = body_with_loc_marker();
        strip_user_location_if_disabled(&mut body, false);
        let messages = body["messages"].as_array().expect("messages array");
        assert_eq!(messages.len(), 2, "location block must be removed");
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[0]["content"], "admin prompt");
        assert_eq!(messages[1]["role"], "user");
    }

    #[test]
    fn strip_user_location_is_a_noop_without_marker_block() {
        // The body never carried a location block (user hasn't shared
        // any). The helper must not mutate the body in either mode.
        let mut body = json!({
            "messages": [
                { "role": "system", "content": "admin" },
                { "role": "user", "content": "hi" },
            ]
        });
        let snapshot = body.clone();
        strip_user_location_if_disabled(&mut body, false);
        assert_eq!(body, snapshot);
    }

    #[test]
    fn strip_user_location_does_not_touch_non_system_or_multimodal_messages() {
        // Defensive: a non-string `content` (OpenAI multimodal parts)
        // is left alone, and only `role: system` messages are
        // inspected.
        let mut body = json!({
            "messages": [
                {
                    "role": "system",
                    "content": [
                        { "type": "text", "text": format!("{USER_LOCATION_MARKER} multimodal") }
                    ]
                },
                {
                    "role": "user",
                    "content": [
                        { "type": "text", "text": format!("{USER_LOCATION_MARKER} user-side") }
                    ]
                }
            ]
        });
        let snapshot = body.clone();
        strip_user_location_if_disabled(&mut body, false);
        assert_eq!(
            body, snapshot,
            "non-string content must never be stripped by the kill-switch"
        );
    }

    fn body_with_tz_marker() -> Value {
        // Hand-built payload matching what `chat.js` would send when
        // the user has opted in to the browser-timezone opt-in: admin
        // system prompt (already prepended by
        // `inject_default_system_prompt`), followed by the
        // browser-injected timezone block, then the user's actual
        // turn. The location block is omitted here — the two
        // strip helpers must be independently correct.
        json!({
            "messages": [
                { "role": "system", "content": "admin prompt" },
                {
                    "role": "system",
                    "content": format!(
                        "{USER_TIMEZONE_MARKER} \"Europe/Paris\" (GMT+2, current local \
                         time on the user's device: 2026-09-28 21:34:18). Always answer \
                         time-related questions in this timezone..."
                    )
                },
                { "role": "user", "content": "what time is it in Tokyo?" },
            ]
        })
    }

    #[test]
    fn strip_user_timezone_keeps_block_when_allowed() {
        // Default-on path: the kill-switch flag is `true`, the
        // browser-injected block survives so the LLM can answer
        // time-relative queries in the user's zone.
        let mut body = body_with_tz_marker();
        let snapshot = body.clone();
        strip_user_timezone_if_disabled(&mut body, true);
        assert_eq!(body, snapshot, "allow=true must be a pure no-op");
    }

    #[test]
    fn strip_user_timezone_drops_only_the_marker_block_when_disabled() {
        // Kill-switch path: the marker-prefixed system message is
        // dropped, but the admin prompt (different prefix) and the
        // user turn both survive untouched.
        let mut body = body_with_tz_marker();
        strip_user_timezone_if_disabled(&mut body, false);
        let messages = body["messages"].as_array().expect("messages array");
        assert_eq!(messages.len(), 2, "timezone block must be removed");
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[0]["content"], "admin prompt");
        assert_eq!(messages[1]["role"], "user");
    }

    #[test]
    fn strip_user_timezone_is_a_noop_without_marker_block() {
        // The body never carried a timezone block (user hasn't
        // opted in). The helper must not mutate the body in either
        // mode.
        let mut body = json!({
            "messages": [
                { "role": "system", "content": "admin" },
                { "role": "user", "content": "hi" },
            ]
        });
        let snapshot = body.clone();
        strip_user_timezone_if_disabled(&mut body, false);
        assert_eq!(body, snapshot);
    }

    #[test]
    fn strip_user_timezone_does_not_touch_non_system_or_multimodal_messages() {
        // Defensive: a non-string `content` (OpenAI multimodal parts)
        // is left alone, and only `role: system` messages are
        // inspected. Mirrors the location-block guard so a future
        // multimodal prompt isn't accidentally dropped by the
        // kill-switch.
        let mut body = json!({
            "messages": [
                {
                    "role": "system",
                    "content": [
                        { "type": "text", "text": format!("{USER_TIMEZONE_MARKER} multimodal") }
                    ]
                },
                {
                    "role": "user",
                    "content": [
                        { "type": "text", "text": format!("{USER_TIMEZONE_MARKER} user-side") }
                    ]
                }
            ]
        });
        let snapshot = body.clone();
        strip_user_timezone_if_disabled(&mut body, false);
        assert_eq!(
            body, snapshot,
            "non-string content must never be stripped by the timezone kill-switch"
        );
    }

    #[test]
    fn strip_user_timezone_does_not_drop_location_marker_block() {
        // The two strip helpers are independent: a kill-switch on
        // timezone must NEVER delete the location block (different
        // marker prefix), and vice versa. This guards against a
        // future refactor that accidentally shares a prefix-match
        // constant between the two helpers.
        let mut body = json!({
            "messages": [
                { "role": "system", "content": format!("{USER_LOCATION_MARKER} lat=48.85, lon=2.35") },
                { "role": "system", "content": format!("{USER_TIMEZONE_MARKER} \"Europe/Paris\"") },
            ]
        });
        strip_user_timezone_if_disabled(&mut body, false);
        let messages = body["messages"].as_array().expect("messages array");
        assert_eq!(
            messages.len(),
            1,
            "timezone strip must NOT touch the location marker block"
        );
        assert!(messages[0]["content"]
            .as_str()
            .unwrap()
            .starts_with(USER_LOCATION_MARKER));
    }

    fn body_with_reply_language_marker() -> Value {
        // Hand-built payload matching what the proxy emits when the
        // authenticated user has set `reply_language = "fr"` on the
        // `user_preferences` row: admin system prompt (already
        // prepended by `inject_default_system_prompt`), followed by
        // the proxy-injected reply-language block, then the user's
        // actual turn. The location + timezone blocks are omitted so
        // this test exercises the reply-language helper in isolation.
        json!({
            "messages": [
                { "role": "system", "content": "admin prompt" },
                {
                    "role": "system",
                    "content": format!(
                        "{USER_REPLY_LANGUAGE_MARKER} fr. Always reply in this \
                         language unless the user explicitly asks for another \
                         language in the same turn."
                    )
                },
                { "role": "user", "content": "bonjour" },
            ]
        })
    }

    #[test]
    fn strip_user_reply_language_keeps_block_when_allowed() {
        let mut body = body_with_reply_language_marker();
        let snapshot = body.clone();
        strip_user_reply_language_if_disabled(&mut body, true);
        assert_eq!(body, snapshot, "allow=true must be a pure no-op");
    }

    #[test]
    fn strip_user_reply_language_drops_only_the_marker_block_when_disabled() {
        let mut body = body_with_reply_language_marker();
        strip_user_reply_language_if_disabled(&mut body, false);
        let messages = body["messages"].as_array().expect("messages array");
        assert_eq!(messages.len(), 2, "reply-language block must be removed");
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[0]["content"], "admin prompt");
        assert_eq!(messages[1]["role"], "user");
    }

    #[test]
    fn strip_user_reply_language_is_a_noop_without_marker_block() {
        let mut body = json!({
            "messages": [
                { "role": "system", "content": "admin" },
                { "role": "user", "content": "hi" },
            ]
        });
        let snapshot = body.clone();
        strip_user_reply_language_if_disabled(&mut body, false);
        assert_eq!(body, snapshot);
    }

    #[test]
    fn strip_user_reply_language_does_not_touch_non_system_or_multimodal_messages() {
        // Defensive: a non-string `content` (OpenAI multimodal parts)
        // is left alone, and only `role: system` messages are
        // inspected. Mirrors the location / timezone guards so a
        // future multimodal prompt isn't accidentally dropped by the
        // kill-switch.
        let mut body = json!({
            "messages": [
                {
                    "role": "system",
                    "content": [
                        { "type": "text", "text": format!("{USER_REPLY_LANGUAGE_MARKER} multimodal") }
                    ]
                },
                {
                    "role": "user",
                    "content": [
                        { "type": "text", "text": format!("{USER_REPLY_LANGUAGE_MARKER} user-side") }
                    ]
                }
            ]
        });
        let snapshot = body.clone();
        strip_user_reply_language_if_disabled(&mut body, false);
        assert_eq!(
            body, snapshot,
            "non-string content must never be stripped by the reply-language kill-switch"
        );
    }

    #[test]
    fn strip_user_reply_language_does_not_drop_other_marker_blocks() {
        // The three strip helpers are independent: a kill-switch on
        // the reply language must NEVER delete the location or
        // timezone blocks (different marker prefixes), and vice
        // versa. This guards against a future refactor that
        // accidentally shares a prefix-match constant between the
        // three helpers.
        let mut body = json!({
            "messages": [
                { "role": "system", "content": format!("{USER_LOCATION_MARKER} lat=48.85, lon=2.35") },
                { "role": "system", "content": format!("{USER_TIMEZONE_MARKER} \"Europe/Paris\"") },
                { "role": "system", "content": format!("{USER_REPLY_LANGUAGE_MARKER} fr") },
            ]
        });
        strip_user_reply_language_if_disabled(&mut body, false);
        let messages = body["messages"].as_array().expect("messages array");
        assert_eq!(
            messages.len(),
            2,
            "reply-language strip must NOT touch the location or timezone marker blocks"
        );
        let contents: Vec<&str> = messages
            .iter()
            .map(|m| m["content"].as_str().expect("content is string"))
            .collect();
        assert!(contents.iter().any(|c| c.starts_with(USER_LOCATION_MARKER)));
        assert!(contents.iter().any(|c| c.starts_with(USER_TIMEZONE_MARKER)));
        assert!(!contents
            .iter()
            .any(|c| c.starts_with(USER_REPLY_LANGUAGE_MARKER)));
    }
}
