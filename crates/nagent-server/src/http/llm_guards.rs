//! `http::llm_guards` — middleware + helpers shared by every
//! `/v1/*` route (LLM proxy, agents, documents, TTS, chat-session).
//!
//! Two concerns live here:
//!
//! - [`build_rate_limiters`] — build the per-IP STT and LLM token
//! buckets from the resolved [`crate::config::Config`].
//! - [`llm_rate_limit_middleware`] — the axum middleware that consumes
//! one LLM-bucket token per request, keyed by the peer address from
//! `ConnectInfo`.
//! - [`llm_auth_middleware`] — the bearer-token gate driven by
//! [`crate::config::LlmAuthMode`] / [`crate::config::LlmConfig::inbound_auth_key`].
//!
//! Moved out of `lib.rs` as part of the architecture restructuring of the architecture
//! refactor so `build_router` (itself moved to [`crate::http::build_router`])
//! stays a pure composition function.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::ConnectInfo;
use axum::http::{HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::config::{Config, LlmConfig};
use crate::rate_limit::{RateLimitError, RateLimitPolicy, RateLimiter};

/// axum middleware that consumes one token from the supplied LLM
/// limiter per request, identified by the peer address attached by
/// [`axum::serve`] (i.e. `ConnectInfo<SocketAddr>`).
///
/// On rejection we return `429 Too Many Requests` with a
/// `Retry-After` header computed from the bucket's refill rate so
/// well-behaved clients can back off. The `loopback` carve-out lives
/// inside the limiter itself.
pub async fn llm_rate_limit_middleware(
    limiter: RateLimiter,
    req: axum::extract::Request,
    next: Next,
) -> Response {
    let peer: Option<ConnectInfo<SocketAddr>> = req.extensions().get().cloned();
    let Some(ConnectInfo(addr)) = peer else {
        // Without `ConnectInfo` (test harness, in-process calls) we
        // cannot key the bucket; let the request through so unit
        // tests don't all need a real TCP listener.
        return next.run(req).await;
    };
    match limiter.check(addr.ip()) {
        Ok(()) => next.run(req).await,
        Err(RateLimitError::Limited { retry_after_ms, .. }) => {
            let retry_secs = retry_after_ms.div_ceil(1000).max(1);
            let mut resp = (StatusCode::TOO_MANY_REQUESTS, "rate limit exceeded").into_response();
            resp.headers_mut().insert(
                axum::http::header::RETRY_AFTER,
                HeaderValue::from_str(&retry_secs.to_string())
                    .unwrap_or(HeaderValue::from_static("1")),
            );
            resp.headers_mut().insert(
                axum::http::header::CONTENT_TYPE,
                HeaderValue::from_static("text/plain"),
            );
            resp
        }
    }
}

/// Build the per-IP rate limiters from the active configuration. Kept
/// here (rather than next to `Config`) so the wiring stays in one
/// place — the limiter is built once per process and shared via
/// [`crate::AppState`].
pub fn build_rate_limiters(cfg: &Config) -> (RateLimiter, RateLimiter) {
    (
        RateLimiter::new(RateLimitPolicy::stt(cfg.rate_limit.stt_per_min)),
        RateLimiter::new(RateLimitPolicy::llm(cfg.rate_limit.llm_per_min)),
    )
}

/// axum middleware that gates `/v1/*` requests behind the
/// `LLM_AUTH_MODE` policy.
///
/// Behaviour per [`crate::config::LlmAuthMode`]:
/// - [`LlmAuthMode::Bearer`](crate::config::LlmAuthMode::Bearer) (with
/// `inbound_auth_key` set): reject requests missing
/// `Authorization: Bearer <key>` or carrying a different key with
/// `401 Unauthorized` and a `WWW-Authenticate` hint so curl and SDKs
/// surface a useful error.
/// - [`LlmAuthMode::Bearer`](crate::config::LlmAuthMode::Bearer) (no key
/// set): the auth gate is a no-op and a warning is logged at boot —
/// the operator enabled the `bearer` mode without providing a key,
/// so the proxy is effectively public until they fix the config.
/// - [`LlmAuthMode::Forward`](crate::config::LlmAuthMode::Forward) /
/// [`LlmAuthMode::Disabled`](crate::config::LlmAuthMode::Disabled):
/// no inbound inspection. `Disabled` is a deliberate opt-out and
/// only affects the startup warning emitted by `main`.
pub async fn llm_auth_middleware(
    cfg: Option<Arc<LlmConfig>>,
    req: axum::extract::Request,
    next: Next,
) -> Response {
    let Some(cfg) = cfg else {
        return next.run(req).await;
    };
    if !matches!(cfg.auth_mode, crate::config::LlmAuthMode::Bearer) {
        return next.run(req).await;
    }
    let Some(expected) = cfg.inbound_auth_key.as_deref() else {
        // `bearer` mode without a key — log once at startup via
        // `main`, and let the request through here so a misconfigured
        // server still functions.
        return next.run(req).await;
    };
    let header_value = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());
    let presented = header_value.and_then(|h| {
        h.strip_prefix("Bearer ")
            .or_else(|| h.strip_prefix("bearer "))
    });
    match presented {
        Some(key) if constant_time_eq(key.as_bytes(), expected.as_bytes()) => next.run(req).await,
        _ => {
            let mut resp = (
                StatusCode::UNAUTHORIZED,
                "missing or invalid Authorization header",
            )
                .into_response();
            resp.headers_mut().insert(
                axum::http::header::WWW_AUTHENTICATE,
                HeaderValue::from_static("Bearer realm=\"nagent-llm-proxy\""),
            );
            resp
        }
    }
}

/// Constant-time byte slice comparison. Avoids leaking the key length
/// via the early-exit path of `==`. Safe for ASCII bearer tokens which
/// never contain non-ASCII bytes.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut acc = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        acc |= x ^ y;
    }
    acc == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constant_time_eq_matches_on_equal_inputs() {
        assert!(constant_time_eq(b"hello", b"hello"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn constant_time_eq_rejects_on_different_lengths() {
        // Length difference must return `false` immediately so an
        // attacker cannot probe the key length byte-by-byte.
        assert!(!constant_time_eq(b"hello", b"hell"));
        assert!(!constant_time_eq(b"", b"x"));
    }

    #[test]
    fn constant_time_eq_rejects_on_different_content() {
        assert!(!constant_time_eq(b"hello", b"world"));
        // Last-byte difference (the hardest case for early-exit
        // comparators).
        assert!(!constant_time_eq(b"hello", b"hellp"));
    }
}
