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

        let llm = LlmConfig::from_env()?;

        Ok(Self {
            bind_addr,
            whisper_model_path,
            max_queue,
            session_idle_timeout,
            infer_timeout,
            llm,
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

        Ok(Self {
            enabled,
            base_url,
            default_model,
            api_key,
            request_timeout,
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
