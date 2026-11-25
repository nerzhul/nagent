//! `llm` — `[llm]` section + `LlmAuthMode`.

use std::time::Duration;

use crate::config::file::TomlLlmConfig;
use crate::config::{
    env_opt, resolve_csv, resolve_opt_primitive, resolve_opt_string, resolve_primitive, ConfigError,
};

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

/// Upstream LLM provider family selected by `[llm].provider`.
///
/// The proxy speaks the OpenAI-compatible `/v1/chat/completions`
/// wire protocol to every variant; the provider only selects
/// (a) the resolution defaults for `base_url` / `default_model` /
/// `api_key` and (b) the request-body adaptation in
/// `crate::llm::provider::adapt_request_body` (Ollama-native
/// `options.*` knobs vs. the generic top-level `max_tokens`).
///
/// `Ollama` is the default so existing deployments keep today's
/// behaviour exactly; `Mistral` targets the hosted Mistral API
/// (`https://api.mistral.ai/v1`, API key required); `OpenAiCompatible`
/// is any other self-hosted OpenAI-compatible server, where the
/// operator must supply `base_url` and `default_model` explicitly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LlmProvider {
    /// Local Ollama (default). Legacy `OLLAMA_*` env names apply.
    #[default]
    Ollama,
    /// Any other OpenAI-compatible upstream (llama.cpp's llama-server,
    /// vLLM, LM Studio, …). No defaults: `base_url` and
    /// `default_model` must be set explicitly (startup error).
    OpenAiCompatible,
    /// Hosted Mistral API (`https://api.mistral.ai/v1`). An outbound
    /// API key is required (startup fails closed without one).
    Mistral,
}

impl LlmProvider {
    /// Parse the human-friendly form (`ollama`, `openai-compatible`,
    /// `mistral`). Case-insensitive; unknown values surface as an
    /// error so a typo in the env var fails at boot instead of
    /// silently reverting to the default.
    pub fn parse(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "ollama" => Ok(Self::Ollama),
            "openai-compatible" | "openai_compatible" | "openai" => Ok(Self::OpenAiCompatible),
            "mistral" => Ok(Self::Mistral),
            other => Err(format!(
                "expected one of `ollama`, `openai-compatible`, `mistral`, got `{other}`"
            )),
        }
    }

    /// Canonical wire/TOML spelling of the provider.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Ollama => "ollama",
            Self::OpenAiCompatible => "openai-compatible",
            Self::Mistral => "mistral",
        }
    }

    /// Provider-default `base_url` when neither env nor TOML set one.
    /// `None` for `OpenAiCompatible` — the operator must be explicit
    /// there, and `validate()` turns the empty result into a startup
    /// error when the proxy is enabled.
    pub fn default_base_url(&self) -> Option<&'static str> {
        match self {
            Self::Ollama => Some("http://localhost:11434"),
            Self::OpenAiCompatible => None,
            Self::Mistral => Some("https://api.mistral.ai/v1"),
        }
    }

    /// Provider-default `default_model` when neither env nor TOML set
    /// one. `None` for `OpenAiCompatible` (see [`Self::default_base_url`]).
    pub fn default_model(&self) -> Option<&'static str> {
        match self {
            Self::Ollama => Some("llama3.1"),
            Self::OpenAiCompatible => None,
            Self::Mistral => Some("mistral-small-latest"),
        }
    }
}

