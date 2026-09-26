//! Server configuration loaded from environment variables and an
//! optional TOML overlay.
//!
//! Precedence (highest wins):
//! 1. Environment variable (or `.env` value — same precedence, since
//!    `dotenvy` writes to `std::env`).
//! 2. TOML file value, when `--config <path>` is supplied at startup
//!    and the key is present in the file.
//! 3. Hardcoded default baked into `Default::default()`.
//!
//! `WHISPER_MODEL_PATH` is the only required knob overall: if it is
//! missing from both env and the TOML file, [`Config::load`] fails
//! fast with [`ConfigError::MissingModelPath`].

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::config_file::{
    merge_toml_configs, TomlAgentConfig, TomlConfig, TomlLimitsConfig, TomlRateLimitConfig,
};

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

/// CLI flags parsed before config loading.
///
/// Kept minimal on purpose: `--config <path>` is the only operator
/// flag today. Parsed manually (no `clap` dep) — the cost is one
/// `match` and the benefit is a single-binary dependency surface.
#[derive(Debug, Default, Clone)]
pub struct CliArgs {
    /// Path to the optional TOML configuration file. When `Some`,
    /// [`Config::load`] reads it and overlays it under the env-vars.
    pub config: Option<PathBuf>,
}

impl CliArgs {
    /// Parse `std::env::args()` for the supported flags.
    ///
    /// Unknown flags exit with status 2 and a one-line error; `--help`
    /// / `-h` print the usage block and exit 0. The exit calls go
    /// through `std::process` directly because this runs before
    /// tracing is initialised and the binary has no business
    /// continuing past a malformed command line.
    pub fn parse() -> Self {
        let mut out = Self::default();
        let mut iter = std::env::args().skip(1);
        while let Some(arg) = iter.next() {
            if arg == "--config" {
                match iter.next() {
                    Some(value) => out.config = Some(PathBuf::from(value)),
                    None => {
                        eprintln!("error: --config requires a path argument");
                        std::process::exit(2);
                    }
                }
            } else if let Some(value) = arg.strip_prefix("--config=") {
                out.config = Some(PathBuf::from(value));
            } else if arg == "--help" || arg == "-h" {
                print_help();
                std::process::exit(0);
            } else {
                eprintln!("error: unknown argument: {arg}");
                eprintln!("(run with --help for usage)");
                std::process::exit(2);
            }
        }
        out
    }
}

fn print_help() {
    println!("stt-server — STT pipeline with optional LLM proxy and chat agents");
    println!();
    println!("USAGE:");
    println!("    stt-server [--config <PATH>]");
    println!();
    println!("OPTIONS:");
    println!("    --config <PATH>    Load defaults from a TOML file before reading env vars.");
    println!("                      Env vars always override the file (12-factor friendly).");
    println!("    -h, --help         Print this help and exit.");
    println!();
    println!("ENVIRONMENT:");
    println!("    All runtime knobs are documented in README.md under \"Configuration\".");
}

