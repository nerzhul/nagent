# nagent — Improvements plan (features, performance, security)

Scope confirmed with the user: product features, performance, and security. The
plan deliberately leaves observability / DevOps work out of scope unless it is a
hard prerequisite of one of the items below.

Priorities: **P0 = foundational / quick win**, **P1 = high value**, **P2 = nice
to have**. Items inside the same priority are listed in implementation order.

---

## 1. Context recap (so the plan is readable standalone)

- `stt-server` (axum + tokio) serves an embedded frontend and a binary
  WebSocket protocol. Each browser tab opens one session; the server assigns a
  `Uuid` and never trusts a client-provided id.
- Inference is currently **serialized**: a single
  `InferenceWorker` consumes `InferenceJob`s from an `mpsc` queue, and inside
  `WhisperRsBackend` a single `WhisperState` is guarded by a `Mutex`. Every
  audio chunk from every session blocks every other session.
- Frontend is vanilla JS modules in `crates/stt-server/src/static/`, with two
  views (`#view-transcript` and `#view-discussion`) toggled by `mode.js`.
- `llm.rs` exposes an OpenAI-compatible proxy (`/v1/chat/completions`,
  `/v1/models`) with bearer auth (`LLM_AUTH_MODE=bearer|forward|disabled`,
  default `forward`). Cross-origin requests are blocked by default and
  gated by `LLM_CORS_ALLOW_ORIGINS`. README still tells users to put a
  reverse proxy with auth in front before exposing it.
- Chat history is in `localStorage` only; transcript history is in-DOM only.

---

## 2. Performance

### P0 — Batched inference for short utterances
- **Why**: a 1–2 s chunk underutilizes Whisper on GPU; batching two or three
  compatible jobs (same language, same translate flag, comparable length)
  roughly doubles throughput on Vulkan/CUDA.
- **What**: introduce a `BatchingPolicy` that holds incoming jobs up to
  `BATCH_MAX_MS` (default 250 ms) or `BATCH_MAX_SIZE` (default 4) before
  handing the batch to a worker. Workers must support `infer_batch(Vec<…>)`
  on the trait; fall back to serial for the `MockBackend`.
- **Config**: `BATCH_MAX_MS`, `BATCH_MAX_SIZE`, `BATCH_ENABLED=true`.
- **Files**: `stt-core/src/batching.rs` (new), trait bump in
  `stt-core/src/backend.rs`, `stt-core/src/whisper_backend.rs`.

### P1 — Streaming partial transcripts
- **Why**: the current server only emits `FinalTranscript`. Whisper can emit
  segment-by-segment progress; users perceive latency as the time to the
  *first* visible text, not the total decode.
- **What**: re-emit `PartialTranscript` from inside `infer` as soon as each
  `whisper_rs::Segment` is finalized (re-use `Tag::PartialTranscript = 0x20`,
  already declared but unused on the server side). Keep the existing
  `FinalTranscript` at the end so existing clients keep working.
- **Wire-compat note**: clients that don't know `PartialTranscript` will
  silently ignore unknown tags — no break.
- **Files**: `stt-core/src/whisper_backend.rs`, `stt-server/src/ws_handler.rs`.

### P1 — WS-level backpressure (bounded outbound + ping watchdog)
- **Why**: `register()` uses `mpsc::unbounded_channel` for outbound. A stalled
  consumer (e.g. browser tab throttled in background) can pin memory in the
  server. The frontend has no ping/pong keepalive.
- **What**: switch outbound to `mpsc::channel(64)`, drop oldest segment on
  full, and add a 30 s WS ping from the server side. Reuse the existing
  `OutboundMessage::Close` for graceful teardown when the bounded channel
  closes.
- **Files**: `stt-server/src/session.rs`, `stt-server/src/ws_handler.rs`.

---

## 3. Security

### P2 — Per-session transcript size cap + LLM prompt-injection logging
- **Why**: long-running sessions accumulate memory; nothing currently bounds
  them. And a malicious upstream Ollama could return HTML that the chat UI
  will sanitize, but the raw stream still gets logged in plaintext.
- **What**: enforce `MAX_SESSION_TRANSCRIPT_BYTES` (default 1 MiB) and rotate
  the buffer. Add a `tracing::warn!` with the redacted prompt when the
  proxy sees `data: [DONE]` without a `finish_reason` (suggests truncation /
  injection).
- **Files**: `stt-server/src/ws_handler.rs`, `stt-server/src/llm.rs`.

---

## 4. Product features

### P0 — Persistent transcript history (server-side, per session)
- **Why**: a refresh wipes the transcript. Users repeatedly ask for "save my
  transcript" in single-user local apps.
