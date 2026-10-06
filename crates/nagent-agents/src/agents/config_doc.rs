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

/// Knobs for the CalDAV agent family (plan 1790963194218).
///
/// v1 ships three agents — `caldav_list_events`,
/// `caldav_get_event`, `caldav_create_event` — and a setup-only
/// `caldav_list_calendars` HTTP endpoint. Edit / delete are
/// explicitly out of scope; the per-user credentials live in the
/// existing AES-GCM vault under the `caldav` service id
/// (`url`, `username`, `password`).
#[derive(Debug, Clone)]
pub struct CalDavAgentConfig {
    /// Per-request connect+read timeout in milliseconds.
    pub timeout_ms: u64,
    /// Hostname allow-list applied by the egress client to the
    /// CalDAV server URL and every per-user `url` value read from
    /// the vault. When non-empty, only the listed hosts (or
    /// their subdomains, for `*.foo` entries) may be reached.
    /// When empty, only loopback / private IPs pass the SSRF
    /// guard; public CalDAV servers (the common case) are
    /// rejected.
    pub allowlist: Vec<String>,
    /// Hard cap on the number of `VEVENT`s the `list_events`
    /// agent returns per call. CalDAV's `REPORT calendar-query`
    /// can return a large slice of the calendar; this caps the
    /// payload the LLM tool loop has to digest in one round.
    pub max_events: usize,
    /// Cap on the size of an iCalendar body the client will
    /// accept (bytes). Caps memory per `get_event` and
    /// `create_event` (PUT response).
    pub max_body_bytes: usize,
}

impl Default for CalDavAgentConfig {
    fn default() -> Self {
        Self {
            timeout_ms: 15_000,
            allowlist: Vec::new(),
            max_events: 250,
            max_body_bytes: 2 * 1024 * 1024,
        }
    }
}

/// Knobs for the `x_timeline` agent
///
/// v1 ships a single read-only agent that reads the calling user's
/// authenticated X home timeline (mode "Abonnements" by default,
/// "Pour Vous" on demand). The agent refreshes the access token
/// itself when X returns 401 (locked decision from the plan) and
/// caches the latest response per `(user_id, mode)` for
/// `cache_ttl_secs` so the user can ask "résume ma timeline X"
/// twice in a row without two upstream calls.
#[derive(Debug, Clone)]
pub struct XTimelineAgentConfig {
    /// Per-request connect+read timeout in milliseconds.
    pub timeout_ms: u64,
    /// LLM-callable upper bound on the number of posts returned
    /// per call (also enforced server-side before the response is
    /// shaped).
    pub max_posts: usize,
    /// Hostname allow-list applied by the egress client to the
    /// timeline URL. When non-empty, only the listed hosts (or
    /// their subdomains, for `*.foo` entries) may be reached. When
    /// empty, the default-deny SSRF policy rejects every public
    /// host.
    pub allowlist: Vec<String>,
    /// In-process cache TTL (per `(user_id, mode)` pair). `0`
    /// disables the cache entirely.
    pub cache_ttl_secs: u64,
    /// Override the upstream base URL — useful for integration
    /// tests against a loopback fixture. Defaults to the
    /// production `https://api.x.com` host.
    pub base_url: String,
}

impl Default for XTimelineAgentConfig {
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

/// Knobs for the four memory agents
/// (`memory_store` / `memory_recall` / `memory_list` /
/// `memory_forget`, plan 1791267136806).
#[derive(Debug, Clone)]
pub struct MemoryAgentConfig {
    /// Maximum number of decrypted rows the `memory_recall` agent
    /// returns in one call. The repository already caps at
    /// `nagent_db::memories::RECALL_HARD_LIMIT = 64`; this knob
    /// trims it further so the LLM only sees a digestable slice.
    pub recalled_top_k: usize,
}

impl Default for MemoryAgentConfig {
    fn default() -> Self {
        // Default mirrors the plan §1.4 "K = 10 for the
        // auto-prompt". Both the auto-prompt injection and the
        // `memory_recall` agent use this knob; the two paths
        // share the same effect because the system block is
        // decrypted at most once per turn.
        Self { recalled_top_k: 10 }
    }
}

/// Knobs for the `search_tools` meta-agent (plan 1791317253718).
///
/// Today the only knob is the `top_k` fallback. The hard cap
/// (`<= 20`) and the lower bound (`>= 1`) live on the agent
/// itself so a runaway request never gets past the argument
/// gate regardless of what the operator sets here.
#[derive(Debug, Clone)]
pub struct ToolSearchAgentConfig {
    /// Default `top_k` the agent returns when the LLM does not
    /// pass one. Mirrors the proxy's pre-selection default
    /// (`build_tools_for_round`) so the two paths ship a
    /// consistent number of tools to the model.
    pub default_top_k: usize,
}

impl Default for ToolSearchAgentConfig {
    fn default() -> Self {
        Self { default_top_k: 5 }
    }
}