impl Config {
    /// Load configuration from environment variables only.
    ///
    /// Backwards-compatible entry point for tests and binaries that do
    /// not need a TOML overlay — internally equivalent to
    /// [`Config::from_env_with_toml`] with `None`.
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_env_with_toml(None)
    }

    /// Load configuration from env vars with an optional TOML overlay.
    ///
    /// For every knob the precedence is: env var > TOML > default.
    /// Missing TOML file is the caller's responsibility (use
    /// [`Config::load`] to centralise the file-read step).
    pub fn from_env_with_toml(toml: Option<&TomlConfig>) -> Result<Self, ConfigError> {
        // Best-effort .env load; missing file is fine in production.
        let _ = dotenvy::dotenv();

        let server = toml.and_then(|t| t.server.as_ref());

        let bind_addr = resolve_bind_addr(
            env_opt("BIND_ADDR").as_deref(),
            server.and_then(|s| s.bind_addr.as_deref()),
        )?;
        let whisper_model_path = resolve_model_path(
            env_opt("WHISPER_MODEL_PATH").as_deref(),
            server.and_then(|s| s.whisper_model_path.as_deref()),
        )?;

        let max_queue = resolve_primitive(
            env_opt("MAX_QUEUE").as_deref(),
            server.and_then(|s| s.max_queue),
            32,
            "MAX_QUEUE",
        )?;
        let session_idle_timeout = Duration::from_millis(resolve_primitive(
            env_opt("SESSION_IDLE_TIMEOUT_MS").as_deref(),
            server.and_then(|s| s.session_idle_timeout_ms),
            30_000,
            "SESSION_IDLE_TIMEOUT_MS",
        )?);
        let infer_timeout = Duration::from_millis(resolve_primitive(
            env_opt("INFER_TIMEOUT_MS").as_deref(),
            server.and_then(|s| s.infer_timeout_ms),
            30_000,
            "INFER_TIMEOUT_MS",
        )?);

        let limits = LimitsConfig::from_env_with_toml(server.and_then(|s| s.limits.as_ref()))?;
        let llm = LlmConfig::from_env_with_toml(toml.and_then(|t| t.llm.as_ref()))?;
        let agents = AgentConfig::from_env_with_toml(toml.and_then(|t| t.agents.as_ref()))?;
        let rate_limit =
            RateLimitConfig::from_env_with_toml(server.and_then(|s| s.rate_limits.as_ref()))?;

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

    /// Top-level entry point used by `main`: parses CLI args, reads
    /// the optional TOML file(s), and builds the [`Config`] from the
    /// combined `env > toml > default` precedence chain.
    ///
    /// File discovery depends on whether `--config` was supplied:
    ///
    /// - `--config <PATH>` — load only that file (operator-chosen,
    ///   bypasses the layered discovery). Useful for tests, container
    ///   setups, and debugging.
    /// - no `--config` — layer the system file (`/etc/nagent/config.toml`)
    ///   under the XDG user file (`$XDG_CONFIG_HOME/nagent/config.toml`
    ///   or `~/.config/nagent/config.toml`). Missing files are silently
    ///   skipped; only parse / read errors are reported.
    ///
    /// In both cases, environment variables (including `.env` values)
    /// still win over whatever the file(s) provided.
    pub fn load(cli: &CliArgs) -> Result<Self, ConfigError> {
        let toml = match &cli.config {
            Some(path) => Some(load_toml(path)?),
            None => load_layered_default_config()?,
        };
        Self::from_env_with_toml(toml.as_ref())
    }
}

/// System-wide config path. Loaded first when no `--config` is given
/// so it acts as the distribution default. On Debian-style distros this
/// matches the FHS `/etc/<package>/` convention.
const SYSTEM_CONFIG_PATH: &str = "/etc/nagent/config.toml";

/// Build the default layered config from the system path and the XDG
/// user path. Missing files are silently skipped; only a file that
/// *exists but is unreadable or unparseable* surfaces an error, with
/// its path in the error message so the operator can fix it.
fn load_layered_default_config() -> Result<Option<TomlConfig>, ConfigError> {
    // Discovery order = precedence order (earlier files are overridden
    // by later ones). `None` after the loop means no file existed →
    // behave exactly like the no-TOML path.
    let mut paths: Vec<PathBuf> = vec![PathBuf::from(SYSTEM_CONFIG_PATH)];
    if let Some(xdg) = xdg_config_path() {
        paths.push(xdg);
    }

    let mut merged: Option<TomlConfig> = None;
    for path in paths {
        if !path.exists() {
            continue;
        }
        let next = load_toml(&path)?;
        merged = Some(match merged {
            Some(prev) => merge_toml_configs(&prev, &next),
            None => next,
        });
    }
    Ok(merged)
}

