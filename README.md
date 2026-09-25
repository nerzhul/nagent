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

## License

Dual-licensed under MIT or Apache-2.0, at your option.