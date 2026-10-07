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
    /// access via the `DocumentSource` trait). The agent itself
    /// is auto-registered when the master `[agents].enabled`
    /// switch is on AND `[documents].enabled = true` produced a
    /// `DocumentStore` at boot — no separate
    /// `read_document_enabled` flag. The TOML field is kept
    /// around for the per-agent knobs (timeout, max chars) but
    /// the historical `read_document_enabled = true` line is
    /// silently ignored.
    pub read_document: ReadDocumentConfig,
    /// Knobs for the CalDAV plugin (plan 1790963194218). v1
    /// exposes three LLM tools (`caldav_list_events`,
    /// `caldav_get_event`, `caldav_create_event`) and a
    /// setup-only HTTP probe endpoint; the field is always
    /// parsed; the actual agents are only registered when the
    /// `caldav-agent` cargo feature is on.
    pub caldav: CalDavConfig,
    /// Knobs for the X timeline agent (plan 1790695073418). The
    /// field is always parsed; the actual agent is only
    /// registered when the `x-agent` cargo feature is on.
    pub x_timeline: XTimelineConfig,
    /// Knobs for the four `memory_*` agents (plan 1791267136806).
    /// The field is always parsed; the actual agents are only
    /// registered when the `memory-agent` cargo feature is on.
    pub memory: MemoryConfig,
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
            caldav: CalDavConfig::default(),
            x_timeline: XTimelineConfig::default(),
            memory: MemoryConfig::default(),
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
            caldav: CalDavConfig::from_env_with_toml(toml.caldav.as_ref())?,
            x_timeline: XTimelineConfig::from_env_with_toml(toml.x_timeline.as_ref())?,
            memory: MemoryConfig::from_env_with_toml(toml.memory.as_ref())?,
        })
    }
}

/// Knobs for the four `memory_*` agents (plan 1791267136806).
///
/// The `[agents.memory]` table only exposes `recalled_top_k` — a
/// single cap that bounds the number of rows the LLM ever sees in
/// one round. A larger config surface (per-call `limit`, throttling
/// window, etc.) is reserved for a follow-up plan once we have
/// usage data on the auto-prompt size.
#[derive(Debug, Clone)]
pub struct MemoryConfig {
    /// Maximum number of decrypted rows the `memory_recall` agent
    /// returns in one call. The repository already caps at
    /// `nagent_db::memories::RECALL_HARD_LIMIT = 64`; this knob
    /// trims it further so the LLM only sees a digestable slice.
    /// Env var `MEMORY_TOP_K`, TOML key
    /// `[agents.memory].recalled_top_k`. Default of `10` mirrors
    /// the plan §1.4.
    pub recalled_top_k: usize,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self { recalled_top_k: 10 }
    }
}