/// Resolve the XDG user config path: `$XDG_CONFIG_HOME/nagent/config.toml`,
/// falling back to `$HOME/.config/nagent/config.toml` when
/// `XDG_CONFIG_HOME` is unset or empty (per the XDG Base Directory
/// spec). Returns `None` when neither variable is set — Linux servers
/// and stripped-down CI containers often omit `HOME`.
fn xdg_config_path() -> Option<PathBuf> {
    xdg_config_path_from(
        env_opt("XDG_CONFIG_HOME").as_deref(),
        env_opt("HOME").as_deref(),
    )
}

/// Pure variant of [`xdg_config_path`] for unit tests: takes the env
/// values as parameters so the function stays free of process-global
/// env mutation.
fn xdg_config_path_from(xdg_config_home: Option<&str>, home: Option<&str>) -> Option<PathBuf> {
    let base = match xdg_config_home {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => PathBuf::from(home?).join(".config"),
    };
    Some(base.join("nagent").join("config.toml"))
}

/// Read and parse the TOML config file, wrapping
/// [`crate::config_file::ConfigFileError`] into the unified
/// [`ConfigError`] enum so callers only deal with one error type.
fn load_toml(path: &Path) -> Result<TomlConfig, ConfigError> {
    TomlConfig::from_file(path)
        .map_err(|e| ConfigError::InvalidConfigFile(path.display().to_string(), e.to_string()))
}

/// Resolve the bind address: env > TOML > `0.0.0.0:8080`.
fn resolve_bind_addr(
    env_value: Option<&str>,
    toml_value: Option<&str>,
) -> Result<SocketAddr, ConfigError> {
    const DEFAULT: &str = "0.0.0.0:8080";
    match env_value {
        Some(v) => v
            .parse::<SocketAddr>()
            .map_err(|e: std::net::AddrParseError| ConfigError::InvalidBindAddr(e.to_string())),
        None => toml_value
            .unwrap_or(DEFAULT)
            .parse::<SocketAddr>()
            .map_err(|e: std::net::AddrParseError| ConfigError::InvalidBindAddr(e.to_string())),
    }
}

/// Resolve the Whisper model path: env > TOML > required.
fn resolve_model_path(
    env_value: Option<&str>,
    toml_value: Option<&str>,
) -> Result<PathBuf, ConfigError> {
    match env_value {
        Some(v) => Ok(PathBuf::from(v)),
        None => match toml_value {
            Some(v) => Ok(PathBuf::from(v)),
            None => Err(ConfigError::MissingModelPath),
        },
    }
}

/// Read an env var and return `None` for both unset and empty values.
/// Empty values (e.g. `env: - ""` in a container manifest) are treated
/// as unset so they never accidentally override a TOML value that the
/// operator deliberately populated.
fn env_opt(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

/// Resolve a `FromStr` value: env > TOML > default.
///
/// Pulled out as a pure function (no direct `std::env::var`) so unit
/// tests can exercise the precedence chain without mutating
/// process-global state — see the existing comment on
/// `rate_limit_defaults_match_plan` for why.
fn resolve_primitive<T>(
    env_value: Option<&str>,
    toml_value: Option<T>,
    default: T,
    env_key: &str,
) -> Result<T, ConfigError>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    match env_value {
        Some(v) => v
            .parse::<T>()
            .map_err(|e| ConfigError::InvalidEnv(env_key.into(), e.to_string())),
        None => Ok(toml_value.unwrap_or(default)),
    }
}

/// Resolve an `Option<String>`-like knob (the API key style): env
/// value if present & non-empty, else TOML value, else `None`.
fn resolve_opt_string(env_value: Option<&str>, toml_value: Option<&str>) -> Option<String> {
    match env_value {
        Some(v) if !v.is_empty() => Some(v.to_string()),
        _ => toml_value.map(str::to_string),
    }
}

/// Resolve a comma-separated string list: env value (split + trimmed
/// + empty-filtered) > TOML list > default.
fn resolve_csv(
    env_value: Option<&str>,
    toml_value: Option<Vec<String>>,
    default: Vec<String>,
) -> Vec<String> {
    match env_value {
        Some(v) => v
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect(),
        None => toml_value.unwrap_or(default),
    }
}