- **What**: keep a SQLite (`rusqlite`, bundled) store keyed by `session_id`,
  opened lazily on first WS connect. Each `FinalTranscript` is appended
  inside the same transaction as the audio duration. New endpoints:
  - `GET /api/sessions/:id/transcript` → `{ segments: [...], lang }`
  - `GET /api/sessions` → list of past sessions with timestamps
  - `DELETE /api/sessions/:id`
- **Config**: `TRANSCRIPT_DB_PATH` (default `~/.local/share/nagent/transcripts.db`).
  Feature-gated behind `stt-server/persistence` so the default build stays
  zero-dep.
- **Files**: `stt-server/src/persistence.rs` (new), `stt-server/src/lib.rs`,
  `stt-server/Cargo.toml`.

### P1 — Diarization (speaker labels)
- **Why**: meeting / interview use cases are the obvious next step; Whisper
  itself doesn't diarize, but we can integrate `pyannote-rs` or a lightweight
  VAD-segment clustering pass.
- **What**: add a `Diarize` config flag; if enabled, run a per-chunk clustering
  on top of Whisper's segments. Emit `Segment { speaker: u8, … }` and let the
  UI prefix the speaker. Out of scope: real-time diarization — keep it
  post-hoc on `FinalTranscript` for now.
- **Files**: `stt-core/src/diarize.rs` (new), `stt-proto/src/lib.rs`
  (extend `Segment`), `stt-server/src/static/app.js`.

### P1 — PWA install + offline shell
- **Why**: local app, mobile / desktop install, instant reload.
- **What**: add a `manifest.webmanifest` + service worker that caches the
  embedded `/static/*` bundle (use the `version.txt` content hash as the
  cache key so rebuilds invalidate automatically — the polling endpoint
  already exists).
- **Files**: `stt-server/src/static/manifest.webmanifest` (new),
  `stt-server/src/static/sw.js` (new), `stt-server/src/static/index.html`.

### P2 — Model auto-download on first boot
- **Why**: `WHISPER_MODEL_PATH` is the only required env var; a new user has
  to know where to fetch the ggml file. The k8s init Job already shows the
  intent.
- **What**: on startup, if `WHISPER_MODEL_PATH` is missing *and*
  `WHISPER_MODEL_URL` is set, download to a cache dir
  (`~/.cache/nagent/models/`) with a streaming `reqwest` GET + sha256
  verification (`WHISPER_MODEL_SHA256`). Surface progress on `/healthz` as a
  JSON body (`{ "phase": "downloading", "pct": 42 }`).
- **Files**: `stt-server/src/model_download.rs` (new),
  `stt-server/src/config.rs`, `stt-server/src/ws_handler.rs`.

### P2 — Shared chat history across devices
- **Why**: localStorage is per-browser. The LLM proxy already exists; pairing
  it with the new persistence store is cheap.
- **What**: store chat sessions server-side (same SQLite), expose
  `/api/chat/sessions` to the Discussion UI. Requires the multi-user auth
  subsystem (now in place from `multi-user-and-plugins.md` PR1) before it
  makes sense to expose beyond localhost.
- **Files**: reuses `persistence.rs`, `stt-server/src/llm.rs`,
  `stt-server/src/static/chat.js`, `stt-server/src/static/chat-sessions.js`.

---

## 5. Open questions to confirm before implementation

These are the only decisions a future implementation agent should not take
alone; everything else in this plan has a single obvious interpretation.

1. **Backend feature gating**: keep adding new features behind Cargo features
   (`persistence`, `auth`, `diarize`) so the default release stays minimal,
   or fold them all into the default build? **Recommendation: gate them.**
2. **Chat history migration**: when we move chat to server-side persistence,
   do we copy the existing localStorage history server-side on first connect
   (one-shot upload), or treat it as ephemeral and let the user re-import?
   **Recommendation: one-shot upload, then delete the localStorage copy.**
3. **Batching compatibility**: turn batching on by default, or keep it
   opt-in for now to avoid behavior changes for existing deployments?
   **Recommendation: opt-in via `BATCH_ENABLED=true` for v1.**

---

## 6. Validation plan

For every P0 item, before merging:
- `make fmt && make clippy && make test` clean.
- A new unit test in the touched crate exercising the change with the
  `MockBackend` (no GPU needed in CI).
- A guard test in `crates/stt-server/tests/static_assets.rs` if a frontend
  file was added or renamed, mirroring the existing `css_hides_inactive_view_*`
  pattern.

For P1+ items, same rule plus a `make run-mock` smoke that hits the relevant
endpoint with `curl` (already documented for `make smoke-llm`).

---

## 7. Out of scope (deliberately)

- Observability / Prometheus / OpenTelemetry (separate workstream).
- Helm chart, docker-compose, CI matrix expansion (DevOps workstream).
- A mobile-native app shell (PWA in P1 covers the install + offline story).
- Replacing the vendored `vad-web` / `onnxruntime-web` / `marked` /
  `DOMPurify` / `katex` bundles (already documented as deliberate in
  `index.html`).
