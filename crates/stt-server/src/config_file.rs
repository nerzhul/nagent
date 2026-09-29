//! Optional TOML configuration file.
//!
//! Loaded only when the operator passes `--config <path>` on the
//! command line. Every field is `Option<T>` so a partial file is
//! allowed and unknown fields are rejected up-front (`deny_unknown_fields`)
//! — typos in section / key names should never silently fall back to the
//! default.
//!
//! ## Precedence
//!
//! For every knob, the resolved value follows this priority order
//! (highest wins):
//!
//! 1. Environment variable (or `.env` value — same precedence).
//! 2. TOML file value (when `--config` is used and the key is present).
//! 3. Hardcoded default.
//!
//! Env vars winning over TOML is the intentional choice: it keeps the
//! 12-factor contract working (container `ConfigMap` and `.env` still
//! override file content), so the TOML file becomes the per-deployment
//! defaults file rather than a hard lock-in.
//!
//! ## Schema
//!
//! ```toml
//! [server]
//! bind_addr = "0.0.0.0:8080"
//! whisper_model_path = "/models/ggml-base.bin"
//! max_queue = 32
//! session_idle_timeout_ms = 30_000
//! infer_timeout_ms = 30_000
//!
//! # WebSocket frame limits
//! [server.limits]
//! max_audio_frame_samples = 480_000
//! required_sample_rate = 16_000
//! max_language_hint_bytes = 16
//!
//! # Rate limits (per source IP)
//! [server.rate_limits]
//! stt_per_min = 120
//! llm_per_min = 30
//!
//! # LLM proxy (Ollama or any OpenAI-compatible upstream)
//! [llm]
//! enabled = false
//! base_url = "http://localhost:11434"
//! default_model = "llama3.1"
//! api_key = ""
//! request_timeout_secs = 120
//! cors_allow_origins = []
//! # system_prompt = "You are a strict, concise assistant."    # see LLM_SYSTEM_PROMPT
//!
//! # Chat agents — top-level flags + per-tool sub-sections
//! [agents]
//! enabled = true
//! llm_max_tool_rounds = 4
//!
//! [agents.web_fetch]
//! allow_public = false
//! allowlist = ["example.com", "*.wikipedia.org"]
//! max_bytes = 2_097_152
//! timeout_ms = 30_000
//!
//! [agents.get_weather]
//! api_key = ""
//! timeout_ms = 8_000
//! base_url = "https://api.weatherapi.com"
//!
//! # Text-to-speech (Piper ONNX, local). See README and
//! # scripts/download-piper-voices.sh for voice acquisition.
//! [tts]
//! enabled = false
//! model_dir = "./models/piper"
//! voice_en = "en_US-lessac-medium"
//! voice_fr = "fr_FR-upmc-medium"
//! default_lang = "en"
//! length_scale = 1.0
//! noise_scale = 0.667
//! noise_w = 0.8
//! max_input_chars = 2000
//!
//! # Authentication (PR1). When `enabled = false` (the default) the
//! # server keeps the pre-PR1 single-user trust boundary: no login
//! # routes, no /api/me, no RequireAuth layer. Turning this on REQUIRES
//! # `[auth.db]` to point at a reachable SQL store (sqlite or postgres);
//! # the server refuses to boot otherwise. See README "Authentication"
//! # for the bootstrap walkthrough.
//! [auth]
//! enabled = false
//! backends = ["local"]                       # subset of: local, oidc, passkey
//! public_url = "https://nagent.example.com"  # scheme + host (no trailing slash)
//! session_ttl_days = 7                       # absolute, NOT sliding; range 1..=90
//! csrf_header = "x-csrf-token"
//!
//! [auth.db]
//! backend = "sqlite"                         # "sqlite" | "postgres"
//! url = "sqlite://./data/auth.db?mode=rwc"
//! max_connections = 16
//!
//! [auth.password]
//! argon2_memory_kib = 19456                  # OWASP 2025 default
//! argon2_iterations = 2
//! argon2_parallelism = 1
//! min_password_length = 8
//! allow_registration = true                  # logged-in users can create accounts
//!
//! [auth.oidc]
//! auto_provision = true
//! issuer = "https://keycloak.example.com/realms/nagent"
//! client_id = "nagent"
//! client_secret = ""                         # env: NAGENT_AUTH_OIDC_CLIENT_SECRET
//! scopes = ["openid", "email", "profile"]
//! required_groups = []
//! role_claim = "groups"
//!
//! [auth.passkey]
//! self_registration = true
//! rp_id = "nagent.example.com"
//! rp_name = "nagent"
//! origins = ["https://nagent.example.com"]
//! ```

use std::path::Path;

use serde::Deserialize;

/// Root of the optional TOML configuration file.
///
/// Server-side knobs live under `[server]` (with sub-tables
/// `[server.limits]` and `[server.rate_limits]`); cross-cutting
/// concerns stay at the top level (`[llm]`, `[agents]`). Every field
/// is `Option<T>` so partial files are accepted. Unknown fields are
/// rejected via `deny_unknown_fields` so a typo never silently reverts
/// to a default.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TomlConfig {
    #[serde(default)]
    pub server: Option<TomlServerConfig>,
    #[serde(default)]
    pub llm: Option<TomlLlmConfig>,
    /// Per-tool agent configuration. The `[agents]` table holds the
    /// master switches (`enabled`, `llm_max_tool_rounds`) and one
    /// sub-table per tool (`[agents.web_fetch]`, `[agents.get_weather]`).
    #[serde(default)]
    pub agents: Option<TomlAgentConfig>,
    /// Local Piper TTS engine knobs. See [`crate::config::TtsConfig`].
    #[serde(default)]
    pub tts: Option<TomlTtsConfig>,
    /// Authentication & user-identity knobs. See [`TomlAuthConfig`]
    /// and [`crate::config::AuthConfig`]. Compiled only when the
    /// `auth` cargo feature is enabled; the field is gated so a
    /// build without `auth` does not even parse the section.
    #[serde(default)]
    pub auth: Option<TomlAuthConfig>,
}

