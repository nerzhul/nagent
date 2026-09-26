//! Server-default system prompt for `/v1/chat/completions`.
//!
//! The admin-configured prompt (env `LLM_SYSTEM_PROMPT` or
//! `[llm].system_prompt` in TOML) is the authoritative system message;
//! the browser-supplied "Additional instructions" textarea is appended
//! after it, so the admin's intent stays dominant in the conversation
//! order. [`inject_default_system_prompt`] performs the prepend in
//! `chat_completions` so every round of the tool loop carries the
//! same prefix (the loop shares one `Value` body across rounds).
//!
//! [`DEFAULT_SYSTEM_PROMPT`] is the binary's built-in fallback used by
//! the demo / mock builds; production deployments are expected to
//! override it via env or TOML.

use serde_json::{json, Value};

/// Built-in default system prompt. English by `AGENTS.md` rule #1;
/// admins override it via `LLM_SYSTEM_PROMPT` or `[llm].system_prompt`
/// in TOML. The agent names match `Agent::name()` in
/// `crates/stt-server/src/agents/{datetime,weather,stock,web_fetch}_agent.rs`
/// — keeping the spelling in sync matters: the LLM matches tool names
/// verbatim when emitting `tool_calls`.
pub const DEFAULT_SYSTEM_PROMPT: &str = "\
You are nagent, a local, privacy-respecting assistant embedded in a \
speech-to-text and chat web app.

