//! `llm::provider` — per-provider adaptation of the upstream request body.
//!
//! The proxy speaks the same OpenAI-compatible wire protocol to every
//! [`LlmProvider`], but two things differ per provider family:
//!
//! 1. The generation-cap knob: Ollama spells it
//!    `options.num_predict` / `options.num_ctx` (native parameters
//!    forwarded under the `options.*` slot), while every other
//!    OpenAI-compatible API (Mistral, OpenAI, vLLM, …) uses the
//!    top-level `max_tokens`. The Ollama-native `options` object is
//!    not part of the OpenAI schema and strict servers may reject the
//!    whole request with a 422 when it is present.
//! 2. `adapt_request_body` is the single place that applies both, so
//!    `proxy::chat_completions` (round 0) and `tool_loop::run_tool_loop`
//!    (round 1+) can never drift apart.

use serde_json::{json, Value};

use crate::config::{LlmConfig, LlmProvider};

/// Adapt the forwarded `/v1/chat/completions` body to the configured
/// provider. Idempotent — safe to call on every tool-loop round.
///
/// - `provider = "ollama"` → current behaviour: merge
///   `options.num_predict` / `options.num_ctx` from
///   `ollama_num_predict` / `ollama_num_ctx` (the two helpers below,
///   unchanged since they lived in `proxy.rs`).
/// - any other provider → remove the `options` object entirely (it is
///   Ollama-native; the browser never sends one today, but stripping
///   is the safe default against strict validation), and inject the
///   top-level `max_tokens` from `[llm].max_tokens`
///   (`LLM_MAX_TOKENS`). `ollama_num_predict` is honoured as
///   `max_tokens` on non-Ollama providers when `max_tokens` itself is
///   unset, so one knob covers both families — the generic
///   `max_tokens` wins when both are set.
pub(crate) fn adapt_request_body(body: &mut Value, cfg: &LlmConfig) {
    match cfg.provider {
        LlmProvider::Ollama => {
            inject_ollama_num_predict(body, cfg.ollama_num_predict);
            inject_ollama_num_ctx(body, cfg.ollama_num_ctx);
        }
        LlmProvider::OpenAiCompatible | LlmProvider::Mistral => {
            if let Some(obj) = body.as_object_mut() {
                obj.remove("options");
            }
            let max_tokens = cfg.max_tokens.or(cfg.ollama_num_predict);
            if let (Some(mt), Some(obj)) = (max_tokens, body.as_object_mut()) {
                obj.insert("max_tokens".into(), json!(mt));
            }
        }
    }
}

/// Merge the admin-configured `ollama_num_predict` into the
/// `options.num_predict` slot of the upstream body. A no-op when the
/// operator has not opted in (`ollama_num_predict = None`). Moved
/// here unchanged from `proxy.rs` so both call sites share one
/// implementation.
pub(crate) fn inject_ollama_num_predict(forward_body: &mut Value, ollama_num_predict: Option<u32>) {
    let Some(np) = ollama_num_predict else { return };
    let Some(obj) = forward_body.as_object_mut() else {
        return;
    };
    let options = if let Some(existing) = obj.get_mut("options") {
        existing
    } else {
        obj.insert("options".into(), json!({}));
        obj.get_mut("options")
            .expect("just inserted `options` as object")
    };
    let Some(options_obj) = options.as_object_mut() else {
        // `options` was set by the client to a non-object
        // (e.g. a string). Don't overwrite — let the upstream
        // surface the mismatch. Logging here is enough; the
        // request is malformed regardless.
        tracing::warn!("forward body `options` is not an object; skipping num_predict injection");
        return;
    };
    options_obj.insert("num_predict".into(), json!(np));
}

