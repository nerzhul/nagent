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

        Ok(Self {
            bind_addr,
            whisper_model_path,
            max_queue,
            session_idle_timeout,
            infer_timeout,
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
