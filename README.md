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

All settings are environment variables (no code changes required):

| Variable                  | Default                  | Meaning                                                           |
| ------------------------- | ------------------------ | ----------------------------------------------------------------- |
| `LLM_ENABLED`             | `false`                  | Master switch; when `false` the `/v1/*` routes are not registered |
| `OLLAMA_BASE_URL`         | `http://localhost:11434` | Base URL of the OpenAI-compatible upstream (Ollama by default)     |
| `OLLAMA_MODEL`            | `llama3.1`               | Default model for `/v1/chat/completions` when the client omits it |
| `OLLAMA_API_KEY`          | _(unset)_                | Optional bearer token forwarded as `Authorization: Bearer …`      |
| `LLM_REQUEST_TIMEOUT_SECS`| `120`                    | Per-chunk idle timeout on the upstream stream                     |

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