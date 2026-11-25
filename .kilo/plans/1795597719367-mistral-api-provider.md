# LLM provider selection — plug the Mistral API alongside the local OpenAI-compatible upstream

Status: proposal, written 2026-11-25. No code changes yet.

## Context

Today the `[llm]` section assumes a **local OpenAI-compatible**
upstream (Ollama by default):

- `crates/nagent-server/src/config/llm.rs` — `LlmConfig::default()`
  pins `base_url = "http://localhost:11434"`, `default_model =
  "llama3.1"`; env vars `OLLAMA_BASE_URL` / `OLLAMA_MODEL` /
  `OLLAMA_API_KEY` (outbound bearer), `LLM_API_KEY` (inbound bearer).
- `crates/nagent-server/src/config_file.rs` — `TomlLlmConfig` mirrors
  every key (the triple-definition pattern, see
  `architecture-restructure.md` finding 3.1.1).
- `crates/nagent-server/src/llm/proxy.rs` — `POST /v1/chat/completions`
  forwards the browser body to `{base_url}/v1/chat/completions` with
  an optional `Authorization: Bearer {api_key}`, and injects the
  **Ollama-native** `options.num_predict` / `options.num_ctx`
  (`inject_ollama_num_predict` / `inject_ollama_num_ctx`).
- `crates/nagent-server/src/llm/tool_loop.rs` — rounds 1+ open their
  own upstream connection to the same URL.
- `GET /v1/models` and `/api/features.llm_models` parse the upstream
  `{data: [{id}]}` shape (`fetch_upstream_model_list`).

The Mistral API (`https://api.mistral.ai/v1`) is OpenAI-compatible for
`/chat/completions` (tools + streaming SSE with indexed
`tool_calls[].index` deltas — already handled by
`llm/sse.rs::SseStreamAccumulator`) and `/models`
(`{object:"list", data:[{id,…}]}` — already handled). So the proxy
works today if you point `base_url` at Mistral, **except**:

1. No first-class `provider` concept — the operator must know to set
   `base_url`/`api_key`/model by hand, and the Ollama-native knobs
   would be forwarded to a server that may reject them.
2. `options.num_predict` / `options.num_ctx` are Ollama-only; Mistral
   (like OpenAI) uses top-level `max_tokens`. Sending `options` to
   Mistral risks a `422` on strict validation.
3. `default_model = "llama3.1"` is meaningless against Mistral's
   catalog (`mistral-large-latest`, `mistral-small-latest`,
   `codestral-latest`, `ministral-8b-latest`, …).
4. No fail-closed check that an API key exists when the provider is a
   hosted one (S-5 spirit).

## Proposal

### 1. New `provider` enum on `[llm]`

```toml
[llm]
provider = "ollama"            # "ollama" (default) | "openai-compatible" | "mistral"
```

- Rust: `pub enum LlmProvider { Ollama, OpenAiCompatible, Mistral }`
  in `crates/nagent-server/src/config/llm.rs`, with `parse()` /
  `From<String>` mirroring `LlmAuthMode` (strict on env, lenient on
  TOML — same contract as `auth_mode`).
- TOML mirror field `provider: Option<String>` on `TomlLlmConfig`.
- Env var: `LLM_PROVIDER` (strict parse, `ConfigError::InvalidEnv` on
  typo). Legacy `OLLAMA_*` env names keep working when
  `provider = "ollama"` (the default), so existing deployments are
  untouched.

### 2. Per-provider defaults (applied at resolution time)

| Field | `ollama` (default) | `openai-compatible` | `mistral` |
| --- | --- | --- | --- |
| `base_url` default | `http://localhost:11434` | none — **startup error** if unset and `enabled = true` | `https://api.mistral.ai/v1` |
| `default_model` default | `llama3.1` | none — startup error if unset | `mistral-small-latest` (tool-capable, cheap; the UI dropdown lists the full catalog anyway) |
| outbound `api_key` | optional | optional | **required** — startup fails closed with a clear error if missing |
| env for `api_key` | `OLLAMA_API_KEY` | `LLM_UPSTREAM_API_KEY` | `MISTRAL_API_KEY`, falling back to `LLM_UPSTREAM_API_KEY` |

Resolution order stays env > TOML > provider-default > built-in
default, implemented in `LlmConfig::from_env_with_toml`.

### 3. Request adaptation (shared helper, used by proxy + tool loop)

New `crates/nagent-server/src/llm/provider.rs` with
`adapt_request_body(body: &mut Value, cfg: &LlmConfig)`:

- `provider == Ollama` → current behaviour: merge
  `options.num_predict` / `options.num_ctx` from
  `ollama_num_predict` / `ollama_num_ctx`.
- `provider != Ollama` → **remove the `options` object entirely**
  from the forwarded body (Ollama-native; the browser never sends it
  today — verified in `static/*.js` — but stripping is the safe
  default), and inject top-level `max_tokens` from a new generic
  knob `[llm].max_tokens` (env `LLM_MAX_TOKENS`). `ollama_num_predict`
  is then honoured as `options.num_predict` on Ollama and as
  `max_tokens` on other providers, so one knob covers both — the
  generic `max_tokens` wins if both are set.
