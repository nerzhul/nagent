//! Server configuration loaded from environment variables.
//!
//! `WHISPER_MODEL_PATH` is the only required knob.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

/// Runtime configuration of the server.
#[derive(Debug, Clone)]
pub struct Config {
    /// Address the HTTP server binds to.
    pub bind_addr: SocketAddr,
    /// Path to the ggml-format whisper model.
    pub whisper_model_path: PathBuf,
    /// Capacity of the global inference queue.
    pub max_queue: usize,
    /// Watchdog threshold: a session with no activity for this long is
    /// dropped.
    pub session_idle_timeout: Duration,
    /// Per-inference timeout.
    pub infer_timeout: Duration,
    /// Optional LLM/Ollama proxy config. When `llm.enabled` is `false`
    /// the `/v1/*` routes are not registered at all.
    pub llm: LlmConfig,
    /// Configuration for server-side chat agents (`web_fetch` and
    /// future tools). The `enabled` flag controls whether the
    /// `/v1/agents*` routes are wired and whether the LLM proxy
    /// injects a `tools` array; the per-agent configs are honoured
    /// whenever `enabled` is true, even when the LLM proxy itself is
    /// off (so `curl /v1/agents/web_fetch/invoke` still works for
    /// local testing).
    pub agents: AgentConfig,
    /// Limits applied to inbound WebSocket frames (defence against
    /// malicious or buggy clients).
    pub limits: LimitsConfig,
    /// Per-source-IP rate limits. Applied at the HTTP layer for the
    /// LLM proxy (`/v1/*`) and at the WebSocket upgrade + per-frame
    /// layer for the STT pipeline. Loopback IPs always bypass.
    pub rate_limit: RateLimitConfig,
}

/// Rate-limit knobs applied per source IP.
///
/// Two independent buckets are exposed because the STT pipeline and
/// the LLM proxy have very different cost profiles: STT is bounded by
/// the inference queue and should be relatively permissive (default
/// 120 frames/min ≈ 2 per second, enough for live VAD-driven speech);
/// the LLM proxy can saturate an external Ollama install much faster
/// and stays at 30 req/min by default.
///
/// See [`crate::rate_limit`] for the implementation.
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

/// Limits applied to inbound WebSocket frames.
///
/// See [`crate::ws_handler::handle_inbound`] for the validation
/// that consumes these knobs.
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
}

impl Default for LimitsConfig {
    fn default() -> Self {
        // 30 seconds at 16 kHz mono Float32.
        const DEFAULT_MAX_AUDIO_FRAME_SAMPLES: usize = 30 * 16_000;
        Self {
            max_audio_frame_samples: DEFAULT_MAX_AUDIO_FRAME_SAMPLES,
            required_sample_rate: 16_000,
            max_language_hint_bytes: 16,
        }
    }
}

impl Config {
    /// Load configuration from environment variables.
    ///
    /// Returns an error if `WHISPER_MODEL_PATH` is missing or the bind
    /// address cannot be parsed.
    pub fn from_env() -> Result<Self, ConfigError> {
        // Best-effort .env load; missing file is fine in production.
        let _ = dotenvy::dotenv();

        let bind_addr: SocketAddr = std::env::var("BIND_ADDR")
            .unwrap_or_else(|_| "0.0.0.0:8080".into())
            .parse()
            .map_err(|e: std::net::AddrParseError| ConfigError::InvalidBindAddr(e.to_string()))?;

        let whisper_model_path = std::env::var("WHISPER_MODEL_PATH")
            .map(PathBuf::from)
            .map_err(|_| ConfigError::MissingModelPath)?;

        let max_queue = parse_env("MAX_QUEUE", 32)?;
        let session_idle_timeout =
            Duration::from_millis(parse_env("SESSION_IDLE_TIMEOUT_MS", 30_000)?);
        let infer_timeout = Duration::from_millis(parse_env("INFER_TIMEOUT_MS", 30_000)?);

        let limits = LimitsConfig::from_env()?;
        let llm = LlmConfig::from_env()?;
        let agents = AgentConfig::from_env()?;
        let rate_limit = RateLimitConfig::from_env()?;

        Ok(Self {
            bind_addr,
            whisper_model_path,
            max_queue,
            session_idle_timeout,
            infer_timeout,
            limits,
            rate_limit,
            llm,
            agents,
        })
    }
}

