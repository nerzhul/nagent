//! `config/` — server configuration grouped by section.
//!
//! Each section has its own sub-module stub (see below). The
//! runtime struct + resolver + inline tests for every section
//! still live in this file; future commits will move them into
//! the dedicated sub-modules. The stubs re-export each type so
//! canonical paths like `crate::config::limits::LimitsConfig`
//! already resolve.
//!
//! ## Section layout
//!
//! - [`server`] — `Config`, `CliArgs`, env/TOML merge helpers, `ConfigError`.
//! - [`ratelimit`] — per-IP rate-limit knobs (`RateLimitConfig`).
//! - [`trusted_proxies`] — `[server].trusted_proxies` CIDR list.
//! - [`limits`] — inbound WebSocket frame limits (`LimitsConfig`).
//! - [`llm`] — `[llm]` section + `LlmAuthMode`.
//! - [`tts`] — `[tts]` section.
//! - [`auth`] — `[auth]` section + per-backend sub-configs.
//! - [`agents`] — `[agents]` section + per-agent sub-configs.
//! - [`documents`] — `[documents]` section.

pub mod agents;
pub mod auth;
pub mod documents;
pub mod limits;
pub mod llm;
pub mod ratelimit;
pub mod server;
pub mod trusted_proxies;
pub mod tts;

// Re-export the TOML mirror module so `crate::config_file::TomlConfig`
// keeps resolving unchanged. The mirror is kept separate from the
// runtime resolvers here so a future operator-UI / `serde` schema
// change does not drag the resolution logic along.
pub use crate::config_file as file;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::config_file::{
    merge_toml_configs, TomlAgentConfig, TomlAuthConfig, TomlConfig, TomlLimitsConfig,
    TomlRateLimitConfig, TomlTrustedProxiesConfig, TomlTtsConfig,
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
    /// Operator override for the inference worker pool size. `None`
    /// means "let the server pick a default from the backend's
    /// [`stt_core::BackendInfo`]" (P0 + P2 of the perf plan).
    pub inference_workers: Option<usize>,
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
    /// Trusted-proxy CIDR list (security plan #5). When non-empty
    /// the rate-limit resolver will honour the `X-Forwarded-For`
    /// header for connections whose peer IP is in the list.
    /// Empty by default — operators behind a reverse proxy must
    /// opt in via `[server].trusted_proxies.cidr` (or
    /// `NAGENT_TRUSTED_PROXIES`).
    pub trusted_proxies: TrustedProxiesConfig,
    /// Optional Piper TTS engine. When `tts.enabled` is `false` the
    /// `/v1/audio/*` routes are not registered and the discussion-mode
    /// "Read response aloud" UI shows no checkbox. The module is
    /// compiled unconditionally and runtime-gated by `TTS_ENABLED`
    /// (mirrors how `[llm]` is treated), so disabling TTS does not
    /// shrink the binary — it just hides the routes.
    pub tts: TtsConfig,
    /// Optional authentication & user-identity subsystem (PR1). When
    /// `auth.enabled` is `false` the server keeps the pre-PR1 single
    /// user trust boundary: no login routes, no `/api/me`, no
    /// `RequireAuth` layer. Compiled only when the `auth` cargo
    /// feature is enabled — when the feature is off, this is an
    /// inert default config (always `enabled = false`).
    pub auth: AuthConfig,
    /// Discussion-mode document uploads + the `read_document` LLM
    /// tool. Master switch + the per-knob limits. The module is
    /// always compiled so a build without the `documents` cargo
    /// feature still parses the section; the runtime flag controls
    /// whether the `/v1/documents*` routes are mounted and the
    /// `read_document` agent is registered.
    pub documents: DocumentsConfig,
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

/// Trusted-proxy configuration (security plan #5).
///
/// The rate-limit resolver uses `cidr` to decide whether
/// `X-Forwarded-For` is trustworthy for a given TCP connection:
/// the peer IP must fall inside one of the listed CIDR ranges
/// for the header to influence the bucket key. Direct
/// connections from outside the list are always keying on the
/// peer IP, so a public client cannot forge a header to push
/// themselves into a different bucket.
///
/// `loopback_bypass` controls whether loopback IPs skip the
/// bucket entirely. Defaults to `true` (the historical dev
/// default); boot emits a `WARN` when this is `true` and the
/// bind address is non-loopback, which is almost always a
/// misconfiguration.
#[derive(Debug, Clone)]
pub struct TrustedProxiesConfig {
    /// Parsed CIDR list. Empty when no proxy is configured.
    pub cidrs: Vec<ipnet::IpNet>,
    /// When `true`, requests whose resolved peer IP is loopback
    /// (`127.0.0.0/8` or `::1`) bypass the limiter entirely.
    pub loopback_bypass: bool,
}

impl Default for TrustedProxiesConfig {
    fn default() -> Self {
        Self {
            cidrs: Vec::new(),
            loopback_bypass: true,
        }
    }
}

impl TrustedProxiesConfig {
    /// `true` when the peer IP falls in any of the configured
    /// CIDR ranges. Used by the rate-limit resolver to decide
    /// whether to honour `X-Forwarded-For`.
    pub fn is_trusted(&self, ip: std::net::IpAddr) -> bool {
        self.cidrs.iter().any(|net| net.contains(&ip))
    }

    /// Convenience for `loopback_bypass && ip.is_loopback()`.
    pub fn loopback_is_bypassed(&self, ip: std::net::IpAddr) -> bool {
        self.loopback_bypass && ip.is_loopback()
    }
}

/// Limits applied to inbound WebSocket frames.
///
/// See [`crate::stt::ws_handler::handle_inbound`] for the validation
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
        // `INFERENCE_WORKERS` is an optional override: when the env var
        // and the TOML key are both unset we keep `None` and let
        // `main` pick a default from the loaded backend's
        // `BackendInfo` (P0/P2 of the perf plan). The value is clamped
        // to at least 1 — `WorkerPool::spawn` panics on 0.
        let inference_workers = match (
            env_opt("INFERENCE_WORKERS").as_deref(),
            server.and_then(|s| s.inference_workers),
        ) {
            (Some(v), _) => {
                Some(resolve_primitive::<usize>(Some(v), None, 1, "INFERENCE_WORKERS")?.max(1))
            }
            (None, Some(v)) => Some(v.max(1)),
            (None, None) => None,
        };

        let limits = LimitsConfig::from_env_with_toml(server.and_then(|s| s.limits.as_ref()))?;
        let llm = LlmConfig::from_env_with_toml(toml.and_then(|t| t.llm.as_ref()))?;
        let agents = AgentConfig::from_env_with_toml(toml.and_then(|t| t.agents.as_ref()))?;
        let rate_limit =
            RateLimitConfig::from_env_with_toml(server.and_then(|s| s.rate_limits.as_ref()))?;
        let trusted_proxies = TrustedProxiesConfig::from_env_with_toml(
            server.and_then(|s| s.trusted_proxies.as_ref()),
        )?;
        let tts = TtsConfig::from_env_with_toml(toml.and_then(|t| t.tts.as_ref()))?;
        let auth = AuthConfig::from_env_with_toml(toml.and_then(|t| t.auth.as_ref()))?;
        let documents =
            DocumentsConfig::from_env_with_toml(toml.and_then(|t| t.documents.as_ref()))?;

        Ok(Self {
            bind_addr,
            whisper_model_path,
            max_queue,
            inference_workers,
            session_idle_timeout,
            infer_timeout,
            limits,
            rate_limit,
            trusted_proxies,
            llm,
            agents,
            tts,
            auth,
            documents,
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

impl TrustedProxiesConfig {
    /// Read the trusted-proxy config from the env var / TOML pair.
    /// Env `NAGENT_TRUSTED_PROXIES` is a comma-separated CIDR list.
    /// Env `NAGENT_TRUSTED_PROXIES_LOOPBACK_BYPASS` (bool) overrides
    /// the TOML `loopback_bypass`. Invalid CIDR strings fail boot.
    fn from_env_with_toml(toml: Option<&TomlTrustedProxiesConfig>) -> Result<Self, ConfigError> {
        let defaults = Self::default();
        let toml = toml.cloned().unwrap_or_default();

        // Env wins over TOML when both are set (consistent with the
        // rest of the config layer).
        let cidr_raw = env_opt("NAGENT_TRUSTED_PROXIES")
            .or_else(|| toml.cidr.clone())
            .unwrap_or_default();
        let mut cidrs = Vec::new();
        for entry in cidr_raw.split(',') {
            let entry = entry.trim();
            if entry.is_empty() {
                continue;
            }
            let net: ipnet::IpNet = entry.parse().map_err(|e: ipnet::AddrParseError| {
                ConfigError::InvalidEnv(
                    "NAGENT_TRUSTED_PROXIES".into(),
                    format!("invalid CIDR `{entry}`: {e}"),
                )
            })?;
            cidrs.push(net);
        }

        let loopback_bypass = resolve_primitive(
            env_opt("NAGENT_TRUSTED_PROXIES_LOOPBACK_BYPASS").as_deref(),
            toml.loopback_bypass,
            defaults.loopback_bypass,
            "NAGENT_TRUSTED_PROXIES_LOOPBACK_BYPASS",
        )?;

        Ok(Self {
            cidrs,
            loopback_bypass,
        })
    }
}

/// Configuration for the optional server-side Piper TTS engine.
///
/// When `enabled` is `false` the `/v1/audio/*` routes are simply not
/// registered, so the discussion-mode "Read response aloud" checkbox
/// disappears and the rest of the server is unaffected. The engine is
/// compiled unconditionally and runtime-gated by `TTS_ENABLED`
/// (mirroring the `[llm]` pattern) so disabling TTS does not shrink
/// the binary — it just hides the routes.
///
/// Voice files are downloaded separately by the operator and stored
/// under `model_dir` as one `<voice>.onnx` + `<voice>.onnx.json` pair
/// per voice. See `scripts/download-piper-voices.sh` and the README
/// "Text-to-Speech (Piper)" section.
#[derive(Debug, Clone)]
pub struct TtsConfig {
    /// Master switch for the `/v1/audio/*` routes.
    pub enabled: bool,
    /// Directory holding the Piper voice checkpoints
    /// (`<voice>.onnx` + `<voice>.onnx.json`). Only consulted when
    /// `enabled = true`; ignored otherwise.
    pub model_dir: PathBuf,
    /// Default voice id used when the request does not specify one
    /// (English-language fallback).
    pub voice_en: String,
    /// Default voice id used when the request does not specify one
    /// (French-language fallback).
    pub voice_fr: String,
    /// Default language code used when the client does not specify
    /// one. `"en"` or `"fr"` map to `voice_en` / `voice_fr`. Any
    /// other value falls back to `voice_en`.
    pub default_lang: String,
    /// Piper `length_scale`: `>1.0` = slower, `<1.0` = faster,
    /// `1.0` = neutral. Per-request `speed` overrides this.
    pub length_scale: f32,
    /// Piper `noise_scale` — controls the variability of the
    /// synthesized audio. Defaults to `0.667` (Piper upstream).
    pub noise_scale: f32,
    /// Piper `noise_w` — controls the variability of the phoneme
    /// durations. Defaults to `0.8` (Piper upstream).
    pub noise_w: f32,
    /// Hard cap on the number of characters accepted in a single
    /// `POST /v1/audio/speech` request body. Defends against
    /// pathological LLM responses that stream a 50 KB paragraph in
    /// one shot.
    pub max_input_chars: usize,
}

impl Default for TtsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            model_dir: PathBuf::from("./models/piper"),
            voice_en: "en_US-lessac-medium".to_string(),
            voice_fr: "fr_FR-upmc-medium".to_string(),
            default_lang: "en".to_string(),
            length_scale: 1.0,
            noise_scale: 0.667,
            noise_w: 0.8,
            max_input_chars: 2_000,
        }
    }
}

