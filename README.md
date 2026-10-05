# nagent

Local web app written in Rust that streams microphone audio from the browser,
runs it through Silero VAD on the client, and transcribes it server-side using
`whisper.cpp` (GPU-accelerated) over a binary WebSocket protocol.

Multiple WebSocket clients are isolated from each other: a session ID is
generated server-side at upgrade time and never accepted from the client, so
no audio or transcript can ever leak across sessions.

![nagent web UI: STT transcript and Discussion chat views side by side](docs/screenshots/example_app.png)

## Workspace layout

- `crates/stt-proto` — wire types and `postcard` codec for the WS protocol.
- `crates/stt-core` — `WhisperBackend` trait, `whisper-rs` implementation,
  inference worker and queue.
- `crates/nagent-server` — `axum` server, WebSocket handler, session map,
  embedded static frontend.

## Prerequisites

- Rust stable (1.75+).
- For GPU builds: a working Vulkan, CUDA, or ROCm/HIP toolchain on the host.
  Without one, whisper.cpp falls back to CPU.
- A ggml-format Whisper model, e.g. `ggml-base.bin` from
  <https://huggingface.co/ggerganov/whisper.cpp>.

## Quick start (local CPU)

```
make build
WHISPER_MODEL_PATH=/path/to/ggml-base.bin make run
```

Then open <http://localhost:8080> in a recent Chrome / Edge / Firefox.

## Docker (GPU)

```
make docker-build
WHISPER_MODEL_PATH=/models/ggml-base.bin make docker-run-gpu-vulkan
```

## Kubernetes

```
make kustomize-build-prod | kubectl apply -f -
```

The model is downloaded into a PVC by an init Job before the deployment
starts. See `deploy/k8s/` for details.

## Configuration

Every runtime setting is either a Cargo feature, a Dockerfile build argument,
an environment variable, or a TOML file passed via `--config <path>` — no
source changes are required. A `.env` file in the working directory is loaded
automatically by `dotenvy` at startup, and a TOML overlay is loaded when
`--config` is supplied. Precedence is **env > TOML > default**, so containers
and `.env` files still override any TOML values.

### Cargo features

`nagent-server` exposes several features; `stt-core` exposes four. They are combined
through the `make` targets and the Dockerfile `BACKEND` arg.

| Feature                         | Effect                                                                                   |
| ------------------------------- | ---------------------------------------------------------------------------------------- |
| `real-backend`                  | Use the `whisper-rs` backend. Without it the server falls back to the in-process mock.   |
| `web-agent`                     | Register the server-side `web_fetch` chat agent.                                         |
| `datetime-agent`                | Register the `get_datetime` chat agent (pulls `chrono` + `chrono-tz`).                   |
| `weather-agent`                 | Register the `get_weather` chat agent (WeatherAPI.com, free API key required).            |
| `stock-agent`                   | Register the `get_stock_quote` chat agent (Stooq CSV, no API key).                       |
| `calculate-agent`               | Register the `calculate` chat agent (local `meval`-backed expression evaluator).         |
| `unit-convert-agent`            | Register the `unit_convert` chat agent (pure-local conversion tables).                  |
| `wikipedia-agent`               | Register the `wikipedia` chat agent (REST `wikipedia.org`, no API key, `User-Agent` set).|
| `dictionary-agent`              | Register the `dictionary` chat agent (Free Dictionary REST API, no API key).             |
| `x-agent`                       | Register the `x_timeline` chat agent (per-user X / Twitter home timeline via the v2 API; PKCE OAuth flow + self-managed refresh). Operator opt-in via `X_OAUTH_CLIENT_ID`. See `docs/integrations/x.md`. |
| `stt-core/whisper-rs-backend`   | Pulls in `whisper-rs` (CPU). Always required, even when a GPU backend is also selected.  |
| `stt-core/whisper-rs-vulkan`    | Enable the Vulkan GPU backend (needs `libvulkan-dev` at build time).                     |
| `stt-core/whisper-rs-cuda`      | Enable the CUDA GPU backend (needs CUDA toolkit at build time).                          |
| `stt-core/whisper-rs-hipblas`   | Enable the ROCm/HIP GPU backend (needs ROCm toolchain at build time).                   |

> **Important:** the `nagent-server/...` prefix you may see in older
> docs, release notes, or local branches is a **silent no-op** when the
> server itself is the build target (`-p nagent-server`). Cargo
> ignores it without a warning, so the binary ships with `default`
> features only and the runtime logs e.g. `backend ready backend=mock`
> or `agent registry empty (no agents compiled in)` — even though the
> Makefile/Dockerfile *appears* to enable everything. Address the
> server's own features by bare name (`real-backend`, `web-agent`,
> `tts`, …) and keep the `stt-core/...` prefix for the GPU features
> (they live on a dependency).

The three GPU features are mutually exclusive — enabling more than one
wastes build time and can fight over system libraries. The seven
`*-agent` features are independent: each adds exactly one tool to the
LLM's `tools` array. All six `make run*` targets enable `web-agent`
plus the six daily agents so a fresh build has the full set
available; disable any of them by editing the target's `--features`
list.

### Dockerfile build arguments

| Arg        | Default   | Values                  | Notes                                                                |
| ---------- | --------- | ----------------------- | -------------------------------------------------------------------- |
| `BACKEND`  | `vulkan`  | `cpu`, `vulkan`, `cuda`, `hipblas` | Translated to the matching Cargo feature pair at build time. |

The CUDA and HIP backends are **not** installed in the `rust:1.81-bookworm`
base image; supply them via an extended base image if you pick those
backends. The Vulkan loader (`libvulkan1`) is installed automatically in the
runtime stage when `BACKEND=vulkan`.

### Environment variables

All variables are read by `Config::from_env()` in
`crates/nagent-server/src/config.rs`. `WHISPER_MODEL_PATH` is the only one
without a default and is required.