/// Merge the admin-configured `ollama_num_ctx` into the
/// `options.num_ctx` slot of the upstream body so reasoning
/// models (deepseek-r1, qwen3.5 with thinking on) see enough
/// context to finish a long reasoning + tool-call round in one
/// upstream request. The Ollama default `num_ctx` of `2048`
/// otherwise cuts them off mid-thought and trips the
/// `llm_max_auto_continues` heuristic into a loop. A no-op
/// when the operator has not opted in (`ollama_num_ctx = None`)
/// so the upstream keeps its own default (e.g. a Modelfile
/// `PARAMETER num_ctx 32768`, or a server-wide
/// `OLLAMA_CONTEXT_LENGTH`); the proxy only injects the field
/// when explicitly told to. Preserves any pre-existing
/// `options.num_ctx` the browser set on the request — the
/// server knob wins on conflict so a misbehaving client
/// cannot shrink the context window. Moved here unchanged
/// from `proxy.rs`.
pub(crate) fn inject_ollama_num_ctx(forward_body: &mut Value, ollama_num_ctx: Option<u32>) {
    let Some(ctx) = ollama_num_ctx else { return };
    let Some(obj) = forward_body.as_object_mut() else {
        return;
    };
    let options = if let Some(existing) = obj.get_mut("options") {
        existing
    } else {
        obj.insert("options".into(), json!({}));
        obj.get_mut("options")
            .expect("just inserted `options` as object")
    };
    let Some(options_obj) = options.as_object_mut() else {
        // `options` was set by the client to a non-object
        // (e.g. a string). Don't overwrite — let the upstream
        // surface the mismatch. Logging here is enough; the
        // request is malformed regardless.
        tracing::warn!("forward body `options` is not an object; skipping num_ctx injection");
        return;
    };
    options_obj.insert("num_ctx".into(), json!(ctx));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(provider: LlmProvider) -> LlmConfig {
        LlmConfig {
            provider,
            ..LlmConfig::default()
        }
    }

    fn body() -> Value {
        json!({
            "model": "m",
            "stream": true,
            "messages": [{"role": "user", "content": "hi"}],
        })
    }

    #[test]
    fn ollama_keeps_and_merges_options() {
        // Regression guard for today's behaviour: on ollama the
        // `options` object is preserved and the two knobs land under
        // `options.num_predict` / `options.num_ctx`.
        let mut cfg = cfg(LlmProvider::Ollama);
        cfg.ollama_num_predict = Some(2048);
        cfg.ollama_num_ctx = Some(32768);
        let mut b = body();
        b["options"] = json!({"seed": 42});
        adapt_request_body(&mut b, &cfg);
        assert_eq!(b["options"]["num_predict"], json!(2048));
        assert_eq!(b["options"]["num_ctx"], json!(32768));
        assert_eq!(b["options"]["seed"], json!(42), "client options survive");
        assert!(b.get("max_tokens").is_none());
    }

    #[test]
    fn ollama_noop_when_knobs_unset() {
        let cfg = cfg(LlmProvider::Ollama);
        let mut b = body();
        b["options"] = json!({"seed": 42});
        adapt_request_body(&mut b, &cfg);
        assert_eq!(b["options"], json!({"seed": 42}));
        assert!(b.get("max_tokens").is_none());
    }

    #[test]
    fn mistral_strips_options_and_sets_max_tokens() {
        let mut cfg = cfg(LlmProvider::Mistral);
        cfg.max_tokens = Some(4096);
        let mut b = body();
        // A stray Ollama-native `options` object (e.g. left by an
        // older client) must not reach a strict hosted API.
        b["options"] = json!({"num_predict": 128});
        adapt_request_body(&mut b, &cfg);
        assert!(b.get("options").is_none(), "options must be stripped");
        assert_eq!(b["max_tokens"], json!(4096));
    }

    #[test]
    fn non_ollama_honours_ollama_num_predict_as_max_tokens_alias() {
        // One knob covers both families: on non-Ollama providers
        // `ollama_num_predict` is forwarded as `max_tokens` when the
        // generic knob is unset.
        let mut cfg = cfg(LlmProvider::Mistral);
        cfg.ollama_num_predict = Some(2048);
        let mut b = body();
        adapt_request_body(&mut b, &cfg);
        assert!(b.get("options").is_none());
        assert_eq!(b["max_tokens"], json!(2048));
    }

    #[test]
    fn generic_max_tokens_wins_over_ollama_num_predict_alias() {
        let mut cfg = cfg(LlmProvider::OpenAiCompatible);
        cfg.max_tokens = Some(1024);
        cfg.ollama_num_predict = Some(2048);
        let mut b = body();
        adapt_request_body(&mut b, &cfg);
        assert_eq!(b["max_tokens"], json!(1024));
    }

    #[test]
    fn non_ollama_without_knobs_leaves_body_untouched() {
        let cfg = cfg(LlmProvider::Mistral);
        let mut b = body();
        adapt_request_body(&mut b, &cfg);
        assert!(b.get("options").is_none());
        assert!(b.get("max_tokens").is_none());
        assert_eq!(b["model"], json!("m"));
    }

    #[test]
    fn adapt_is_idempotent_across_rounds() {
        // The tool loop re-applies the adapter on every round; the
        // result must not accumulate or change.
        let mut cfg = cfg(LlmProvider::Mistral);
        cfg.max_tokens = Some(512);
        let mut b = body();
        adapt_request_body(&mut b, &cfg);
        let once = b.clone();
        adapt_request_body(&mut b, &cfg);
        assert_eq!(b, once);
    }
}