/// Server-side knobs grouped under `[server]`.
///
/// Includes the bind address, model path, queue sizes, timeouts, and
/// the two nested sub-tables for inbound WS frame limits
/// (`[server.limits]`) and per-IP rate limits (`[server.rate_limits]`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TomlServerConfig {
    pub bind_addr: Option<String>,
    pub whisper_model_path: Option<String>,
    pub max_queue: Option<usize>,
    pub session_idle_timeout_ms: Option<u64>,
    pub infer_timeout_ms: Option<u64>,
    /// Operator override for the inference worker pool size. When unset,
    /// the server picks a default from the backend's [`stt_core::BackendInfo`].
    pub inference_workers: Option<usize>,
    #[serde(default)]
    pub limits: Option<TomlLimitsConfig>,
    #[serde(default)]
    pub rate_limits: Option<TomlRateLimitConfig>,
}

/// LLM proxy knobs. Mirrors [`crate::config::LlmConfig`].
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TomlLlmConfig {
    pub enabled: Option<bool>,
    pub base_url: Option<String>,
    pub default_model: Option<String>,
    pub api_key: Option<String>,
    /// Bearer key required on inbound `/v1/*` requests when
    /// `auth_mode = "bearer"`. Mirrors the `LLM_API_KEY` env var; env
    /// wins when both are set. Has no effect when `auth_mode` is
    /// `forward` or `disabled`.
    pub inbound_auth_key: Option<String>,
    /// Inbound auth mode (`bearer`, `forward`, `disabled`). Mirrors
    /// the `LLM_AUTH_MODE` env var; env wins when both are set.
    pub auth_mode: Option<String>,
    pub request_timeout_secs: Option<u64>,
    pub cors_allow_origins: Option<Vec<String>>,
    /// Server-default system prompt prepended to every
    /// `/v1/chat/completions` request. Mirrors the `LLM_SYSTEM_PROMPT`
    /// env var; env wins when both are set.
    pub system_prompt: Option<String>,
    /// Whether the browser is allowed to forward the user's
    /// approximate geolocation to the LLM. Mirrors the
    /// `LLM_ALLOW_USER_LOCATION` env var; env wins when both are set.
    /// Defaults to `true`.
    pub allow_user_location: Option<bool>,
    /// Whether the browser is allowed to forward the user's IANA
    /// timezone to the LLM. Mirrors the `LLM_ALLOW_USER_TIMEZONE`
    /// env var; env wins when both are set. Defaults to `true`.
    /// Independent from `allow_user_location`: an operator may
    /// forbid one without touching the other.
    pub allow_user_timezone: Option<bool>,
}

/// Agent master switches + per-tool sub-tables.
///
/// The tool sub-tables are keyed by the agent's `name()` (e.g.
/// `web_fetch`, `get_weather`) so the TOML schema matches what the LLM
/// sees on the wire — there's no extra mental mapping between the
/// configuration file and the registry.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TomlAgentConfig {
    pub enabled: Option<bool>,
    pub llm_max_tool_rounds: Option<u32>,
    #[serde(default)]
    pub web_fetch: Option<TomlWebFetchConfig>,
    #[serde(default)]
    pub get_weather: Option<TomlWeatherConfig>,
    #[serde(default)]
    pub unit_convert: Option<TomlUnitConvertConfig>,
    #[serde(default)]
    pub wikipedia: Option<TomlWikipediaConfig>,
    #[serde(default)]
    pub dictionary: Option<TomlDictionaryConfig>,
}

/// Sandbox and transfer knobs for the built-in `web_fetch` tool.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TomlWebFetchConfig {
    pub allow_public: Option<bool>,
    pub allowlist: Option<Vec<String>>,
    pub max_bytes: Option<usize>,
    pub timeout_ms: Option<u64>,
}

/// WeatherAPI.com credentials for the `get_weather` tool.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TomlWeatherConfig {
    pub api_key: Option<String>,
    pub timeout_ms: Option<u64>,
    pub base_url: Option<String>,
}

/// Knobs for the `unit_convert` tool (no API key). Mirrors
/// [`crate::config::UnitConvertConfig`].
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TomlUnitConvertConfig {
    pub timeout_ms: Option<u64>,
}

/// Knobs for the `wikipedia` tool (no API key, only `User-Agent`).
/// Mirrors [`crate::config::WikipediaConfig`].
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TomlWikipediaConfig {
    pub timeout_ms: Option<u64>,
    pub base_url: Option<String>,
    pub user_agent: Option<String>,
}

/// Knobs for the `dictionary` tool (no API key). Mirrors
/// [`crate::config::DictionaryConfig`].
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TomlDictionaryConfig {
    pub timeout_ms: Option<u64>,
    pub base_url: Option<String>,
}

/// WebSocket frame-limit knobs. Mirrors [`crate::config::LimitsConfig`].
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TomlLimitsConfig {
    pub max_audio_frame_samples: Option<usize>,
    pub required_sample_rate: Option<u32>,
    pub max_language_hint_bytes: Option<usize>,
}

/// Per-IP rate-limit knobs. Mirrors [`crate::config::RateLimitConfig`].
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TomlRateLimitConfig {
    pub stt_per_min: Option<u32>,
    pub llm_per_min: Option<u32>,
}

/// Piper TTS engine knobs. Mirrors [`crate::config::TtsConfig`].
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TomlTtsConfig {
    pub enabled: Option<bool>,
    pub model_dir: Option<String>,
    pub voice_en: Option<String>,
    pub voice_fr: Option<String>,
    pub default_lang: Option<String>,
    pub length_scale: Option<f32>,
    pub noise_scale: Option<f32>,
    pub noise_w: Option<f32>,
    pub max_input_chars: Option<usize>,
}