| Variable                     | Default                                       | Section        | Meaning                                                                                                       |
| ---------------------------- | --------------------------------------------- | -------------- | ------------------------------------------------------------------------------------------------------------- |
| `BIND_ADDR`                  | `0.0.0.0:8080`                                | Core           | Socket address the HTTP server binds to.                                                                      |
| `WHISPER_MODEL_PATH`         | _(required)_                                  | Core           | Path to the ggml-format Whisper model.                                                                        |
| `MAX_QUEUE`                  | `32`                                          | Core           | Capacity of the global inference queue (one per session).                                                     |
| `INFERENCE_WORKERS`          | _(backend-derived)_                           | Core           | Number of parallel inference workers (sticky per-session dispatch). Default is taken from the loaded model's size on GPU builds, or 1 on CPU. |
| `SESSION_IDLE_TIMEOUT_MS`    | `30000`                                       | Core           | Watchdog threshold — sessions idle for this long are dropped.                                                |
| `INFER_TIMEOUT_MS`           | `30000`                                       | Core           | Per-inference timeout for a single Whisper call.                                                              |
| `MAX_AUDIO_FRAME_SAMPLES`    | `480000` (30 s × 16 kHz)                      | WS frame limits| Maximum PCM Float32 samples accepted in one `AudioFrame`.                                                     |
| `REQUIRED_SAMPLE_RATE`       | `16000`                                       | WS frame limits| Sample rate the server accepts; any other value is rejected with `INVALID_FRAME`.                             |
| `MAX_LANGUAGE_HINT_BYTES`    | `16`                                          | WS frame limits| Maximum length of the ISO 639-1 language hint.                                                                |
| `REQUEST_TIMEOUT_MS`         | `30000`                                       | HTTP guardrails| Per-request timeout (`tower_http::timeout::TimeoutLayer`) applied to every non-streaming `/v1/*` and `/api/*` route. SSE (`/v1/chat/completions`) and the WebSocket upgrade stay exempt. Floored at 1 s. |
| `BODY_LIMIT_BYTES`           | `2097152` (2 MiB)                             | HTTP guardrails| Maximum size of a single HTTP request body, applied via `axum::extract::DefaultBodyLimit` to the whole `/v1/*` and `/api/*` subtree. Oversized uploads are rejected with `413 Payload Too Large` (or a transport-level abort) before they reach a handler. Floored at 1 KiB. |
| `WS_MAX_CONCURRENT`          | `1024`                                        | HTTP guardrails| Global cap on concurrent WebSocket sessions on `/ws`. Reached upgrades are rejected with `503 Service Unavailable` + `Retry-After: 1` instead of being silently dropped. |
| `WS_MAX_PER_IP`              | `16`                                          | HTTP guardrails| Per-source-IP cap on concurrent WebSocket sessions. Reached upgrades are rejected with `503` + `Retry-After: 1`. |
| `NAGENT_ALLOWED_ORIGINS`     | _(empty → derived from `BIND_ADDR`)_          | Origin/Host guard| Comma-separated list of `Origin` allow-list entries (`scheme://host[:port]`). Empty by default — the allow-list is derived from `[server].bind_addr` (loopback names plus the bind host). Mirrors `[server].allowed_origins].origins` in TOML. |
| `STT_RATE_PER_MIN`           | `120`                                         | Rate limits    | Inbound STT WS frames allowed per source IP per minute (loopback bypasses).                                   |
| `LLM_RATE_PER_MIN`           | `30`                                          | Rate limits    | Inbound LLM HTTP requests allowed per source IP per minute (`/v1/chat/completions`, `/v1/models`).           |
| `LLM_ENABLED`                | `false`                                       | LLM proxy      | Master switch. When `false` the `/v1/*` routes are not registered at all.                                     |
| `OLLAMA_BASE_URL`            | `http://localhost:11434`                      | LLM proxy      | Base URL of the upstream OpenAI-compatible server (Ollama by default).                                        |
| `OLLAMA_MODEL`               | `llama3.1`                                    | LLM proxy      | Default model for `/v1/chat/completions` when the client omits it.                                            |
| `OLLAMA_API_KEY`             | _(unset)_                                     | LLM proxy      | Optional bearer token forwarded as `Authorization: Bearer …`.                                                 |
| `LLM_REQUEST_TIMEOUT_SECS`   | `120`                                         | LLM proxy      | Per-chunk idle timeout on the upstream stream.                                                                |
| `LLM_CORS_ALLOW_ORIGINS`     | _(empty)_                                     | LLM proxy      | Comma-separated list of origins allowed to call `/v1/*` cross-origin. Empty = same-origin only (preflight blocked for others). |
| `LLM_SYSTEM_PROMPT`          | _(unset)_                                     | LLM proxy      | Optional default system prompt prepended to every `/v1/chat/completions` request as `messages[0]`. The browser's "Additional instructions" textarea is appended after it. Empty / whitespace-only values are treated as unset (no injection). Sent on every round of the tool loop — very long custom prompts may exhaust the model's context window. |
| `LLM_ALLOW_USER_LOCATION`    | `true`                                        | LLM proxy      | Defence-in-depth kill-switch for the browser-injected geolocation block. When `false`, the server strips any `{role:"system"}` message whose content starts with `User's approximate location:` before forwarding to the upstream model, regardless of what the browser sends. The browser still requires explicit user consent for the geolocation prompt; this flag is for operators handling sensitive deployments. |
| `LLM_ALLOW_USER_TIMEZONE`    | `true`                                        | LLM proxy      | Defence-in-depth kill-switch for the browser-injected timezone block. When `false`, the server strips any `{role:"system"}` message whose content starts with `The user's local timezone is` before forwarding to the upstream model. Independent of `LLM_ALLOW_USER_LOCATION` — an operator may forbid one without touching the other. The browser still requires explicit consent in the Advanced drawer. |
| `LLM_ALLOW_USER_REPLY_LANGUAGE` | `true`                                     | LLM proxy      | Defence-in-depth kill-switch for the server-injected reply-language block. When `false`, the server strips any `{role:"system"}` message whose content starts with `The user's preferred reply language is` before forwarding to the upstream model. The block is emitted only when the authenticated user has set a non-default reply language on the Advanced drawer picker. Independent of the two flags above. |
| `AGENTS_ENABLED`             | `true`                                        | Chat agents    | Master switch for server-side chat agents (`web_fetch`, `get_datetime`, `get_weather`, `get_stock_quote`, `calculate`, `unit_convert`, `wikipedia`, `dictionary`). When `false` the registry is empty. |
| `LLM_MAX_TOOL_ROUNDS`        | `4`                                           | LLM proxy      | Maximum tool-call rounds per user turn before the proxy aborts.                                                 |
| `LLM_MAX_AUTO_CONTINUES`      | `1`                                           | LLM proxy      | Max number of auto-continue rounds when the upstream reasoning model hits `finish_reason: "length"` mid-reasoning (qwen3.5 with reasoning on, DeepSeek-R1). Set to `0` to disable. |
| `WEB_FETCH_ALLOW_PUBLIC`     | `false`                                       | `web_fetch`    | When `true`, the agent may reach public IP ranges (SSRF defence still blocks loopback/RFC1918).               |
| `WEB_FETCH_ALLOWLIST`        | _(empty)_                                     | `web_fetch`    | Comma-separated hostname allow-list (suffix match; `*.foo` wildcards). Takes precedence over `WEB_FETCH_ALLOW_PUBLIC`. |
| `WEB_FETCH_MAX_BYTES`        | `2097152`                                     | `web_fetch`    | Maximum response size the agent will read (server cap). The LLM-callable `max_bytes` parameter starts the first fetch; if the page is larger the agent transparently doubles the budget and retries until the page fits or this cap is hit. |
| `WEB_FETCH_TIMEOUT_MS`       | `30000`                                       | `web_fetch`    | Per-request timeout in milliseconds.                                                                          |
| `WEATHER_API_KEY`            | _(required for `get_weather`)_                | `get_weather`  | WeatherAPI.com key. Register for a free key at <https://www.weatherapi.com/>. Without it the agent refuses to run with a clear error. |
| `WEATHER_TIMEOUT_MS`         | `8000`                                        | `get_weather`  | Per-request timeout in milliseconds.                                                                          |
| `WEATHER_BASE_URL`           | `https://api.weatherapi.com`                  | `get_weather`  | Override the upstream base URL — useful for tests against a loopback fixture.                                  |
| `UNIT_CONVERT_TIMEOUT_MS`    | `5000`                                        | `unit_convert` | Per-call timeout in milliseconds. The agent is local (no I/O); the knob exists for future-proofing and tests. |
| `WIKIPEDIA_TIMEOUT_MS`       | `5000`                                        | `wikipedia`    | Per-request timeout in milliseconds.                                                                          |
| `WIKIPEDIA_BASE_URL`         | `https://en.wikipedia.org/api/rest_v1`        | `wikipedia`    | Override the upstream base URL — useful for tests against a loopback fixture.                                  |
| `WIKIPEDIA_USER_AGENT`       | `nagent-wikipedia-agent/<version>`            | `wikipedia`    | Override the `User-Agent` header. Wikimedia rejects unidentified clients — keep this descriptive and add a contact URL. |
| `DICTIONARY_TIMEOUT_MS`      | `5000`                                        | `dictionary`   | Per-request timeout in milliseconds.                                                                          |
| `DICTIONARY_BASE_URL`        | `https://api.dictionaryapi.dev/api/v2`        | `dictionary`   | Override the upstream base URL — useful for tests against a loopback fixture.                                  |
| `X_OAUTH_ENABLED`            | `false`                                       | `x_timeline`   | Master switch for the X OAuth flow + the `x_timeline` agent. When `false`, neither the routes nor the agent are registered. |
| `X_OAUTH_CLIENT_ID`          | _(empty)_                                     | `x_timeline`   | X Developer app client id. Required for the OAuth flow to mount.                                                |
| `X_OAUTH_CLIENT_SECRET`      | _(empty)_                                     | `x_timeline`   | Optional `client_secret` (confidential-client mode). Leave empty for PKCE-only public clients.                |
| `X_OAUTH_REDIRECT_PATH`      | `/api/auth/login/x/callback`                 | `x_timeline`   | Path component of the OAuth callback URL; combined with `NAGENT_AUTH_PUBLIC_URL`.                              |
| `X_OAUTH_SCOPES`             | `tweet.read,users.read,follows.read`          | `x_timeline`   | Comma-separated OAuth scopes. The v1 surface is read-only.                                                     |
| `X_OAUTH_TIMEOUT_MS`         | `8000`                                        | `x_timeline`   | Per-request timeout in milliseconds for `/oauth2/token` + `/users/me` + timeline.                              |
| `X_TIMELINE_TIMEOUT_MS`      | `8000`                                        | `x_timeline`   | Per-request connect+read timeout in milliseconds for the timeline GET.                                         |
| `X_TIMELINE_MAX_POSTS`       | `20`                                          | `x_timeline`   | LLM-callable upper bound on returned posts.                                                                   |
| `X_TIMELINE_ALLOWLIST`       | `api.x.com,x.com`                             | `x_timeline`   | Hostname allow-list applied to the timeline URL.                                                               |
| `X_TIMELINE_CACHE_TTL_SECS`  | `60`                                          | `x_timeline`   | In-process cache TTL per `(user_id, mode)` pair. `0` disables.                                                 |
| `X_TIMELINE_BASE_URL`        | `https://api.x.com`                           | `x_timeline`   | Override the upstream base URL — useful for tests against a loopback fixture.                                  |
| `NAGENT_AUTH_ENABLED`        | `false`                                       | Auth           | Master switch. When `false`, the server keeps the single-user trust boundary (no `/api/me`, no `RequireAuth`, no login routes). |
| `NAGENT_AUTH_BACKENDS`       | _(empty)_                                     | Auth           | Comma-separated subset of `local`, `oidc`, `passkey`. Each enabled backend exposes its own login route.       |
| `NAGENT_AUTH_DB_BACKEND`     | _(empty)_                                     | Auth           | `"sqlite"` or `"postgres"`. Required when `auth.enabled = true`. The choice is runtime — both engines compile into the same binary. |
| `NAGENT_AUTH_DB_URL`         | _(empty)_                                     | Auth           | Connection URL — e.g. `sqlite://./data/auth.db?mode=rwc` or `postgres://user:pwd@host/nagent`. Required when `auth.enabled = true`. |
| `NAGENT_AUTH_DB_MAX_CONNECTIONS` | `16`                                      | Auth           | Maximum simultaneous DB connections. Clamped to ≥ 1.                                                          |
| `NAGENT_AUTH_PUBLIC_URL`     | _(empty)_                                     | Auth           | Public origin used for OIDC/Passkey callback URLs + cookie `Secure` flag. E.g. `https://nagent.example.com`.   |
| `NAGENT_AUTH_SESSION_TTL_DAYS` | `7`                                         | Auth           | Absolute session TTL (NIST SP 800-63B). Range `1..=90`. The session is created with `expires_at = now() + ttl` and is **never** extended by activity. |
| `NAGENT_AUTH_CSRF_HEADER`    | `x-csrf-token`                               | Auth           | CSRF token header name. State-changing requests (POST/PUT/PATCH/DELETE) must echo the per-session token.     |
| `NAGENT_AUTH_PASSWORD_ALLOW_REGISTRATION` | `true`                          | Auth / local   | When `true`, any logged-in user can POST to `/api/auth/password/register` to create a new local account.       |
| `NAGENT_AUTH_PASSWORD_ARGON2_MEMORY_KIB` | `19456`                          | Auth / local   | Argon2id memory cost in KiB. Clamped to ≥ 19456 (OWASP 2025 minimum) so a careless operator cannot weaken the hash. |
| `NAGENT_AUTH_PASSWORD_ARGON2_ITERATIONS` | `2`                             | Auth / local   | Argon2id time cost. Clamped to ≥ 1.                                                                          |
| `NAGENT_AUTH_PASSWORD_ARGON2_PARALLELISM` | `1`                            | Auth / local   | Argon2id parallelism (lanes). Clamped to ≥ 1.                                                                |
| `NAGENT_AUTH_PASSWORD_MIN_LENGTH` | `8`                                     | Auth / local   | Minimum password length accepted by `/api/auth/password/register`.                                            |
| `NAGENT_AUTH_OIDC_AUTO_PROVISION` | `true`                                  | Auth / OIDC    | When `true`, an OIDC user logging in for the first time has a `users` row created automatically.               |
| `NAGENT_AUTH_OIDC_ISSUER`     | _(empty)_                                     | Auth / OIDC    | OIDC issuer URL (e.g. `https://keycloak.example.com/realms/nagent`). Required when `oidc` is in `backends`.   |
| `NAGENT_AUTH_OIDC_CLIENT_ID` | _(empty)_                                     | Auth / OIDC    | OIDC client id registered with the IdP.                                                                       |
| `NAGENT_AUTH_OIDC_CLIENT_SECRET` | _(empty)_                                 | Auth / OIDC    | OIDC client secret. Prefer env-var injection over TOML to avoid leaking the secret in version control.       |
| `NAGENT_AUTH_OIDC_SCOPES`    | `openid,email,profile`                       | Auth / OIDC    | Comma-separated OIDC scopes.                                                                                 |
| `NAGENT_AUTH_OIDC_REQUIRED_GROUPS` | _(empty)_                              | Auth / OIDC    | Comma-separated IdP group allow-list; empty = no restriction.                                                 |
| `NAGENT_AUTH_OIDC_ROLE_CLAIM` | `groups`                                    | Auth / OIDC    | IdP claim name to map onto the local `roles` list. Reserved for a future RBAC layer; ignored at the HTTP layer today. |
| `NAGENT_AUTH_PASSKEY_SELF_REGISTRATION` | `true`                             | Auth / passkey | When `true`, any logged-in user can enrol a new passkey without an admin.                                      |
| `NAGENT_AUTH_PASSKEY_RP_ID`  | _(empty)_                                     | Auth / passkey | WebAuthn relying party id (no scheme, no port — e.g. `nagent.example.com`). MUST match the browser's effective domain. |
| `NAGENT_AUTH_PASSKEY_RP_NAME` | `nagent`                                    | Auth / passkey | Human-readable RP name shown by the authenticator.                                                              |
| `NAGENT_AUTH_PASSKEY_ORIGINS` | _(empty)_                                   | Auth / passkey | Comma-separated allowed origins — each entry MUST include scheme + port (e.g. `https://nagent.example.com`). |
| `RUST_LOG`                   | `info,nagent_server=debug,stt_core=debug` (local); `info,nagent_server=info,stt_core=info` (Docker) | Logging | Standard `tracing-subscriber` `EnvFilter` directive. |

