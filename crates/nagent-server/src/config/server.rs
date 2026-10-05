//! `server` — top-level `Config`, `CliArgs`, `ConfigError`, and the
//! env/TOML merge helpers used by every section.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::config::agents::AgentConfig;
use crate::config::allowed_origins::AllowedOriginsConfig;
use crate::config::auth::AuthConfig;
use crate::config::documents::DocumentsConfig;
use crate::config::file::{merge_toml_configs, TomlConfig};
use crate::config::limits::LimitsConfig;
use crate::config::llm::LlmConfig;
use crate::config::ratelimit::RateLimitConfig;
use crate::config::trusted_proxies::TrustedProxiesConfig;
use crate::config::tts::TtsConfig;
use crate::config::x_oauth::XOAuthConfig;
use crate::config::{env_opt, resolve_primitive};

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
    /// Optional authentication & user-identity subsystem . When
    /// `auth.enabled` is `false` the server keeps the single
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
    /// `[server].allowed_origins` — operator-supplied allow-list
    /// for the `Origin` / `Host` header checks (plan S-2). When
    /// empty, the allow-list is derived from `bind_addr` so the
    /// historical dev workflow keeps working on loopback binds.
    pub allowed_origins: AllowedOriginsConfig,
    /// `[x_oauth]` — X (Twitter) OAuth 2.0 PKCE flow config
    /// (plan 1790695073418). Top-level (not under `[auth.*]`)
    /// because the X flow is a per-user integration, not a
    /// nagent login backend. The routes are mounted only when
    /// `x_oauth.enabled = true` AND `x_oauth.client_id` is
    /// non-empty AND the `x-agent` cargo feature is on.
    pub x_oauth: XOAuthConfig,
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
        let allowed_origins = AllowedOriginsConfig::from_env_with_toml(
            server.and_then(|s| s.allowed_origins.as_ref()),
        )?;
        let x_oauth = XOAuthConfig::from_env_with_toml(toml.and_then(|t| t.x_oauth.as_ref()))?;

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
            allowed_origins,
            x_oauth,
        })
    }

    /// Top-level entry point used by `main`: parses CLI args, reads
    /// the optional TOML file(s), and builds the [`Config`] from the
    /// combined `env > toml > default` precedence chain.
    ///
    /// File discovery depends on whether `--config` was supplied:
    ///
    /// - `--config <PATH>` — load only that file (operator-chosen,
    /// bypasses the layered discovery). Useful for tests, container
    /// setups, and debugging.
    /// - no `--config` — layer the system file (`/etc/nagent/config.toml`)
    /// under the XDG user file (`$XDG_CONFIG_HOME/nagent/config.toml`
    /// or `~/.config/nagent/config.toml`). Missing files are silently
    /// skipped; only parse / read errors are reported.
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

/// Test seam for [`xdg_config_path_from`]: re-exported as a
/// `pub(crate)` helper so the tests in `crate::config::tests` can
/// drive it without going through process-global env vars.
#[cfg(test)]
pub(crate) fn xdg_config_path_from_for_test(
    xdg_config_home: Option<&str>,
    home: Option<&str>,
) -> Option<PathBuf> {
    xdg_config_path_from(xdg_config_home, home)
}

/// Test seam for [`load_layered_default_config`]: identical logic
/// but takes the candidate paths as an argument so unit tests can
/// point at writable temp dirs instead of `/etc/nagent/`.
#[cfg(test)]
pub(crate) fn load_layered_paths_for_test(
    paths: &[PathBuf],
) -> Result<Option<TomlConfig>, ConfigError> {
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

/// Read and parse the TOML config file, wrapping
/// [`crate::config::file::ConfigFileError`] into the unified
/// [`ConfigError`] enum so callers only deal with one error type.
fn load_toml(path: &Path) -> Result<TomlConfig, ConfigError> {
    TomlConfig::from_file(path)
        .map_err(|e| ConfigError::InvalidConfigFile(path.display().to_string(), e.to_string()))
}

/// Resolve the bind address: env > TOML > `0.0.0.0:8080`.
pub(crate) fn resolve_bind_addr(
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
pub(crate) fn resolve_model_path(
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
