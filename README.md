# nagent

Local web app written in Rust that streams microphone audio from the browser,
runs it through Silero VAD on the client, and transcribes it server-side using
`whisper.cpp` (GPU-accelerated) over a binary WebSocket protocol.

Multiple WebSocket clients are isolated from each other: a session ID is
generated server-side at upgrade time and never accepted from the client, so
no audio or transcript can ever leak across sessions.

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
or an environment variable — no source changes are required. A `.env` file in
the working directory is loaded automatically by `dotenvy` at startup.

### Cargo features

`stt-server` exposes one feature; `stt-core` exposes four. They are combined
through the `make` targets and the Dockerfile `BACKEND` arg.

| Feature                         | Effect                                                                                   |
| ------------------------------- | ---------------------------------------------------------------------------------------- |
| `stt-server/real-backend`       | Use the `whisper-rs` backend. Without it the server falls back to the in-process mock.   |
| `stt-core/whisper-rs-backend`   | Pulls in `whisper-rs` (CPU). Always required, even when a GPU backend is also selected.  |
| `stt-core/whisper-rs-vulkan`    | Enable the Vulkan GPU backend (needs `libvulkan-dev` at build time).                     |
| `stt-core/whisper-rs-cuda`      | Enable the CUDA GPU backend (needs CUDA toolkit at build time).                          |
| `stt-core/whisper-rs-hipblas`   | Enable the ROCm/HIP GPU backend (needs ROCm toolchain at build time).                   |

The three GPU features are mutually exclusive — enabling more than one
wastes build time and can fight over system libraries.

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
| `RUST_LOG`                   | `info,stt_server=debug,stt_core=debug` (local); `info,stt_server=info,stt_core=info` (Docker) | Logging | Standard `tracing-subscriber` `EnvFilter` directive. |

Per-source-IP rate limiting applies at both layers: HTTP for the LLM proxy
(`/v1/chat/completions`, `/v1/models`) and at the WebSocket upgrade +
per-frame level for the STT pipeline. Loopback IPs always bypass.

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
   #     --features stt-server/real-backend,stt-core/whisper-rs-backend
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
| `LLM_RATE_PER_MIN`           | `30`                     | Inbound LLM HTTP requests per source IP per minute                |

### Security

The proxy relies on `stt-server` binding to localhost and Ollama being
on the same host; there is no auth on the chat endpoint. Do not expose
the server to the network without adding a reverse proxy with
authentication in front of it.

### Smoke test

```
make smoke-llm
# equivalent to: curl -N POST /v1/chat/completions | grep '^data:'
```

## License

Dual-licensed under MIT or Apache-2.0, at your option.