impl LimitsConfig {
    fn from_env() -> Result<Self, ConfigError> {
        let defaults = Self::default();
        Ok(Self {
            max_audio_frame_samples: parse_env(
                "MAX_AUDIO_FRAME_SAMPLES",
                defaults.max_audio_frame_samples,
            )?,
            required_sample_rate: parse_env("REQUIRED_SAMPLE_RATE", defaults.required_sample_rate)?,
            max_language_hint_bytes: parse_env(
                "MAX_LANGUAGE_HINT_BYTES",
                defaults.max_language_hint_bytes,
            )?,
        })
    }
}

impl RateLimitConfig {
    fn from_env() -> Result<Self, ConfigError> {
        let defaults = Self::default();
        Ok(Self {
            stt_per_min: parse_env("STT_RATE_PER_MIN", defaults.stt_per_min)?,
            llm_per_min: parse_env("LLM_RATE_PER_MIN", defaults.llm_per_min)?,
        })
    }
}

/// Configuration for the optional server-side Ollama proxy.
///
/// When `enabled` is `false` the `/v1/chat/completions` and `/v1/models`
/// routes are simply not registered, so the chat view in the UI 404s
/// gracefully and the rest of the STT server is unaffected.
#[derive(Debug, Clone)]
pub struct LlmConfig {
    /// Master switch for the `/v1/*` routes.
    pub enabled: bool,
    /// Base URL of the upstream OpenAI-compatible server (typically
    /// `http://localhost:11434` for Ollama).
    pub base_url: String,
    /// Default model id for `/v1/chat/completions` when the browser
    /// does not specify one.
    pub default_model: String,
    /// Optional bearer token to forward as `Authorization: Bearer …`.
    pub api_key: Option<String>,
    /// Per-chunk idle timeout (no bytes for this long → drop the stream).
    pub request_timeout: Duration,
    /// Comma-separated list of origins allowed to call `/v1/*` via
    /// cross-origin requests. Empty (the default) means the proxy is
    /// same-origin only — preflight requests from any other origin are
    /// rejected and the browser will never even attempt the call.
    pub cors_allow_origins: Vec<String>,
}

impl LlmConfig {
    fn from_env() -> Result<Self, ConfigError> {
        let enabled = parse_env::<bool>("LLM_ENABLED", false)?;
        let base_url = std::env::var("OLLAMA_BASE_URL")
            .unwrap_or_else(|_| "http://localhost:11434".to_string());
        let default_model =
            std::env::var("OLLAMA_MODEL").unwrap_or_else(|_| "llama3.1".to_string());
        let api_key = std::env::var("OLLAMA_API_KEY")
            .ok()
            .filter(|s| !s.is_empty());
        let request_timeout = Duration::from_secs(parse_env("LLM_REQUEST_TIMEOUT_SECS", 120)?);
        let cors_allow_origins = std::env::var("LLM_CORS_ALLOW_ORIGINS")
            .ok()
            .map(|s| {
                s.split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_default();

        Ok(Self {
            enabled,
            base_url,
            default_model,
            api_key,
            request_timeout,
            cors_allow_origins,
        })
    }
}

fn parse_env<T>(key: &str, default: T) -> Result<T, ConfigError>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    match std::env::var(key) {
        Ok(v) => v
            .parse::<T>()
            .map_err(|e| ConfigError::InvalidEnv(key.into(), e.to_string())),
        Err(_) => Ok(default),
    }
}

/// Configuration for server-side chat agents.
///
/// `enabled` is the master switch for the `/v1/agents*` HTTP routes
/// and the LLM-proxy tool-loop. The per-agent sub-configs are
/// honoured whenever `enabled` is true; each agent applies its own
/// sandbox policy on top of the global settings.
#[derive(Debug, Clone)]
pub struct AgentConfig {
    /// Master switch. When `false`, no agents are registered, the LLM
    /// proxy injects no `tools` field, and `/v1/agents` returns `[]`.
    /// Defaults to `true` so first-time users get the wired-up
    /// experience; set `AGENTS_ENABLED=false` to disable.
    pub enabled: bool,
    /// Maximum number of tool-call rounds a single user turn may
    /// trigger before the proxy bails out and surfaces an error
    /// bubble. Defends against models that loop on a tool call.
    pub llm_max_tool_rounds: u32,
    /// Sandbox + transfer knobs for the built-in `web_fetch` agent.
    /// Always parsed; the agent is only registered when the `web-agent`
    /// cargo feature is on AND `enabled` is true.
    pub web_fetch: WebFetchConfig,
    /// WeatherAPI.com credentials for the `get_weather` agent. The
    /// agent refuses to run when `api_key` is empty — register at
    /// <https://www.weatherapi.com/> for a free key.
    pub weather: WeatherConfig,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            llm_max_tool_rounds: 4,
            web_fetch: WebFetchConfig::default(),
            weather: WeatherConfig::default(),
        }
    }
}