Per-source-IP rate limiting applies at both layers: HTTP for the LLM proxy
(`/v1/chat/completions`, `/v1/models`) and at the WebSocket upgrade +
per-frame level for the STT pipeline. Loopback IPs always bypass.

### TOML configuration file

Pass `--config <path>` on the command line to load a TOML file. The
file provides per-deployment defaults; environment variables
(including values from `.env`) always override it. Unknown fields are
rejected at load time (`deny_unknown_fields`) so a typo never silently
reverts to a default.

When `--config` is **omitted**, the server still tries to load two
default files and merges them — the later one wins, missing files are
silently skipped, and env vars still trump both:

1. `/etc/nagent/config.toml` (system-wide defaults — FHS convention).
2. `$XDG_CONFIG_HOME/nagent/config.toml` (or
   `~/.config/nagent/config.toml` when `XDG_CONFIG_HOME` is unset/empty,
   per the XDG Base Directory spec). This is the per-user override.

A commented-out starter file lives at
[`docs/examples/config.toml.example`](docs/examples/config.toml.example). Copy it,
edit the values you want, and point the binary at it:

```
nagent-server --config /etc/nagent/config.toml
```

Server knobs live under `[server]`; the two grouped sub-tables are
`[server.limits]` for inbound WS frame limits and `[server.rate_limits]`
for per-source-IP rate limits:

| TOML key                          | Equivalent env var          | Notes                                          |
| --------------------------------- | --------------------------- | ---------------------------------------------- |
| `[server].bind_addr`              | `BIND_ADDR`                 |                                                |
| `[server].whisper_model_path`     | `WHISPER_MODEL_PATH`        | Required (env or TOML).                        |
| `[server].max_queue`              | `MAX_QUEUE`                 |                                                |
| `[server].inference_workers`      | `INFERENCE_WORKERS`         | Stick to backend-derived default unless overridden. |
| `[server].session_idle_timeout_ms`| `SESSION_IDLE_TIMEOUT_MS`   |                                                |
| `[server].infer_timeout_ms`       | `INFER_TIMEOUT_MS`          |                                                |
| `[server.limits].*`               | `MAX_*`, `REQUIRED_*`, `REQUEST_TIMEOUT_MS`, `BODY_LIMIT_BYTES`, `WS_MAX_*` | WebSocket frame knobs + HTTP transport guardrails (S-1). |
| `[server.allowed_origins].*`      | `NAGENT_ALLOWED_ORIGINS`    | `Origin` / `Host` allow-list (plan S-2). Empty = derived from `bind_addr`. |
| `[server.rate_limits].*`          | `STT_RATE_PER_MIN`, `LLM_*` | Per-IP buckets.                                |
| `[server.trusted_proxies].*`      | `NAGENT_TRUSTED_PROXIES*`   | Reverse-proxy CIDR list (see "Trusted proxies" below). |
| `[llm].*`                         | `LLM_*`, `OLLAMA_*`         | OpenAI-compatible proxy.                       |
| `[auth].*`                        | `NAGENT_AUTH_*`             | Multi-user authentication (see "Authentication" below). `[auth.db]` selects sqlite vs postgres at runtime. |
| `[auth.credentials].key`          | _(none)_                    | AES-256-GCM encryption key for the per-user credentials vault, in plaintext inside the TOML file (64 hex chars / 32 bytes). REQUIRED when `auth.enabled = true` AND at least one agent is registered; the server refuses to boot otherwise. See "Per-user credentials" below for the key-generation recipe and the secret-handling caveat. |

The `[agents]` section is a general block for chat-agent settings: it
carries the master switch (`enabled`) plus one sub-table per tool,
keyed by the agent's name. The tool-loop knobs
(`llm_max_tool_rounds`, `llm_max_auto_continues`) live on the
`[llm]` section because they gate the proxy's tool loop, not the
agent registry.

```toml
[server]
bind_addr = "0.0.0.0:8080"
whisper_model_path = "/models/ggml-base.bin"
max_queue = 32

[server.limits]
max_audio_frame_samples = 480_000
required_sample_rate = 16_000

[server.rate_limits]
stt_per_min = 120
llm_per_min = 30

[server.trusted_proxies]
cidr = "10.0.0.0/8,192.168.0.0/16"
loopback_bypass = true

[llm]
llm_max_tool_rounds = 4
llm_max_auto_continues = 1

[agents]
enabled = true

[agents.web_fetch]
allow_public = false
allowlist = ["*.wikipedia.org", "example.com"]
max_bytes = 2_097_152
timeout_ms = 30_000

[agents.get_weather]
api_key = "your-weatherapi-key"
timeout_ms = 8_000
base_url = "https://api.weatherapi.com"
```

Unknown sub-tables under `[agents]` (e.g. `[agents.web_fetxh]`) are
rejected at load time. The five daily tools take small overrides
(`unit_convert`, `wikipedia`, `dictionary`); `get_datetime`,
`get_stock_quote`, and `calculate` need no configuration today.

### Trusted proxies (security plan #5)

When the server sits behind a reverse proxy (ingress-nginx, Caddy,
an AWS ALB, a Cloudflare tunnel, …) every TCP connection peers
from the proxy's IP — putting every client in the same
`[server.rate_limits]` bucket. Configure
`[server.trusted_proxies].cidr` (or `NAGENT_TRUSTED_PROXIES`) with
the proxy's IP range so the rate-limit resolver honours
`X-Forwarded-For` for those peers, while leaving the header
ignored (and unspoofable) for direct connections:

```toml
[server.trusted_proxies]
# Comma-separated CIDR list. Examples:
#   k8s pod CIDR:        cidr = "10.0.0.0/8"
#   Docker bridge:        cidr = "172.16.0.0/12"
#   local sidecar:        cidr = "127.0.0.1/32"
#   multiple ranges:      cidr = "10.0.0.0/8,192.168.0.0/16"
cidr = "10.0.0.0/8,192.168.0.0/16"
# Loopback IPs bypass the rate-limit bucket by default so local
# dev / tests do not need to fight the limiter. Set to `false`
# in production to remove the carve-out.
loopback_bypass = true
```

Boot emits a `WARN` when the bind address is non-loopback and
`trusted_proxies.cidr` is empty — that combination is almost
always a misconfiguration behind a reverse proxy.

### Origin / Host allow-list (security plan S-2)

Browsers do not apply CORS to WebSocket upgrades, so a public
deployment of `nagent-server` is wide-open to cross-site WebSocket
hijacking (CSWSH) and DNS-rebinding when the bind address is
reachable. With `auth.enabled = false` (the default) the
[`RequireAuth`] middleware is not installed — any web page the user
visits can open `ws://localhost:8080/ws` and start streaming audio
through the inference pipeline.

The server validates `Origin` on every state-changing route
(`POST` / `PUT` / `PATCH` / `DELETE`) and on the WebSocket upgrade,
and validates `Host` against an allow-list derived from
`[server].bind_addr` plus any operator-supplied entries. Cross-
origin POSTs return `403 Forbidden`; cross-site WS upgrades are
rejected at the HTTP layer (no WS handshake is started). A request
with a forged `Host` (DNS rebinding attempt) is rejected with
`421 Misdirected Request`.

The default allow-list (no operator override) is derived from
`[server].bind_addr` (loopback names plus the bind host) so the
historical dev workflow keeps working on a single-user loopback
deployment. Operators exposing the server on a non-loopback
address MUST populate the allow-list with the publicly-
reachable scheme + host:

```toml
[server.allowed_origins]
# Empty by default — derived from `bind_addr` so a single-user
# loopback bind keeps working. Required when the bind address is
# non-loopback, otherwise every state-changing request fails.
origins = ["https://nagent.example.com"]
```