/// Authentication & user-identity knobs. Mirrors
/// [`crate::config::AuthConfig`]. Master switch + backend list +
/// per-backend sub-tables. Compiled only when the `auth` cargo
/// feature is enabled so the binary without auth does not have to
/// drag `serde` defaults through TOML parsing on every request.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TomlAuthConfig {
    /// Master switch. When `false`, the auth routes are not
    /// registered and `RequireAuth` stays off — the server keeps
    /// the pre-PR1 single-user trust boundary.
    pub enabled: Option<bool>,
    /// Subset of enabled backends (`local`, `oidc`, `passkey`).
    pub backends: Option<Vec<String>>,
    /// Public origin used for OIDC/Passkey origin + cookie security.
    pub public_url: Option<String>,
    /// Session absolute TTL in days. Range `1..=90`.
    pub session_ttl_days: Option<u32>,
    /// CSRF token header name.
    pub csrf_header: Option<String>,
    /// DB sub-table. Required when `enabled = true`.
    #[serde(default)]
    pub db: Option<TomlAuthDbConfig>,
    /// Password backend knobs (argon2id + registration gate).
    #[serde(default)]
    pub password: Option<TomlAuthPasswordConfig>,
    /// OIDC backend knobs.
    #[serde(default)]
    pub oidc: Option<TomlAuthOidcConfig>,
    /// Passkey (WebAuthn) backend knobs.
    #[serde(default)]
    pub passkey: Option<TomlAuthPasskeyConfig>,
    /// Per-user credentials vault knobs. Required when
    /// `auth.enabled = true` AND at least one agent is registered.
    #[serde(default)]
    pub credentials: Option<TomlAuthCredentialsConfig>,
}

/// Per-user credentials vault knobs. Mirrors
/// [`crate::config::AuthCredentialsConfig`].
///
/// The AES-256-GCM key is held in plaintext inside `[auth.credentials].key`
/// (64 hex chars / 32 raw bytes). Operators who want to keep the
/// secret out of disk should mount the TOML file from an encrypted
/// volume (k8s `Secret` via `subPath`, Vault Agent, …); the
/// configuration shape itself does not force plaintext storage on
/// disk.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TomlAuthCredentialsConfig {
    /// 64 hex chars = 32 bytes (AES-256-GCM). Required when
    /// `auth.enabled = true` AND at least one agent is registered.
    /// Generate once with `openssl rand -hex 32` and paste the
    /// value here.
    pub key: Option<String>,
}

/// DB connection knobs for the auth store. Mirrors
/// [`crate::config::AuthDbConfig`].
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TomlAuthDbConfig {
    /// `"sqlite"` or `"postgres"`. Required when `auth.enabled = true`.
    pub backend: Option<String>,
    /// Connection URL (e.g. `sqlite://./data/auth.db?mode=rwc` or
    /// `postgres://user:pwd@host/db`).
    pub url: Option<String>,
    /// Maximum simultaneous connections. Defaults to 16.
    pub max_connections: Option<u32>,
    /// When `true` (default), the server runs `migrate up` against
    /// the auth DB at boot. Operators who want full control over the
    /// schema lifecycle can flip this to `false` and run
    /// `stt-server migrate up` manually before each boot.
    pub auto_migrate: Option<bool>,
}

/// Password backend (argon2id) knobs. Mirrors
/// [`crate::config::AuthPasswordConfig`].
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TomlAuthPasswordConfig {
    pub argon2_memory_kib: Option<u32>,
    pub argon2_iterations: Option<u32>,
    pub argon2_parallelism: Option<u32>,
    pub min_password_length: Option<usize>,
    /// When `true`, any logged-in user can POST to
    /// `/api/auth/password/register` to create a new local account.
    pub allow_registration: Option<bool>,
}

/// OIDC backend knobs. Mirrors [`crate::config::AuthOidcConfig`].
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TomlAuthOidcConfig {
    pub auto_provision: Option<bool>,
    pub issuer: Option<String>,
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
    pub scopes: Option<Vec<String>>,
    pub required_groups: Option<Vec<String>>,
    pub role_claim: Option<String>,
}

/// Passkey (WebAuthn) backend knobs. Mirrors
/// [`crate::config::AuthPasskeyConfig`].
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TomlAuthPasskeyConfig {
    pub self_registration: Option<bool>,
    /// WebAuthn `rp_id` (no scheme, no port — e.g. `nagent.example.com`).
    pub rp_id: Option<String>,
    pub rp_name: Option<String>,
    /// Origins allowed for WebAuthn ceremonies — must include the
    /// scheme + port (e.g. `https://nagent.example.com`).
    pub origins: Option<Vec<String>>,
}

impl TomlConfig {
    /// Read and parse a TOML config file.
    ///
    /// Missing file is reported as `ConfigFileError::Io` with the OS
    /// error (typically `NotFound`) so the operator sees the exact path
    /// that failed. A parse error carries the underlying
    /// `toml::de::Error` formatted string, which already includes a
    /// line + column marker for fast debugging.
    pub fn from_file(path: &Path) -> Result<Self, ConfigFileError> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| ConfigFileError::Io(path.display().to_string(), e.to_string()))?;
        toml::from_str(&text)
            .map_err(|e| ConfigFileError::Parse(path.display().to_string(), e.to_string()))
    }
}

/// Errors produced when reading or parsing a TOML config file.
#[derive(Debug, thiserror::Error)]
pub enum ConfigFileError {
    /// `std::fs::read_to_string` failed (missing file, permission
    /// denied, etc.). The path is included so the operator knows which
    /// file the binary tried to open.
    #[error("could not read config file {0}: {1}")]
    Io(String, String),
    /// The TOML syntax was invalid or contained an unknown field.
    /// The error string carries the line/column marker from `toml`.
    #[error("invalid config file {0}: {1}")]
    Parse(String, String),
}