- Both paths also set a `User-Agent: nagent/<version>` header on the
  upstream request (some hosted APIs require it).

`proxy.rs::chat_completions` and `tool_loop.rs` both call
`adapt_request_body` instead of the two `inject_ollama_*` helpers;
those two helpers move into `provider.rs` unchanged in behaviour.

### 4. Response / SSE

No code change expected: Mistral streams OpenAI-shaped SSE with
indexed `tool_calls` deltas and a final `data: [DONE]`, both already
handled. **Add captured-fixture unit tests** in `llm/sse.rs` /
`llm/tool_loop.rs` using a recorded Mistral stream (tool call +
content + `usage` chunk) to prove it, so a future Mistral wire
drift fails CI instead of production.

### 5. Startup validation (fail closed)

In `LlmConfig::validate()` (called from `app.rs::build_app`):

- `provider = "mistral"` and `api_key` empty → startup error naming
  `MISTRAL_API_KEY` / `[llm].api_key`.
- `provider = "mistral"` and `base_url` host is not `api.mistral.ai`
  → allowed (Mistral-compatible gateways, Azure AI) but logged at
  `WARN` with the resolved host.
- any provider, non-loopback `base_url` over plain `http://` with an
  `api_key` set → `WARN` (cleartext upstream key on the wire).
- `provider = "openai-compatible"` without `base_url` /
  `default_model` → startup error.

### 6. What does NOT change

- The inbound `/v1/*` auth (`auth_mode`, `inbound_auth_key`) —
  orthogonal.
- The tool loop, permission intercept, privacy stripping, SSE
  framing towards the browser, `/api/features.llm_models` (the
  Mistral model list flows through unchanged).
- The LLM client keeps its own `reqwest::Client` outside the
  `EgressPool` (R-7: the LLM upstream is a separate trust boundary,
  operator-configured `base_url`, not subject to the agent SSRF
  policy).

### 7. Files touched

- `crates/nagent-server/src/config/llm.rs` — `LlmProvider`,
  `provider` field, per-provider defaults, `max_tokens`,
  `validate()`; tests for precedence + fail-closed.
- `crates/nagent-server/src/config_file.rs` — `TomlLlmConfig.provider`
  + `max_tokens` mirrors and merge.
- `crates/nagent-server/src/llm/provider.rs` (new) —
  `adapt_request_body`, moved `inject_ollama_*`.
- `crates/nagent-server/src/llm/proxy.rs`, `llm/tool_loop.rs` — call
  the shared adapter; both URL builders stay as-is.
- `crates/nagent-server/src/llm/client.rs` — `user_agent` on the
  upstream request builder.
- `crates/nagent-server/src/llm/sse.rs` — Mistral fixture tests only.
- `crates/nagent-server/src/static/chat.js` — make the
  "Run `ollama pull <name>`" error hint provider-aware
  (chat.js:2092, 5992): for `mistral` say "pick another model in the
  dropdown".
- Docs (AGENTS.md §5, same commit): `docs/llm-configuration.md` (new
  "Providers" section + `provider`/`max_tokens` knobs),
  `docs/examples/config.toml.example`, README env-var table.
- Optional `make run-mistral` target (`LLM_PROVIDER=mistral
  MISTRAL_API_KEY=… cargo run …`) documented in README.

### 8. Validation

1. `cargo fmt --all`, `cargo build --all-targets --all-features` zero
   warnings, `cargo clippy --all-targets -- -D warnings`,
   `cargo test --all`, `cargo audit`.
2. Config tests: default provider stays `ollama` with today's
   defaults (backward compat); `LLM_PROVIDER=mistral` yields
   `base_url = https://api.mistral.ai/v1`,
   `default_model = mistral-small-latest`; missing key →
   startup error; `OLLAMA_*` env still honoured when
   provider unset/ollama.
3. Proxy tests against a loopback `wiremock` fixture:
   - provider=mistral: forwarded body has **no** `options` object,
     `max_tokens` present when configured, `Authorization: Bearer`
     from `MISTRAL_API_KEY`, URL is the Mistral one.
   - provider=ollama: body unchanged from today (`options.*`
     preserved) — regression guard.
   - `/v1/models` parses a Mistral-shaped `{data:[{id}]}` list.
4. SSE fixture test: recorded Mistral stream with an indexed
   `tool_calls` delta round-trips through `run_tool_loop`'s
   accumulator into one complete tool call.
5. Manual smoke: `MISTRAL_API_KEY=… LLM_PROVIDER=mistral make run-mock`,
   chat "what time is it" → `get_datetime` tool fires, reply streams,
   model dropdown lists Mistral models.

## Out of scope

- Mistral-specific extras (`safe_prompt`, `random_seed`,
  `response_format` passthrough, embeddings, OCR, the Agents API) —
  the proxy stays a transparent OpenAI-compatible pipe; these can be
  added later behind `provider = "mistral"` if wanted.
- Migrating existing configs: not needed, the default provider
  reproduces today's behaviour exactly.
- Per-user provider choice (the provider is operator-level, like
  `base_url` today). A per-user model *picker* already exists in the
  UI; per-user provider is a bigger product decision, not in scope.