The matching env var is `NAGENT_ALLOWED_ORIGINS` (comma-
separated). Env wins over the TOML file when both are set.

[`RequireAuth`]: #routing-contract

## Authentication

The server ships a multi-user identity subsystem. When the runtime
config sets `auth.enabled = false` (the default), the server
keeps the single-user trust boundary: no login routes, no
`/api/me`, no `RequireAuth` layer. Operators opt in by setting
`[auth].enabled = true` (or `NAGENT_AUTH_ENABLED=true`) and
configuring at least one backend in `[auth].backends`.

### Routing contract

When `auth.enabled = true`, the `RequireAuth` middleware is
installed on every functional endpoint. The set of routes that
stay reachable without a session cookie is intentionally small,
so the browser can fetch the login page and submit credentials,
and ops tooling keeps working:

| Route                                                | Why it is public                                  |
| ---------------------------------------------------- | ------------------------------------------------- |
| `GET /`                                              | Serves the login page (index.html).               |
| `GET /static/*`                                      | Frontend assets bundled into the binary.          |
| `GET /healthz`                                       | Health probe for orchestrators / load balancers.  |
| `GET /api/version`                                   | Version probe used by the frontend update banner. |
| `POST /api/auth/login/password`                      | Password login.                                   |
| `POST /api/auth/login/passkey/start` / `/finish`     | Passkey login ceremony.                           |
| `GET /api/auth/login/oidc/start` / `/callback`       | OIDC redirect + callback.                         |
| `POST /api/auth/password/register`                   | First-time account creation.                      |

Everything else — the STT WebSocket upgrade (`/ws`),
`/v1/chat/completions`, `/v1/models`, `/v1/agents*`,
`/v1/audio/*`, `/api/me`, `/api/integrations*`, `POST /api/auth/logout`,
the passkey register start/finish routes — requires a valid session
cookie (or `Authorization: Bearer <session-id>`). Anonymous
requests get `401 authentication required` with a
`WWW-Authenticate: Cookie realm="nagent"` header.

### Backends

Three independent backends can coexist on the same server. Each one
exposes its own login route; the operator enables a subset via
`[auth].backends`:

| Backend   | Login route(s)                                      | Storage                  | Notes |
| --------- | --------------------------------------------------- | ------------------------ | ----- |
| `local`   | `POST /api/auth/login/password`                     | argon2id in `users.password_hash` | Argon2id with OWASP 2025 default parameters (m = 19 MiB, t = 2, p = 1). Passwords are never logged, never echoed back to the client. |
| `oidc`    | `GET /api/auth/login/oidc/start` + `/callback`       | none (IdP is the source of truth) | PKCE + state persisted in the `pending_oidc_states` table. Auto-provisioning is on by default — first login creates a `users` row keyed on the IdP's email claim. Set `auth.oidc.auto_provision = false` to reject unknown emails with `403`. |
| `passkey` | `POST /api/auth/login/passkey/{start,finish}`       | WebAuthn credentials in `passkeys` | Discoverable credentials (no `userHandle` round-trip). `webauthn-rs` 0.6 pre-release with `conditional-ui` + `resident-key-support` features. The registration ceremony is gated by `RequireAuth` and `auth.passkey.self_registration` (default `true`). |

The OIDC handlers accept the `state` and `nonce` standard claims and
verify them, but the JWT signature is not checked against the IdP's
JWKS yet — the verification path is logged as a TODO in
`crates/nagent-server/src/auth/oidc.rs`. HTTPS deployments where the
transport already authenticates the IdP are unaffected; operators
sensitive to network-level attacks should keep OIDC disabled until
the JWKS check lands.

The backends share one DB-backed session table (`sessions`) and one
user table (`users`). A user created via the local backend can log
in with a passkey later (and vice versa) — the `provider` column on
`users` is a label, not a unique constraint.

### Storage engine

`auth.db.backend` chooses the storage engine **at runtime** — the
same binary compiles both, the operator picks at boot. The
`sqlx::migrate!` macro applies the embedded `crates/nagent-server/src/db/migrations/0001_init.sql`
on first start (idempotent; sqlx tracks applied versions in its
own `_sqlx_migrations` table). The migration files were relocated
under `src/db/migrations/` as part of plan 5.D so every domain
table is owned by exactly one migration, indexed next to the
per-domain repository it belongs to.

| Backend    | Connection URL example                                            | Notes |
| ---------- | ----------------------------------------------------------------- | ----- |
| `sqlite`   | `sqlite://./data/auth.db?mode=rwc`                                | Single binary, no external service. The connection enables `WAL` journal mode + `foreign_keys = ON` + a 5 s busy timeout. The data directory (`./data/`) is auto-created on boot. |
| `postgres` | `postgres://nagent:pwd@db.internal/nagent`                        | Standard Postgres 14+. The schema uses `TEXT PRIMARY KEY` for UUIDs (not the native `uuid` type) so the migrations stay portable across engines. |

`PRAGMA foreign_keys = ON` is set on every sqlite connection so the
`ON DELETE CASCADE` clauses on `passkeys` / `sessions` actually
fire (sqlite ships with FK enforcement off by default).

### Cookie + CSRF model

Authenticated browser clients carry a `nagent_session` cookie
holding the session id (a UUID v4). API clients can pass the same id
via `Authorization: Bearer <session-id>`. State-changing browser
requests (POST / PUT / PATCH / DELETE) must also send a per-session
CSRF token via the `x-csrf-token` header (configurable via
`auth.csrf_header`); bearer requests skip the CSRF check because
they cannot be tricked into cross-site submissions. CSRF tokens are
32 random bytes hex-encoded, generated server-side at session
creation, and never leave the server otherwise.

The cookie `Secure` flag is **auto-disabled on `http://localhost`**
(`127.0.0.1` too) so a local dev setup does not silently drop
cookies. For every other URL the `Secure` flag mirrors the scheme
(an `https://` public URL gets `Secure = true`; `http://`
anywhere gets `Secure = false`).

Sessions are **absolute** (NIST SP 800-63B pattern): `expires_at` is
set to `now() + auth.session_ttl_days` at login and is never
extended by activity. The default TTL is 7 days, clamped to the
documented range `1..=90`. The `last_seen_at` column is debug-only
and does not feed back into the expiry calculation.

### Login rate-limit

The local backend counts login attempts per `(email, ip)` pair with
a 5-attempts / 15-min budget (progressive, in-process via `DashMap`).
Loopback IPs bypass. On exhaustion the server returns `429 Too Many
Requests` with a `Retry-After` header.

### Bootstrap

On server boot, when `auth.enabled = true`:

1. The server connects to the auth DB and runs the migrations
   (idempotent — sqlx tracks applied versions in its
   `_sqlx_migrations` table).
2. **SQLite only**: if the `users` table has zero rows, the server
   auto-bootstraps the first local admin with a random 24-char
   password, hashes it with argon2id, and logs the credentials to
   stderr at `WARN` level (along with a one-time "save this now"
   reminder). Postgres deployments never auto-create — admins
   must be created via the CLI to avoid silent privilege grants on
   a shared cluster.

To create additional admins after the bootstrap, run the CLI:

```bash
# The CLI opens the pool, runs the migration, and inserts the row
# — the server does NOT need to be running. It refuses (without
# --force) when data/server.pid points at a live process.
nagent-server auth create-admin \
  --email admin@example.com \
  --from-stdin           # password read from stdin to avoid argv / shell history
```

The CLI returns the new `user_id` on stdout and exits 0. Two
additional CLI subcommands round out the operator surface:

```bash
nagent-server auth list-users [--provider local|oidc|passkey]   # tab-separated
nagent-server auth delete-user --email <email> [--yes]          # refuses to remove the last local user when auth is enabled
```

The CLI reads the same `[auth.db]` configuration + env vars the
server uses, so there is no second source of truth. Boot the server
once to apply the schema; the CLI can run before or after.
note lives in `crates/nagent-server/src/auth/oidc.rs`.

### Per-user credentials (framework)

The server ships a **per-user credentials vault** so chat agents can
read hostnames, logins, and tokens the user has configured for
third-party integrations (IMAP/SMTP, CalDAV, Home Assistant, GitHub,
…) without leaking those secrets to the LLM prompt or to logs. This
section documents the framework only; concrete services land in
follow-up PRs.

#### Threat model

- Plaintext credentials are encrypted at rest with **AES-256-GCM**.
  The encryption key (64 hex chars / 32 raw bytes) is held in
  plaintext inside `[auth.credentials].key` in the server's TOML
  config. Operators who want to keep the secret out of disk should
  mount the TOML file from an encrypted volume (k8s `Secret` via
  `subPath`, Vault Agent, …) — the configuration shape itself does
  not force plaintext storage on disk, only on the source-of-truth
  file.
- Each agent call constructs a fresh `UserContext` that owns one
  in-memory `SecretCache`. The cache zeroises on drop, so
  plaintexts only live for the duration of one LLM tool round (or
  one direct `/v1/agents/:name/invoke` HTTP call). There is no
  long-lived plaintext cache anywhere in the process.
- Every successful credential read writes one `auth_events` row
  (`kind = "credential_access"`, `target_service = <service-id>`).
  Missing rows write `"credential_missing"`; AES-GCM authentication
  failures write `"credential_decrypt_failed"`. Secret values are
  never stored in `auth_events`.
- Sessions are **per-user**: the credential owner is always the
  resolved `AuthUser`. There is no `?as=…` parameter and no admin
  override.

#### Boot wiring

The server refuses to boot when **`auth.enabled = true` AND at
least one agent is registered AND `[auth.credentials].key` is unset
or malformed**. Generate a 32-byte key once at install time and
paste it under `[auth.credentials].key` in the TOML config:

```bash
# 64 hex chars = 32 bytes. Paste the output under
# [auth.credentials].key in the TOML file. Rotating means:
# generate a new key, then re-encrypt every row in
# `user_credentials` (rotation is NOT supported in v1 — see
# "Risks" below).
openssl rand -hex 32
```

The matching TOML block:

```toml
[auth.credentials]
# 64 hex chars = 32 raw bytes (AES-256-GCM). Treat this file as
# a secret — encrypted volume, restricted ACLs, never VCS.
key = "<paste the 64-char hex string here>"
```

When `auth.enabled = false` OR every agent is disabled, the field
is silently ignored and the framework stays dormant (the
pre-credentials single-user trust boundary is preserved).

#### HTTP surface

All routes are mounted under the existing `RequireAuth` middleware
— there is no anonymous access. The credential owner is always the
session user.

| Method   | Path                                       | Description                                                                                       |
| -------- | ------------------------------------------ | ------------------------------------------------------------------------------------------------- |
| `GET`    | `/api/integrations`                        | List every registered service with `configured: bool` per service for the caller.                 |
| `GET`    | `/api/integrations/:id`                    | Single-service summary. `404` (via `400 unknown service`) on unknown id.                            |
| `PUT`    | `/api/integrations/:id/credentials`        | Atomic replace of every field for the service. `204 No Content` on success.                       |
| `DELETE` | `/api/integrations/:id/credentials`        | Clear every field for the service. `204 No Content` on success.                                   |

`PUT` / `DELETE` are CSRF-protected via the same `x-csrf-token`
header that guards the auth subtree. `GET` responses never carry
plaintext values; only the per-field `filled` boolean is exposed.

Example (configure an integration):

```bash
# 1. Log in to obtain a session cookie + CSRF token.
# 2. PUT the credentials (plaintext — encrypted server-side at rest).
curl -X PUT http://localhost:8080/api/integrations/email_imap/credentials \
  -b "nagent_session=$SESSION_ID" \
  -H "x-csrf-token: $CSRF" \
  -H "Content-Type: application/json" \
  -d '{"fields": {"host": "imap.example.com", "port": "993", "username": "alice", "password": "…"}}'
```

The browser UI surfaces the same flow under **Advanced → Integrations**
in the Discussion view (hidden until `auth.enabled = true` and the
user is logged in). The drawer lists every registered service with
a "Configured" / "Not configured" pill and a Configure / Edit
button that opens a per-field modal.

#### Agent trait

Every `Agent::invoke` now takes `&UserContext` as its first
argument. Agents that do not need credentials ignore the parameter
(let-binding `_ctx: &UserContext`). Agents that need a credential
call:

```rust
let password = ctx.secret("email_imap", "password").await?
    .ok_or_else(|| anyhow::anyanyhow!("missing email_imap.password"))?;
// `password` is a `SecretString`; the compiler will refuse to log it.
```

Failures surface as `AgentError::CredentialsMissing { service,
field }` (the user has not configured it) or
`AgentError::CredentialsDecryptFailed { service, field }` (the
server-side key was rotated without re-encrypting the row). The
chat UI detects the first variant and renders a clickable
"configure `<service>`" link so the LLM can recover without a
round-trip.

#### Schema

The `0002_credentials.sql` migration adds:

- `auth_events.target_service TEXT` — backfilled as `NULL` for
  pre-migration rows. Never carries a secret value.
- `user_credentials` — one row per `(user, service, field)`. Holds
  the AES-256-GCM nonce + ciphertext. FK-cascades on `users.id`
  so deleting a user wipes their creds.

#### Risks & follow-ups

- **Key rotation is NOT supported in v1.** The
  `[auth.credentials].key` value is read once at boot. Rotating
  requires re-encrypting every row in `user_credentials`; a
  follow-up PR will add a double-write migration helper.
- **Server-wide fallback credentials** are not in v1. The
  framework is per-user only; shared agents (e.g. weather) keep
  their global config (`[agents.get_weather].api_key`).
 - **Concrete integrations** (IMAP, CalDAV, GitHub, Home Assistant,
   …) land in their own follow-up PRs. Adding one is a single
   `ServiceDef` to `crates/nagent-agents/src/services.rs` plus
   an agent that reads creds via `ctx.secret(...)`. The CalDAV
   plugin is the first such concrete integration: build with
   `--features nagent-server/caldav-agent` to enable
   `caldav_list_events` / `caldav_get_event` / `caldav_create_event`
   plus the setup-only `POST /api/integrations/caldav/probe-calendars`
    endpoint (see [`docs/integrations/caldav.md`](docs/integrations/caldav.md)
    and [`examples/caldav.toml`](examples/caldav.toml)).
  - **X (Twitter) timeline** is the second concrete integration
    (plan 1790695073418). Build with
    `--features nagent-server/x-agent` to enable the
    `x_timeline` chat agent, the PKCE OAuth 2.0 flow at
    `/api/auth/login/x/{start,callback,disconnect}`, and the
    `x_account` entry in the per-user service catalogue.
    Operator opt-in is `X_OAUTH_CLIENT_ID` (and optionally
    `X_OAUTH_CLIENT_SECRET` for confidential-client mode). The
    agent refreshes the access token itself on 401. See
    [`docs/integrations/x.md`](docs/integrations/x.md).

### Kubernetes overlays

`deploy/k8s/` ships one base manifest and three overlays. Pick exactly one
overlay per cluster.

| Overlay                                | What it adds                                                                                                  |
| -------------------------------------- | ------------------------------------------------------------------------------------------------------------- |
| `deploy/k8s/base`                      | Deployment, Service, PVC, ServiceAccount/RBAC, model-downloader Job, default ConfigMap.                      |
| `deploy/k8s/overlays/dev`              | Namespaced service patch for local development (no ingress).                                                  |
| `deploy/k8s/overlays/prod`             | `stt` namespace, 2 replicas, image pinned to `v0.1.0`, ingress, prod-sized resources, `RUST_LOG` toned down.   |
| `deploy/k8s/overlays/prod-gpu-nvidia`  | Reuses `prod` and requests one NVIDIA GPU per pod with the matching toleration.                               |

Render any overlay locally with `make kustomize-build-<name>` (see `make help`).

### Build / run targets

`make help` lists every target; the commonly relevant ones are:

- `make build` / `make debug` — release / debug build of the workspace.
- `make run` — CPU Whisper backend (real).
- `make run-mock` — in-process mock backend, no model needed (CI / tests).
- `make run-vulkan` / `make run-cuda` / `make run-hipblas` — GPU backends.
- `make run-llm` — CPU backend + LLM proxy enabled (`LLM_ENABLED=true`).
- `make docker-build[-<backend>]` and `make docker-run[-gpu-*]` — image and container entry points per backend.
- `make kustomize-build-{base,dev,prod}` / `make apply-prod` — Kubernetes workflow.

## Testing

```
make fmt
make clippy
make test
```

The multiuser isolation test (Phase 6) lives in
`crates/nagent-server/tests/multiuser_isolation.rs` and uses a mock backend so
it runs without any GPU.

## Supply chain hygiene (S2a)

```
cargo install cargo-deny --locked        # bans / sources / licenses / advisories
cargo install cargo-cyclonedx --locked   # CycloneDX SBOM from Cargo.lock
```

Three independent checks plus a weekly Dependabot scan
keep the dependency tree auditable:

- `cargo deny check` — driven by [`deny.toml`](deny.toml).
  Bans cover `chrono <0.4.20` (Y2K38), `webauthn-rs <0.6.1-dev`
  (must be the audited dev build), and any future supply-chain
  addition; licenses allow the standard permissive family plus
  the project's `LicenseRef-OpenCore-Source-Available-1.0`;
  sources pin `crates-io`. CI runs `cargo deny check bans
  licenses sources` so CVEs (separate `cargo audit` run) do not
  hide the other categories.
- `cargo cyclonedx` — emits `target/sbom.cargo.json`, uploaded
  as a build artefact for downstream consumers.
- Syft (`anchore/sbom-action`) — emits a CycloneDX manifest of
  the runtime Docker image, uploaded alongside the cargo one.
  Together the two SBOMs cover the source tree and the shipped
  image.

## Chat (Ollama)

The web UI has two modes, switched at the top of the page:

- **Transcript** — the default STT UI (unchanged).
- **Discussion** — a chat UI backed by a server-side proxy to a local
  Ollama-compatible endpoint.

Discussion mode is opt-in: the LLM proxy is **off** unless
`LLM_ENABLED=true` is set. When disabled, the `/v1/*` routes are not
registered at all and the Discussion view shows a "Chat disabled on this
server" notice.

### Quick start