/// Merge two [`TomlConfig`] overlays: every `Some` field of `later`
/// wins over `earlier`, `None` fields fall through. Used to layer
/// `/etc/nagent/config.toml` under the XDG user config so the user
/// file acts as a per-user override on top of the system defaults.
///
/// Sub-tables (`[server]`, `[server.limits]`, `[server.rate_limits]`,
/// `[llm]`, `[agents]`, `[agents.web_fetch]`, `[agents.get_weather]`,
/// `[tts]`)
/// are merged recursively with the same semantics — a `Some` inside a
/// later sub-table overrides only that specific field, leaving sibling
/// fields from the earlier config untouched.
pub fn merge_toml_configs(earlier: &TomlConfig, later: &TomlConfig) -> TomlConfig {
    TomlConfig {
        server: merge_toml_server(earlier.server.as_ref(), later.server.as_ref()),
        llm: merge_toml_llm(earlier.llm.as_ref(), later.llm.as_ref()),
        agents: merge_toml_agents(earlier.agents.as_ref(), later.agents.as_ref()),
        tts: merge_toml_tts(earlier.tts.as_ref(), later.tts.as_ref()),
        auth: merge_toml_auth(earlier.auth.as_ref(), later.auth.as_ref()),
    }
}

fn merge_toml_server(
    earlier: Option<&TomlServerConfig>,
    later: Option<&TomlServerConfig>,
) -> Option<TomlServerConfig> {
    match (earlier, later) {
        (None, None) => None,
        (Some(e), None) => Some(e.clone()),
        (None, Some(l)) => Some(l.clone()),
        (Some(e), Some(l)) => Some(TomlServerConfig {
            bind_addr: l.bind_addr.clone().or_else(|| e.bind_addr.clone()),
            whisper_model_path: l
                .whisper_model_path
                .clone()
                .or_else(|| e.whisper_model_path.clone()),
            max_queue: l.max_queue.or(e.max_queue),
            inference_workers: l.inference_workers.or(e.inference_workers),
            session_idle_timeout_ms: l.session_idle_timeout_ms.or(e.session_idle_timeout_ms),
            infer_timeout_ms: l.infer_timeout_ms.or(e.infer_timeout_ms),
            limits: merge_toml_limits(e.limits.as_ref(), l.limits.as_ref()),
            rate_limits: merge_toml_rate_limits(e.rate_limits.as_ref(), l.rate_limits.as_ref()),
        }),
    }
}

fn merge_toml_limits(
    earlier: Option<&TomlLimitsConfig>,
    later: Option<&TomlLimitsConfig>,
) -> Option<TomlLimitsConfig> {
    match (earlier, later) {
        (None, None) => None,
        (Some(e), None) => Some(e.clone()),
        (None, Some(l)) => Some(l.clone()),
        (Some(e), Some(l)) => Some(TomlLimitsConfig {
            max_audio_frame_samples: l.max_audio_frame_samples.or(e.max_audio_frame_samples),
            required_sample_rate: l.required_sample_rate.or(e.required_sample_rate),
            max_language_hint_bytes: l.max_language_hint_bytes.or(e.max_language_hint_bytes),
        }),
    }
}

fn merge_toml_rate_limits(
    earlier: Option<&TomlRateLimitConfig>,
    later: Option<&TomlRateLimitConfig>,
) -> Option<TomlRateLimitConfig> {
    match (earlier, later) {
        (None, None) => None,
        (Some(e), None) => Some(e.clone()),
        (None, Some(l)) => Some(l.clone()),
        (Some(e), Some(l)) => Some(TomlRateLimitConfig {
            stt_per_min: l.stt_per_min.or(e.stt_per_min),
            llm_per_min: l.llm_per_min.or(e.llm_per_min),
        }),
    }
}

fn merge_toml_llm(
    earlier: Option<&TomlLlmConfig>,
    later: Option<&TomlLlmConfig>,
) -> Option<TomlLlmConfig> {
    match (earlier, later) {
        (None, None) => None,
        (Some(e), None) => Some(e.clone()),
        (None, Some(l)) => Some(l.clone()),
        (Some(e), Some(l)) => Some(TomlLlmConfig {
            enabled: l.enabled.or(e.enabled),
            base_url: l.base_url.clone().or_else(|| e.base_url.clone()),
            default_model: l.default_model.clone().or_else(|| e.default_model.clone()),
            api_key: l.api_key.clone().or_else(|| e.api_key.clone()),
            inbound_auth_key: l
                .inbound_auth_key
                .clone()
                .or_else(|| e.inbound_auth_key.clone()),
            auth_mode: l.auth_mode.clone().or_else(|| e.auth_mode.clone()),
            request_timeout_secs: l.request_timeout_secs.or(e.request_timeout_secs),
            cors_allow_origins: l
                .cors_allow_origins
                .clone()
                .or_else(|| e.cors_allow_origins.clone()),
            system_prompt: l.system_prompt.clone().or_else(|| e.system_prompt.clone()),
            allow_user_location: l.allow_user_location.or(e.allow_user_location),
            allow_user_timezone: l.allow_user_timezone.or(e.allow_user_timezone),
        }),
    }
}

fn merge_toml_agents(
    earlier: Option<&TomlAgentConfig>,
    later: Option<&TomlAgentConfig>,
) -> Option<TomlAgentConfig> {
    match (earlier, later) {
        (None, None) => None,
        (Some(e), None) => Some(e.clone()),
        (None, Some(l)) => Some(l.clone()),
        (Some(e), Some(l)) => Some(TomlAgentConfig {
            enabled: l.enabled.or(e.enabled),
            llm_max_tool_rounds: l.llm_max_tool_rounds.or(e.llm_max_tool_rounds),
            web_fetch: merge_toml_web_fetch(e.web_fetch.as_ref(), l.web_fetch.as_ref()),
            get_weather: merge_toml_weather(e.get_weather.as_ref(), l.get_weather.as_ref()),
            unit_convert: merge_toml_unit_convert(e.unit_convert.as_ref(), l.unit_convert.as_ref()),
            wikipedia: merge_toml_wikipedia(e.wikipedia.as_ref(), l.wikipedia.as_ref()),
            dictionary: merge_toml_dictionary(e.dictionary.as_ref(), l.dictionary.as_ref()),
        }),
    }
}