impl AgentConfig {
    fn from_env() -> Result<Self, ConfigError> {
        let defaults = Self::default();
        Ok(Self {
            enabled: parse_env("AGENTS_ENABLED", defaults.enabled)?,
            llm_max_tool_rounds: parse_env("LLM_MAX_TOOL_ROUNDS", defaults.llm_max_tool_rounds)?
                .clamp(1, 32),
            web_fetch: WebFetchConfig::from_env()?,
            weather: WeatherConfig::from_env()?,
        })
    }
}

/// Sandbox and transfer knobs for the `web_fetch` agent.
#[derive(Debug, Clone)]
pub struct WebFetchConfig {
    /// When `true`, the agent is allowed to connect to public IP
    /// ranges. Loopback and private (RFC1918 / ULA) addresses are
    /// still blocked as SSRF protection. Default: `false`.
    pub allow_public: bool,
    /// Hostname allow-list (suffix match, case-insensitive). When
    /// non-empty, takes precedence over `allow_public` and only the
    /// listed hosts (or their subdomains, for `*.foo` entries) may be
    /// fetched. Default: empty.
    pub allowlist: Vec<String>,
    /// Maximum number of response bytes the agent will read. Hard cap
    /// so a misbehaving server cannot exhaust memory.
    pub max_bytes: usize,
    /// Per-request connect+read timeout, in milliseconds.
    pub timeout_ms: u64,
}

impl Default for WebFetchConfig {
    fn default() -> Self {
        Self {
            allow_public: false,
            allowlist: Vec::new(),
            max_bytes: 2 * 1024 * 1024,
            timeout_ms: 30_000,
        }
    }
}

impl WebFetchConfig {
    fn from_env() -> Result<Self, ConfigError> {
        let defaults = Self::default();
        Ok(Self {
            allow_public: parse_env("WEB_FETCH_ALLOW_PUBLIC", defaults.allow_public)?,
            allowlist: std::env::var("WEB_FETCH_ALLOWLIST")
                .ok()
                .map(|s| {
                    s.split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect()
                })
                .unwrap_or_default(),
            max_bytes: parse_env("WEB_FETCH_MAX_BYTES", defaults.max_bytes)?,
            timeout_ms: parse_env("WEB_FETCH_TIMEOUT_MS", defaults.timeout_ms)?,
        })
    }
}

/// WeatherAPI.com credentials for the `get_weather` agent.
///
/// The free WeatherAPI.com tier covers 1M calls/month and returns
/// current conditions + 14-day forecast + 24h hourly + history +
/// astronomy (sunrise/sunset, moon phase) for any location. The
/// agent is hard-failed when `api_key` is empty: an unauthenticated
/// user gets a clear error pointing at the signup page rather than
/// a confusing 401 from the upstream.
#[derive(Debug, Clone)]
pub struct WeatherConfig {
    /// WeatherAPI.com API key. Register at
    /// <https://www.weatherapi.com/> for a free key.
    pub api_key: String,
    /// Per-request connect+read timeout, in milliseconds.
    pub timeout_ms: u64,
    /// Override the upstream base URL — useful for integration
    /// tests against a loopback fixture. Defaults to the production
    /// WeatherAPI.com host.
    pub base_url: String,
}

impl Default for WeatherConfig {
    fn default() -> Self {
        Self {
            api_key: String::new(),
            timeout_ms: 8_000,
            base_url: "https://api.weatherapi.com".to_string(),
        }
    }
}

impl WeatherConfig {
    fn from_env() -> Result<Self, ConfigError> {
        let defaults = Self::default();
        Ok(Self {
            api_key: std::env::var("WEATHER_API_KEY").unwrap_or_default(),
            timeout_ms: parse_env("WEATHER_TIMEOUT_MS", defaults.timeout_ms)?,
            base_url: std::env::var("WEATHER_BASE_URL")
                .unwrap_or_else(|_| defaults.base_url.clone()),
        })
    }
}

/// Errors produced by [`Config::from_env`].
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("WHISPER_MODEL_PATH is required")]
    MissingModelPath,
    #[error("invalid BIND_ADDR: {0}")]
    InvalidBindAddr(String),
    #[error("invalid env var {0}: {1}")]
    InvalidEnv(String, String),
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
