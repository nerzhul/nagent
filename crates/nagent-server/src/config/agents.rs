//! `agents` — `[agents]` section + per-agent sub-configs.
//!
//! Each per-agent sub-config (web fetch, weather, etc.) is a sibling
//! struct; the static factory table that turns a `&AgentConfig` into a
//! concrete agent instance lives in [`crate::agents::registry`].

use crate::config::file::TomlAgentConfig;
use crate::config::{env_opt, resolve_csv, resolve_opt_string, resolve_primitive, ConfigError};

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
    /// Sandbox + transfer knobs for the built-in `web_fetch` agent.
    /// Always parsed; the agent is only registered when the `web-agent`
    /// cargo feature is on AND `enabled` is true.
    pub web_fetch: WebFetchConfig,
    /// WeatherAPI.com credentials for the `get_weather` agent. The
    /// agent refuses to run when `api_key` is empty — register at
    /// <https://www.weatherapi.com/> for a free key.
    pub weather: WeatherConfig,
    /// Knobs for the `unit_convert` agent. Empty by default (the agent
    /// has no API key and a sensible default timeout/base URL).
    pub unit_convert: UnitConvertConfig,
    /// Knobs for the `wikipedia` agent. Empty by default (no API key,
    /// only a `User-Agent` header is required by Wikimedia).
    pub wikipedia: WikipediaConfig,
    /// Knobs for the `dictionary` agent (no API key — anonymous
    /// Free Dictionary API).
    pub dictionary: DictionaryConfig,
    /// Knobs for the `get_stock` agent (no API key — anonymous
    /// Yahoo Finance fallback).
    pub stock: StockConfig,
    /// Knobs for the `read_document` agent (server-bound doc
    /// access via the `DocumentSource` trait).
    pub read_document: ReadDocumentConfig,
    /// Whether the `read_document` agent is wired in. Mirrors the
    /// historical `from_config_with_documents` guard so enabling
    /// docs alone (without a registered document store) does not
    /// silently drop the agent.
    pub read_document_enabled: bool,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            web_fetch: WebFetchConfig::default(),
            weather: WeatherConfig::default(),
            unit_convert: UnitConvertConfig::default(),
            wikipedia: WikipediaConfig::default(),
            dictionary: DictionaryConfig::default(),
            stock: StockConfig::default(),
            read_document: ReadDocumentConfig::default(),
            read_document_enabled: false,
        }
    }
}