fn merge_toml_web_fetch(
    earlier: Option<&TomlWebFetchConfig>,
    later: Option<&TomlWebFetchConfig>,
) -> Option<TomlWebFetchConfig> {
    match (earlier, later) {
        (None, None) => None,
        (Some(e), None) => Some(e.clone()),
        (None, Some(l)) => Some(l.clone()),
        (Some(e), Some(l)) => Some(TomlWebFetchConfig {
            allow_public: l.allow_public.or(e.allow_public),
            allowlist: l.allowlist.clone().or_else(|| e.allowlist.clone()),
            max_bytes: l.max_bytes.or(e.max_bytes),
            timeout_ms: l.timeout_ms.or(e.timeout_ms),
        }),
    }
}

fn merge_toml_weather(
    earlier: Option<&TomlWeatherConfig>,
    later: Option<&TomlWeatherConfig>,
) -> Option<TomlWeatherConfig> {
    match (earlier, later) {
        (None, None) => None,
        (Some(e), None) => Some(e.clone()),
        (None, Some(l)) => Some(l.clone()),
        (Some(e), Some(l)) => Some(TomlWeatherConfig {
            api_key: l.api_key.clone().or_else(|| e.api_key.clone()),
            timeout_ms: l.timeout_ms.or(e.timeout_ms),
            base_url: l.base_url.clone().or_else(|| e.base_url.clone()),
        }),
    }
}

fn merge_toml_unit_convert(
    earlier: Option<&TomlUnitConvertConfig>,
    later: Option<&TomlUnitConvertConfig>,
) -> Option<TomlUnitConvertConfig> {
    match (earlier, later) {
        (None, None) => None,
        (Some(e), None) => Some(e.clone()),
        (None, Some(l)) => Some(l.clone()),
        (Some(e), Some(l)) => Some(TomlUnitConvertConfig {
            timeout_ms: l.timeout_ms.or(e.timeout_ms),
        }),
    }
}

fn merge_toml_wikipedia(
    earlier: Option<&TomlWikipediaConfig>,
    later: Option<&TomlWikipediaConfig>,
) -> Option<TomlWikipediaConfig> {
    match (earlier, later) {
        (None, None) => None,
        (Some(e), None) => Some(e.clone()),
        (None, Some(l)) => Some(l.clone()),
        (Some(e), Some(l)) => Some(TomlWikipediaConfig {
            timeout_ms: l.timeout_ms.or(e.timeout_ms),
            base_url: l.base_url.clone().or_else(|| e.base_url.clone()),
            user_agent: l.user_agent.clone().or_else(|| e.user_agent.clone()),
        }),
    }
}

fn merge_toml_dictionary(
    earlier: Option<&TomlDictionaryConfig>,
    later: Option<&TomlDictionaryConfig>,
) -> Option<TomlDictionaryConfig> {
    match (earlier, later) {
        (None, None) => None,
        (Some(e), None) => Some(e.clone()),
        (None, Some(l)) => Some(l.clone()),
        (Some(e), Some(l)) => Some(TomlDictionaryConfig {
            timeout_ms: l.timeout_ms.or(e.timeout_ms),
            base_url: l.base_url.clone().or_else(|| e.base_url.clone()),
        }),
    }
}

fn merge_toml_tts(
    earlier: Option<&TomlTtsConfig>,
    later: Option<&TomlTtsConfig>,
) -> Option<TomlTtsConfig> {
    match (earlier, later) {
        (None, None) => None,
        (Some(e), None) => Some(e.clone()),
        (None, Some(l)) => Some(l.clone()),
        (Some(e), Some(l)) => Some(TomlTtsConfig {
            enabled: l.enabled.or(e.enabled),
            model_dir: l.model_dir.clone().or_else(|| e.model_dir.clone()),
            voice_en: l.voice_en.clone().or_else(|| e.voice_en.clone()),
            voice_fr: l.voice_fr.clone().or_else(|| e.voice_fr.clone()),
            default_lang: l.default_lang.clone().or_else(|| e.default_lang.clone()),
            length_scale: l.length_scale.or(e.length_scale),
            noise_scale: l.noise_scale.or(e.noise_scale),
            noise_w: l.noise_w.or(e.noise_w),
            max_input_chars: l.max_input_chars.or(e.max_input_chars),
        }),
    }
}

fn merge_toml_auth(
    earlier: Option<&TomlAuthConfig>,
    later: Option<&TomlAuthConfig>,
) -> Option<TomlAuthConfig> {
    match (earlier, later) {
        (None, None) => None,
        (Some(e), None) => Some(e.clone()),
        (None, Some(l)) => Some(l.clone()),
        (Some(e), Some(l)) => Some(TomlAuthConfig {
            enabled: l.enabled.or(e.enabled),
            backends: l.backends.clone().or_else(|| e.backends.clone()),
            public_url: l.public_url.clone().or_else(|| e.public_url.clone()),
            session_ttl_days: l.session_ttl_days.or(e.session_ttl_days),
            csrf_header: l.csrf_header.clone().or_else(|| e.csrf_header.clone()),
            db: merge_toml_auth_db(e.db.as_ref(), l.db.as_ref()),
            password: merge_toml_auth_password(e.password.as_ref(), l.password.as_ref()),
            oidc: merge_toml_auth_oidc(e.oidc.as_ref(), l.oidc.as_ref()),
            passkey: merge_toml_auth_passkey(e.passkey.as_ref(), l.passkey.as_ref()),
            credentials: merge_toml_auth_credentials(
                e.credentials.as_ref(),
                l.credentials.as_ref(),
            ),
        }),
    }
}

fn merge_toml_auth_credentials(
    earlier: Option<&TomlAuthCredentialsConfig>,
    later: Option<&TomlAuthCredentialsConfig>,
) -> Option<TomlAuthCredentialsConfig> {
    match (earlier, later) {
        (None, None) => None,
        (Some(e), None) => Some(e.clone()),
        (None, Some(l)) => Some(l.clone()),
        (Some(e), Some(l)) => Some(TomlAuthCredentialsConfig {
            key: l.key.clone().or_else(|| e.key.clone()),
        }),
    }
}