impl TtsConfig {
    fn from_env_with_toml(toml: Option<&TomlTtsConfig>) -> Result<Self, ConfigError> {
        let defaults = Self::default();
        let toml = toml.cloned().unwrap_or_default();
        let enabled = resolve_primitive(
            env_opt("TTS_ENABLED").as_deref(),
            toml.enabled,
            defaults.enabled,
            "TTS_ENABLED",
        )?;
        // The model dir is only meaningful when TTS is on. We still
        // resolve it from the env / TOML so misconfigurations show up
        // at boot rather than at the first request, but a default of
        // `./models/piper` keeps first-run harmless.
        let model_dir = resolve_opt_string(
            env_opt("TTS_MODEL_DIR").as_deref(),
            toml.model_dir.as_deref(),
        )
        .map(PathBuf::from)
        .unwrap_or(defaults.model_dir.clone());
        let voice_en =
            resolve_opt_string(env_opt("TTS_VOICE_EN").as_deref(), toml.voice_en.as_deref())
                .unwrap_or_else(|| defaults.voice_en.clone());
        let voice_fr =
            resolve_opt_string(env_opt("TTS_VOICE_FR").as_deref(), toml.voice_fr.as_deref())
                .unwrap_or_else(|| defaults.voice_fr.clone());
        let default_lang = resolve_opt_string(
            env_opt("TTS_DEFAULT_LANG").as_deref(),
            toml.default_lang.as_deref(),
        )
        .unwrap_or_else(|| defaults.default_lang.clone());
        let length_scale = resolve_primitive(
            env_opt("TTS_LENGTH_SCALE").as_deref(),
            toml.length_scale,
            defaults.length_scale,
            "TTS_LENGTH_SCALE",
        )?
        .clamp(0.1, 5.0);
        let noise_scale = resolve_primitive(
            env_opt("TTS_NOISE_SCALE").as_deref(),
            toml.noise_scale,
            defaults.noise_scale,
            "TTS_NOISE_SCALE",
        )?
        .clamp(0.0, 5.0);
        let noise_w = resolve_primitive(
            env_opt("TTS_NOISE_W").as_deref(),
            toml.noise_w,
            defaults.noise_w,
            "TTS_NOISE_W",
        )?
        .clamp(0.0, 5.0);
        let max_input_chars = resolve_primitive(
            env_opt("TTS_MAX_INPUT_CHARS").as_deref(),
            toml.max_input_chars,
            defaults.max_input_chars,
            "TTS_MAX_INPUT_CHARS",
        )?;
        Ok(Self {
            enabled,
            model_dir,
            voice_en,
            voice_fr,
            default_lang,
            length_scale,
            noise_scale,
            noise_w,
            max_input_chars,
        })
    }
}

/// Authentication mode applied to inbound `/v1/*` requests.
///
/// `Disabled` is a deliberate convenience for trusted local deployments
/// where the bind address is loopback and the operator accepts that
/// anyone on the same host can drive the LLM. `Bearer` requires an
/// `Authorization: Bearer <key>` header on every `/v1/*` request and
/// returns `401 Unauthorized` when the header is missing or the key
/// does not match `api_key`. `Forward` is the historical behaviour:
/// the server only attaches an `Authorization` header on outbound
/// upstream requests (when `api_key` is set) and never inspects the
/// inbound header — equivalent to `Disabled` for the local trust
/// model but documented as a separate value so the operator has to
/// make the choice explicit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LlmAuthMode {
    /// Require `Authorization: Bearer <key>` on every `/v1/*` request.
    Bearer,
    /// Do not inspect the inbound header. Kept for backwards
    /// compatibility with deployments that already gate the proxy via
    /// a reverse proxy.
    #[default]
    Forward,
    /// Explicit "no auth". Same runtime behaviour as `Forward` but
    /// distinguishes deployments that have deliberately opted out so
    /// `main` can emit a startup warning when the server binds a
    /// non-loopback address.
    Disabled,
}

impl LlmAuthMode {
    /// Parse the human-friendly form (`disabled`, `bearer`, `forward`).
    /// Case-insensitive; unknown values surface as `InvalidEnv` so a
    /// typo in the config never silently reverts to the default.
    fn parse(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "bearer" => Ok(Self::Bearer),
            "forward" => Ok(Self::Forward),
            "disabled" => Ok(Self::Disabled),
            other => Err(format!(
                "expected one of `bearer`, `forward`, `disabled`, got `{other}`"
            )),
        }
    }
}

