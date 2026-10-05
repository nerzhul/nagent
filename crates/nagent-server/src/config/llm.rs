//! `llm` — `[llm]` section + `LlmAuthMode`.

use std::time::Duration;

use crate::config::file::TomlLlmConfig;
use crate::config::{env_opt, resolve_csv, resolve_opt_string, resolve_primitive, ConfigError};

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
    pub fn parse(s: &str) -> Result<Self, String> {
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
    /// Whether the proxy is allowed to inject the per-user reply-
    /// language system message (`USER_REPLY_LANGUAGE_MARKER`) when the
    /// authenticated user has set a non-default reply language in
    /// the Advanced drawer. The kill-switch exists for symmetry with
    /// `allow_user_location` / `allow_user_timezone` so operators
    /// handling sensitive deployments can forbid the language hint
    /// from reaching the upstream model regardless of what the user
    /// picked. Env var `LLM_ALLOW_USER_REPLY_LANGUAGE`, TOML key
    /// `[llm].allow_user_reply_language`. Defaults to `true` — the
    /// user's preference in the UI is the primary gate. Independent
    /// from the other two flags: an operator may forbid one without
    /// touching the others.
    pub allow_user_reply_language: bool,
    /// Maximum number of tool-call rounds a single user turn may
    /// trigger before the proxy bails out and surfaces an error
    /// bubble. Defends against models that loop on a tool call.
    /// Env var `LLM_MAX_TOOL_ROUNDS`, TOML key
    /// `[llm].llm_max_tool_rounds`. Previously mis-housed on
    /// `AgentConfig`; moved here because it gates the proxy's tool
    /// loop, not the agent registry.
    pub llm_max_tool_rounds: u32,
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
            allow_user_reply_language: true,
            llm_max_tool_rounds: 8,
        }
    }
}

impl LlmConfig {
    pub fn from_env_with_toml(toml: Option<&TomlLlmConfig>) -> Result<Self, ConfigError> {
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
        let allow_user_reply_language = resolve_primitive(
            env_opt("LLM_ALLOW_USER_REPLY_LANGUAGE").as_deref(),
            toml.allow_user_reply_language,
            defaults.allow_user_reply_language,
            "LLM_ALLOW_USER_REPLY_LANGUAGE",
        )?;
        let llm_max_tool_rounds = resolve_primitive(
            env_opt("LLM_MAX_TOOL_ROUNDS").as_deref(),
            toml.llm_max_tool_rounds,
            defaults.llm_max_tool_rounds,
            "LLM_MAX_TOOL_ROUNDS",
        )?
        .clamp(1, 32);

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
            allow_user_reply_language,
            llm_max_tool_rounds,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn llm_allow_user_reply_language_env_overrides_toml_and_default() {
        // Mirror of the two existing kill-switch precedence tests for
        // the new reply-language flag. Kept structurally identical so
        // a future refactor that shares the resolve call between the
        // three flags would silently couple the behaviour and this
        // test would catch it.
        let default = LlmConfig::default().allow_user_reply_language;
        assert!(default, "allow_user_reply_language must default to true");
        let from_env = resolve_primitive(
            Some("false"),
            Some(true),
            default,
            "LLM_ALLOW_USER_REPLY_LANGUAGE",
        )
        .expect("env 'false' must parse");
        assert!(!from_env, "env var must beat TOML when both are set");
        let from_toml =
            resolve_primitive(None, Some(false), default, "LLM_ALLOW_USER_REPLY_LANGUAGE")
                .expect("toml bool must parse");
        assert!(!from_toml, "TOML value must apply when env is unset");
        let from_default =
            resolve_primitive::<bool>(None, None, default, "LLM_ALLOW_USER_REPLY_LANGUAGE")
                .expect("default must parse");
        assert!(from_default, "default must win when neither is set");
    }
}