You have access to the following tool agents when the server has them \
enabled:
- get_datetime: current date and time, optionally in an IANA timezone \
(for example \"Europe/Paris\").
- get_weather: current weather, multi-day forecast, hourly data and \
astronomy for a location. The chat UI renders a structured weather card \
from the tool's JSON response; the assistant's prose should be a brief \
acknowledgment in the user's language (\"Voici les informations \
demandées.\" / \"Here are the details.\") and NOTHING ELSE — the card \
already shows temperature, condition, wind, humidity, UV and the 3-day \
strip, so do not re-state any of those fields in prose.
- get_stock_quote: latest stock quote for a ticker symbol.
- web_fetch: fetch a public URL and return its main text as Markdown.

Tool-usage rules:
- NEVER invent specific factual data: weather, current date or time, \
stock prices, or the content of a fetched URL. For these domains you \
either call the matching tool or you say you cannot answer. Guessing \
is a regression that breaks user trust. \
\
The earlier \"answer directly from general knowledge\" rule below is \
overridden for these domains only.
- Whenever the user's request is time-sensitive (\"today\", \"this week\", \
\"latest\", a past date, a forecast, a stock price), call `get_datetime` \
FIRST to learn the current date and time before invoking any other \
tool. The local model has no internal clock and stale context will \
produce wrong answers.
- When calling `get_weather` with an explicit date, build the YYYY-MM-DD \
argument from the value returned by `get_datetime`; never invent a \
date. The tool may also be called without a date for current conditions.
- If a tool call fails or returns an error, surface that to the user \
verbatim rather than substituting a plausible-sounding answer from \
memory.
- For domains outside the tool list (general knowledge, reasoning, \
writing, code), answer directly as before. Do not call tools when the \
user has not asked for live, factual, or externally-sourced data.

Formatting:
- Reply in the language the user wrote in.
- Use Markdown. Inline math in $...$ and block math in $$...$$ are \
rendered with KaTeX.
- Keep answers concise unless the user explicitly asks for more detail.";

/// Prepend the admin's system prompt as `messages[0]`.
///
/// Rules:
/// - `None` or whitespace-only → no-op (backwards-compatible
///   passthrough).
/// - Missing `messages` array → no-op; the caller is responsible for
///   rejecting the request upstream (`ChatRequest` deserialisation
///   already returns 400 when `messages` is absent).
/// - Existing `messages[0]` system message (rare but legal per
///   OpenAI's schema) is preserved *after* the admin's prompt so the
///   admin's intent stays authoritative.
///
/// Pure function: no `serde_json::from_slice`, no env reads — the
/// caller hands in the already-parsed `forward_body`. This keeps the
/// helper cheap to unit-test in isolation.
pub fn inject_default_system_prompt(forward_body: &mut Value, prompt: Option<&str>) {
    let Some(prompt) = prompt else { return };
    let trimmed = prompt.trim();
    if trimmed.is_empty() {
        return;
    }
    let Some(obj) = forward_body.as_object_mut() else {
        return;
    };
    let Some(messages) = obj.get_mut("messages").and_then(|m| m.as_array_mut()) else {
        return;
    };
    messages.insert(0, json!({ "role": "system", "content": trimmed }));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(role: &str, content: &str) -> Value {
        json!({ "role": role, "content": content })
    }

    #[test]
    fn inject_appends_to_empty_messages() {
        let mut body = json!({ "messages": [] });
        inject_default_system_prompt(&mut body, Some("hello"));
        assert_eq!(
            body["messages"],
            json!([{ "role": "system", "content": "hello" }])
        );
    }

    #[test]
    fn inject_prepends_to_existing_messages() {
        let mut body = json!({
            "messages": [
                msg("user", "what time is it?"),
                msg("assistant", "let me check")
            ]
        });
        inject_default_system_prompt(&mut body, Some("server prompt"));
        let messages = body["messages"].as_array().expect("array");
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[0]["content"], "server prompt");
        // User / assistant ordering preserved after the prepend.
        assert_eq!(messages[1]["role"], "user");
        assert_eq!(messages[2]["role"], "assistant");
    }

    #[test]
    fn inject_does_not_remove_client_system_messages() {
        // The OpenAI schema allows several system messages; an admin's
        // prompt must come first so the LLM treats it as authoritative,
        // but client-supplied system messages must survive.
        let mut body = json!({
            "messages": [
                msg("system", "client extension"),
                msg("user", "hi")
            ]
        });
        inject_default_system_prompt(&mut body, Some("admin"));
        let messages = body["messages"].as_array().expect("array");
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[0]["content"], "admin");
        assert_eq!(messages[1]["role"], "system");
        assert_eq!(messages[1]["content"], "client extension");
        assert_eq!(messages[2]["role"], "user");
    }

    #[test]
    fn inject_skipped_when_prompt_is_none() {
        let mut body = json!({
            "messages": [msg("user", "hi")]
        });
        let original = body.clone();
        inject_default_system_prompt(&mut body, None);
        assert_eq!(body, original, "body must be unchanged when prompt is None");
    }

    #[test]
    fn inject_skipped_when_prompt_is_whitespace_only() {
        for ws in ["", " ", "\n", "  \t\n  "] {
            let mut body = json!({
                "messages": [msg("user", "hi")]
            });
            let original = body.clone();
            inject_default_system_prompt(&mut body, Some(ws));
            assert_eq!(
                body, original,
                "body must be unchanged when prompt is whitespace-only ({ws:?})"
            );
        }
    }

    #[test]
    fn inject_trims_prompt_before_injection() {
        let mut body = json!({ "messages": [] });
        inject_default_system_prompt(&mut body, Some("  hello\n"));
        assert_eq!(body["messages"][0]["content"], "hello");
    }

    #[test]
    fn inject_no_op_when_messages_missing() {
        // Defensive: callers validate `messages` upstream, but if a
        // future code path mutates the body without the array we must
        // not panic — silently leave it alone.
        let mut body = json!({});
        let original = body.clone();
        inject_default_system_prompt(&mut body, Some("admin"));
        assert_eq!(body, original);
    }

    #[test]
    fn inject_no_op_when_body_is_not_an_object() {
        let mut body = json!("not an object");
        let original = body.clone();
        inject_default_system_prompt(&mut body, Some("admin"));
        assert_eq!(body, original);
    }

    #[test]
    fn default_prompt_nudges_short_weather_reply() {
        // The chat UI renders a structured weather card for
        // get_weather; if the prompt lets the assistant write a long
        // paragraph the card's information is duplicated in prose
        // and the widget loses most of its value. This substring-
        // level guard keeps the nudge visible to a future rewrite
        // of the prompt.
        let prompt = DEFAULT_SYSTEM_PROMPT;
        assert!(
            prompt.contains("get_weather")
                && prompt.contains("weather card")
                && prompt.contains("acknowledgment")
                && (prompt.contains("Voici les informations")
                    || prompt.contains("Here are the details")),
            "DEFAULT_SYSTEM_PROMPT no longer nudges the model to keep get_weather replies to a brief acknowledgment ('Voici les informations demandées.' / 'Here are the details.') while the widget renders the detail. Card loses most of its value without the nudge."
        );
    }

    #[test]
    fn default_prompt_forbids_fabricating_tool_data() {
        // The biggest user-facing regression on time-sensitive queries
        // is the LLM answering weather / datetime / stock quotes from
        // training data instead of calling the tool. The prompt must
        // explicitly forbid fabrication in those domains, and must no
        // longer carry the old "answer from general knowledge when
        // possible" line that legitimised it.
        let prompt = DEFAULT_SYSTEM_PROMPT;
        assert!(
            prompt.contains("NEVER invent")
                && prompt.contains("weather")
                && prompt.contains("current date")
                && prompt.contains("stock"),
            "DEFAULT_SYSTEM_PROMPT is missing the anti-fabrication rule. The LLM will answer time-sensitive factual queries from training data and the user sees invented temperatures / dates / prices."
        );
        assert!(
            !prompt.contains("Do not call tools for general knowledge you can answer directly"),
            "DEFAULT_SYSTEM_PROMPT still carries the old 'answer directly from general knowledge' rule that lets the model skip tool calls for weather/datetime/stocks. Remove the line or scope it to non-factual domains only."
        );
    }
}