impl From<String> for LlmAuthMode {
    /// Conversion used by the TOML path: the TOML file holds the
    /// human-friendly form (`"bearer"`, `"forward"`, `"disabled"`).
    /// Unknown values fall back to [`LlmAuthMode::default`] so a typo
    /// in a config file never prevents the server from booting — the
    /// env-var path is the strict one because the operator is more
    /// likely to spot a startup error than a silently-ignored file
    /// value.
    fn from(s: String) -> Self {
        Self::parse(&s).unwrap_or_default()
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
    /// Optional bearer token to forward as `Authorization: Bearer …`
    /// on outbound upstream requests. Unrelated to `inbound_auth_key`
    /// below: setting this lets the proxy talk to an authenticated
    /// upstream without making the inbound `/v1/*` surface private.
    pub api_key: Option<String>,
    /// Bearer key required on inbound `/v1/*` requests when
    /// `auth_mode = Bearer`. Has no effect when `auth_mode` is
    /// `Forward` or `Disabled` — the proxy never inspects the
    /// inbound `Authorization` header in those modes.
    pub inbound_auth_key: Option<String>,
    /// How the proxy authenticates inbound `/v1/*` requests. Defaults
    /// to [`LlmAuthMode::Forward`] (historical behaviour: outbound
    /// header forwarding only). See [`LlmAuthMode`] for the full
    /// semantics.
    pub auth_mode: LlmAuthMode,
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
    /// Whether the browser is allowed to forward the user's IANA
    /// timezone to the LLM as part of the request. The browser still
    /// asks for explicit consent in the Advanced drawer; this flag is
    /// a defence-in-depth kill-switch so operators handling sensitive
    /// deployments can strip the ephemeral `The user's local
    /// timezone is` system message before it ever reaches the
    /// upstream model, regardless of what the browser sends. Env var
    /// `LLM_ALLOW_USER_TIMEZONE`, TOML key
    /// `[llm].allow_user_timezone`. Defaults to `true` — the user's
    /// consent in the UI is the primary gate. Independent from
    /// `allow_user_location`: an operator may forbid one without
    /// touching the other.
    pub allow_user_timezone: bool,
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
        let inbound_auth_key = resolve_opt_string(
            env_opt("LLM_API_KEY").as_deref(),
            toml.inbound_auth_key.as_deref(),
        );
        let auth_mode = match env_opt("LLM_AUTH_MODE").as_deref() {
            Some(v) => LlmAuthMode::parse(v)
                .map_err(|e| ConfigError::InvalidEnv("LLM_AUTH_MODE".into(), e))?,
            None => toml.auth_mode.map(LlmAuthMode::from).unwrap_or_default(),
        };
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
        let allow_user_timezone = resolve_primitive(
            env_opt("LLM_ALLOW_USER_TIMEZONE").as_deref(),
            toml.allow_user_timezone,
            defaults.allow_user_timezone,
            "LLM_ALLOW_USER_TIMEZONE",
        )?;

        Ok(Self {
            enabled,
            base_url,
            default_model,
            api_key,
            inbound_auth_key,
            auth_mode,
            request_timeout,
            cors_allow_origins,
            system_prompt,
            allow_user_location,
            allow_user_timezone,
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
            inbound_auth_key: None,
            auth_mode: LlmAuthMode::default(),
            request_timeout: Duration::from_secs(120),
            cors_allow_origins: Vec::new(),
            system_prompt: None,
            allow_user_location: true,
            allow_user_timezone: true,
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
    /// Knobs for the `unit_convert` agent. Empty by default (the agent
    /// has no API key and a sensible default timeout/base URL).
    pub unit_convert: UnitConvertConfig,
    /// Knobs for the `wikipedia` agent. Empty by default (no API key,
    /// only a `User-Agent` header is required by Wikimedia).
    pub wikipedia: WikipediaConfig,
    /// Knobs for the `dictionary` agent (no API key — anonymous
    /// Free Dictionary API).
    pub dictionary: DictionaryConfig,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            llm_max_tool_rounds: 4,
            web_fetch: WebFetchConfig::default(),
            weather: WeatherConfig::default(),
            unit_convert: UnitConvertConfig::default(),
            wikipedia: WikipediaConfig::default(),
            dictionary: DictionaryConfig::default(),
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
            unit_convert: UnitConvertConfig::from_env_with_toml(toml.unit_convert.as_ref())?,
            wikipedia: WikipediaConfig::from_env_with_toml(toml.wikipedia.as_ref())?,
            dictionary: DictionaryConfig::from_env_with_toml(toml.dictionary.as_ref())?,
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
    fn from_env_with_toml(
        toml: Option<&crate::config_file::TomlUnitConvertConfig>,
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
    fn from_env_with_toml(
        toml: Option<&crate::config_file::TomlWikipediaConfig>,
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
    fn from_env_with_toml(
        toml: Option<&crate::config_file::TomlDictionaryConfig>,
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
    /// The auth section is enabled but is missing or invalid. We
    /// surface this as a separate variant so the operator gets a
    /// pointed message rather than a generic "invalid env var".
    #[error("invalid auth config: {0}")]
    InvalidAuth(String),
}

/// Authentication & user-identity subsystem (PR1).
///
/// Always present in [`Config`] so the rest of the code does not need
/// feature gates. When the runtime config sets `auth.enabled = false`
/// (the default), `enabled` is `false` and every other field is the
/// "no auth" default — the server then behaves exactly as it did
/// before PR1. When the feature is on, the operator enables the
/// subsystem via `NAGENT_AUTH_ENABLED=true` (or `auth.enabled = true`
/// in the TOML overlay) and picks the per-backend knobs.
///
/// See plan PR1 §"Configuration schema" for the operator-facing
/// reference.
#[derive(Debug, Clone)]
pub struct AuthConfig {
    /// Master switch. When `false`, no auth routes are registered
    /// and `RequireAuth` stays off — the server keeps the pre-PR1
    /// single-user trust boundary. Mirrors `NAGENT_AUTH_ENABLED` /
    /// `[auth].enabled`.
    pub enabled: bool,
    /// Subset of `"local"`, `"oidc"`, `"passkey"` enabled on this
    /// server. Each enabled backend exposes its own login route.
    /// Mirrors `NAGENT_AUTH_BACKENDS` (comma-separated) /
    /// `[auth].backends`.
    pub backends: Vec<AuthBackendKind>,
    /// Public origin used for OIDC/Passkey origin + cookie security
    /// flags. E.g. `https://nagent.example.com`. Mirrors
    /// `[auth].public_url`.
    pub public_url: String,
    /// Absolute session TTL in days, computed at login as
    /// `now() + session_ttl_days`. Range `1..=90`. Mirrors
    /// `NAGENT_AUTH_SESSION_TTL_DAYS` / `[auth].session_ttl_days`.
    pub session_ttl_days: u32,
    /// CSRF token header name. Defaults to `x-csrf-token`. Operators
    /// can rename it to dodge a proxy that strips the default.
    pub csrf_header: String,
    /// DB connection knobs (required when `enabled = true`).
    pub db: AuthDbConfig,
    /// Password backend knobs (argon2id + registration gate).
    pub password: AuthPasswordConfig,
    /// OIDC backend knobs.
    pub oidc: AuthOidcConfig,
    /// Passkey (WebAuthn) backend knobs.
    pub passkey: AuthPasskeyConfig,
    /// Per-user credentials vault knobs. Required when
    /// `enabled = true` AND at least one agent is registered.
    pub credentials: AuthCredentialsConfig,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            backends: Vec::new(),
            public_url: String::new(),
            session_ttl_days: 7,
            csrf_header: "x-csrf-token".to_string(),
            db: AuthDbConfig::default(),
            password: AuthPasswordConfig::default(),
            oidc: AuthOidcConfig::default(),
            passkey: AuthPasskeyConfig::default(),
            credentials: AuthCredentialsConfig::default(),
        }
    }
}

/// Per-user credentials vault knobs. The AES-256-GCM key lives in
/// plaintext inside `[auth.credentials].key` (64 hex chars / 32
/// bytes). Operators who want to keep the secret out of disk should
/// mount the TOML file from an encrypted volume (k8s `Secret`
/// mounted as `subPath`, Vault Agent, etc.); the configuration
/// shape itself does not force plaintext storage on disk.
#[derive(Debug, Clone, Default)]
pub struct AuthCredentialsConfig {
    /// 64 hex chars = 32 raw bytes. Empty string means "unset" —
    /// the `AuthConfig::from_env_with_toml` resolution fails
    /// with a clear error when `auth.enabled = true` AND at
    /// least one agent is registered AND this is empty.
    pub key: String,
}

/// Names of the auth backends an operator can enable. Matches the
/// strings the plan uses in `[auth].backends`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthBackendKind {
    Local,
    Oidc,
    Passkey,
}

impl AuthBackendKind {
    fn parse(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "local" => Ok(Self::Local),
            "oidc" => Ok(Self::Oidc),
            "passkey" => Ok(Self::Passkey),
            other => Err(format!(
                "expected one of `local`, `oidc`, `passkey`, got `{other}`"
            )),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Oidc => "oidc",
            Self::Passkey => "passkey",
        }
    }
}

/// DB connection knobs for the auth store.
#[derive(Debug, Clone)]
pub struct AuthDbConfig {
    /// `"sqlite"` or `"postgres"`. Empty string means "unset" — the
    /// `AuthConfig::from_env_with_toml` resolution fails when
    /// `auth.enabled = true` and this is unset.
    pub backend: String,
    /// Connection URL — see the plan §"DB schema" for the canonical
    /// sqlite + postgres URLs.
    pub url: String,
    /// Maximum simultaneous connections. Defaults to 16.
    pub max_connections: u32,
    /// When `true` (the default), `auth::boot::auto_bootstrap` runs
    /// `migrate up` against the auth DB at server start. Set this
    /// to `false` to disable boot-time auto-migration; operators
    /// then run `stt-server migrate up` themselves before each boot
    /// (e.g. as a separate init container in Kubernetes, or as a
    /// pre-deploy hook in CI).
    pub auto_migrate: bool,
}

impl Default for AuthDbConfig {
    fn default() -> Self {
        Self {
            backend: String::new(),
            url: String::new(),
            max_connections: 16,
            auto_migrate: true,
        }
    }
}

/// Password backend (argon2id) knobs. Mirrors `[auth.password]`.
#[derive(Debug, Clone)]
pub struct AuthPasswordConfig {
    /// Argon2id memory cost in KiB. OWASP 2025 default: 19456
    /// (19 MiB). The resolver clamps this to at least 19456 so a
    /// careless operator cannot silently weaken the hash.
    pub argon2_memory_kib: u32,
    /// Argon2id time cost (iterations). OWASP default: 2.
    pub argon2_iterations: u32,
    /// Argon2id parallelism (lanes). OWASP default: 1.
    pub argon2_parallelism: u32,
    /// Minimum password length accepted by `/api/auth/password/register`.
    pub min_password_length: usize,
    /// When `true`, any logged-in user can POST to
    /// `/api/auth/password/register` to create a new local account.
    pub allow_registration: bool,
}

impl Default for AuthPasswordConfig {
    fn default() -> Self {
        Self {
            argon2_memory_kib: 19_456,
            argon2_iterations: 2,
            argon2_parallelism: 1,
            min_password_length: 8,
            allow_registration: true,
        }
    }
}

/// OIDC backend knobs. Mirrors `[auth.oidc]`.
#[derive(Debug, Clone)]
pub struct AuthOidcConfig {
    /// When `true`, an OIDC user logging in for the first time has a
    /// `users` row created automatically. When `false`, unknown
    /// emails get `403`.
    pub auto_provision: bool,
    /// OIDC issuer URL — the discovery URL is derived from this.
    pub issuer: String,
    pub client_id: String,
    /// OIDC client secret. `NAGENT_AUTH_OIDC_CLIENT_SECRET` env var
    /// overrides the TOML file.
    pub client_secret: Option<String>,
    /// OIDC scopes — defaults to `["openid", "email", "profile"]`.
    pub scopes: Vec<String>,
    /// Optional allow-list of IdP groups. Empty = no restriction.
    pub required_groups: Vec<String>,
    /// IdP claim name mapped onto the local `roles` list (forward-
    /// compat with PR2). Defaults to `groups`.
    pub role_claim: String,
}

impl Default for AuthOidcConfig {
    fn default() -> Self {
        Self {
            auto_provision: true,
            issuer: String::new(),
            client_id: String::new(),
            client_secret: None,
            scopes: vec!["openid".into(), "email".into(), "profile".into()],
            required_groups: Vec::new(),
            role_claim: "groups".into(),
        }
    }
}

/// Passkey (WebAuthn) backend knobs. Mirrors `[auth.passkey]`.
#[derive(Debug, Clone)]
pub struct AuthPasskeyConfig {
    /// When `true`, any logged-in user can enroll a new passkey
    /// without an admin. Mirrors `[auth.passkey].self_registration`.
    pub self_registration: bool,
    /// WebAuthn relying party id (no scheme, no port — e.g.
    /// `nagent.example.com`). MUST match the browser's effective
    /// domain.
    pub rp_id: String,
    /// Human-readable relying party name shown to the user by the
    /// authenticator.
    pub rp_name: String,
    /// Origins the authenticator will accept. Each entry MUST
    /// include scheme + port (e.g. `https://nagent.example.com`).
    pub origins: Vec<String>,
}

impl Default for AuthPasskeyConfig {
    fn default() -> Self {
        Self {
            self_registration: true,
            rp_id: String::new(),
            rp_name: "nagent".into(),
            origins: Vec::new(),
        }
    }
}

impl AuthConfig {
    fn from_env_with_toml(toml: Option<&TomlAuthConfig>) -> Result<Self, ConfigError> {
        let defaults = Self::default();
        let toml = toml.cloned().unwrap_or_default();

        // `NAGENT_AUTH_ENABLED` wins. Missing → TOML → default.
        // Forcing `false` when the env var is exactly "false" / "0"
        // lets an operator disable auth via env even when the
        // TOML file enables it (the canonical 12-factor contract).
        let enabled_raw = env_opt("NAGENT_AUTH_ENABLED");
        let enabled = match enabled_raw.as_deref() {
            Some(v) => resolve_primitive(
                Some(v),
                toml.enabled,
                defaults.enabled,
                "NAGENT_AUTH_ENABLED",
            )?,
            None => toml.enabled.unwrap_or(defaults.enabled),
        };

        // Backend list — env (comma-separated) wins.
        let backends_csv = env_opt("NAGENT_AUTH_BACKENDS");
        let backends_raw: Vec<String> = match backends_csv.as_deref() {
            Some(v) => v
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect(),
            None => toml.backends.unwrap_or_default(),
        };
        if enabled && backends_raw.is_empty() {
            return Err(ConfigError::InvalidAuth(
                "auth.enabled = true but auth.backends is empty; \
                     set [auth].backends = [\"local\"] (or another subset)"
                    .into(),
            ));
        }
        let mut backends = Vec::with_capacity(backends_raw.len());
        for raw in &backends_raw {
            backends.push(AuthBackendKind::parse(raw).map_err(ConfigError::InvalidAuth)?);
        }

        let public_url = resolve_opt_string(
            env_opt("NAGENT_AUTH_PUBLIC_URL").as_deref(),
            toml.public_url.as_deref(),
        )
        .unwrap_or_default();

        let session_ttl_days = resolve_primitive(
            env_opt("NAGENT_AUTH_SESSION_TTL_DAYS").as_deref(),
            toml.session_ttl_days,
            defaults.session_ttl_days,
            "NAGENT_AUTH_SESSION_TTL_DAYS",
        )?
        // Clamp to the documented range so a typo (`0` or `365`)
        // surfaces as a boot error rather than silently weakening
        // the session timeout. `1..=90` matches the NIST / GDPR
        // guidance cited in plan D6a.
        .clamp(1, 90);

        let csrf_header = resolve_opt_string(
            env_opt("NAGENT_AUTH_CSRF_HEADER").as_deref(),
            toml.csrf_header.as_deref(),
        )
        .unwrap_or_else(|| defaults.csrf_header.clone());

        let db = AuthDbConfig::from_toml(toml.db.as_ref())?;
        if enabled {
            if db.backend.is_empty() {
                return Err(ConfigError::InvalidAuth(
                    "auth.enabled = true but [auth.db].backend is unset; \
                         set it to \"sqlite\" or \"postgres\""
                        .into(),
                ));
            }
            if db.url.is_empty() {
                return Err(ConfigError::InvalidAuth(
                    "auth.enabled = true but [auth.db].url is unset; \
                         set it to a sqlite://… or postgres://… URL"
                        .into(),
                ));
            }
            if db.backend != "sqlite" && db.backend != "postgres" {
                return Err(ConfigError::InvalidAuth(format!(
                    "auth.db.backend must be \"sqlite\" or \"postgres\", got {:?}",
                    db.backend
                )));
            }
            // Cross-check: each enabled backend must have its
            // minimum config present so the operator cannot boot
            // a server that just sits at "missing OIDC issuer"
            // until the first login attempt.
            for kind in &backends {
                match kind {
                    AuthBackendKind::Local => { /* password config has sane defaults */ }
                    AuthBackendKind::Oidc => {
                        let oidc = AuthOidcConfig::from_toml(toml.oidc.as_ref())?;
                        if oidc.issuer.is_empty() {
                            return Err(ConfigError::InvalidAuth(
                                "auth.oidc is enabled but [auth.oidc].issuer is unset".into(),
                            ));
                        }
                        if oidc.client_id.is_empty() {
                            return Err(ConfigError::InvalidAuth(
                                "auth.oidc is enabled but [auth.oidc].client_id is unset".into(),
                            ));
                        }
                    }
                    AuthBackendKind::Passkey => {
                        let pk = AuthPasskeyConfig::from_toml(toml.passkey.as_ref())?;
                        if pk.rp_id.is_empty() {
                            return Err(ConfigError::InvalidAuth(
                                "auth.passkey is enabled but [auth.passkey].rp_id is unset".into(),
                            ));
                        }
                        if pk.origins.is_empty() {
                            return Err(ConfigError::InvalidAuth(
                                "auth.passkey is enabled but [auth.passkey].origins is empty"
                                    .into(),
                            ));
                        }
                    }
                }
            }
        }

        // Per-backend config is parsed unconditionally so the
        // operator can supply `oidc` config even when only
        // `local` is currently enabled (and flip on OIDC later
        // without a server restart needing a config edit first).
        let password = AuthPasswordConfig::from_toml(
            toml.password.as_ref(),
            env_opt("NAGENT_AUTH_PASSWORD_ALLOW_REGISTRATION").as_deref(),
        )?;
        let oidc = AuthOidcConfig::from_toml_with_env(toml.oidc.as_ref())?;
        let passkey = AuthPasskeyConfig::from_toml(toml.passkey.as_ref())?;
        let credentials = AuthCredentialsConfig::from_toml(toml.credentials.as_ref())?;

        Ok(Self {
            enabled,
            backends,
            public_url,
            session_ttl_days,
            csrf_header,
            db,
            password,
            oidc,
            passkey,
            credentials,
        })
    }

    /// Returns the cookie `Secure` flag for the configured public URL.
    /// `Secure` must auto-disable on `http://localhost` to keep the
    /// dev story working (a `Secure` cookie set over plain HTTP is
    /// dropped by the browser). For everything else, `Secure` mirrors
    /// the scheme — an operator deploying over HTTPS gets the right
    /// default with no extra knob.
    pub fn cookie_secure(&self) -> bool {
        let url = self.public_url.trim().to_ascii_lowercase();
        if url.starts_with("http://localhost") || url.starts_with("http://127.0.0.1") {
            return false;
        }
        url.starts_with("https://")
    }

    /// Returns the cookie `SameSite` policy. We default to `Lax` so
    /// OIDC callbacks (cross-site GET) still work — `Strict` would
    /// block the IdP redirect. Operators who do not enable OIDC can
    /// tighten to `Strict` via a future knob.
    pub fn cookie_same_site(&self) -> &'static str {
        "Lax"
    }

    /// Cookie name used for the session id on the browser side.
    pub fn cookie_name(&self) -> &'static str {
        "nagent_session"
    }

