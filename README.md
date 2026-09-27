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
- `crates/stt-server` — `axum` server, WebSocket handler, session map,
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

`stt-server` exposes several features; `stt-core` exposes four. They are combined
through the `make` targets and the Dockerfile `BACKEND` arg.

| Feature                         | Effect                                                                                   |
| ------------------------------- | ---------------------------------------------------------------------------------------- |
| `stt-server/real-backend`       | Use the `whisper-rs` backend. Without it the server falls back to the in-process mock.   |
| `stt-server/web-agent`          | Register the server-side `web_fetch` chat agent.                                         |
| `stt-server/datetime-agent`     | Register the `get_datetime` chat agent (pulls `chrono` + `chrono-tz`).                   |
| `stt-server/weather-agent`      | Register the `get_weather` chat agent (WeatherAPI.com, free API key required).            |
| `stt-server/stock-agent`        | Register the `get_stock_quote` chat agent (Stooq CSV, no API key).                       |
| `stt-server/calculate-agent`    | Register the `calculate` chat agent (local `meval`-backed expression evaluator).         |
| `stt-server/unit-convert-agent` | Register the `unit_convert` chat agent (pure-local conversion tables).                  |
| `stt-server/wikipedia-agent`    | Register the `wikipedia` chat agent (REST `wikipedia.org`, no API key, `User-Agent` set).|
| `stt-server/dictionary-agent`   | Register the `dictionary` chat agent (Free Dictionary REST API, no API key).             |
| `stt-core/whisper-rs-backend`   | Pulls in `whisper-rs` (CPU). Always required, even when a GPU backend is also selected.  |
| `stt-core/whisper-rs-vulkan`    | Enable the Vulkan GPU backend (needs `libvulkan-dev` at build time).                     |
| `stt-core/whisper-rs-cuda`      | Enable the CUDA GPU backend (needs CUDA toolkit at build time).                          |
| `stt-core/whisper-rs-hipblas`   | Enable the ROCm/HIP GPU backend (needs ROCm toolchain at build time).                   |

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
`crates/stt-server/src/config.rs`. `WHISPER_MODEL_PATH` is the only one
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
| `AGENTS_ENABLED`             | `true`                                        | Chat agents    | Master switch for server-side chat agents (`web_fetch`, `get_datetime`, `get_weather`, `get_stock_quote`, `calculate`, `unit_convert`, `wikipedia`, `dictionary`). When `false` the registry is empty. |
| `LLM_MAX_TOOL_ROUNDS`        | `4`                                           | Chat agents    | Maximum tool-call rounds per user turn before the proxy aborts.                                               |
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
| `RUST_LOG`                   | `info,stt_server=debug,stt_core=debug` (local); `info,stt_server=info,stt_core=info` (Docker) | Logging | Standard `tracing-subscriber` `EnvFilter` directive. |

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
[`examples/config.toml.example`](examples/config.toml.example). Copy it,
edit the values you want, and point the binary at it:

```
stt-server --config /etc/nagent/config.toml
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
| `[server.limits].*`               | `MAX_*`, `REQUIRED_*`       | WebSocket frame knobs.                         |
| `[server.rate_limits].*`          | `STT_RATE_PER_MIN`, `LLM_*` | Per-IP buckets.                                |
| `[llm].*`                         | `LLM_*`, `OLLAMA_*`         | OpenAI-compatible proxy.                       |

The `[agents]` section is a general block for chat-agent settings: it
carries the master switches (`enabled`, `llm_max_tool_rounds`) plus
one sub-table per tool, keyed by the agent's name:

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

[agents]
enabled = true
llm_max_tool_rounds = 4

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
`crates/stt-server/tests/multiuser_isolation.rs` and uses a mock backend so
it runs without any GPU.

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

2. Start `stt-server` with the proxy enabled:

   ```
   make run-llm
   # equivalent to: LLM_ENABLED=true cargo run -p stt-server --release \
   #     --features stt-server/real-backend,stt-server/web-agent,stt-core/whisper-rs-backend
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
| `OLLAMA_API_KEY`             | _(unset)_                | Optional bearer token forwarded as `Authorization: Bearer …`      |
| `LLM_REQUEST_TIMEOUT_SECS`   | `120`                    | Per-chunk idle timeout on the upstream stream                     |
| `LLM_CORS_ALLOW_ORIGINS`     | _(empty)_                | Comma-separated list of origins allowed to call `/v1/*` cross-origin (empty = same-origin only) |
| `LLM_SYSTEM_PROMPT`          | _(unset)_                | Optional default system prompt prepended to every `/v1/chat/completions` request as `messages[0]` (the browser's "Additional instructions" textarea is appended after it). Empty / whitespace-only values are treated as unset. |
| `LLM_ALLOW_USER_LOCATION`    | `true`                   | When `false`, the server strips the browser-injected `User's approximate location:` system message before forwarding to the upstream model. The browser still requires explicit user consent; this is a defence-in-depth kill-switch for sensitive deployments. |
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

The proxy relies on `stt-server` binding to localhost and Ollama being
on the same host; there is no auth on the chat endpoint. Do not expose
the server to the network without adding a reverse proxy with
authentication in front of it. The `web_fetch` agent defaults to
**blocking all public internet access** — only loopback and RFC1918
ranges are reachable out of the box — so even a malicious prompt cannot
trick the LLM into exfiltrating data to an attacker-controlled host
unless an operator has explicitly opted in via
`WEB_FETCH_ALLOW_PUBLIC=true` or `WEB_FETCH_ALLOWLIST`. The daily
agents (`get_datetime`, `get_weather`, `get_stock_quote`) reach their
fixed public endpoints (WeatherAPI.com, Stooq, and `chrono-tz`'s
bundled IANA data); they expose no SSRF surface.

### Smoke test

```
make smoke-llm
# equivalent to: curl -N POST /v1/chat/completions | grep '^data:'
```

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
> to Cargo. `stt-server`'s own build script (`build.rs`)
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

Example TOML block (mirrors `examples/config.toml.example`):

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
`crates/stt-server/src/tts.rs` (`Synthesizer` trait) is the seam where
such a backend would slot in.

### Build & runtime impact

`piper-rs` is gated behind the `stt-server/tts` cargo feature. Every
`make run*` target enables it (`run`, `run-vulkan`, `run-hipblas`,
`run-cuda`, `run-mock`, `run-llm`, `run-tts`) so operators never have
to think about the feature flag — TTS is part of the standard
delivery. Without `--features stt-server/tts` the binary compiles
without `piper-rs`, `ort`, or the espeak-ng FFI crate, so a
downstream consumer who wants to slim their binary can opt out
manually with `cargo build -p stt-server`.

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