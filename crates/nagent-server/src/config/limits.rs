//! `limits` — `[server.limits]` section: inbound WebSocket frame limits
//! and HTTP transport guardrails (S-1).
//!
//! See [`crate::stt::ws_handler::handle_inbound`] for the validation
//! that consumes the frame limits, and [`crate::http::build_router`]
//! for the body / timeout / WebSocket concurrency layers that consume
//! the transport guardrails.

use crate::config::file::TomlLimitsConfig;
use crate::config::{env_opt, resolve_primitive, ConfigError};

/// Limits applied to inbound WebSocket frames and the HTTP transport.
#[derive(Debug, Clone)]
pub struct LimitsConfig {
    /// Maximum number of PCM Float32 samples accepted in a single
    /// `AudioFrame`. Whisper's hard cap is 30 s of audio at 16 kHz
    /// (= 480 000 samples); anything longer is rejected with
    /// `ErrorCode::INVALID_FRAME`.
    pub max_audio_frame_samples: usize,
    /// Only `16_000` Hz audio is supported (whisper's required input
    /// rate). A different value is rejected with
    /// `ErrorCode::INVALID_FRAME`.
    pub required_sample_rate: u32,
    /// Maximum length of an accepted ISO 639-1 language hint, in
    /// bytes. Two-letter codes plus a region suffix (`pt-BR`,
    /// `zh-CN`) never exceed a handful of bytes; a 4 KiB string is
    /// almost certainly an attack.
    pub max_language_hint_bytes: usize,
    /// Per-request timeout for non-streaming routes, in milliseconds.
    /// Applied through `tower_http::timeout::TimeoutLayer` to every
    /// `/api/*`, `/v1/agents/*`, `/v1/credentials*`, `/v1/audio/voices`,
    /// and `/v1/models` route. SSE (`/v1/chat/completions`) and
    /// WebSocket upgrades are exempt — the upstream LLM client already
    /// bounds `/v1/chat/completions` and the WS handshake is bounded
    /// by axum itself. Defaults to 30 s.
    pub request_timeout_ms: u64,
    /// Maximum size of a single HTTP request body, in bytes. Applied
    /// through `axum::extract::DefaultBodyLimit` to every `/v1/*` and
    /// `/api/*` route so axum rejects oversized uploads with
    /// `413 Payload Too Large` before they reach a handler. The
    /// default (2 MiB) mirrors axum's built-in limit, but is now an
    /// operator-tunable knob so `/v1/documents` (which needs a larger
    /// upload) and the LLM proxy (which needs smaller prompts) can
    /// diverge from the default.
    pub body_limit_bytes: usize,
    /// Global cap on the number of concurrent WebSocket sessions
    /// attached to `/ws`. Reached upgrades are rejected with
    /// `503 Service Unavailable` + `Retry-After: 1` instead of being
    /// silently dropped. Defaults to 1024.
    pub ws_max_concurrent: usize,
    /// Per-source-IP cap on the number of concurrent WebSocket
    /// sessions. Defends against a single attacker exhausting the
    /// global cap with parallel upgrades from one peer. Defaults to 16.
    pub ws_max_per_ip: usize,
}

impl Default for LimitsConfig {
    fn default() -> Self {
        // 30 seconds at 16 kHz mono Float32.
        const DEFAULT_MAX_AUDIO_FRAME_SAMPLES: usize = 30 * 16_000;
        Self {
            max_audio_frame_samples: DEFAULT_MAX_AUDIO_FRAME_SAMPLES,
            required_sample_rate: 16_000,
            max_language_hint_bytes: 16,
            request_timeout_ms: 30_000,
            body_limit_bytes: 2 * 1024 * 1024,
            ws_max_concurrent: 1024,
            ws_max_per_ip: 16,
        }
    }
}

impl LimitsConfig {
    pub fn from_env_with_toml(toml: Option<&TomlLimitsConfig>) -> Result<Self, ConfigError> {
        let defaults = Self::default();
        let toml = toml.cloned().unwrap_or_default();
        Ok(Self {
            max_audio_frame_samples: resolve_primitive(
                env_opt("MAX_AUDIO_FRAME_SAMPLES").as_deref(),
                toml.max_audio_frame_samples,
                defaults.max_audio_frame_samples,
                "MAX_AUDIO_FRAME_SAMPLES",
            )?,
            required_sample_rate: resolve_primitive(
                env_opt("REQUIRED_SAMPLE_RATE").as_deref(),
                toml.required_sample_rate,
                defaults.required_sample_rate,
                "REQUIRED_SAMPLE_RATE",
            )?,
            max_language_hint_bytes: resolve_primitive(
                env_opt("MAX_LANGUAGE_HINT_BYTES").as_deref(),
                toml.max_language_hint_bytes,
                defaults.max_language_hint_bytes,
                "MAX_LANGUAGE_HINT_BYTES",
            )?,
            // Floor at 1 s so a misconfigured `0` does not make
            // every handler crash immediately; the floor is small
            // enough that no real-world call needs to take longer
            // than the operator can reasonably expect.
            request_timeout_ms: resolve_primitive(
                env_opt("REQUEST_TIMEOUT_MS").as_deref(),
                toml.request_timeout_ms,
                defaults.request_timeout_ms,
                "REQUEST_TIMEOUT_MS",
            )?
            .max(1_000),
            // Floor at 1 KiB so the knob cannot accidentally become 0
            // (which disables the body limit and re-opens the axum
            // default 2 MiB via a different code path).
            body_limit_bytes: resolve_primitive(
                env_opt("BODY_LIMIT_BYTES").as_deref(),
                toml.body_limit_bytes,
                defaults.body_limit_bytes,
                "BODY_LIMIT_BYTES",
            )?
            .max(1024),
            // Floor at 1 so an operator cannot accidentally lock
            // out the server entirely.
            ws_max_concurrent: resolve_primitive(
                env_opt("WS_MAX_CONCURRENT").as_deref(),
                toml.ws_max_concurrent,
                defaults.ws_max_concurrent,
                "WS_MAX_CONCURRENT",
            )?
            .max(1),
            ws_max_per_ip: resolve_primitive(
                env_opt("WS_MAX_PER_IP").as_deref(),
                toml.ws_max_per_ip,
                defaults.ws_max_per_ip,
                "WS_MAX_PER_IP",
            )?
            .max(1),
        })
    }
}