    /// Returns the enabled backend names as a lowercase slice — used
    /// by the UI's login panel to know which buttons to render.
    pub fn enabled_backend_names(&self) -> Vec<&'static str> {
        self.backends.iter().map(|b| b.as_str()).collect()
    }
}

impl AuthDbConfig {
    fn from_toml(toml: Option<&crate::config_file::TomlAuthDbConfig>) -> Result<Self, ConfigError> {
        let defaults = Self::default();
        let toml = toml.cloned().unwrap_or_default();
        Ok(Self {
            backend: resolve_opt_string(
                env_opt("NAGENT_AUTH_DB_BACKEND").as_deref(),
                toml.backend.as_deref(),
            )
            .unwrap_or_default(),
            url: resolve_opt_string(
                env_opt("NAGENT_AUTH_DB_URL").as_deref(),
                toml.url.as_deref(),
            )
            .unwrap_or_default(),
            max_connections: resolve_primitive(
                env_opt("NAGENT_AUTH_DB_MAX_CONNECTIONS").as_deref(),
                toml.max_connections,
                defaults.max_connections,
                "NAGENT_AUTH_DB_MAX_CONNECTIONS",
            )?
            .max(1),
            auto_migrate: resolve_primitive(
                env_opt("NAGENT_AUTH_DB_AUTO_MIGRATE").as_deref(),
                toml.auto_migrate,
                defaults.auto_migrate,
                "NAGENT_AUTH_DB_AUTO_MIGRATE",
            )?,
        })
    }
}