1. Install Ollama (<https://ollama.com>) and pull a model:

   ```
   ollama pull llama3.1
   ```

2. Start `nagent-server` with the proxy enabled:

   ```
   make run-llm
   # equivalent to: LLM_ENABLED=true cargo run -p nagent-server --release \
   #     --features real-backend,web-agent,stt-core/whisper-rs-backend
   ```

3. Open <http://localhost:8080>, click **Discussion**, type a message.

### Configuration

All settings are environment variables (no code changes required). The full
master table lives in the [Configuration](#configuration) section above; the
LLM-specific knobs are:

| Variable                     | Default                  | Meaning                                                           |
| ---------------------------- | ------------------------ | ----------------------------------------------------------------- |
| `LLM_ENABLED`                | `false`                  | Master switch; when `false` the `/v1/*` routes are not registered |
| `OLLAMA_BASE_URL`            | `http://localhost:11434` | Base URL of the OpenAI-compatible upstream (Ollama by default)    |
| `OLLAMA_MODEL`               | `llama3.1`               | Default model for `/v1/chat/completions` when the client omits it |
| `OLLAMA_API_KEY`             | _(unset)_                | Optional bearer token forwarded as `Authorization: Bearer …` on outbound upstream requests. Unrelated to `LLM_API_KEY` below. |
| `LLM_API_KEY`                | _(unset)_                | Optional bearer token required on inbound `/v1/*` requests when `LLM_AUTH_MODE=bearer`. Has no effect in `forward` / `disabled` modes. |
| `LLM_AUTH_MODE`              | `forward`                | Inbound auth gate for `/v1/*` requests. `bearer` requires `Authorization: Bearer <LLM_API_KEY>` (returns `401` otherwise). `forward` keeps the historical behaviour (no inbound inspection). `disabled` is the explicit opt-out — `main` logs a startup warning when the server binds a non-loopback address. |
| `LLM_REQUEST_TIMEOUT_SECS`   | `120`                    | Per-chunk idle timeout on the upstream stream                     |
| `LLM_CORS_ALLOW_ORIGINS`     | _(empty)_                | Comma-separated list of origins allowed to call `/v1/*` cross-origin (empty = same-origin only) |
| `LLM_SYSTEM_PROMPT`          | _(unset)_                | Optional default system prompt prepended to every `/v1/chat/completions` request as `messages[0]` (the browser's "Additional instructions" textarea is appended after it). Empty / whitespace-only values are treated as unset. |
| `LLM_ALLOW_USER_LOCATION`    | `true`                   | When `false`, the server strips the browser-injected `User's approximate location:` system message before forwarding to the upstream model. The browser still requires explicit user consent; this is a defence-in-depth kill-switch for sensitive deployments. |
| `LLM_ALLOW_USER_TIMEZONE`    | `true`                   | When `false`, the server strips the browser-injected `The user's local timezone is` system message before forwarding to the upstream model. Independent of `LLM_ALLOW_USER_LOCATION`. |
| `LLM_ALLOW_USER_REPLY_LANGUAGE` | `true`                | When `false`, the server strips the server-injected `The user's preferred reply language is` system message before forwarding to the upstream model. The block is emitted only when the authenticated user has set a non-default reply language in the Advanced drawer. Independent of the two flags above. |
| `LLM_RATE_PER_MIN`           | `30`                     | Inbound LLM HTTP requests per source IP per minute                |

### Agents (`web_fetch`)

All `make run*` targets (`run`, `run-vulkan`, `run-hipblas`, `run-cuda`,
`run-mock`, `run-llm`) compile the server with the `web-agent` cargo
feature, so the LLM can call a server-side `web_fetch` agent that
fetches an HTTP(S) URL and feeds the cleaned text back into the model
as a `role: "tool"` message whenever the LLM proxy is also enabled
(`LLM_ENABLED=true`, which only `make run-llm` does by default). Two
bubbles appear inline under the assistant message while the agent
runs:

```
assistant ┃ Sure, let me fetch that for you.
tool      ┃ 🔎 web_fetch https://example.com
tool      ┃ ✓ 1.2 KB — Example Domain
assistant ┃ Example Domain is a simple illustrative page created by…
```

Tool bubbles are persisted into the chat history; a follow-up turn
keeps the fetched content in the LLM's context without re-fetching.

| Variable                  | Default | Meaning                                                                              |
| ------------------------- | ------- | ------------------------------------------------------------------------------------ |
| `AGENTS_ENABLED`          | `true`  | Master switch; when `false` the registry is empty and the LLM sees no `tools` array |
| `LLM_MAX_TOOL_ROUNDS`     | `4`     | Hard cap on tool-call rounds per user turn (defends against a runaway tool loop)     |
| `WEB_FETCH_ALLOW_PUBLIC`  | `false` | When `true`, the agent may reach public IP ranges. Loopback and RFC1918 are still blocked as SSRF protection. |
| `WEB_FETCH_ALLOWLIST`     | _(empty)_ | Comma-separated hostname allow-list (suffix match, `*.foo` wildcards, bare `*` for everything). Takes precedence over `WEB_FETCH_ALLOW_PUBLIC`. |
| `WEB_FETCH_MAX_BYTES`     | `2097152` | Maximum response size the agent will read (2 MiB). The agent iteratively doubles the budget on overflow, capped here. |
| `WEB_FETCH_TIMEOUT_MS`    | `30000` | Per-request timeout in milliseconds.                                                |

Every `Agent::invoke` carries a `&UserContext` (the per-request
context built by the LLM proxy). Agents that ignore credentials
let-bind `_ctx: &UserContext`; agents that need them call
`ctx.secret("service_id", "field_key").await` to decrypt on demand
— see the "Per-user credentials" section below.

### Daily agents (datetime, weather, stock quote, calculate, unit convert, wikipedia, dictionary)

Seven small, read-only agents complement `web_fetch` for everyday chat
queries. They are wired by default on every `make run*` target
through the `datetime-agent`, `weather-agent`, `stock-agent`,
`calculate-agent`, `unit-convert-agent`, `wikipedia-agent`, and
`dictionary-agent` cargo features; set `AGENTS_ENABLED=false` to
disable all agents at runtime without recompiling.

**`get_datetime`** — current local time, optionally localised to an
IANA timezone. No network.

- FR: "quelle heure est-il à Tokyo ?", "date du jour"
- EN: "what time is it in New York?", "current date"
- Params: `timezone` (optional, e.g. `Europe/Paris`)

```
curl -s -X POST localhost:8080/v1/agents/get_datetime/invoke \
  -H 'content-type: application/json' \
  -d '{"arguments":{"timezone":"Europe/Paris"}}'
```

**`get_weather`** — current conditions, 14-day forecast, 24h
hourly, historical data, and astronomy (sunrise/sunset, moon phase)
for any location. Backed by [WeatherAPI.com](https://www.weatherapi.com/);
requires `WEATHER_API_KEY` (free tier: 1M calls/month, key issued
by email — no card required).

- FR: "météo à Paris demain", "il va pleuvoir à Londres ce soir ?",
  "coucher de soleil à Tokyo", "UV à Lyon ce week-end"
- EN: "weather in Tokyo", "will it rain in London tonight",
  "sunset in Paris tomorrow"
- Params: `location` (required — city name, `"lat,lon"`, postal
  code, or iata code), `days` (1-14, default 1, ignored when
  `date` is set), `date` (YYYY-MM-DD, past dates back to
  2010-01-01), `hourly` (bool, default false — include 24h
  hourly breakdown).

```
curl -s -X POST localhost:8080/v1/agents/get_weather/invoke \
  -H 'content-type: application/json' \
  -d '{"arguments":{"location":"Paris","days":3}}'

curl -s -X POST localhost:8080/v1/agents/get_weather/invoke \
  -H 'content-type: application/json' \
  -d '{"arguments":{"location":"48.8566,2.3522","hourly":true}}'
```

**`get_stock_quote`** — latest Stooq quote for a ticker. Three
resolution layers, tried in order:

1. **Company-name shortcut** — a small built-in table maps
   common European / French company names (Atos, LVMH, BNP
   Paribas, Sanofi, Airbus, …) plus US big-tech names (Apple,
   Microsoft, NVIDIA, …) to their Stooq ticker. The agent lands
   on the right exchange on the first round-trip — type "Atos",
   get the Euronext Paris quote for `ATO.PA`.
2. **Ticker normalisation** — bare tickers (`AAPL`, `NVDA`) get
   the `.US` suffix; already-suffixed tickers (`AIR.PA`,
   `MC.PA`) pass through.
3. **Multi-exchange fallback** — when the primary lookup returns
   no data and the user did not pin an explicit exchange, the
   agent tries the same root ticker on `.PA`, `.L`, `.DE`,
   `.MI`, then `.US` last. The first success wins; on total
   failure the agent reports every ticker it tried so the LLM
   can suggest the next step.

- FR: "cours de Atos", "prix action LVMH", "BNP Paribas"
- EN: "AAPL stock price", "quote for TSLA"
- Params: `ticker` (required, ≤40 chars)

```
curl -s -X POST localhost:8080/v1/agents/get_stock_quote/invoke \
  -H 'content-type: application/json' \
  -d '{"arguments":{"ticker":"AAPL"}}'

curl -s -X POST localhost:8080/v1/agents/get_stock_quote/invoke \
  -H 'content-type: application/json' \
  -d '{"arguments":{"ticker":"Atos"}}'
```

**`calculate`** — evaluates an arithmetic expression locally with
[`meval`](https://crates.io/crates/meval). Supports `+ - * / ^ %`,
parentheses, common math functions (`sin`, `cos`, `sqrt`, `log`,
`abs`, …), and constants (`pi`, `e`). A character-class allow-list
plus a 256-character cap is applied before evaluation; anything
outside `[0-9a-zA-Z_+\-*/()., \t\n]` is rejected with a clear error
so a typo never reaches the parser. No network, no I/O, no user
variables.

- FR: "combien font 15 % de 87,50 ?", "sqrt(2) + 1", "2^10"
- EN: "what is 17% of 230?", "log(1000) in base 10"
- Params: `expression` (required, ≤256 chars)

```
curl -s -X POST localhost:8080/v1/agents/calculate/invoke \
  -H 'content-type: application/json' \
  -d '{"arguments":{"expression":"15*87.5/100"}}'
```

**`unit_convert`** — pure-local unit conversions across eight
categories: length, mass, volume, time, data (binary + decimal),
speed, area, and temperature (special-cased since °F / °C / K are
not linear). Each unit entry knows its category; cross-category
conversions (`km` → `kg`) are rejected. Symbols that map to
multiple categories (e.g. `pt` = pint / typographic point) are
intentionally absent from the table — the agent asks the LLM to
pick a less ambiguous name rather than guessing.

- FR: "12 miles en km", "100 GB en MB", "100 °C en °F", "1 h en secondes"
- EN: "convert 12 miles to km", "100 GB to MB", "100°C to °F"
- Params: `value` (number, required), `from` (string, required),
  `to` (string, required). Accepts full names (`kilometre`,
  `celsius`) or symbols (`km`, `°C`, `K`, `GB`, `MiB`).

```
curl -s -X POST localhost:8080/v1/agents/unit_convert/invoke \
  -H 'content-type: application/json' \
  -d '{"arguments":{"value":12,"from":"mile","to":"km"}}'

curl -s -X POST localhost:8080/v1/agents/unit_convert/invoke \
  -H 'content-type: application/json' \
  -d '{"arguments":{"value":100,"from":"°C","to":"°F"}}'
```

**`wikipedia`** — short summary of a Wikipedia article via the
public REST endpoint
`https://en.wikipedia.org/api/rest_v1/page/summary/{title}`. No API
key required; Wikimedia's policy is to reject unidentified clients,
so the agent unconditionally sets a descriptive `User-Agent`
header (override via `WIKIPEDIA_USER_AGENT`).

- FR: "qui est Marie Curie ?", "parle-moi de la Renaissance", "résumé wikipédia de la photosynthèse"
- EN: "who is Marie Curie?", "tell me about the Renaissance", "Wikipedia summary of photosynthesis"
- Params: `title` (required, ≤200 chars)
- NOT for cities-as-places (use `get_weather` for weather forecasts or
  a geo tool for "où suis-je"); this tool returns the encyclopedia
  article ABOUT a subject (people, events, concepts, works, historical
  places).

```
curl -s -X POST localhost:8080/v1/agents/wikipedia/invoke \
  -H 'content-type: application/json' \
  -d '{"arguments":{"title":"Lyon"}}'
```

**`dictionary`** — definitions, phonetics, examples, and synonyms
for an English word via the anonymous Free Dictionary REST API
(<https://api.dictionaryapi.dev/api/v2/entries/en/{word}>). No API
key required; the agent trims the upstream payload to the fields
the LLM actually needs and surfaces a 404 as a clear
"no definitions found" so the LLM does not silently fabricate a
definition.

- FR: "définition de 'sérendipité'", "synonyme de 'rapide'", "comment prononcer 'quinoa'"
- EN: "define serendipity", "what does 'ephemeral' mean?", "synonym of 'fast'", "how do you pronounce 'quinoa'?"
- Params: `word` (required, ≤100 chars, ASCII letters / hyphens / apostrophes / spaces).
- NOT for encyclopedic / biographical / historical questions
  (answer those from your own knowledge); the tool is scoped to
  English vocabulary lookups.

```
curl -s -X POST localhost:8080/v1/agents/dictionary/invoke \
  -H 'content-type: application/json' \
  -d '{"arguments":{"word":"serendipity"}}'
```

**Caching.** v1 ships without a cache. All three providers tolerate
~10 req/s from a single IP, which is comfortable for a personal
chat deployment; revisit if rate limits bite.

#### Supported models

The wire format is OpenAI-flavoured `tools` + `tool_calls`, so any
model that supports function calling works. Tested / known-good:

- Qwen 2.5 (7B / 14B / 32B) — **Qwen 2.5 14B is the recommended default**
- Llama 3.1+
- Mistral-Nemo
- Firefunction-v2

Smaller quantisations (e.g. Qwen 2.5 3B) sometimes ignore the
injected `tools` schema and answer from memory. If a model
consistently skips the tool call, add a one-line nudge to the system
prompt: *"You may use the provided `web_fetch`, `get_datetime`,
`get_weather`, `get_stock_quote`, `calculate`, `unit_convert`,
`wikipedia`, and `dictionary` tools when the user asks for live data."*
No code change required.

### Security

The proxy binds to localhost by default and Ollama is expected to be
on the same host. Out of the box, `LLM_AUTH_MODE=forward` keeps the
historical behaviour (no inbound inspection) so an existing
deployment behind a reverse proxy with authentication keeps working
unmodified. To make the proxy itself the auth gate, switch to
`LLM_AUTH_MODE=bearer` and set `LLM_API_KEY=<token>`; every `/v1/*`
request then needs to carry `Authorization: Bearer <token>` and a
missing / wrong key gets a `401` with a `WWW-Authenticate` hint. A
boot-time warning is logged when `LLM_AUTH_MODE=disabled` is paired
with a non-loopback bind, or when `LLM_AUTH_MODE=bearer` is enabled
without `LLM_API_KEY`. The `web_fetch` agent defaults to **blocking
all public internet access** — only loopback and RFC1918 ranges are
reachable out of the box — so even a malicious prompt cannot trick
the LLM into exfiltrating data to an attacker-controlled host unless
an operator has explicitly opted in via `WEB_FETCH_ALLOW_PUBLIC=true`
or `WEB_FETCH_ALLOWLIST`. The daily agents (`get_datetime`,
`get_weather`, `get_stock_quote`) reach their fixed public endpoints
(WeatherAPI.com, Stooq, and `chrono-tz`'s bundled IANA data); they
expose no SSRF surface.

### Smoke test

```
make smoke-llm
# equivalent to: curl -N POST /v1/chat/completions | grep '^data:'
```

## Documents (Discussion-mode uploads + `read_document` tool)

The Discussion-mode sidebar ships a **Documents** panel. Users
upload `.txt` or `.pdf` files via the `+` button, drag-and-drop
on the panel, or `Ctrl+V` from the OS clipboard. The LLM picks
which document to consult through the `read_document` tool —
the prompt includes the list of available documents by name +
size, and the model calls the tool when it needs a specific
file's text.

### Cargo feature

```sh
make run-llm        # documents module is compiled in
DOCS_ENABLED=false make run-llm   # runtime gate to disable routes
```

### Configuration

Add a `[documents]` block to the TOML overlay (or rely on the
env-var overrides listed below):

```toml
[documents]
enabled = true
cache_dir = "/var/cache/nagent/docs"   # must be writable; PVC in k8s
max_file_size_bytes = 20_971_520
max_extracted_chars = 100_000          # past the cap, a `[… truncated …]` marker is appended
max_docs_per_session = 50
pdf_extract_timeout_secs = 30
purge_interval_hours = 24              # 0 disables the periodic task
default_ttl_days = 30                  # rows + files older than this are purged
```

Equivalent env vars (env always wins over TOML):

| Setting                | Env var                 |
| ---------------------- | ----------------------- |
| `enabled`              | `DOCS_ENABLED`          |
| `cache_dir`            | `DOCS_CACHE_DIR`        |
| `max_file_size_bytes`  | `DOCS_MAX_FILE_BYTES`   |
| `max_extracted_chars`  | `DOCS_MAX_CHARS`        |
| `max_docs_per_session` | `DOCS_MAX_PER_SESSION`  |
| `pdf_extract_timeout_secs` | `DOCS_PDF_TIMEOUT_SECS` |
| `purge_interval_hours` | `DOCS_PURGE_INTERVAL_H` |
| `default_ttl_days`     | `DOCS_TTL_DAYS`         |

`documents.enabled = true` requires `auth.enabled = true` (the
table lives in the auth DB). The server refuses to boot when the
cache dir is not writable at boot, surfacing a clear error in
the startup log.

### Disk layout

Uploaded files land at
`<cache_dir>/<aa>/<bb>/<uuid>.<ext>` — two hex characters
from the UUID form a 2-level shard, capping any single
directory at ~65 Ki entries. The DB row records the absolute
`disk_path`; the `read_document` tool reads it back.

### `read_document` tool

```json
{
  "name": "read_document",
  "description": "Read the text content of a document previously uploaded by the user to this chat session. Use the `name` field returned by GET /v1/documents. For PDFs, optionally restrict to a `page_range` (e.g. \"3-7\") to limit context size.",
  "parameters": {
    "type": "object",
    "properties": {
      "name":       { "type": "string", "description": "Document id (UUID)" },
      "page_range": { "type": "string", "description": "Optional, format 'N' or 'N-M'" }
    },
    "required": ["name"]
  }
}
```

The tool looks up the document by `(id, user_id, session_id)` —
uploads from another user, or from a different chat session in
the same browser profile, are invisible to the caller. The
browser sends the active session id on every `/v1/chat/completions`
request as the `X-Chat-Session-Id` header.

### Server-bound chat session id

The `X-Chat-Session-Id` header value is **server-bound**, not
client-minted (SEV 2 fix):

```http
POST /v1/chat/session
Cookie: nagent_session=<id>
x-csrf-token: <csrf>

{"id": "01234567-89ab-cdef-0123-456789abcdef"}
```

The browser calls this endpoint on every page load to get a
fresh UUID + binding to the authenticated user. The returned id
is cached in `localStorage` and reused on every subsequent
request. A `403` from any documents / chat-completions endpoint
(typically because the binding was lost — e.g. logout from
another tab) triggers an automatic re-mint + retry.

Failure modes the tool surfaces (mapped to `role: "tool"` errors
so the LLM can recover):

- `name` not found in this session → `unknown document`
- file on disk was purged between upload and call → `document
  file no longer available on disk; ask the user to re-upload`
- `page_range` malformed → `invalid page range`

### CLI: `nagent documents purge`

Sweep expired rows + files from a shell — useful for cron /
k8s `CronJob`, recovery from a misconfigured TTL, or freeing
disk space without restarting the server.

```sh
# Default: same TTL the periodic task uses.
nagent documents purge --older-than 30d

# Inspect only — print what WOULD be removed.
nagent documents purge --older-than 30d --dry-run

# Override the config-file path.
nagent documents purge --older-than 7d --config /etc/nagent/config.toml

# Run even when data/server.pid points at a live process.
nagent documents purge --older-than 30d --force
```

`--older-than` accepts `Nd` / `Nh` / `Nm` / `Ns` (or bare
seconds). Exit code is non-zero on any unlink / DB error so
the CLI can be wired to cron / k8s `CronJob` without further
plumbing.

### Kubernetes

The kustomize base (`deploy/k8s/base/`) ships a
`nagent-docs-cache` PVC (5 Gi) mounted at `/var/cache/nagent/docs`
in the server pod. Set `DOCS_ENABLED=true` in the configmap to
enable it; the server refuses to boot otherwise (the cache dir
is not writable without the PVC bound).

### Hiding the panel when disabled

The frontend decides whether to render the panel by hitting
`GET /api/features` after `/api/me` succeeds. The endpoint is
authenticated (under `RequireAuth` when `auth.enabled = true`);

```http
GET /api/features
Cookie: nagent_session=<id>

{
  "documents": true,
  "llm": true,
  "tts": false,
  "agents": false,
  "agent_names": [],
  "chat_sessions": true,
  "tools": []
}
```

The same endpoint also drives the Discussion-mode tab
(`features.llm` — hidden when the LLM proxy is off), the TTS
settings drawer (`features.tts`, future), and any other gated
control. Operators flip a flag, rebuild the server, refresh the
browser; the UI hides the section that no longer has a backend.

## Text-to-Speech (Piper, local)

The Discussion view can read the LLM's replies aloud in the browser
default-output voice using a fully local [Piper](https://github.com/rhasspy/piper)
ONNX model. Synthesis runs on the server (CPU, no GPU required); the
browser just plays the resulting WAV through Web Audio.

### Prerequisites

Install the `espeak-ng` C library + headers — `piper-rs` calls into it
for phonemisation — plus `libsonic` (audio time-stretching) and
`libpcaudio` (audio output backend). On Debian/Ubuntu:

```
sudo apt install espeak-ng libsonic-dev libpcaudio-dev
```

On Arch Linux:

```
sudo pacman -S espeak-ng libsonic libpcaudio
```

On Fedora:

```
sudo dnf install espeak-ng libsonic libpcaudio
```

> **Why `libsonic` + `libpcaudio`?** espeak-ng with
> `COMPILE_INTONATIONS=ON` and `USE_LIBPCAUDIO=ON` (both defaults)
> calls into libsonic for pitch / time-stretch control and into
> libpcaudio for audio output. `piper-rs` 0.2's build script bundles
> espeak-ng + libsonic via CMake but has packaging bugs where it
> forgets to relay the `-lsonic` and `-lpcaudio` link directives
> to Cargo. `nagent-server`'s own build script (`build.rs`)
> re-injects both directives whenever the `tts` cargo feature is
> enabled, so the link step resolves against the system
> `libsonic.so` / `libpcaudio.so` you just installed. If you want
> fully static links (no runtime deps on either), set
> `SONIC_LINK_KIND=static` and `PCAUDIO_LINK_KIND=static` at build
> time.

### Download voices

The engine discovers voices from `TTS_MODEL_DIR` (default
`./models/piper`). Each voice is a `<id>.onnx` + `<id>.onnx.json` pair.
The two voices used by default are:

- `en_US-lessac-medium` — natural-sounding US English female
- `fr_FR-upmc-medium` — natural-sounding French (UPMC/LIMSI)

Fetch the official Piper voice set with:

```
./scripts/download-piper-voices.sh en_US-lessac-medium fr_FR-upmc-medium
# → writes ./models/piper/en_US-lessac-medium.onnx{,.json}
#        ./models/piper/fr_FR-upmc-medium.onnx{,.json}
```

Or browse the full catalogue at
[huggingface.co/rhasspy/piper-voices](https://huggingface.co/rhasspy/piper-voices/tree/main)
and drop the matching files into `TTS_MODEL_DIR` directly.

### Configuration

`[tts]` TOML section, or matching env vars:

| Key | Env var | Default | Purpose |
|---|---|---|---|
| `enabled` | `TTS_ENABLED` | `false` | Master switch. When off, `/v1/audio/*` routes are not registered and the discussion UI hides the "Read response aloud" checkbox. |
| `model_dir` | `TTS_MODEL_DIR` | `./models/piper` | Directory holding `<voice>.onnx` + `<voice>.onnx.json` files. |
| `voice_en` | `TTS_VOICE_EN` | `en_US-lessac-medium` | English voice used when the request's `lang` is non-French. |
| `voice_fr` | `TTS_VOICE_FR` | `fr_FR-upmc-medium` | French voice used when the request's `lang` starts with `fr`. |
| `default_lang` | `TTS_DEFAULT_LANG` | `en` | Language hint when the client doesn't send one. |
| `length_scale` | `TTS_LENGTH_SCALE` | `1.0` | Piper `length_scale`; `>1.0` = slower, `<1.0` = faster. Per-request `speed` overrides this. |
| `noise_scale` | `TTS_NOISE_SCALE` | `0.667` | Piper upstream default; controls audio variability. |
| `noise_w` | `TTS_NOISE_W` | `0.8` | Piper upstream default; controls phoneme variability. |
| `max_input_chars` | `TTS_MAX_INPUT_CHARS` | `2000` | Hard cap on a single `/v1/audio/speech` request body. |

Example TOML block (mirrors `docs/examples/config.toml.example`):

```toml
[tts]
enabled = true
model_dir = "./models/piper"
voice_en = "en_US-lessac-medium"
voice_fr = "fr_FR-upmc-medium"
default_lang = "en"
length_scale = 1.0
max_input_chars = 2000
```

### HTTP endpoints

| Method | Path | Purpose |
|---|---|---|
| `POST` | `/v1/audio/speech` | Body: `{"input": "...", "voice": "...", "lang": "...", "speed": 1.0}`. Returns `audio/wav` PCM 16-bit mono at the model's native sample rate. |
| `GET` | `/v1/audio/voices` | Lists voices discovered in `model_dir`, plus the per-language defaults. The UI calls this on boot to populate its voice selectors. |

Errors are surfaced as plain-text HTTP responses:

- `400` — empty input or input over `max_input_chars`.
- `404` — unknown voice id.
- `429` — per-IP LLM/TTS rate limit (`LLM_RATE_PER_MIN`).
- `503` — TTS disabled on the server, or no voices installed.

### Voice attribution & license

All Piper voices ship under **CC-BY-NC-SA** (some under MIT — the
catalogue metadata on each Hugging Face page is authoritative). The
included defaults are:

- `en_US-lessac-medium` — © Lessac (https://github.com/rhasspy/piper/blob/master/VOICES.md), CC-BY-NC-SA.
- `fr_FR-upmc-medium` — © UPMC / LIMSI, CC-BY-NC-SA.

When redistributing the binary or the voice files together, you MUST
preserve this attribution and the non-commercial clause. If you ship a
fork with a different default voice, update this section accordingly.
**Do not use Piper voices (or this binary configured to use them) in a
commercial product without picking a voice with a permissive
license.** The Chromium `Orca` offline voices bundled with modern Linux
desktops are MIT-licensed and a drop-in alternative if you need a
permissive-license TTS path; the engine abstraction in
`crates/nagent-server/src/tts.rs` (`Synthesizer` trait) is the seam where
such a backend would slot in.

### Build & runtime impact

`piper-rs` is gated behind the `nagent-server/tts` cargo feature. Every
`make run*` target enables it (`run`, `run-vulkan`, `run-hipblas`,
`run-cuda`, `run-mock`, `run-llm`, `run-tts`) so operators never have
to think about the feature flag — TTS is part of the standard
delivery. Without `--features nagent-server/tts` the binary compiles
without `piper-rs`, `ort`, or the espeak-ng FFI crate, so a
downstream consumer who wants to slim their binary can opt out
manually with `cargo build -p nagent-server`.

The runtime surface follows `TTS_ENABLED`: when `false`, the
`/v1/audio/*` routes are not registered and the discussion UI hides
the "Read response aloud" checkbox. The Cargo feature only controls
the *binary's* ability to serve TTS; the runtime knob controls
*whether it does*.

### Browser-side playback model

When the Discussion-mode master TTS toggle is on, the discussion UI
does two things:

- **Autoplay (streaming)** — each SSE delta is fed to a sentence
  buffer in `tts.js`. The buffer splits on `[.!?]+\s+` and
  `\n\n+`, and each completed sentence triggers one
  `POST /v1/audio/speech` round-trip. Chunks play gapless via
  Web Audio's `AudioBufferSourceNode.start(t)` with absolute
  timestamps. The user hears the first sentence while the LLM
  is still generating the rest.
- **Per-message replay** — every assistant bubble gets a small
  speaker-icon button (top-right). Clicking it fetches the *full*
  sanitised bubble text in one HTTP round-trip and plays the
  whole message as a single utterance. Useful for re-listening
  to a finished reply, or to share a specific response with
  someone at the desk.

The autoplay and replay paths share the same underlying
`TtsPlayer` (single Web Audio `AudioContext`, single voice
configuration) but use different API surfaces: `feed(delta)` /
`flush()` for streaming, `speak(fullText)` for replay.

### Markdown sanitization

Before text reaches the Piper synth pipeline, `chat.js` runs it
through `sanitizeForTts()` which strips the most common markdown
markers (`**bold**`, `` `code` ``, `[link](url)`, headers,
bullets, etc.). Without this step espeak-ng falls back to
phonemicising the raw punctuation, which makes Piper read
"astérisque" out loud for `*` and ruins the prose rhythm. The
visible bubble still renders the original markdown via `marked.parse`
+ `DOMPurify`; only the audio path gets the stripped variant.

The sanitizer is regex-based (not a full markdown parser) and
applied at SSE-delta granularity. A multi-delta construct like
`**bo` followed by `ld**` may briefly match an extra `*`
mid-stream; in practice LLM tokens are short enough that this
rarely matters, and a stray single `*` is far less audible than
the doubled form would be.
the *binary's* ability to serve TTS; the runtime knob controls
*whether it does*.

`piper-rs` depends on `ort` (ONNX Runtime) and `espeak-ng`. Building
from source therefore requires `libclang` + `espeak-ng` development
headers + `libssl-dev` (apt: `apt install libclang-dev libespeak-ng-dev
libssl-dev`). CI / tests do **not** require Piper voices — the HTTP
integration tests inject a mock synthesizer that produces a sine
wave, so the test harness never touches `espeak-ng` or loads an
`.onnx` file.

At runtime, `espeak-ng` needs its phoneme + voice tables under an
`espeak-ng-data/` directory. `main.rs` auto-detects this directory
at boot by trying, in order:

1. The bundled build output (`target/release/build/espeak-rs-sys-*/
   out/share/espeak-ng-data/`) — picked up when you run via
   `cargo run`.
2. A system-installed `espeak-ng` (`/usr/share/espeak-ng-data/` on
   Arch / Debian / Fedora) — picked up when you run from
   `cargo install` and the package is present.
3. `${PIPER_ESPEAKNG_DATA_DIRECTORY}/espeak-ng-data/` and the
   current working directory + executable directory — handled by
   `espeak-rs` itself.

Operators who install the binary into a non-standard layout can
override the auto-detect by setting `PIPER_ESPEAKNG_DATA_DIRECTORY`
explicitly in their environment.

## License

Dual-licensed under MIT or Apache-2.0, at your option. Piper voices are
not part of this license; see the "Voice attribution & license" section
above for their separate CC-BY-NC-SA terms.