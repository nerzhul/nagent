//! Plain per-agent `*Config` structs.
//!
//! Each config struct is "plain" — only a `Default` impl and the
//! fields the agent reads. TOML / env parsing lives in
//! `nagent-server::config` because the toml-mirror structs
//! (`TomlWebFetchConfig`, …) are part of the server's
//! `ConfigFile`/`Config` surface, not of this crate.
//!
//! A new agent writes its config here (one struct, one Default impl);
//! the server's `config::agents.rs` adds the matching TOML mirror +
//! `from_env_with_toml` parser.

// Re-exported at crate root.
#![allow(dead_code)]

/// Knobs for the `web_fetch` agent.
#[derive(Debug, Clone)]
pub struct WebFetchAgentConfig {
    /// When `true`, the agent is allowed to connect to public IP
    /// ranges. Loopback and private (RFC1918 / ULA) addresses are
    /// still blocked as SSRF protection. Default: `false`.
    pub allow_public: bool,
    /// Hostname allow-list (suffix match, case-insensitive). When
    /// non-empty, takes precedence over `allow_public` and only the
    /// listed hosts (or their subdomains, for `*.foo` entries) may
    /// be fetched. Default: empty.
    pub allowlist: Vec<String>,
    /// Maximum number of response bytes the agent will read. Hard cap
    /// so a misbehaving server cannot exhaust memory.
    pub max_bytes: usize,
    /// Per-request connect+read timeout, in milliseconds.
    pub timeout_ms: u64,
}

impl Default for WebFetchAgentConfig {
    fn default() -> Self {
        Self {
            allow_public: false,
            allowlist: Vec::new(),
            max_bytes: 2 * 1024 * 1024,
            timeout_ms: 30_000,
        }
    }
}

/// Knobs for the `get_weather` agent (WeatherAPI.com).
#[derive(Debug, Clone)]
pub struct WeatherAgentConfig {
    /// WeatherAPI.com API key. The agent refuses to run when empty.
    pub api_key: String,
    /// Per-request connect+read timeout, in milliseconds.
    pub timeout_ms: u64,
    /// Override the upstream base URL — useful for integration
    /// tests against a loopback fixture. Defaults to the production
    /// WeatherAPI.com host.
    pub base_url: String,
}

impl Default for WeatherAgentConfig {
    fn default() -> Self {
        Self {
            api_key: String::new(),
            timeout_ms: 8_000,
            base_url: "https://api.weatherapi.com".to_string(),
        }
    }
}

/// Knobs for the `unit_convert` agent (pure local — no API key).
#[derive(Debug, Clone)]
pub struct UnitConvertAgentConfig {
    /// Per-request timeout in milliseconds.
    pub timeout_ms: u64,
}

impl Default for UnitConvertAgentConfig {
    fn default() -> Self {
        Self { timeout_ms: 5_000 }
    }
}

/// Knobs for the `wikipedia` agent.
#[derive(Debug, Clone)]
pub struct WikipediaAgentConfig {
    /// Per-request timeout in milliseconds. Defaults to 5 s.
    pub timeout_ms: u64,
    /// Override the upstream base URL.
    pub base_url: String,
    /// `User-Agent` sent on every request. Wikimedia rejects
    /// clients without a descriptive `User-Agent`.
    pub user_agent: String,
}

impl Default for WikipediaAgentConfig {
    fn default() -> Self {
        Self {
            timeout_ms: 5_000,
            base_url: "https://en.wikipedia.org/api/rest_v1".to_string(),
            user_agent: format!("nagent-wikipedia-agent/{}", env!("CARGO_PKG_VERSION")),
        }
    }
}

/// Knobs for the `dictionary` agent (anonymous Free Dictionary API).
#[derive(Debug, Clone)]
pub struct DictionaryAgentConfig {
    /// Per-request timeout in milliseconds. Defaults to 5 s.
    pub timeout_ms: u64,
    /// Override the upstream base URL.
    pub base_url: String,
}

impl Default for DictionaryAgentConfig {
    fn default() -> Self {
        Self {
            timeout_ms: 5_000,
            base_url: "https://api.dictionaryapi.dev/api/v2".to_string(),
        }
    }
}

/// Knobs for the `get_stock` agent. No external API key — anonymous
/// Yahoo Finance fallback.
#[derive(Debug, Clone)]
pub struct StockAgentConfig {
    /// Per-request timeout in milliseconds.
    pub timeout_ms: u64,
}

impl Default for StockAgentConfig {
    fn default() -> Self {
        Self { timeout_ms: 8_000 }
    }
}

/// Knobs for the `calculate` agent (pure local — `meval`).
#[derive(Debug, Clone)]
pub struct CalculateAgentConfig {
    /// Per-request timeout in milliseconds.
    pub timeout_ms: u64,
}

impl Default for CalculateAgentConfig {
    fn default() -> Self {
        Self { timeout_ms: 3_000 }
    }
}

/// Knobs for the `get_datetime` agent (pure local — `chrono-tz`).
#[derive(Debug, Clone)]
pub struct DateTimeAgentConfig {
    /// Per-request timeout in milliseconds.
    pub timeout_ms: u64,
}

impl Default for DateTimeAgentConfig {
    fn default() -> Self {
        Self { timeout_ms: 3_000 }
    }
}

/// Knobs for the `read_document` agent.
#[derive(Debug, Clone)]
pub struct ReadDocumentAgentConfig {
    /// Per-request timeout in milliseconds for the underlying
    /// document fetch.
    pub timeout_ms: u64,
    /// Maximum characters the agent returns to the LLM per call.
    pub max_extracted_chars: usize,
}

impl Default for ReadDocumentAgentConfig {
    fn default() -> Self {
        Self {
            timeout_ms: 5_000,
            max_extracted_chars: 100_000,
        }
    }
}