impl AuthCredentialsConfig {
    /// Read the per-user credentials vault knobs from TOML. The
    /// `key` field holds the AES-256-GCM encryption key in
    /// plaintext (64 hex chars). No env-var indirection: the
    /// value comes from `[auth.credentials].key` and nowhere else,
    /// so operators who want secret confidentiality can mount
    /// the TOML file from an encrypted volume.
    fn from_toml(
        toml: Option<&crate::config_file::TomlAuthCredentialsConfig>,
    ) -> Result<Self, ConfigError> {
        let toml = toml.cloned().unwrap_or_default();
        Ok(Self {
            key: toml.key.unwrap_or_default().trim().to_string(),
        })
    }
}

impl AuthPasswordConfig {
    fn from_toml(
        toml: Option<&crate::config_file::TomlAuthPasswordConfig>,
        env_allow_registration: Option<&str>,
    ) -> Result<Self, ConfigError> {
        let defaults = Self::default();
        let toml = toml.cloned().unwrap_or_default();
        // Clamp memory cost to ≥ 19 456 KiB so an operator cannot
        // silently weaken the hash by typing a smaller value. The
        // OWASP 2025 default (19 MiB) is the floor.
        let argon2_memory_kib = resolve_primitive(
            env_opt("NAGENT_AUTH_PASSWORD_ARGON2_MEMORY_KIB").as_deref(),
            toml.argon2_memory_kib,
            defaults.argon2_memory_kib,
            "NAGENT_AUTH_PASSWORD_ARGON2_MEMORY_KIB",
        )?
        .max(19_456);
        let argon2_iterations = resolve_primitive(
            env_opt("NAGENT_AUTH_PASSWORD_ARGON2_ITERATIONS").as_deref(),
            toml.argon2_iterations,
            defaults.argon2_iterations,
            "NAGENT_AUTH_PASSWORD_ARGON2_ITERATIONS",
        )?
        .max(1);
        let argon2_parallelism = resolve_primitive(
            env_opt("NAGENT_AUTH_PASSWORD_ARGON2_PARALLELISM").as_deref(),
            toml.argon2_parallelism,
            defaults.argon2_parallelism,
            "NAGENT_AUTH_PASSWORD_ARGON2_PARALLELISM",
        )?
        .max(1);
        let min_password_length = resolve_primitive(
            env_opt("NAGENT_AUTH_PASSWORD_MIN_LENGTH").as_deref(),
            toml.min_password_length,
            defaults.min_password_length,
            "NAGENT_AUTH_PASSWORD_MIN_LENGTH",
        )?
        .max(1);
        let allow_registration = match env_allow_registration {
            Some(v) => resolve_primitive(
                Some(v),
                toml.allow_registration,
                defaults.allow_registration,
                "NAGENT_AUTH_PASSWORD_ALLOW_REGISTRATION",
            )?,
            None => toml
                .allow_registration
                .unwrap_or(defaults.allow_registration),
        };
        Ok(Self {
            argon2_memory_kib,
            argon2_iterations,
            argon2_parallelism,
            min_password_length,
            allow_registration,
        })
    }
}