fn merge_toml_auth_db(
    earlier: Option<&TomlAuthDbConfig>,
    later: Option<&TomlAuthDbConfig>,
) -> Option<TomlAuthDbConfig> {
    match (earlier, later) {
        (None, None) => None,
        (Some(e), None) => Some(e.clone()),
        (None, Some(l)) => Some(l.clone()),
        (Some(e), Some(l)) => Some(TomlAuthDbConfig {
            backend: l.backend.clone().or_else(|| e.backend.clone()),
            url: l.url.clone().or_else(|| e.url.clone()),
            max_connections: l.max_connections.or(e.max_connections),
            auto_migrate: l.auto_migrate.or(e.auto_migrate),
        }),
    }
}

fn merge_toml_auth_password(
    earlier: Option<&TomlAuthPasswordConfig>,
    later: Option<&TomlAuthPasswordConfig>,
) -> Option<TomlAuthPasswordConfig> {
    match (earlier, later) {
        (None, None) => None,
        (Some(e), None) => Some(e.clone()),
        (None, Some(l)) => Some(l.clone()),
        (Some(e), Some(l)) => Some(TomlAuthPasswordConfig {
            argon2_memory_kib: l.argon2_memory_kib.or(e.argon2_memory_kib),
            argon2_iterations: l.argon2_iterations.or(e.argon2_iterations),
            argon2_parallelism: l.argon2_parallelism.or(e.argon2_parallelism),
            min_password_length: l.min_password_length.or(e.min_password_length),
            allow_registration: l.allow_registration.or(e.allow_registration),
        }),
    }
}

fn merge_toml_auth_oidc(
    earlier: Option<&TomlAuthOidcConfig>,
    later: Option<&TomlAuthOidcConfig>,
) -> Option<TomlAuthOidcConfig> {
    match (earlier, later) {
        (None, None) => None,
        (Some(e), None) => Some(e.clone()),
        (None, Some(l)) => Some(l.clone()),
        (Some(e), Some(l)) => Some(TomlAuthOidcConfig {
            auto_provision: l.auto_provision.or(e.auto_provision),
            issuer: l.issuer.clone().or_else(|| e.issuer.clone()),
            client_id: l.client_id.clone().or_else(|| e.client_id.clone()),
            client_secret: l.client_secret.clone().or_else(|| e.client_secret.clone()),
            scopes: l.scopes.clone().or_else(|| e.scopes.clone()),
            required_groups: l
                .required_groups
                .clone()
                .or_else(|| e.required_groups.clone()),
            role_claim: l.role_claim.clone().or_else(|| e.role_claim.clone()),
        }),
    }
}