impl MemoryConfig {
    pub fn from_env_with_toml(
        toml: Option<&crate::config::file::TomlMemoryConfig>,
    ) -> Result<Self, ConfigError> {
        let defaults = Self::default();
        let toml = toml.cloned().unwrap_or_default();
        Ok(Self {
            recalled_top_k: resolve_primitive(
                env_opt("MEMORY_TOP_K").as_deref(),
                toml.recalled_top_k,
                defaults.recalled_top_k,
                "MEMORY_TOP_K",
            )?
            .clamp(1, 64),
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

/// Knobs for the CalDAV plugin (plan 1790963194218).
///
/// v1 ships three LLM tools — `caldav_list_events`,
/// `caldav_get_event`, `caldav_create_event` — and a
/// setup-only `caldav_list_calendars` HTTP endpoint. The
/// `allowlist` is mandatory for any deployment that talks to
/// a public CalDAV server: when empty, the default-deny SSRF
/// policy of the egress layer rejects every public host.
#[derive(Debug, Clone)]
pub struct CalDavConfig {
    /// Per-request connect+read timeout in milliseconds.
    pub timeout_ms: u64,
    /// Hostname allow-list (suffix match, case-insensitive).
    /// When non-empty, only the listed hosts (or their
    /// subdomains, for `*.foo` entries) may be reached.
    pub allowlist: Vec<String>,
    /// Hard cap on the number of events `caldav_list_events`
    /// returns per call.
    pub max_events: usize,
    /// Cap on the iCalendar body the client will accept
    /// (bytes). Caps memory per `caldav_get_event` /
    /// `caldav_create_event` round-trip.
    pub max_body_bytes: usize,
}

impl Default for CalDavConfig {
    fn default() -> Self {
        Self {
            timeout_ms: 15_000,
            allowlist: Vec::new(),
            max_events: 250,
            max_body_bytes: 2 * 1024 * 1024,
        }
    }
}

impl CalDavConfig {
    pub fn from_env_with_toml(
        toml: Option<&crate::config::file::TomlCalDavConfig>,
    ) -> Result<Self, ConfigError> {
        let defaults = Self::default();
        let toml = toml.cloned().unwrap_or_default();
        Ok(Self {
            timeout_ms: resolve_primitive(
                env_opt("CALDAV_TIMEOUT_MS").as_deref(),
                toml.timeout_ms,
                defaults.timeout_ms,
                "CALDAV_TIMEOUT_MS",
            )?,
            allowlist: resolve_csv(
                env_opt("CALDAV_ALLOWLIST").as_deref(),
                toml.allowlist,
                defaults.allowlist,
            ),
            max_events: resolve_primitive(
                env_opt("CALDAV_MAX_EVENTS").as_deref(),
                toml.max_events,
                defaults.max_events,
                "CALDAV_MAX_EVENTS",
            )?,
            max_body_bytes: resolve_primitive(
                env_opt("CALDAV_MAX_BODY_BYTES").as_deref(),
                toml.max_body_bytes,
                defaults.max_body_bytes,
                "CALDAV_MAX_BODY_BYTES",
            )?,
        })
    }
}

/// Knobs for the X timeline agent (plan 1790695073418). Mirrors
/// the shape of the agents crate's `XTimelineAgentConfig`; the
/// `From` impl in `crate::agents` converts one to the other.
#[derive(Debug, Clone)]
pub struct XTimelineConfig {
    /// Per-request connect+read timeout in milliseconds.
    pub timeout_ms: u64,
    /// LLM-callable upper bound on the number of posts returned
    /// per call.
    pub max_posts: usize,
    /// Hostname allow-list applied to the timeline URL. Default
    /// `["api.x.com", "x.com"]`.
    pub allowlist: Vec<String>,
    /// In-process cache TTL (per `(user_id, mode)` pair). `0`
    /// disables the cache entirely.
    pub cache_ttl_secs: u64,
    /// Override the upstream base URL — useful for integration
    /// tests against a loopback fixture. Defaults to
    /// `https://api.x.com`.
    pub base_url: String,
}

impl Default for XTimelineConfig {
    fn default() -> Self {
        Self {
            timeout_ms: 8_000,
            max_posts: 20,
            allowlist: vec!["api.x.com".to_string(), "x.com".to_string()],
            cache_ttl_secs: 60,
            base_url: "https://api.x.com".to_string(),
        }
    }
}

impl XTimelineConfig {
    pub fn from_env_with_toml(
        toml: Option<&crate::config::file::TomlXTimelineConfig>,
    ) -> Result<Self, ConfigError> {
        let defaults = Self::default();
        let toml = toml.cloned().unwrap_or_default();
        Ok(Self {
            timeout_ms: resolve_primitive(
                env_opt("X_TIMELINE_TIMEOUT_MS").as_deref(),
                toml.timeout_ms,
                defaults.timeout_ms,
                "X_TIMELINE_TIMEOUT_MS",
            )?,
            max_posts: resolve_primitive(
                env_opt("X_TIMELINE_MAX_POSTS").as_deref(),
                toml.max_posts,
                defaults.max_posts,
                "X_TIMELINE_MAX_POSTS",
            )?,
            allowlist: resolve_csv(
                env_opt("X_TIMELINE_ALLOWLIST").as_deref(),
                toml.allowlist.clone(),
                defaults.allowlist.clone(),
            ),
            cache_ttl_secs: resolve_primitive(
                env_opt("X_TIMELINE_CACHE_TTL_SECS").as_deref(),
                toml.cache_ttl_secs,
                defaults.cache_ttl_secs,
                "X_TIMELINE_CACHE_TTL_SECS",
            )?,
            base_url: resolve_opt_string(
                env_opt("X_TIMELINE_BASE_URL").as_deref(),
                toml.base_url.as_deref(),
            )
            .unwrap_or_else(|| defaults.base_url.clone()),
        })
    }
}