impl AuthOidcConfig {
    fn from_toml_with_env(
        toml: Option<&crate::config_file::TomlAuthOidcConfig>,
    ) -> Result<Self, ConfigError> {
        let defaults = Self::default();
        let toml = toml.cloned().unwrap_or_default();
        let auto_provision = resolve_primitive(
            env_opt("NAGENT_AUTH_OIDC_AUTO_PROVISION").as_deref(),
            toml.auto_provision,
            defaults.auto_provision,
            "NAGENT_AUTH_OIDC_AUTO_PROVISION",
        )?;
        let issuer = resolve_opt_string(
            env_opt("NAGENT_AUTH_OIDC_ISSUER").as_deref(),
            toml.issuer.as_deref(),
        )
        .unwrap_or_default();
        let client_id = resolve_opt_string(
            env_opt("NAGENT_AUTH_OIDC_CLIENT_ID").as_deref(),
            toml.client_id.as_deref(),
        )
        .unwrap_or_default();
        let client_secret = resolve_opt_string(
            env_opt("NAGENT_AUTH_OIDC_CLIENT_SECRET").as_deref(),
            toml.client_secret.as_deref(),
        );
        let scopes = resolve_csv(
            env_opt("NAGENT_AUTH_OIDC_SCOPES").as_deref(),
            toml.scopes,
            defaults.scopes,
        );
        let required_groups = resolve_csv(
            env_opt("NAGENT_AUTH_OIDC_REQUIRED_GROUPS").as_deref(),
            toml.required_groups,
            defaults.required_groups,
        );
        let role_claim = resolve_opt_string(
            env_opt("NAGENT_AUTH_OIDC_ROLE_CLAIM").as_deref(),
            toml.role_claim.as_deref(),
        )
        .unwrap_or_else(|| defaults.role_claim.clone());
        Ok(Self {
            auto_provision,
            issuer,
            client_id,
            client_secret,
            scopes,
            required_groups,
            role_claim,
        })
    }

    fn from_toml(
        toml: Option<&crate::config_file::TomlAuthOidcConfig>,
    ) -> Result<Self, ConfigError> {
        Self::from_toml_with_env(toml)
    }
}

impl AuthPasskeyConfig {
    fn from_toml(
        toml: Option<&crate::config_file::TomlAuthPasskeyConfig>,
    ) -> Result<Self, ConfigError> {
        let defaults = Self::default();
        let toml = toml.cloned().unwrap_or_default();
        let self_registration = resolve_primitive(
            env_opt("NAGENT_AUTH_PASSKEY_SELF_REGISTRATION").as_deref(),
            toml.self_registration,
            defaults.self_registration,
            "NAGENT_AUTH_PASSKEY_SELF_REGISTRATION",
        )?;
        let rp_id = resolve_opt_string(
            env_opt("NAGENT_AUTH_PASSKEY_RP_ID").as_deref(),
            toml.rp_id.as_deref(),
        )
        .unwrap_or_default();
        let rp_name = resolve_opt_string(
            env_opt("NAGENT_AUTH_PASSKEY_RP_NAME").as_deref(),
            toml.rp_name.as_deref(),
        )
        .unwrap_or_else(|| defaults.rp_name.clone());
        let origins = resolve_csv(
            env_opt("NAGENT_AUTH_PASSKEY_ORIGINS").as_deref(),
            toml.origins,
            defaults.origins,
        );
        Ok(Self {
            self_registration,
            rp_id,
            rp_name,
            origins,
        })
    }
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

    /// `inference_workers` defaults to `None` so the server falls back
    /// to the backend-derived recommendation at startup. Operators
    /// that need a different count can pin it via `[server].inference_workers`
    /// in the TOML overlay; the resolution helper mirrors the
    /// `env > TOML > default` precedence.
    #[test]
    fn inference_workers_resolution_precedence() {
        // TOML-only value is picked up.
        let toml = toml_from(
            r#"
                [server]
                whisper_model_path = "/tmp/m.bin"
                inference_workers = 4
            "#,
        );
        let cfg = Config::from_env_with_toml(Some(&toml)).expect("config must load");
        assert_eq!(cfg.inference_workers, Some(4), "TOML value must apply");

        // Invalid TOML value (zero) must be clamped to 1, never panic.
        let toml = toml_from(
            r#"
                [server]
                whisper_model_path = "/tmp/m.bin"
                inference_workers = 0
            "#,
        );
        let cfg = Config::from_env_with_toml(Some(&toml)).expect("config must load");
        assert_eq!(cfg.inference_workers, Some(1), "zero must clamp to 1");
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
        let from_toml = resolve_primitive(None, Some(false), default, "LLM_ALLOW_USER_LOCATION")
            .expect("toml bool must parse");
        assert!(!from_toml, "TOML value must apply when env is unset");
        let from_default =
            resolve_primitive::<bool>(None, None, default, "LLM_ALLOW_USER_LOCATION")
                .expect("default must parse");
        assert!(from_default, "default must win when neither is set");
    }