impl AgentConfig {
    pub fn from_env_with_toml(toml: Option<&TomlAgentConfig>) -> Result<Self, ConfigError> {
        let defaults = Self::default();
        let toml = toml.cloned().unwrap_or_default();
        Ok(Self {
            enabled: resolve_primitive(
                env_opt("AGENTS_ENABLED").as_deref(),
                toml.enabled,
                defaults.enabled,
                "AGENTS_ENABLED",
            )?,
            web_fetch: WebFetchConfig::from_env_with_toml(toml.web_fetch.as_ref())?,
            weather: WeatherConfig::from_env_with_toml(toml.get_weather.as_ref())?,
            unit_convert: UnitConvertConfig::from_env_with_toml(toml.unit_convert.as_ref())?,
            wikipedia: WikipediaConfig::from_env_with_toml(toml.wikipedia.as_ref())?,
            dictionary: DictionaryConfig::from_env_with_toml(toml.dictionary.as_ref())?,
            stock: StockConfig::from_env_with_toml(toml.stock.as_ref())?,
            read_document: ReadDocumentConfig::from_env_with_toml(toml.read_document.as_ref())?,
            read_document_enabled: resolve_primitive(
                env_opt("READ_DOCUMENT_AGENT_ENABLED").as_deref(),
                toml.read_document_enabled,
                defaults.read_document_enabled,
                "READ_DOCUMENT_AGENT_ENABLED",
            )?,
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
    pub fn from_env_with_toml(
        toml: Option<&crate::config::file::TomlWebFetchConfig>,
    ) -> Result<Self, ConfigError> {
        let defaults = Self::default();
        let toml = toml.cloned().unwrap_or_default();
        Ok(Self {
            allow_public: resolve_primitive(
                env_opt("WEB_FETCH_ALLOW_PUBLIC").as_deref(),
                toml.allow_public,
                defaults.allow_public,
                "WEB_FETCH_ALLOW_PUBLIC",
            )?,
            allowlist: resolve_csv(
                env_opt("WEB_FETCH_ALLOWLIST").as_deref(),
                toml.allowlist,
                defaults.allowlist,
            ),
            max_bytes: resolve_primitive(
                env_opt("WEB_FETCH_MAX_BYTES").as_deref(),
                toml.max_bytes,
                defaults.max_bytes,
                "WEB_FETCH_MAX_BYTES",
            )?,
            timeout_ms: resolve_primitive(
                env_opt("WEB_FETCH_TIMEOUT_MS").as_deref(),
                toml.timeout_ms,
                defaults.timeout_ms,
                "WEB_FETCH_TIMEOUT_MS",
            )?,
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
    pub fn from_env_with_toml(
        toml: Option<&crate::config::file::TomlWeatherConfig>,
    ) -> Result<Self, ConfigError> {
        let defaults = Self::default();
        let toml = toml.cloned().unwrap_or_default();
        Ok(Self {
            api_key: resolve_opt_string(
                env_opt("WEATHER_API_KEY").as_deref(),
                toml.api_key.as_deref(),
            )
            .unwrap_or_default(),
            timeout_ms: resolve_primitive(
                env_opt("WEATHER_TIMEOUT_MS").as_deref(),
                toml.timeout_ms,
                defaults.timeout_ms,
                "WEATHER_TIMEOUT_MS",
            )?,
            base_url: resolve_opt_string(
                env_opt("WEATHER_BASE_URL").as_deref(),
                toml.base_url.as_deref(),
            )
            .unwrap_or_else(|| defaults.base_url.clone()),
        })
    }
}

/// Knobs for the `unit_convert` agent (pure local — no API key).
#[derive(Debug, Clone)]
pub struct UnitConvertConfig {
    /// Per-request timeout in milliseconds. Defaults to 5 s, which is
    /// generous for a local table lookup; the knob exists primarily so
    /// integration tests can drop it when calling the agent
    /// synchronously from the LLM proxy loop.
    pub timeout_ms: u64,
}

impl Default for UnitConvertConfig {
    fn default() -> Self {
        Self { timeout_ms: 5_000 }
    }
}

impl UnitConvertConfig {
    pub fn from_env_with_toml(
        toml: Option<&crate::config::file::TomlUnitConvertConfig>,
    ) -> Result<Self, ConfigError> {
        let defaults = Self::default();
        let toml = toml.cloned().unwrap_or_default();
        Ok(Self {
            timeout_ms: resolve_primitive(
                env_opt("UNIT_CONVERT_TIMEOUT_MS").as_deref(),
                toml.timeout_ms,
                defaults.timeout_ms,
                "UNIT_CONVERT_TIMEOUT_MS",
            )?,
        })
    }
}

/// Knobs for the `wikipedia` agent.
///
/// Wikipedia's REST API is anonymous (no API key) and unlimited for
/// reasonable use, but Wikimedia's policy requires a `User-Agent`
/// header identifying the client. The agent sets one unconditionally;
/// `user_agent` is exposed here so operators can customise the
/// contact string (e.g. add their own contact URL).
#[derive(Debug, Clone)]
pub struct WikipediaConfig {
    /// Per-request timeout in milliseconds. Defaults to 5 s.
    pub timeout_ms: u64,
    /// Override the upstream base URL. Defaults to the canonical
    /// `https://en.wikipedia.org/api/rest_v1`. Useful for tests
    /// pointing at a loopback fixture.
    pub base_url: String,
    /// `User-Agent` sent on every request. Wikimedia rejects clients
    /// without a `User-Agent`, and de-prioritises generic ones — keep
    /// this descriptive and add a contact URL.
    pub user_agent: String,
}

impl Default for WikipediaConfig {
    fn default() -> Self {
        Self {
            timeout_ms: 5_000,
            base_url: "https://en.wikipedia.org/api/rest_v1".to_string(),
            user_agent: format!("nagent-wikipedia-agent/{}", env!("CARGO_PKG_VERSION")),
        }
    }
}

impl WikipediaConfig {
    pub fn from_env_with_toml(
        toml: Option<&crate::config::file::TomlWikipediaConfig>,
    ) -> Result<Self, ConfigError> {
        let defaults = Self::default();
        let toml = toml.cloned().unwrap_or_default();
        Ok(Self {
            timeout_ms: resolve_primitive(
                env_opt("WIKIPEDIA_TIMEOUT_MS").as_deref(),
                toml.timeout_ms,
                defaults.timeout_ms,
                "WIKIPEDIA_TIMEOUT_MS",
            )?,
            base_url: resolve_opt_string(
                env_opt("WIKIPEDIA_BASE_URL").as_deref(),
                toml.base_url.as_deref(),
            )
            .unwrap_or_else(|| defaults.base_url.clone()),
            user_agent: resolve_opt_string(
                env_opt("WIKIPEDIA_USER_AGENT").as_deref(),
                toml.user_agent.as_deref(),
            )
            .unwrap_or_else(|| defaults.user_agent.clone()),
        })
    }
}

/// Knobs for the `dictionary` agent.
///
/// The Free Dictionary API (<https://api.dictionaryapi.dev/>) is
/// anonymous (no API key required) and returns definitions,
/// phonetics, examples, and synonyms for English words. The agent
/// calls it over plain HTTPS; no `User-Agent` policy to honour, no
/// rate-limit beyond the published fair-use cap.
#[derive(Debug, Clone)]
pub struct DictionaryConfig {
    /// Per-request timeout in milliseconds. Defaults to 5 s.
    pub timeout_ms: u64,
    /// Override the upstream base URL. Defaults to the canonical
    /// `https://api.dictionaryapi.dev/api/v2`. Useful for tests
    /// pointing at a loopback fixture.
    pub base_url: String,
}

impl Default for DictionaryConfig {
    fn default() -> Self {
        Self {
            timeout_ms: 5_000,
            base_url: "https://api.dictionaryapi.dev/api/v2".to_string(),
        }
    }
}

impl DictionaryConfig {
    pub fn from_env_with_toml(
        toml: Option<&crate::config::file::TomlDictionaryConfig>,
    ) -> Result<Self, ConfigError> {
        let defaults = Self::default();
        let toml = toml.cloned().unwrap_or_default();
        Ok(Self {
            timeout_ms: resolve_primitive(
                env_opt("DICTIONARY_TIMEOUT_MS").as_deref(),
                toml.timeout_ms,
                defaults.timeout_ms,
                "DICTIONARY_TIMEOUT_MS",
            )?,
            base_url: resolve_opt_string(
                env_opt("DICTIONARY_BASE_URL").as_deref(),
                toml.base_url.as_deref(),
            )
            .unwrap_or_else(|| defaults.base_url.clone()),
        })
    }
}

/// Knobs for the `get_stock` agent. No external API key — anonymous
/// Yahoo Finance fallback.
#[derive(Debug, Clone)]
pub struct StockConfig {
    /// Per-request timeout in milliseconds.
    pub timeout_ms: u64,
}

impl Default for StockConfig {
    fn default() -> Self {
        Self { timeout_ms: 8_000 }
    }
}

impl StockConfig {
    pub fn from_env_with_toml(
        toml: Option<&crate::config::file::TomlStockConfig>,
    ) -> Result<Self, ConfigError> {
        let defaults = Self::default();
        let toml = toml.cloned().unwrap_or_default();
        Ok(Self {
            timeout_ms: resolve_primitive(
                env_opt("STOCK_TIMEOUT_MS").as_deref(),
                toml.timeout_ms,
                defaults.timeout_ms,
                "STOCK_TIMEOUT_MS",
            )?,
        })
    }
}

/// Knobs for the `read_document` agent (server-bound doc access via
/// the `DocumentSource` trait).
#[derive(Debug, Clone)]
pub struct ReadDocumentConfig {
    /// Per-request timeout in milliseconds for the underlying
    /// document fetch.
    pub timeout_ms: u64,
    /// Maximum characters the agent returns to the LLM per call.
    pub max_extracted_chars: usize,
}

impl Default for ReadDocumentConfig {
    fn default() -> Self {
        Self {
            timeout_ms: 5_000,
            max_extracted_chars: 100_000,
        }
    }
}

impl ReadDocumentConfig {
    pub fn from_env_with_toml(
        toml: Option<&crate::config::file::TomlReadDocumentConfig>,
    ) -> Result<Self, ConfigError> {
        let defaults = Self::default();
        let toml = toml.cloned().unwrap_or_default();
        Ok(Self {
            timeout_ms: resolve_primitive(
                env_opt("READ_DOCUMENT_TIMEOUT_MS").as_deref(),
                toml.timeout_ms,
                defaults.timeout_ms,
                "READ_DOCUMENT_TIMEOUT_MS",
            )?,
            max_extracted_chars: resolve_primitive(
                env_opt("READ_DOCUMENT_MAX_CHARS").as_deref(),
                toml.max_extracted_chars,
                defaults.max_extracted_chars,
                "READ_DOCUMENT_MAX_CHARS",
            )?,
        })
    }
}