impl LimitsConfig {
    fn from_env_with_toml(toml: Option<&TomlLimitsConfig>) -> Result<Self, ConfigError> {
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
        })
    }
}

impl RateLimitConfig {
    fn from_env_with_toml(toml: Option<&TomlRateLimitConfig>) -> Result<Self, ConfigError> {
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
    /// Server-default system prompt prepended to every
    /// `/v1/chat/completions` request. The browser-supplied
    /// "Additional instructions" textarea is appended *after* this so
    /// the admin's intent stays authoritative. Env var
    /// `LLM_SYSTEM_PROMPT`, TOML key `[llm].system_prompt`. An empty
    /// or whitespace-only value is treated as unset (no injection),
    /// keeping the proxy a perfect passthrough by default. Note that
    /// the prompt is sent on every round of the tool loop, so very
    /// long custom prompts multiply with the round count and may
    /// exhaust the model's context window.
    pub system_prompt: Option<String>,
    /// Whether the browser is allowed to forward the user's
    /// approximate geolocation to the LLM as part of the request.
    /// The browser still asks for explicit consent on every visit; this
    /// flag is a defence-in-depth kill-switch so operators handling
    /// sensitive deployments can strip the ephemeral `User's
    /// approximate location:` system message before it ever reaches
    /// the upstream model, regardless of what the browser sends. Env
    /// var `LLM_ALLOW_USER_LOCATION`, TOML key
    /// `[llm].allow_user_location`. Defaults to `true` — the user's
    /// consent in the UI is the primary gate.
    pub allow_user_location: bool,
}

impl LlmConfig {
    fn from_env_with_toml(
        toml: Option<&crate::config_file::TomlLlmConfig>,
    ) -> Result<Self, ConfigError> {
        let defaults = LlmConfig::default();
        let toml = toml.cloned().unwrap_or_default();
        let enabled = resolve_primitive(
            env_opt("LLM_ENABLED").as_deref(),
            toml.enabled,
            defaults.enabled,
            "LLM_ENABLED",
        )?;
        let base_url = resolve_opt_string(
            env_opt("OLLAMA_BASE_URL").as_deref(),
            toml.base_url.as_deref(),
        )
        .unwrap_or_else(|| defaults.base_url.clone());
        let default_model = resolve_opt_string(
            env_opt("OLLAMA_MODEL").as_deref(),
            toml.default_model.as_deref(),
        )
        .unwrap_or_else(|| defaults.default_model.clone());
        let api_key = resolve_opt_string(
            env_opt("OLLAMA_API_KEY").as_deref(),
            toml.api_key.as_deref(),
        );
        let request_timeout = Duration::from_secs(resolve_primitive(
            env_opt("LLM_REQUEST_TIMEOUT_SECS").as_deref(),
            toml.request_timeout_secs,
            defaults.request_timeout.as_secs(),
            "LLM_REQUEST_TIMEOUT_SECS",
        )?);
        let cors_allow_origins = resolve_csv(
            env_opt("LLM_CORS_ALLOW_ORIGINS").as_deref(),
            toml.cors_allow_origins,
            defaults.cors_allow_origins,
        );
        let system_prompt = resolve_opt_string(
            env_opt("LLM_SYSTEM_PROMPT").as_deref(),
            toml.system_prompt.as_deref(),
        );
        let allow_user_location = resolve_primitive(
            env_opt("LLM_ALLOW_USER_LOCATION").as_deref(),
            toml.allow_user_location,
            defaults.allow_user_location,
            "LLM_ALLOW_USER_LOCATION",
        )?;

        Ok(Self {
            enabled,
            base_url,
            default_model,
            api_key,
            request_timeout,
            cors_allow_origins,
            system_prompt,
            allow_user_location,
        })
    }
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            base_url: "http://localhost:11434".to_string(),
            default_model: "llama3.1".to_string(),
            api_key: None,
            request_timeout: Duration::from_secs(120),
            cors_allow_origins: Vec::new(),
            system_prompt: None,
            allow_user_location: true,
        }
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
    fn from_env_with_toml(toml: Option<&TomlAgentConfig>) -> Result<Self, ConfigError> {
        let defaults = Self::default();
        let toml = toml.cloned().unwrap_or_default();
        Ok(Self {
            enabled: resolve_primitive(
                env_opt("AGENTS_ENABLED").as_deref(),
                toml.enabled,
                defaults.enabled,
                "AGENTS_ENABLED",
            )?,
            llm_max_tool_rounds: resolve_primitive(
                env_opt("LLM_MAX_TOOL_ROUNDS").as_deref(),
                toml.llm_max_tool_rounds,
                defaults.llm_max_tool_rounds,
                "LLM_MAX_TOOL_ROUNDS",
            )?
            .clamp(1, 32),
            web_fetch: WebFetchConfig::from_env_with_toml(toml.web_fetch.as_ref())?,
            weather: WeatherConfig::from_env_with_toml(toml.get_weather.as_ref())?,
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
    fn from_env_with_toml(
        toml: Option<&crate::config_file::TomlWebFetchConfig>,
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
    fn from_env_with_toml(
        toml: Option<&crate::config_file::TomlWeatherConfig>,
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

/// Errors produced by [`Config::from_env`], [`Config::from_env_with_toml`]
/// and [`Config::load`].
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("WHISPER_MODEL_PATH is required")]
    MissingModelPath,
    #[error("invalid BIND_ADDR: {0}")]
    InvalidBindAddr(String),
    #[error("invalid env var {0}: {1}")]
    InvalidEnv(String, String),
    /// Reading or parsing the file passed via `--config` failed. The
    /// path is included so the operator sees exactly which file the
    /// binary tried to open.
    #[error("invalid config file {0}: {1}")]
    InvalidConfigFile(String, String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config_file::TomlConfig;

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

    /// Helper: build a TOML overlay from inline text without touching
    /// the filesystem. Returns the parsed struct ready to feed into
    /// `from_env_with_toml`.
    fn toml_from(text: &str) -> TomlConfig {
        toml::from_str(text).expect("test TOML must parse")
    }

    /// The TOML path must be honoured when the env var is *unset*. We
    /// can't unset `BIND_ADDR` directly (process-global), but every
    /// other knob the test reads here is not set by `cargo test`, so
    /// the TOML value should win for `whisper_model_path`,
    /// `max_queue`, and `agents.get_weather.api_key`.
    #[test]
    fn toml_overlay_is_used_when_env_unset() {
        let toml = toml_from(
            r#"
                [server]
                max_queue = 8
                whisper_model_path = "/tmp/from-toml.bin"

                [agents]
                enabled = false
                llm_max_tool_rounds = 2

                [agents.web_fetch]
                allow_public = true
                allowlist = ["example.com"]
                max_bytes = 1024
                timeout_ms = 5000

                [agents.get_weather]
                api_key = "weather-toml-key"
            "#,
        );
        let cfg = Config::from_env_with_toml(Some(&toml)).expect("config must load");
        assert_eq!(cfg.max_queue, 8, "max_queue from TOML");
        assert_eq!(
            cfg.whisper_model_path,
            PathBuf::from("/tmp/from-toml.bin"),
            "model path from TOML"
        );
        assert!(!cfg.agents.enabled, "agents.enabled from TOML");
        assert_eq!(cfg.agents.llm_max_tool_rounds, 2);
        assert!(cfg.agents.web_fetch.allow_public);
        assert_eq!(cfg.agents.web_fetch.allowlist, vec!["example.com"]);
        assert_eq!(cfg.agents.web_fetch.max_bytes, 1024);
        assert_eq!(cfg.agents.web_fetch.timeout_ms, 5000);
        assert_eq!(cfg.agents.weather.api_key, "weather-toml-key");
    }

    /// Env var must beat TOML when both are present. Exercised through
    /// the pure `resolve_primitive` helper so the test stays free of
    /// process-global env mutations (see the comment on
    /// `rate_limit_defaults_match_plan` for why this matters under
    /// `cargo test`'s parallel harness).
    #[test]
    fn env_var_beats_toml_for_primitives() {
        // env Some, toml Some → env wins
        assert_eq!(
            resolve_primitive(Some("99"), Some(8usize), 32usize, "MAX_QUEUE").unwrap(),
            99
        );
        // env None, toml Some → toml wins
        assert_eq!(
            resolve_primitive(None, Some(8usize), 32usize, "MAX_QUEUE").unwrap(),
            8
        );
        // env None, toml None → default wins
        assert_eq!(
            resolve_primitive(None, None::<usize>, 32usize, "MAX_QUEUE").unwrap(),
            32
        );
        // env Some, toml None → env wins
        assert_eq!(
            resolve_primitive(Some("99"), None::<usize>, 32usize, "MAX_QUEUE").unwrap(),
            99
        );
        // Invalid env value is an error, not a silent default.
        let err = resolve_primitive(Some("not-a-number"), Some(8usize), 32usize, "MAX_QUEUE")
            .unwrap_err();
        match err {
            ConfigError::InvalidEnv(k, _) => assert_eq!(k, "MAX_QUEUE"),
            other => panic!("expected InvalidEnv, got {other:?}"),
        }
    }

    #[test]
    fn opt_string_helpers_handle_empty_env() {
        // env non-empty → wins over TOML
        assert_eq!(
            resolve_opt_string(Some("env-key"), Some("toml-key")),
            Some("env-key".to_string())
        );
        // env empty → treated as unset, TOML wins
        assert_eq!(
            resolve_opt_string(Some(""), Some("toml-key")),
            Some("toml-key".to_string())
        );
        // env unset, TOML unset → None
        assert_eq!(resolve_opt_string(None, None), None);
    }

    /// Helper: build a `LlmConfig` directly with the `system_prompt`
    /// resolution short-circuited to a single `resolve_opt_string`
    /// call. Avoids mutating process-global env vars (see the comment
    /// on `rate_limit_defaults_match_plan` for why).
    fn llm_system_prompt_resolved(env: Option<&str>, toml: Option<&str>) -> Option<String> {
        resolve_opt_string(env, toml)
    }

    #[test]
    fn llm_system_prompt_env_overrides_toml() {
        assert_eq!(
            llm_system_prompt_resolved(Some("from-env"), Some("from-toml")),
            Some("from-env".to_string())
        );
    }

    #[test]
    fn llm_system_prompt_toml_used_when_env_unset() {
        assert_eq!(
            llm_system_prompt_resolved(None, Some("from-toml")),
            Some("from-toml".to_string())
        );
    }

    #[test]
    fn llm_system_prompt_defaults_to_none() {
        assert_eq!(llm_system_prompt_resolved(None, None), None);
    }

    #[test]
    fn llm_system_prompt_empty_env_string_falls_back_to_toml() {
        // Empty env vars (e.g. `LLM_SYSTEM_PROMPT=""` in a container
        // manifest) must NOT silently override the TOML value.
        assert_eq!(
            llm_system_prompt_resolved(Some(""), Some("from-toml")),
            Some("from-toml".to_string())
        );
        // Both empty / unset → None (no injection).
        assert_eq!(llm_system_prompt_resolved(Some(""), None), None);
    }

    #[test]
    fn llm_allow_user_location_env_overrides_toml_and_default() {
        // The kill-switch defaults to true so consent in the UI is the
        // primary gate; the env var still beats the TOML file (defence
        // in depth for sensitive deployments).
        let default = LlmConfig::default().allow_user_location;
        assert!(default, "allow_user_location must default to true");
        let from_env = resolve_primitive(
            Some("false"),
            Some(true),
            default,
            "LLM_ALLOW_USER_LOCATION",
        )
        .expect("env 'false' must parse");
        assert!(!from_env, "env var must beat TOML when both are set");
        let from_toml = resolve_primitive(
            None,
            Some(false),
            default,
            "LLM_ALLOW_USER_LOCATION",
        )
        .expect("toml bool must parse");
        assert!(!from_toml, "TOML value must apply when env is unset");
        let from_default =
            resolve_primitive::<bool>(None, None, default, "LLM_ALLOW_USER_LOCATION")
                .expect("default must parse");
        assert!(from_default, "default must win when neither is set");
    }

    #[test]
    fn csv_helper_trims_and_dedups_blanks() {
        // env wins over TOML; splits + trims + drops empties
        assert_eq!(
            resolve_csv(
                Some("a, b , ,c"),
                Some(vec!["should-not-appear".into()]),
                vec!["default".into()]
            ),
            vec!["a".to_string(), "b".to_string(), "c".to_string()]
        );
        // env unset → TOML wins
        assert_eq!(
            resolve_csv(
                None,
                Some(vec!["x".into(), "y".into()]),
                vec!["default".into()]
            ),
            vec!["x".to_string(), "y".to_string()]
        );
        // both unset → default wins
        assert_eq!(
            resolve_csv(None, None, vec!["d1".into(), "d2".into()]),
            vec!["d1".to_string(), "d2".to_string()]
        );
    }

    #[test]
    fn bind_addr_uses_default_when_neither_is_set() {
        let addr = resolve_bind_addr(None, None).expect("default bind addr must parse");
        assert_eq!(addr.to_string(), "0.0.0.0:8080");
    }

    #[test]
    fn bind_addr_prefers_env_over_toml() {
        let addr = resolve_bind_addr(Some("127.0.0.1:9001"), Some("127.0.0.1:9002"))
            .expect("env override must parse");
        assert_eq!(addr.to_string(), "127.0.0.1:9001");
    }

    #[test]
    fn model_path_is_required_when_neither_is_set() {
        let err = resolve_model_path(None, None).unwrap_err();
        assert!(matches!(err, ConfigError::MissingModelPath));
    }

    #[test]
    fn model_path_falls_back_to_toml_when_env_unset() {
        let p = resolve_model_path(None, Some("/from/toml.bin")).expect("toml path must parse");
        assert_eq!(p, PathBuf::from("/from/toml.bin"));
    }

    /// `CliArgs::parse` accepts both `--config PATH` and the
    /// `--config=PATH` form so operators don't have to remember which
    /// one the binary supports.
    #[test]
    fn cli_args_parse_space_and_equals_forms() {
        // Drive `parse()` against a synthetic argv by re-implementing
        // the loop inline; `std::env::args` is process-global so we
        // can't stub it from a unit test.
        fn run(argv: &[&str]) -> Result<CliArgs, String> {
            let mut iter = argv.iter().copied();
            let mut out = CliArgs::default();
            while let Some(arg) = iter.next() {
                if arg == "--config" {
                    let v = iter.next().ok_or("missing value".to_string())?;
                    out.config = Some(PathBuf::from(v));
                } else if let Some(v) = arg.strip_prefix("--config=") {
                    out.config = Some(PathBuf::from(v));
                } else {
                    return Err(format!("unknown arg: {arg}"));
                }
            }
            Ok(out)
        }
        assert_eq!(
            run(&["--config", "/etc/nagent.toml"]).unwrap().config,
            Some(PathBuf::from("/etc/nagent.toml"))
        );
        assert_eq!(
            run(&["--config=/etc/nagent.toml"]).unwrap().config,
            Some(PathBuf::from("/etc/nagent.toml"))
        );
        assert!(run(&[]).unwrap().config.is_none());
        assert!(run(&["--bogus"]).is_err());
    }

    #[test]
    fn xdg_path_uses_xdg_config_home_when_set() {
        let p = xdg_config_path_from(Some("/custom/xdg"), Some("/home/test"))
            .expect("must resolve when XDG is set");
        assert_eq!(p, PathBuf::from("/custom/xdg/nagent/config.toml"));
    }

    #[test]
    fn xdg_path_falls_back_to_home_dot_config() {
        let p = xdg_config_path_from(None, Some("/home/test")).expect("HOME fallback must resolve");
        assert_eq!(p, PathBuf::from("/home/test/.config/nagent/config.toml"));
    }

    #[test]
    fn xdg_path_treats_empty_xdg_as_unset() {
        // XDG Base Directory spec: empty $XDG_CONFIG_HOME must be
        // treated as if it were unset.
        let p = xdg_config_path_from(Some(""), Some("/home/test"))
            .expect("empty XDG must fall through to HOME");
        assert_eq!(p, PathBuf::from("/home/test/.config/nagent/config.toml"));
    }

    #[test]
    fn xdg_path_returns_none_when_no_env() {
        assert!(xdg_config_path_from(None, None).is_none());
        assert!(xdg_config_path_from(Some(""), None).is_none());
    }

    #[test]
    fn layered_load_skips_missing_files_and_merges_existing() {
        // Write two real files into a temp dir and pass their paths to
        // a refactored layered loader — the public layered loader
        // hardcodes /etc + XDG, which we can't exercise from a unit
        // test (no permission to write /etc on most runners).
        let dir = std::env::temp_dir().join(format!(
            "nagent-layered-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();

        let system = dir.join("system.toml");
        let xdg = dir.join("xdg.toml");
        let missing = dir.join("missing.toml");
        std::fs::write(
            &system,
            r#"
                [server]
                bind_addr = "127.0.0.1:1111"
                max_queue = 8

                [agents]
                enabled = true

                [agents.web_fetch]
                allow_public = false
                max_bytes = 1024
            "#,
        )
        .unwrap();
        std::fs::write(
            &xdg,
            r#"
                [server]
                max_queue = 64

                [agents.web_fetch]
                allow_public = true
            "#,
        )
        .unwrap();

        let paths = vec![system.clone(), missing, xdg.clone()];
        let merged = load_layered_paths(&paths)
            .expect("layered load must succeed")
            .expect("merged config must be Some when files exist");

        // bind_addr only in system → kept.
        let server = merged.server.expect("server merged");
        assert_eq!(server.bind_addr.as_deref(), Some("127.0.0.1:1111"));
        // max_queue in both → xdg (later) wins.
        assert_eq!(server.max_queue, Some(64));
        // agents.enabled only in system → kept.
        let agents = merged.agents.expect("agents merged");
        assert_eq!(agents.enabled, Some(true));
        // agents.web_fetch.allow_public in both → xdg wins.
        let wf = agents.web_fetch.expect("web_fetch merged");
        assert_eq!(wf.allow_public, Some(true));
        // agents.web_fetch.max_bytes only in system → kept.
        assert_eq!(wf.max_bytes, Some(1024));

        // Cleanup.
        let _ = std::fs::remove_file(&system);
        let _ = std::fs::remove_file(&xdg);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn layered_load_returns_none_when_all_files_missing() {
        let dir = std::env::temp_dir().join(format!(
            "nagent-layered-empty-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let a = dir.join("a.toml");
        let b = dir.join("b.toml");
        let merged = load_layered_paths(&[a, b]).expect("missing files are not an error");
        assert!(
            merged.is_none(),
            "no existing file → None, mirroring the no-TOML path"
        );
    }

    /// Test seam for [`load_layered_default_config`]: identical logic
    /// but takes the candidate paths as an argument so unit tests can
    /// point at writable temp dirs instead of `/etc/nagent/`.
    fn load_layered_paths(paths: &[PathBuf]) -> Result<Option<TomlConfig>, ConfigError> {
        let mut merged: Option<TomlConfig> = None;
        for path in paths {
            if !path.exists() {
                continue;
            }
            let next = load_toml(path)?;
            merged = Some(match merged {
                Some(prev) => merge_toml_configs(&prev, &next),
                None => next,
            });
        }
        Ok(merged)
    }
}
