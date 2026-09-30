//! `ratelimit` — per-IP rate-limit knobs (`[server.rate_limits]`).
//!
//! Two independent buckets are exposed because the STT pipeline and
//! the LLM proxy have very different cost profiles: STT is bounded by
//! the inference queue and should be relatively permissive (default
//! 120 frames/min ≈ 2 per second, enough for live VAD-driven speech);
//! the LLM proxy can saturate an external Ollama install much faster
//! and stays at 30 req/min by default.
//!
//! See [`crate::rate_limit`] for the implementation.

use crate::config::file::TomlRateLimitConfig;
use crate::config::{env_opt, resolve_primitive, ConfigError};

/// Rate-limit knobs applied per source IP.
#[derive(Debug, Clone)]
pub struct RateLimitConfig {
    /// Maximum STT WS frames per source IP per minute. A token is
    /// consumed per inbound `AudioFrame` / `StartSession` / `Config`
    /// payload (and one at the WS upgrade).
    pub stt_per_min: u32,
    /// Maximum LLM HTTP requests per source IP per minute. Applied
    /// to `/v1/chat/completions` and `/v1/models`.
    pub llm_per_min: u32,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        // Defaults match the values committed in the plan:
        // 120 STT frames/min, 30 LLM req/min.
        const DEFAULT_STT_PER_MIN: u32 = 120;
        const DEFAULT_LLM_PER_MIN: u32 = 30;
        Self {
            stt_per_min: DEFAULT_STT_PER_MIN,
            llm_per_min: DEFAULT_LLM_PER_MIN,
        }
    }
}

impl RateLimitConfig {
    pub fn from_env_with_toml(toml: Option<&TomlRateLimitConfig>) -> Result<Self, ConfigError> {
        let defaults = Self::default();
        let toml = toml.cloned().unwrap_or_default();
        Ok(Self {
            stt_per_min: resolve_primitive(
                env_opt("STT_RATE_PER_MIN").as_deref(),
                toml.stt_per_min,
                defaults.stt_per_min,
                "STT_RATE_PER_MIN",
            )?,
            llm_per_min: resolve_primitive(
                env_opt("LLM_RATE_PER_MIN").as_deref(),
                toml.llm_per_min,
                defaults.llm_per_min,
                "LLM_RATE_PER_MIN",
            )?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_limit_defaults_match_plan() {
        // We test the `Default` impl directly rather than round-tripping
        // through `from_env()`: mutating process-global env vars from
        // unit tests is racy under `cargo test`'s parallel harness and
        // would race with the WS-validation tests that also rely on
        // env state. The env-var wiring is covered end-to-end by the
        // `rate_limit` integration tests' `start_test_server_with` helper.
        let cfg = RateLimitConfig::default();
        assert_eq!(
            cfg.stt_per_min, 120,
            "STT_RATE_PER_MIN default should be 120"
        );
        assert_eq!(cfg.llm_per_min, 30, "LLM_RATE_PER_MIN default should be 30");
    }
}