impl From<String> for LlmProvider {
    /// Conversion used by the TOML path — same lenient contract as
    /// [`LlmAuthMode::from`]: an unknown value in a config file falls
    /// back to the default instead of blocking boot; the env-var
    /// path (`LLM_PROVIDER`) is the strict one.
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
    /// Upstream provider family. Selects the resolution defaults for
    /// `base_url` / `default_model` / `api_key` (see
    /// [`LlmProvider::default_base_url`]) and the request-body
    /// adaptation in `crate::llm::provider::adapt_request_body`.
    /// Env var `LLM_PROVIDER` (strict parse), TOML key
    /// `[llm].provider` (lenient). Defaults to `ollama`.
    pub provider: LlmProvider,
    /// Base URL of the upstream OpenAI-compatible server (typically
    /// `http://localhost:11434` for Ollama, `https://api.mistral.ai/v1`
    /// for Mistral). Env var `LLM_BASE_URL`; the legacy
    /// `OLLAMA_BASE_URL` is honoured when `provider = "ollama"`.
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
    /// Whether the proxy is allowed to inject the per-user long-term
    /// memory system message (`USER_MEMORIES_MARKER`) when the
    /// authenticated user has opted in via `memory_enabled`. The
    /// kill-switch exists for symmetry with `allow_user_location` /
    /// `allow_user_timezone` / `allow_user_reply_language` so
    /// operators handling sensitive deployments can forbid the
    /// memory hint from reaching the upstream model regardless of
    /// what the user picked in Settings → Memory. Env var
    /// `LLM_ALLOW_USER_MEMORY`, TOML key
    /// `[llm].allow_user_memory`. Defaults to `true` — the user's
    /// `memory_enabled` opt-in is the primary gate; the auto-prompt
    /// injection is skipped regardless when this flag is `false`.
    /// Independent from the other three flags: an operator may
    /// forbid one without touching the others.
    pub allow_user_memory: bool,
    /// Maximum number of tool-call rounds a single user turn may
    /// trigger before the proxy bails out and surfaces an error
    /// bubble. Defends against models that loop on a tool call.
    /// Env var `LLM_MAX_TOOL_ROUNDS`, TOML key
    /// `[llm].llm_max_tool_rounds`. Previously mis-housed on
    /// `AgentConfig`; moved here because it gates the proxy's tool
    /// loop, not the agent registry. Default of `64` covers
    /// deep agent compositions where the model chains many
    /// tool calls in a single turn; lower the cap if a specific
    /// upstream shows runaway behaviour.
    pub llm_max_tool_rounds: u32,
    /// Maximum number of auto-continue rounds the loop appends
    /// when the upstream emits `finish_reason: "length"` while
    /// still in the reasoning phase of a reasoning-capable
    /// model (qwen3.5 with reasoning on, DeepSeek-R1, o1/o3,
    /// …). Each auto-continue is a fresh upstream request that
    /// asks the model to "continue from where you left off and
    /// produce the visible answer now" without re-emitting the
    /// prior reasoning (the chat UI already showed it). The
    /// default of `32` is generous — a single long question
    /// rarely needs more than a handful of truncations on
    /// upstreams with a sane `num_ctx`; raise the cap for
    /// providers with a small fixed context window (e.g.
    /// cloud-hosted models where you cannot bump the upstream).
    /// Set to `0` to disable the heuristic; the loop then
    /// closes the stream on truncation, surfacing the reasoning
    /// text to the user and leaving continuation to an explicit
    /// "continue" prompt. Env var `LLM_MAX_AUTO_CONTINUES`,
    /// TOML key `[llm].llm_max_auto_continues`. See
    /// `docs/llm-configuration.md` for the full contract.
    pub llm_max_auto_continues: u32,
    /// Optional per-response generation cap (in tokens) forwarded
    /// to the upstream as `options.num_predict`. Reasoners
    /// (deepseek-r1, qwen3.5 with thinking on, o1/o3) emit a long
    /// reasoning phase before any tool call; the Ollama default
    /// of `128` tokens cuts them off mid-thought and trips the
    /// `llm_max_auto_continues` heuristic into a loop. Setting
    /// this to `2048` or `4096` lets the model finish a single
    /// reasoning + tool-call round in one upstream request, so the
    /// auto-continue heuristic rarely fires. `None` (the default)
    /// leaves the upstream's `num_predict` untouched — the proxy
    /// only injects the field when the operator opts in.
    /// **Ollama-specific**: the field name comes from Ollama's
    /// native parameters and is forwarded under the
    /// `options.*` slot of the OpenAI-compatible
    /// `/v1/chat/completions` body. Other upstreams (llama.cpp's
    /// llama-server, vLLM, OpenRouter) do not honour this key.
    /// Env var `LLM_OLLAMA_NUM_PREDICT`, TOML key
    /// `[llm].ollama_num_predict`. See
    /// `docs/llm-configuration.md` for the full contract.
    pub ollama_num_predict: Option<u32>,
    /// Optional context-window size forwarded to the upstream as
    /// `options.num_ctx`. Ollama defaults to a 2048-token context
    /// window, which is too small for reasoning models
    /// (deepseek-r1, qwen3.5 with thinking on): they truncate
    /// mid-thought and trip the `llm_max_auto_continues`
    /// heuristic.
    ///
    /// Setting this to `32768` or `65536` lets the model finish
    /// a long reasoning + tool-call round in one upstream
    /// request, so the auto-continue heuristic rarely fires.
    ///
    /// `None` (the default) leaves the upstream's `num_ctx`
    /// untouched — the proxy only injects the field when the
    /// operator opts in, so a deployment that already pins
    /// `num_ctx` via the Modelfile or `OLLAMA_CONTEXT_LENGTH`
    /// keeps working without modification.
    ///
    /// **Ollama-specific**: the field name comes from Ollama's
    /// native parameters and is forwarded under the
    /// `options.*` slot of the OpenAI-compatible
    /// `/v1/chat/completions` body. Other upstreams (llama.cpp's
    /// llama-server, vLLM, OpenRouter) do not honour this key.
    /// Env var `LLM_OLLAMA_NUM_CTX`, TOML key
    /// `[llm].ollama_num_ctx`. See
    /// `docs/llm-configuration.md` for the full contract.
    pub ollama_num_ctx: Option<u32>,
    /// Optional generic per-response generation cap (in tokens)
    /// forwarded to the upstream as the top-level `max_tokens` field
    /// of the `/v1/chat/completions` body — the spelling every
    /// non-Ollama OpenAI-compatible API (Mistral, OpenAI, vLLM, …)
    /// honours. On `provider = "ollama"` this knob is ignored in favour
    /// of `ollama_num_predict`; on every other provider
    /// `ollama_num_predict` is honoured *as* `max_tokens` when
    /// `max_tokens` itself is unset, so one knob covers both families —
    /// the generic `max_tokens` wins when both are set. Env var
    /// `LLM_MAX_TOKENS`, TOML key `[llm].max_tokens`. `None` (the
    /// default) leaves the upstream's own cap untouched.
    pub max_tokens: Option<u32>,
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            provider: LlmProvider::default(),
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
            allow_user_memory: true,
            llm_max_tool_rounds: 64,
            llm_max_auto_continues: 32,
            ollama_num_predict: None,
            ollama_num_ctx: None,
            max_tokens: None,
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
        // The provider is resolved first because the env-var names for
        // `base_url` / `default_model` / `api_key` are provider-scoped:
        // the generic `LLM_BASE_URL` / `LLM_MODEL` / `LLM_UPSTREAM_API_KEY`
        // apply to every provider, the legacy `OLLAMA_*` names only when
        // `provider = "ollama"` (so existing deployments are untouched),
        // and `MISTRAL_API_KEY` only when `provider = "mistral"`.
        // `LLM_PROVIDER` is the strict env path (a typo is a boot
        // error); the TOML `provider` key is lenient like `auth_mode`.
        let provider = match env_opt("LLM_PROVIDER").as_deref() {
            Some(v) => LlmProvider::parse(v)
                .map_err(|e| ConfigError::InvalidEnv("LLM_PROVIDER".into(), e))?,
            None => toml.provider.map(LlmProvider::from).unwrap_or_default(),
        };
        // env (generic, then provider-specific legacy alias) > TOML >
        // provider default > built-in default. The provider default
        // reproduces today's `LlmConfig::default()` values for
        // `ollama`; `openai-compatible` has no default and leaves the
        // string empty for `validate()` to reject at boot.
        let base_url_env = env_opt("LLM_BASE_URL").or_else(|| match provider {
            LlmProvider::Ollama => env_opt("OLLAMA_BASE_URL"),
            _ => None,
        });
        // No fallthrough to `defaults.base_url` here: for
        // `openai-compatible` the provider default is `None` and the
        // empty string is what `validate()` turns into a startup
        // error when the proxy is enabled. For `ollama` the provider
        // default *is* the built-in default, so existing deployments
        // resolve exactly as before.
        let base_url = resolve_opt_string(base_url_env.as_deref(), toml.base_url.as_deref())
            .or_else(|| provider.default_base_url().map(str::to_string))
            .unwrap_or_default();
        let default_model_env = env_opt("LLM_MODEL").or_else(|| match provider {
            LlmProvider::Ollama => env_opt("OLLAMA_MODEL"),
            _ => None,
        });
        let default_model =
            resolve_opt_string(default_model_env.as_deref(), toml.default_model.as_deref())
                .or_else(|| provider.default_model().map(str::to_string))
                .unwrap_or_default();
        let api_key_env = match provider {
            LlmProvider::Mistral => {
                env_opt("MISTRAL_API_KEY").or_else(|| env_opt("LLM_UPSTREAM_API_KEY"))
            }
            LlmProvider::OpenAiCompatible => env_opt("LLM_UPSTREAM_API_KEY"),
            LlmProvider::Ollama => {
                env_opt("OLLAMA_API_KEY").or_else(|| env_opt("LLM_UPSTREAM_API_KEY"))
            }
        };
        let api_key = resolve_opt_string(api_key_env.as_deref(), toml.api_key.as_deref());
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
        let allow_user_memory = resolve_primitive(
            env_opt("LLM_ALLOW_USER_MEMORY").as_deref(),
            toml.allow_user_memory,
            defaults.allow_user_memory,
            "LLM_ALLOW_USER_MEMORY",
        )?;
        let llm_max_tool_rounds = resolve_primitive(
            env_opt("LLM_MAX_TOOL_ROUNDS").as_deref(),
            toml.llm_max_tool_rounds,
            defaults.llm_max_tool_rounds,
            "LLM_MAX_TOOL_ROUNDS",
        )?
        .clamp(1, 256);
        let llm_max_auto_continues = resolve_primitive(
            env_opt("LLM_MAX_AUTO_CONTINUES").as_deref(),
            toml.llm_max_auto_continues,
            defaults.llm_max_auto_continues,
            "LLM_MAX_AUTO_CONTINUES",
        )?
        .clamp(0, 64);
        // `None` is the contract for both Ollama-native knobs: the proxy
        // only injects `options.num_predict` / `options.num_ctx`
        // when the operator opts in. Resolving an env var on top of
        // `None` keeps the three-way merge (env > TOML > default)
        // intact.
        let ollama_num_predict = resolve_opt_primitive(
            env_opt("LLM_OLLAMA_NUM_PREDICT").as_deref(),
            toml.ollama_num_predict,
            "LLM_OLLAMA_NUM_PREDICT",
        )?;
        let ollama_num_ctx = resolve_opt_primitive(
            env_opt("LLM_OLLAMA_NUM_CTX").as_deref(),
            toml.ollama_num_ctx,
            "LLM_OLLAMA_NUM_CTX",
        )?;
        // Generic generation cap — forwarded as top-level `max_tokens`
        // on non-Ollama providers (see `adapt_request_body`). Same
        // `None`-means-unset contract as the two Ollama-native knobs.
        let max_tokens = resolve_opt_primitive(
            env_opt("LLM_MAX_TOKENS").as_deref(),
            toml.max_tokens,
            "LLM_MAX_TOKENS",
        )?;