fn merge_toml_auth_passkey(
    earlier: Option<&TomlAuthPasskeyConfig>,
    later: Option<&TomlAuthPasskeyConfig>,
) -> Option<TomlAuthPasskeyConfig> {
    match (earlier, later) {
        (None, None) => None,
        (Some(e), None) => Some(e.clone()),
        (None, Some(l)) => Some(l.clone()),
        (Some(e), Some(l)) => Some(TomlAuthPasskeyConfig {
            self_registration: l.self_registration.or(e.self_registration),
            rp_id: l.rp_id.clone().or_else(|| e.rp_id.clone()),
            rp_name: l.rp_name.clone().or_else(|| e.rp_name.clone()),
            origins: l.origins.clone().or_else(|| e.origins.clone()),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_toml_is_default() {
        let cfg: TomlConfig = toml::from_str("").unwrap();
        assert!(cfg.server.is_none());
        assert!(cfg.llm.is_none());
        assert!(cfg.agents.is_none());
        assert!(cfg.tts.is_none());
    }

    #[test]
    fn full_toml_parses_with_per_tool_subtables() {
        let text = r#"
            [server]
            bind_addr = "127.0.0.1:9000"
            whisper_model_path = "/tmp/ggml-base.bin"
            max_queue = 16

            [server.limits]
            max_audio_frame_samples = 100

            [server.rate_limits]
            stt_per_min = 60
            llm_per_min = 10

            [llm]
            enabled = true
            base_url = "http://localhost:11434"
            default_model = "qwen2.5"
            api_key = "abc"
            request_timeout_secs = 30
            cors_allow_origins = ["https://example.com"]
            system_prompt = "from-toml"

            [agents]
            enabled = true
            llm_max_tool_rounds = 2

            [agents.web_fetch]
            allow_public = true
            allowlist = ["*.example.com", "foo.bar"]
            max_bytes = 1024
            timeout_ms = 5000

            [agents.get_weather]
            api_key = "weather-key"
            timeout_ms = 4000
            base_url = "https://api.weatherapi.com"

            [tts]
            enabled = true
            model_dir = "/var/lib/nagent/piper"
            voice_en = "en_US-lessac-medium"
            length_scale = 1.1
        "#;
        let cfg: TomlConfig = toml::from_str(text).unwrap();
        let server = cfg.server.expect("server section parsed");
        assert_eq!(server.bind_addr.as_deref(), Some("127.0.0.1:9000"));
        assert_eq!(
            server.whisper_model_path.as_deref(),
            Some("/tmp/ggml-base.bin")
        );
        assert_eq!(server.max_queue, Some(16));

        let limits = server.limits.expect("server.limits parsed");
        assert_eq!(limits.max_audio_frame_samples, Some(100));

        let rates = server.rate_limits.expect("server.rate_limits parsed");
        assert_eq!(rates.stt_per_min, Some(60));

        let agents = cfg.agents.expect("agents section parsed");
        assert_eq!(agents.enabled, Some(true));
        assert_eq!(agents.llm_max_tool_rounds, Some(2));

        let wf = agents.web_fetch.expect("agents.web_fetch parsed");
        assert_eq!(wf.allow_public, Some(true));
        assert_eq!(wf.timeout_ms, Some(5000));

        let wx = agents.get_weather.expect("agents.get_weather parsed");
        assert_eq!(wx.api_key.as_deref(), Some("weather-key"));
        assert_eq!(wx.timeout_ms, Some(4000));

        let tts = cfg.tts.expect("tts section parsed");
        assert_eq!(tts.enabled, Some(true));
        assert_eq!(tts.model_dir.as_deref(), Some("/var/lib/nagent/piper"));
        assert_eq!(tts.voice_en.as_deref(), Some("en_US-lessac-medium"));
        assert_eq!(tts.length_scale, Some(1.1));
    }

    #[test]
    fn tts_section_merges_with_overrides() {
        let earlier: TomlConfig = toml::from_str(
            r#"
                [tts]
                enabled = true
                voice_en = "en_US-amy-low"
                length_scale = 1.0
            "#,
        )
        .unwrap();
        let later: TomlConfig = toml::from_str(
            r#"
                [tts]
                length_scale = 1.3
            "#,
        )
        .unwrap();
        let merged = merge_toml_configs(&earlier, &later);
        let tts = merged.tts.expect("tts merged");
        assert_eq!(tts.enabled, Some(true), "kept from earlier");
        assert_eq!(tts.voice_en.as_deref(), Some("en_US-amy-low"), "kept");
        assert_eq!(tts.length_scale, Some(1.3), "later wins");
    }

    #[test]
    fn old_root_level_keys_are_rejected() {
        // After the move to [server], a stray root-level `bind_addr`
        // should fail loudly rather than silently fall back to the
        // default — guards against operators copy-pasting pre-refactor
        // snippets.
        let text = r#"bind_addr = "0.0.0.0:8080""#;
        let err = toml::from_str::<TomlConfig>(text).unwrap_err();
        assert!(
            err.to_string().contains("bind_addr"),
            "unknown-field error must mention the offending key, got: {err}"
        );
    }

    #[test]
    fn unknown_field_is_rejected() {
        let text = r#"
            typo_field = 42
        "#;
        let err = toml::from_str::<TomlConfig>(text).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("typo_field"),
            "unknown-field error must mention the offending key, got: {msg}"
        );
    }

    #[test]
    fn unknown_field_inside_server_is_rejected() {
        // A typo under [server] (e.g. `bind_adr`) must surface
        // rather than silently fall back to a default — same
        // rationale as `unknown_agent_subtable_is_rejected`.
        let text = r#"
            [server]
            bind_adr = "0.0.0.0:8080"
        "#;
        let err = toml::from_str::<TomlConfig>(text).unwrap_err();
        assert!(
            err.to_string().contains("bind_adr"),
            "unknown-field error must mention the offending key, got: {err}"
        );
    }

    #[test]
    fn unknown_agent_subtable_is_rejected() {
        // The agent tool list is fixed by the cargo features compiled
        // into the binary. A typo like `agents.web_fetxh` must surface
        // rather than silently be ignored.
        let text = r#"
            [agents]
            enabled = true

            [agents.web_fetxh]
            allow_public = true
        "#;
        let err = toml::from_str::<TomlConfig>(text).unwrap_err();
        assert!(err.to_string().contains("web_fetxh"));
    }

    #[test]
    fn from_file_missing_path_is_io_error() {
        let err = TomlConfig::from_file(Path::new("/does/not/exist.toml")).unwrap_err();
        match err {
            ConfigFileError::Io(path, _) => {
                assert!(path.contains("/does/not/exist.toml"));
            }
            other => panic!("expected Io error, got {other:?}"),
        }
    }

    #[test]
    fn from_file_invalid_toml_is_parse_error() {
        // Write straight to `/tmp` with a PID-scoped filename to avoid
        // parallel test runners trampling each other; cleanup is best
        // effort because the OS reclaims the inode on exit anyway.
        let path = std::env::temp_dir().join(format!(
            "nagent-bad-toml-{}-{}.toml",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&path, "this is not = valid = toml ==").unwrap();
        let err = TomlConfig::from_file(&path).unwrap_err();
        let _ = std::fs::remove_file(&path);
        match err {
            ConfigFileError::Parse(p, _) => {
                assert!(p.contains("nagent-bad-toml-"), "path echo missing: {p}");
            }
            other => panic!("expected Parse error, got {other:?}"),
        }
    }

    #[test]
    fn merge_takes_later_over_earlier() {
        let earlier: TomlConfig = toml::from_str(
            r#"
                [server]
                bind_addr = "127.0.0.1:1111"
                max_queue = 8
                whisper_model_path = "/from/earlier.bin"

                [server.rate_limits]
                stt_per_min = 60

                [agents]
                enabled = true

                [agents.web_fetch]
                allow_public = false
                max_bytes = 1024
            "#,
        )
        .unwrap();
        let later: TomlConfig = toml::from_str(
            r#"
                [server]
                max_queue = 99
                whisper_model_path = "/from/later.bin"

                [server.rate_limits]
                llm_per_min = 30

                [agents]
                llm_max_tool_rounds = 7

                [agents.web_fetch]
                allow_public = true
            "#,
        )
        .unwrap();
        let merged = merge_toml_configs(&earlier, &later);
        // bind_addr only set in earlier → kept.
        let server = merged.server.expect("server section present after merge");
        assert_eq!(server.bind_addr.as_deref(), Some("127.0.0.1:1111"));
        // max_queue in both → later wins.
        assert_eq!(server.max_queue, Some(99));
        // whisper_model_path in both → later wins.
        assert_eq!(
            server.whisper_model_path.as_deref(),
            Some("/from/later.bin")
        );
        // stt_per_min only in earlier → kept; llm_per_min only in later → kept.
        let rates = server.rate_limits.expect("rate_limits merged");
        assert_eq!(rates.stt_per_min, Some(60));
        assert_eq!(rates.llm_per_min, Some(30));

        let agents = merged.agents.expect("agents section present after merge");
        // enabled only in earlier → kept.
        assert_eq!(agents.enabled, Some(true));
        // llm_max_tool_rounds only in later → taken from later.
        assert_eq!(agents.llm_max_tool_rounds, Some(7));

        let wf = agents
            .web_fetch
            .expect("web_fetch section present after merge");
        // allow_public in both → later wins.
        assert_eq!(wf.allow_public, Some(true));
        // max_bytes only in earlier → kept.
        assert_eq!(wf.max_bytes, Some(1024));
    }

    #[test]
    fn merge_with_earlier_only_is_identity() {
        let earlier: TomlConfig = toml::from_str(
            r#"
                [server]
                bind_addr = "127.0.0.1:2222"
                max_queue = 4
            "#,
        )
        .unwrap();
        let later: TomlConfig = toml::from_str("").unwrap();
        let merged = merge_toml_configs(&earlier, &later);
        let server = merged.server.expect("server section present after merge");
        assert_eq!(server.bind_addr.as_deref(), Some("127.0.0.1:2222"));
        assert_eq!(server.max_queue, Some(4));
    }

    #[test]
    fn merge_with_later_only_takes_all_from_later() {
        let earlier: TomlConfig = toml::from_str("").unwrap();
        let later: TomlConfig = toml::from_str(
            r#"
                [server]
                bind_addr = "127.0.0.1:3333"
                max_queue = 64
            "#,
        )
        .unwrap();
        let merged = merge_toml_configs(&earlier, &later);
        let server = merged.server.expect("server section present after merge");
        assert_eq!(server.bind_addr.as_deref(), Some("127.0.0.1:3333"));
        assert_eq!(server.max_queue, Some(64));
    }

    #[test]
    fn full_auth_toml_parses_with_subtables() {
        // The full [auth] block — mirrors the example in
        // examples/config.toml.example. We deliberately keep the
        // rp_id + origins pairing correct so a future regression in
        // the schema (e.g. dropping one of the sub-tables) fails the
        // parse loudly rather than silently losing the WebAuthn
        // config.
        let text = r#"
            [auth]
            enabled = true
            backends = ["local", "oidc", "passkey"]
            public_url = "https://nagent.example.com"
            session_ttl_days = 14
            csrf_header = "x-csrf-token"

            [auth.db]
            backend = "postgres"
            url = "postgres://nagent:pwd@db/nagent"
            max_connections = 32

            [auth.password]
            argon2_memory_kib = 19456
            argon2_iterations = 3
            argon2_parallelism = 1
            min_password_length = 12
            allow_registration = false

            [auth.oidc]
            auto_provision = false
            issuer = "https://keycloak.example.com/realms/nagent"
            client_id = "nagent"
            client_secret = "shh"
            scopes = ["openid", "email", "profile", "groups"]
            required_groups = ["users"]
            role_claim = "groups"

            [auth.passkey]
            self_registration = false
            rp_id = "nagent.example.com"
            rp_name = "nagent"
            origins = ["https://nagent.example.com"]
        "#;
        let cfg: TomlConfig = toml::from_str(text).expect("full auth TOML must parse");
        let auth = cfg.auth.expect("auth section parsed");
        assert_eq!(auth.enabled, Some(true));
        assert_eq!(
            auth.backends,
            Some(vec!["local".into(), "oidc".into(), "passkey".into()])
        );
        assert_eq!(auth.session_ttl_days, Some(14));
        let db = auth.db.expect("auth.db parsed");
        assert_eq!(db.backend.as_deref(), Some("postgres"));
        assert_eq!(db.max_connections, Some(32));
        let pwd = auth.password.expect("auth.password parsed");
        assert_eq!(pwd.argon2_iterations, Some(3));
        assert_eq!(pwd.allow_registration, Some(false));
        let oidc = auth.oidc.expect("auth.oidc parsed");
        assert_eq!(
            oidc.issuer.as_deref(),
            Some("https://keycloak.example.com/realms/nagent")
        );
        let pk = auth.passkey.expect("auth.passkey parsed");
        assert_eq!(pk.rp_id.as_deref(), Some("nagent.example.com"));
        let origins = pk.origins.as_deref().expect("origins parsed");
        assert_eq!(origins, &["https://nagent.example.com".to_string()][..]);
    }

    #[test]
    fn unknown_field_inside_auth_is_rejected() {
        // The auth schema is fixed by the cargo features compiled
        // into the binary. A typo under `[auth]` must surface as a
        // parse error, not silently revert to the default.
        let text = r#"
            [auth]
            enabeld = true
        "#;
        let err = toml::from_str::<TomlConfig>(text).unwrap_err();
        assert!(
            err.to_string().contains("enabeld"),
            "unknown-field error must mention the offending key, got: {err}"
        );
    }

    #[test]
    fn merge_takes_later_over_earlier_for_auth() {
        // The same "later overrides earlier" semantics the other
        // sub-tables obey must apply to `[auth]` too — operator
        // changes in the XDG-user file must win over the system
        // file's defaults.
        let earlier: TomlConfig = toml::from_str(
            r#"
                [auth]
                enabled = true
                backends = ["local"]
                session_ttl_days = 7

                [auth.password]
                min_password_length = 8
            "#,
        )
        .unwrap();
        let later: TomlConfig = toml::from_str(
            r#"
                [auth]
                backends = ["local", "oidc"]
                session_ttl_days = 30

                [auth.password]
                min_password_length = 12
            "#,
        )
        .unwrap();
        let merged = merge_toml_configs(&earlier, &later);
        let auth = merged.auth.expect("auth section present after merge");
        // enabled only in earlier → kept.
        assert_eq!(auth.enabled, Some(true));
        // backends in both → later wins.
        assert_eq!(auth.backends, Some(vec!["local".into(), "oidc".into()]));
        // session_ttl_days in both → later wins.
        assert_eq!(auth.session_ttl_days, Some(30));
        let pwd = auth.password.expect("auth.password merged");
        assert_eq!(pwd.min_password_length, Some(12), "later wins");
    }
}