    #[test]
    fn llm_allow_user_timezone_env_overrides_toml_and_default() {
        // Same precedence contract as `allow_user_location`, exercised
        // on the new timezone kill-switch. The two flags must
        // resolve independently — a future refactor that shares the
        // resolve call between them would silently couple the
        // behaviour and this test would catch it.
        let default = LlmConfig::default().allow_user_timezone;
        assert!(default, "allow_user_timezone must default to true");
        let from_env = resolve_primitive(
            Some("false"),
            Some(true),
            default,
            "LLM_ALLOW_USER_TIMEZONE",
        )
        .expect("env 'false' must parse");
        assert!(!from_env, "env var must beat TOML when both are set");
        let from_toml = resolve_primitive(None, Some(false), default, "LLM_ALLOW_USER_TIMEZONE")
            .expect("toml bool must parse");
        assert!(!from_toml, "TOML value must apply when env is unset");
        let from_default =
            resolve_primitive::<bool>(None, None, default, "LLM_ALLOW_USER_TIMEZONE")
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

    #[test]
    fn llm_auth_mode_parse_accepts_known_values() {
        // Lowercase + trimmed variants all map to the same variant so
        // an operator can hand-type either case.
        assert!(matches!(
            LlmAuthMode::parse("bearer").unwrap(),
            LlmAuthMode::Bearer
        ));
        assert!(matches!(
            LlmAuthMode::parse("FORWARD").unwrap(),
            LlmAuthMode::Forward
        ));
        assert!(matches!(
            LlmAuthMode::parse(" Disabled ").unwrap(),
            LlmAuthMode::Disabled
        ));
    }

    #[test]
    fn llm_auth_mode_parse_rejects_unknown_values() {
        // A typo in the env var must surface as an error so the
        // operator sees it at boot rather than silently reverting to
        // the default and wondering why their `Authorization` header
        // is being ignored.
        let err = LlmAuthMode::parse("barer").unwrap_err();
        assert!(
            err.contains("bearer") && err.contains("barer"),
            "error must list valid options and echo the offending value, got: {err}"
        );
    }

    #[test]
    fn llm_auth_mode_default_is_forward() {
        // Documented default: `forward` preserves the pre-auth
        // behaviour so an upgrade does not break deployments that
        // already gate the proxy via a reverse proxy.
        assert!(matches!(LlmAuthMode::default(), LlmAuthMode::Forward));
    }

    #[test]
    fn llm_config_default_keeps_inbound_auth_key_unset() {
        // Default-on path: `LLM_API_KEY` and `LLM_AUTH_MODE` are unset,
        // so the auth gate is a no-op. The `main` warning only fires
        // when the operator binds a non-loopback address AND sets
        // `LLM_AUTH_MODE=disabled` explicitly — leaving both at their
        // defaults must stay silent.
        let cfg = LlmConfig::default();
        assert!(cfg.inbound_auth_key.is_none());
        assert!(matches!(cfg.auth_mode, LlmAuthMode::Forward));
    }

    // ---- Auth config tests (PR1) -----------------------------------------

    /// Helper: minimal TOML that satisfies `[auth].enabled = true` —
    /// the rest of the auth tests layer extra sections on top of this.
    fn minimal_auth_toml() -> TomlConfig {
        toml::from_str(
            r#"
                [server]
                whisper_model_path = "/tmp/m.bin"

                [auth]
                enabled = true
                backends = ["local"]
                session_ttl_days = 7

                [auth.db]
                backend = "sqlite"
                url = "sqlite://./data/auth.db?mode=rwc"
            "#,
        )
        .expect("minimal auth TOML must parse")
    }

    #[test]
    fn auth_defaults_disabled_when_no_toml() {
        // No [auth] section at all → enabled=false, no backends.
        // We need to provide a TOML with a `whisper_model_path` so
        // the no-auth code path doesn't trip on the model-path
        // required knob (which is unrelated to the test).
        let toml: TomlConfig = toml::from_str(
            r#"
                [server]
                whisper_model_path = "/tmp/m.bin"
            "#,
        )
        .unwrap();
        let cfg = Config::from_env_with_toml(Some(&toml)).expect("config must load");
        assert!(!cfg.auth.enabled, "auth must default to disabled");
        assert!(cfg.auth.backends.is_empty());
        assert_eq!(cfg.auth.session_ttl_days, 7);
        assert_eq!(cfg.auth.cookie_name(), "nagent_session");
        assert_eq!(cfg.auth.csrf_header, "x-csrf-token");
    }

    #[test]
    fn auth_toml_overlay_enables_local_backend() {
        let toml = minimal_auth_toml();
        let cfg = Config::from_env_with_toml(Some(&toml)).expect("config must load");
        assert!(cfg.auth.enabled);
        assert_eq!(cfg.auth.backends, vec![AuthBackendKind::Local]);
        assert_eq!(cfg.auth.db.backend, "sqlite");
        assert!(cfg.auth.db.url.contains("auth.db"));
    }

    #[test]
    fn auth_enabled_without_backends_is_rejected() {
        let toml: TomlConfig = toml::from_str(
            r#"
                [server]
                whisper_model_path = "/tmp/m.bin"

                [auth]
                enabled = true
            "#,
        )
        .unwrap();
        let err = Config::from_env_with_toml(Some(&toml)).unwrap_err();
        match err {
            ConfigError::InvalidAuth(msg) => {
                assert!(
                    msg.contains("backends is empty"),
                    "error must mention empty backends, got: {msg}"
                );
            }
            other => panic!("expected InvalidAuth, got {other:?}"),
        }
    }

    #[test]
    fn auth_enabled_without_db_backend_is_rejected() {
        let toml: TomlConfig = toml::from_str(
            r#"
                [server]
                whisper_model_path = "/tmp/m.bin"

                [auth]
                enabled = true
                backends = ["local"]
                # NB: no [auth.db] section
            "#,
        )
        .unwrap();
        let err = Config::from_env_with_toml(Some(&toml)).unwrap_err();
        match err {
            ConfigError::InvalidAuth(msg) => {
                assert!(
                    msg.contains("[auth.db].backend"),
                    "error must mention the missing backend key, got: {msg}"
                );
            }
            other => panic!("expected InvalidAuth, got {other:?}"),
        }
    }

    #[test]
    fn auth_oidc_enabled_without_issuer_is_rejected() {
        let toml: TomlConfig = toml::from_str(
            r#"
                [server]
                whisper_model_path = "/tmp/m.bin"

                [auth]
                enabled = true
                backends = ["oidc"]

                [auth.db]
                backend = "sqlite"
                url = "sqlite://./x.db?mode=rwc"

                [auth.oidc]
                client_id = "nagent"
                # NB: no issuer
            "#,
        )
        .unwrap();
        let err = Config::from_env_with_toml(Some(&toml)).unwrap_err();
        match err {
            ConfigError::InvalidAuth(msg) => {
                assert!(
                    msg.contains("issuer"),
                    "error must mention missing issuer, got: {msg}"
                );
            }
            other => panic!("expected InvalidAuth, got {other:?}"),
        }
    }

    #[test]
    fn auth_passkey_enabled_without_rp_id_is_rejected() {
        let toml: TomlConfig = toml::from_str(
            r#"
                [server]
                whisper_model_path = "/tmp/m.bin"

                [auth]
                enabled = true
                backends = ["passkey"]

                [auth.db]
                backend = "sqlite"
                url = "sqlite://./x.db?mode=rwc"

                [auth.passkey]
                origins = ["https://nagent.example.com"]
                # NB: no rp_id
            "#,
        )
        .unwrap();
        let err = Config::from_env_with_toml(Some(&toml)).unwrap_err();
        match err {
            ConfigError::InvalidAuth(msg) => {
                assert!(
                    msg.contains("rp_id"),
                    "error must mention missing rp_id, got: {msg}"
                );
            }
            other => panic!("expected InvalidAuth, got {other:?}"),
        }
    }

    #[test]
    fn session_ttl_clamped_to_documented_range() {
        // 0 → clamped to 1, 91 → clamped to 90. We do not have to
        // mutate process env for this — `from_env_with_toml` with an
        // explicit TOML exercises the same `clamp(1, 90)` path.
        let mut base = minimal_auth_toml();
        base.auth.as_mut().unwrap().session_ttl_days = Some(0);
        let cfg = Config::from_env_with_toml(Some(&base)).expect("clamp must not error");
        assert_eq!(cfg.auth.session_ttl_days, 1, "0 must clamp to 1");

        base.auth.as_mut().unwrap().session_ttl_days = Some(91);
        let cfg = Config::from_env_with_toml(Some(&base)).expect("clamp must not error");
        assert_eq!(cfg.auth.session_ttl_days, 90, "91 must clamp to 90");

        // 7 / 1 / 90 all pass through unchanged.
        for ok in [1_u32, 7, 90] {
            base.auth.as_mut().unwrap().session_ttl_days = Some(ok);
            let cfg = Config::from_env_with_toml(Some(&base)).expect("value in range must pass");
            assert_eq!(cfg.auth.session_ttl_days, ok);
        }
    }

    #[test]
    fn argon2_memory_clamped_to_owasp_minimum() {
        // The resolver clamps argon2_memory_kib to >= 19456 so a
        // careless operator cannot silently weaken the hash by typing
        // a smaller value.
        let mut base = minimal_auth_toml();
        base.auth.as_mut().unwrap().password = Some(crate::config_file::TomlAuthPasswordConfig {
            argon2_memory_kib: Some(1024),
            argon2_iterations: Some(2),
            argon2_parallelism: Some(1),
            min_password_length: Some(8),
            allow_registration: Some(true),
        });
        let cfg = Config::from_env_with_toml(Some(&base)).expect("clamp must not error");
        assert_eq!(
            cfg.auth.password.argon2_memory_kib, 19_456,
            "1024 must clamp to OWASP minimum"
        );
    }

    #[test]
    fn cookie_secure_auto_disables_on_http_localhost() {
        // Dev story: `http://localhost:8080` must NOT set the Secure
        // flag on cookies, otherwise the browser silently drops them.
        let mut base = minimal_auth_toml();
        base.auth.as_mut().unwrap().public_url = Some("http://localhost:8080".into());
        let cfg = Config::from_env_with_toml(Some(&base)).expect("config must load");
        assert!(
            !cfg.auth.cookie_secure(),
            "http://localhost must be insecure"
        );

        // 127.0.0.1 is the same carve-out.
        base.auth.as_mut().unwrap().public_url = Some("http://127.0.0.1:9000".into());
        let cfg = Config::from_env_with_toml(Some(&base)).expect("config must load");
        assert!(
            !cfg.auth.cookie_secure(),
            "http://127.0.0.1 must be insecure"
        );

        // Plain http to a non-loopback host must also be insecure
        // (we don't try to be clever — http is http).
        base.auth.as_mut().unwrap().public_url = Some("http://nagent.example.com".into());
        let cfg = Config::from_env_with_toml(Some(&base)).expect("config must load");
        assert!(
            !cfg.auth.cookie_secure(),
            "http to any host must be insecure"
        );

        // https to anything loopback or not must be Secure.
        for url in [
            "https://localhost",
            "https://nagent.example.com",
            "https://127.0.0.1:8443",
        ] {
            base.auth.as_mut().unwrap().public_url = Some(url.into());
            let cfg = Config::from_env_with_toml(Some(&base)).expect("config must load");
            assert!(cfg.auth.cookie_secure(), "https must be Secure ({url})");
        }
    }

    #[test]
    fn auth_backend_kinds_parse_case_insensitive() {
        // Operators occasionally typo-case; we accept any case to keep
        // the TOML ergonomic.
        for (raw, expected) in [
            ("local", AuthBackendKind::Local),
            ("LOCAL", AuthBackendKind::Local),
            (" Local ", AuthBackendKind::Local),
            ("Oidc", AuthBackendKind::Oidc),
            ("PASSKEY", AuthBackendKind::Passkey),
        ] {
            assert_eq!(
                AuthBackendKind::parse(raw).expect("known variant must parse"),
                expected,
                "parse({raw:?})"
            );
        }
        assert!(AuthBackendKind::parse("magic-link").is_err());
    }

    #[test]
    fn enabled_backend_names_returns_lowercase_slice() {
        let mut base = minimal_auth_toml();
        base.auth.as_mut().unwrap().backends = Some(vec!["local".into(), "passkey".into()]);
        base.auth.as_mut().unwrap().passkey = Some(crate::config_file::TomlAuthPasskeyConfig {
            self_registration: Some(true),
            rp_id: Some("nagent.example.com".into()),
            rp_name: Some("nagent".into()),
            origins: Some(vec!["https://nagent.example.com".into()]),
        });
        let cfg = Config::from_env_with_toml(Some(&base)).expect("config must load");
        assert_eq!(
            cfg.auth.enabled_backend_names(),
            vec!["local", "passkey"],
            "UI iterates these to decide which buttons to render"
        );
    }
}

// ---------------------------------------------------------------------------
// Documents — Discussion-mode uploads + the `read_document` LLM tool.
// ---------------------------------------------------------------------------

/// Configuration for the optional document-upload subsystem.
///
/// `enabled` is the master switch. When `false`, the `/v1/documents*`
/// routes are not mounted and the `read_document` agent is not
/// registered. The config is always parsed (no cargo feature gate on
/// the env / TOML plumbing) so a build without the `documents` cargo
/// feature still surfaces configuration mistakes at boot time — the
/// `documents` feature only controls the heavy deps (`pdf-extract` +
/// `mime_guess`).
#[derive(Debug, Clone)]
pub struct DocumentsConfig {
    /// Master switch for the `/v1/documents*` routes + the
    /// `read_document` agent. Defaults to `false` so first-time users
    /// do not accidentally expose the cache dir.
    pub enabled: bool,
    /// Directory where uploaded files are staged on disk. Defaults to
    /// `/var/cache/nagent/docs` (the kustomize base mounts a PVC
    /// there). The server refuses to start when the dir is not
    /// writable at boot — see `documents::store::check_writable`.
    pub cache_dir: PathBuf,
    /// Maximum upload size, in bytes. Hard cap on the multipart body.
    pub max_file_size_bytes: usize,
    /// Maximum number of characters the server extracts from a
    /// document at upload time. Past the cap the extractor
    /// truncates and adds a `[… truncated …]` marker; the file is
    /// still stored so the LLM can request the relevant page range
    /// later.
    pub max_extracted_chars: usize,
    /// Hard cap on documents per chat session. The 51st upload is
    /// rejected with `429 Too Many Documents`.
    pub max_docs_per_session: u32,
    /// PDF parse timeout in seconds. `pdf-extract` is synchronous so
    /// the cap is enforced by wrapping the call in
    /// `tokio::task::spawn_blocking` + a timeout.
    pub pdf_extract_timeout_secs: u64,
    /// Background sweep interval, in hours. A dedicated `tokio::spawn`
    /// task in `main.rs` calls `purge_older_than` every
    /// `purge_interval_hours`. Set to `0` to disable the background
    /// task (operators who only want the CLI sweep keep working).
    pub purge_interval_hours: u64,
    /// Default document TTL in days. Rows older than `created_at +
    /// default_ttl_days` are purged (file + DB row). Operators can
    /// override per-row with `expires_at`.
    pub default_ttl_days: u32,
}

impl Default for DocumentsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            cache_dir: PathBuf::from("/var/cache/nagent/docs"),
            max_file_size_bytes: 20 * 1024 * 1024,
            max_extracted_chars: 100_000,
            max_docs_per_session: 50,
            pdf_extract_timeout_secs: 30,
            purge_interval_hours: 24,
            default_ttl_days: 30,
        }
    }
}