        Ok(Self {
            enabled,
            provider,
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
            allow_user_memory,
            llm_max_tool_rounds,
            llm_max_auto_continues,
            ollama_num_predict,
            ollama_num_ctx,
            max_tokens,
        })
    }

    /// Fail-closed startup validation, called from
    /// `app::build_app` before the LLM client is built.
    ///
    /// - `provider = "mistral"` without an outbound `api_key` is a
    ///   hard error naming the env vars / TOML key that fix it — a
    ///   hosted provider cannot work keyless, and failing at boot is
    ///   far better than a 401 on every chat round.
    /// - `provider = "openai-compatible"` without `base_url` /
    ///   `default_model` is a hard error (there is no sane default
    ///   for an arbitrary upstream).
    /// - `provider = "mistral"` with a `base_url` host other than
    ///   `api.mistral.ai` is allowed (Mistral-compatible gateways,
    ///   Azure AI) but logged at `WARN` with the resolved host.
    /// - any provider with a non-loopback `base_url` over plain
    ///   `http://` while an `api_key` is set logs a `WARN` — the
    ///   bearer token would cross the wire in cleartext.
    ///
    /// Skipped entirely when `enabled = false` (the `/v1/*` routes
    /// are not registered, so the values are inert).
    pub fn validate(&self) -> Result<(), ConfigError> {
        if !self.enabled {
            return Ok(());
        }
        match self.provider {
            LlmProvider::Mistral => {
                if self
                    .api_key
                    .as_deref()
                    .map(str::trim)
                    .unwrap_or("")
                    .is_empty()
                {
                    return Err(ConfigError::InvalidLlm(
                        "[llm] provider = \"mistral\" requires an outbound API key: \
                         set MISTRAL_API_KEY (or LLM_UPSTREAM_API_KEY) or [llm].api_key"
                            .into(),
                    ));
                }
            }
            LlmProvider::OpenAiCompatible => {
                if self.base_url.trim().is_empty() {
                    return Err(ConfigError::InvalidLlm(
                        "[llm] provider = \"openai-compatible\" requires base_url: \
                         set LLM_BASE_URL or [llm].base_url"
                            .into(),
                    ));
                }
                if self.default_model.trim().is_empty() {
                    return Err(ConfigError::InvalidLlm(
                        "[llm] provider = \"openai-compatible\" requires default_model: \
                         set LLM_MODEL or [llm].default_model"
                            .into(),
                    ));
                }
            }
            LlmProvider::Ollama => {}
        }
        if let Ok(parsed) = url::Url::parse(&self.base_url) {
            let host = parsed.host_str().unwrap_or("");
            if self.provider == LlmProvider::Mistral && host != "api.mistral.ai" {
                tracing::warn!(
                    provider = "mistral",
                    host,
                    "[llm] provider = \"mistral\" with a non-default base_url host; \
                     assuming a Mistral-compatible gateway (Azure AI, …)"
                );
            }
            let loopback = match host.parse::<std::net::IpAddr>() {
                Ok(ip) => ip.is_loopback(),
                Err(_) => host == "localhost" || host.ends_with(".localhost"),
            };
            if parsed.scheme() == "http" && !loopback && self.api_key.is_some() {
                tracing::warn!(
                    base_url = %self.base_url,
                    "[llm] non-loopback base_url over plain http:// with an api_key set — \
                     the upstream bearer token crosses the wire in cleartext; use https://"
                );
            }
        }
        Ok(())
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
    fn llm_provider_parse_accepts_known_values() {
        assert!(matches!(
            LlmProvider::parse("ollama").unwrap(),
            LlmProvider::Ollama
        ));
        assert!(matches!(
            LlmProvider::parse(" OpenAI-Compatible ").unwrap(),
            LlmProvider::OpenAiCompatible
        ));
        assert!(matches!(
            LlmProvider::parse("MISTRAL").unwrap(),
            LlmProvider::Mistral
        ));
    }

    #[test]
    fn llm_provider_parse_rejects_unknown_values() {
        // A typo in `LLM_PROVIDER` must surface as an error at boot,
        // not silently revert to ollama.
        let err = LlmProvider::parse("mistrall").unwrap_err();
        assert!(
            err.contains("mistral") && err.contains("mistrall"),
            "error must list valid options and echo the value, got: {err}"
        );
    }

    #[test]
    fn llm_provider_toml_from_is_lenient() {
        // Same contract as `LlmAuthMode::from`: a typo in the TOML file
        // falls back to the default instead of blocking boot.
        assert!(matches!(
            LlmProvider::from("bogus".to_string()),
            LlmProvider::Ollama
        ));
        assert!(matches!(
            LlmProvider::from("mistral".to_string()),
            LlmProvider::Mistral
        ));
    }

    #[test]
    fn llm_provider_default_is_ollama_with_today_defaults() {
        // Backward compatibility: the default provider reproduces the
        // historical base_url / default_model exactly.
        let cfg = LlmConfig::default();
        assert!(matches!(cfg.provider, LlmProvider::Ollama));
        assert_eq!(cfg.base_url, "http://localhost:11434");
        assert_eq!(cfg.default_model, "llama3.1");
        assert!(cfg.max_tokens.is_none());
        assert!(LlmProvider::default().default_base_url() == Some("http://localhost:11434"));
        assert_eq!(LlmProvider::default().default_model(), Some("llama3.1"));
    }

    #[test]
    fn llm_provider_per_provider_defaults() {
        assert_eq!(
            LlmProvider::Mistral.default_base_url(),
            Some("https://api.mistral.ai/v1")
        );
        assert_eq!(
            LlmProvider::Mistral.default_model(),
            Some("mistral-small-latest")
        );
        // `openai-compatible` has no defaults — `validate()` turns the
        // empty resolution into a startup error when enabled.
        assert!(LlmProvider::OpenAiCompatible.default_base_url().is_none());
        assert!(LlmProvider::OpenAiCompatible.default_model().is_none());
    }

    #[test]
    fn llm_provider_toml_resolution_applies_per_provider_defaults() {
        // TOML-only path (env unset in CI): `provider = "mistral"`
        // must yield the Mistral base_url / model defaults without
        // any env var. `validate()` is NOT called here (it runs in
        // `build_app`), so the missing API key does not fail this
        // resolution test.
        let toml: crate::config::file::TomlLlmConfig = toml::from_str(
            r#"
                provider = "mistral"
                enabled = true
            "#,
        )
        .unwrap();
        let cfg = LlmConfig::from_env_with_toml(Some(&toml)).expect("config must load");
        assert!(matches!(cfg.provider, LlmProvider::Mistral));
        assert_eq!(cfg.base_url, "https://api.mistral.ai/v1");
        assert_eq!(cfg.default_model, "mistral-small-latest");
    }

    #[test]
    fn llm_provider_toml_base_url_and_model_override_provider_default() {
        // An explicit TOML base_url / default_model beats the provider
        // default (gateway / pinned-model case).
        let toml: crate::config::file::TomlLlmConfig = toml::from_str(
            r#"
                provider = "mistral"
                base_url = "https://my-gateway.example.com/v1"
                default_model = "mistral-large-latest"
                max_tokens = 4096
            "#,
        )
        .unwrap();
        let cfg = LlmConfig::from_env_with_toml(Some(&toml)).expect("config must load");
        assert_eq!(cfg.base_url, "https://my-gateway.example.com/v1");
        assert_eq!(cfg.default_model, "mistral-large-latest");
        assert_eq!(cfg.max_tokens, Some(4096));
    }

    #[test]
    fn llm_provider_default_resolution_keeps_ollama_defaults() {
        // No `[llm]` section at all → provider ollama, historical
        // defaults, no API key.
        let cfg = LlmConfig::from_env_with_toml(None).expect("config must load");
        assert!(matches!(cfg.provider, LlmProvider::Ollama));
        assert_eq!(cfg.base_url, "http://localhost:11434");
        assert_eq!(cfg.default_model, "llama3.1");
        assert!(cfg.api_key.is_none());
    }

    #[test]
    fn llm_validate_fails_closed_for_mistral_without_api_key() {
        let cfg = LlmConfig {
            enabled: true,
            provider: LlmProvider::Mistral,
            base_url: "https://api.mistral.ai/v1".into(),
            default_model: "mistral-small-latest".into(),
            api_key: None,
            ..LlmConfig::default()
        };
        let err = cfg.validate().unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("MISTRAL_API_KEY") && msg.contains("[llm].api_key"),
            "error must name the env var and the TOML key, got: {msg}"
        );
    }

    #[test]
    fn llm_validate_accepts_mistral_with_api_key() {
        let cfg = LlmConfig {
            enabled: true,
            provider: LlmProvider::Mistral,
            api_key: Some("sk-test".into()),
            ..LlmConfig::default()
        };
        // Default provider base_url for mistral is set via default();
        // override to the canonical one for clarity.
        let cfg = LlmConfig {
            base_url: "https://api.mistral.ai/v1".into(),
            default_model: "mistral-small-latest".into(),
            ..cfg
        };
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn llm_validate_fails_for_openai_compatible_without_base_url_or_model() {
        let cfg = LlmConfig {
            enabled: true,
            provider: LlmProvider::OpenAiCompatible,
            base_url: String::new(),
            default_model: String::new(),
            ..LlmConfig::default()
        };
        let err = cfg.validate().unwrap_err();
        assert!(err.to_string().contains("base_url"), "got: {err}");
        let cfg = LlmConfig {
            base_url: "http://localhost:8000/v1".into(),
            ..cfg
        };
        let err = cfg.validate().unwrap_err();
        assert!(err.to_string().contains("default_model"), "got: {err}");
        let cfg = LlmConfig {
            default_model: "my-model".into(),
            ..cfg
        };
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn llm_validate_skipped_when_disabled() {
        // `enabled = false` → the /v1/* routes are not registered, so
        // even a keyless mistral config must not block boot.
        let cfg = LlmConfig {
            enabled: false,
            provider: LlmProvider::Mistral,
            api_key: None,
            ..LlmConfig::default()
        };
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn llm_validate_warns_but_allows_custom_mistral_gateway() {
        // A non-api.mistral.ai host is allowed (WARN only) — the
        // warning itself is a log line, so the assertion is just that
        // validation passes.
        let cfg = LlmConfig {
            enabled: true,
            provider: LlmProvider::Mistral,
            base_url: "https://azure-mistral.example.com/v1".into(),
            default_model: "mistral-small-latest".into(),
            api_key: Some("sk-test".into()),
            ..LlmConfig::default()
        };
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn llm_validate_allows_loopback_http_with_api_key() {
        // The cleartext warning only fires for NON-loopback http
        // upstreams; a loopback Ollama with a key is the historical
        // setup and must stay silent.
        let cfg = LlmConfig {
            enabled: true,
            provider: LlmProvider::Ollama,
            base_url: "http://localhost:11434".into(),
            api_key: Some("key".into()),
            ..LlmConfig::default()
        };
        assert!(cfg.validate().is_ok());
        let cfg = LlmConfig {
            base_url: "http://192.0.2.10:11434".into(),
            ..cfg
        };
        // Still Ok — the cleartext case is a WARN, not a hard error.
        assert!(cfg.validate().is_ok());
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