impl DocumentsConfig {
    fn from_env_with_toml(
        toml: Option<&crate::config_file::TomlDocumentsConfig>,
    ) -> Result<Self, ConfigError> {
        let defaults = Self::default();
        let toml = toml.cloned().unwrap_or_default();
        let cache_dir = resolve_opt_string(
            env_opt("DOCS_CACHE_DIR").as_deref(),
            toml.cache_dir.as_deref(),
        )
        .map(PathBuf::from)
        .unwrap_or(defaults.cache_dir.clone());
        Ok(Self {
            enabled: resolve_primitive(
                env_opt("DOCS_ENABLED").as_deref(),
                toml.enabled,
                defaults.enabled,
                "DOCS_ENABLED",
            )?,
            cache_dir,
            max_file_size_bytes: resolve_primitive(
                env_opt("DOCS_MAX_FILE_BYTES").as_deref(),
                toml.max_file_size_bytes,
                defaults.max_file_size_bytes,
                "DOCS_MAX_FILE_BYTES",
            )?,
            max_extracted_chars: resolve_primitive(
                env_opt("DOCS_MAX_CHARS").as_deref(),
                toml.max_extracted_chars,
                defaults.max_extracted_chars,
                "DOCS_MAX_CHARS",
            )?,
            max_docs_per_session: resolve_primitive(
                env_opt("DOCS_MAX_PER_SESSION").as_deref(),
                toml.max_docs_per_session,
                defaults.max_docs_per_session,
                "DOCS_MAX_PER_SESSION",
            )?,
            pdf_extract_timeout_secs: resolve_primitive(
                env_opt("DOCS_PDF_TIMEOUT_SECS").as_deref(),
                toml.pdf_extract_timeout_secs,
                defaults.pdf_extract_timeout_secs,
                "DOCS_PDF_TIMEOUT_SECS",
            )?,
            purge_interval_hours: resolve_primitive(
                env_opt("DOCS_PURGE_INTERVAL_H").as_deref(),
                toml.purge_interval_hours,
                defaults.purge_interval_hours,
                "DOCS_PURGE_INTERVAL_H",
            )?,
            default_ttl_days: resolve_primitive(
                env_opt("DOCS_TTL_DAYS").as_deref(),
                toml.default_ttl_days,
                defaults.default_ttl_days,
                "DOCS_TTL_DAYS",
            )?,
        })
    }
}
