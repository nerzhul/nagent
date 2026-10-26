# Plan — Extract STT and TTS into an External Voice Service

## Goal

Make STT and TTS optional inside `nagent-server`. A new `voice-embedded` cargo feature
(default ON) keeps today's in-process behaviour. When OFF, the main server compiles
without any voice code, and a new standalone binary `nagent-voice-service` runs the
STT and TTS endpoints as a separate process. The browser talks to the voice service
directly using a short-lived HMAC-SHA256 JWT minted by the main server; the token
is delivered once per page load via a new `GET /api/voice/token` endpoint.

Backward compatibility: default behaviour is byte-for-byte identical to today.
Existing deployments that keep `voice-embedded` (default) need no change.

---

## Architecture

```
+--------------------+         +--------------------------+
| Browser (page at   |  /ws    | nagent-voice-service     |
|  nagent-server)    +-------->+  - verifies HMAC JWT     |
|                    |  ?token |  - same /ws and /v1/audio|
|  audio.js /tts.js  |         |    routes as today       |
|  window.__voice-   |  /v1/   |  - no DB, stateless      |
|   Config{wsUrl,    +-------->+  - listens on :8081      |
|   httpUrl, token}  |  /auth  |                          |
+--------+-----------+         +--------------------------+
         |                              ^
         | cookie auth                  | shared HMAC secret
         v                              | (config, never on the wire
+--------+-----------+                    from browser to voice service)
| nagent-server      +--------------------+
|  - /api/voice/token| mints JWT using    |
|  - mints JWT       | voice.external.   |
|  - no voice code   | shared_secret     |
|  when feature OFF  |                   |
+--------------------+                   |
                                         |
       operator copies the SAME shared_secret into
       nagent-server.[voice.external].shared_secret
       AND voice-service.toml [auth].shared_secret
```

- Libraries (`stt-proto`, `stt-core`, `nagent-tts`, `nagent-support`) stay where they
  are. Both binaries depend on them. The voice service reuses the wire types and
  engine crates as-is; only the axum HTTP/WS wiring is duplicated.
- The main server never proxies `/ws` or `/v1/audio/*` in external mode. The browser
  reaches the voice service directly. This avoids streaming-binary proxy complexity
  and keeps the wire protocol identical between modes.

---

## Crate and feature changes

### Workspace `Cargo.toml`
- Add `crates/nagent-voice-service` as a new workspace member.

### `crates/nagent-server/Cargo.toml`
- Add `voice-embedded = ["dep:nagent-tts", "dep:stt-core", "dep:stt-proto"]`.
- `default = ["voice-embedded"]` so existing builds don't change.
- Make `real-backend` and `tts` features imply `voice-embedded` (so enabling them
  without `voice-embedded` becomes a compile error, not a silent misconfig).
- `#[cfg(feature = "voice-embedded")]` gates the in-process wiring in
  `app.rs` (lines 185-237 STT, 352-362 TTS), `stt/` and `tts/` modules,
  the `/ws` and `/v1/audio/*` route mounts in `http/mod.rs`, and the
  vendored JS asset copy if applicable.

### New `crates/nagent-voice-service/Cargo.toml`
- Features: `real-backend = ["stt-core/whisper-rs-backend"]`,
  `tts = ["nagent-tts/tts"]`, `whisper-rs-vulkan/cuda/hipblas` mirroring
  the existing `stt-core` features.
- Deps: `axum`, `tokio`, `tower`, `tower-http`, `serde`, `serde_json`,
  `jsonwebtoken`, `anyhow`, `tracing`, plus the workspace libs
  `stt-proto`, `stt-core`, `nagent-tts`, `nagent-support`.
- Single binary target `nagent-voice-service`.

---

## Config

### Main server — `docs/examples/config.toml.example`
Add a new section (ignored when `voice-embedded = true`):
```toml
# Only consulted when the server is built with --no-default-features
# (i.e. without the `voice-embedded` feature). Has no effect otherwise.
[voice.external]
url = "http://127.0.0.1:8081"      # base URL of the voice service
ws_url = "ws://127.0.0.1:8081/ws"  # default: derived from url
http_url = "http://127.0.0.1:8081" # default: derived from url
shared_secret = "<32+ random bytes, hex or base64>"  # REQUIRED in external mode
token_audience = "nagent-voice"
token_ttl_seconds = 86400          # 24h default
issuer = "nagent-server"
```

The existing `[stt]` and `[tts]` sections: still parsed, but when
`voice-embedded = false` the main server logs a single warning at startup
that they are ignored. The voice service reads its own TOML file
(`voice.toml`) for these values.

### Voice service — `docs/examples/voice-service.toml.example` (new)
```toml
bind_addr = "0.0.0.0:8081"

[auth]
shared_secret = "<same value as main server voice.external.shared_secret>"
token_audience = "nagent-voice"
expected_issuer = "nagent-server"
leeway_seconds = 30             # clock skew tolerance for exp/nbf/iat
allowed_origins = ["http://localhost:8080"]  # CORS allowlist for browser

[stt]
enabled = true
whisper_model_path = "/var/lib/nagent/models/ggml-base.en.bin"

[tts]
enabled = false
model_dir = "/var/lib/nagent/models/piper"
voice_en = "en_US-lessac-medium"
voice_fr = "fr_FR-upmc-medium"
default_lang = "en"
length_scale = 1.0
noise_scale = 0.667
noise_w = 0.8
max_input_chars = 2000
synth_concurrency = 1
synth_max_queue = 0
synth_queue_timeout_ms = 0

[server.limits]
max_audio_frame_samples = 480000
required_sample_rate = 16000
max_language_hint_bytes = 16
ws_max_concurrent = 1024
ws_max_per_ip = 16

[server.rate_limits]
stt_per_min = 120
```

The voice service reuses the existing config types in
`crates/nagent-server/src/config/{stt,tts,limits,ratelimit}.rs` by either
re-exporting them or by sharing the structs through a new `nagent-voice-config`
sub-module. Implementation detail — pick whichever avoids the smallest diff.

---

## Auth flow

1. User loads `https://main.example/` → existing cookie/session auth on the main
   server (unchanged).
2. `app.js` calls `GET /api/voice/token` (existing cookie sent automatically).
3. Main server handler:
   - Reads session id from the cookie (`sub` claim).
   - Mints a JWT `{ iss, aud, sub=session_id, iat, exp }` using HS256 +
     `voice.external.shared_secret` and `token_ttl_seconds`.
   - Returns `{ ws_url, http_url, token, expires_at, voice_embedded }`.
4. `app.js` writes `window.__voiceConfig = { wsUrl, httpUrl, token, expiresAt }`
   BEFORE `audio.js` and `tts.js` initialise (existing build-time injection or
   runtime fetch — runtime is simpler and avoids `build.rs` changes).
5. `audio.js` (line 842) and `tts.js` (lines 5, 267, 459) check
   `window.__voiceConfig`:
   - Empty (embedded mode) → use `ws://${location.host}/ws` and `${location.origin}/v1/audio/*` (today's behaviour).
   - Set (external mode) → use `${wsUrl}?token=${token}` for WS and
     `${httpUrl}/v1/audio/speech` with `Authorization: Bearer ${token}` for TTS.
6. Voice service's `/ws` upgrade reads `?token=`, verifies HMAC + `aud` + `iss` +
   `exp` (with leeway), admits the connection or closes with 4401.
7. Voice service's `/v1/audio/*` handlers read `Authorization: Bearer`,
   verify, admit or return 401.

CORS on the voice service: an `axum` middleware applies the `allowed_origins`
list and answers `OPTIONS` preflight. Browsers are expected to send the standard
`Origin` header on the TTS POST.

---

## New and changed code

### Main server
- `Cargo.toml`: add `voice-embedded` feature as above.
- `app.rs`: gate the STT and TTS state init blocks behind `#[cfg(feature = "voice-embedded")]`.
- `http/mod.rs`: gate the `/ws` and `/v1/audio/*` route mounts behind the same cfg.
- `http/voice_token.rs` (new): `async fn voice_token_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse`. Reads session, mints JWT, returns JSON. Mounted under the same auth subtree as `/api/features`.
- `http/features.rs`: add `voice_embedded: bool` to the `Features` struct (lines 74-142). Driven by the cargo feature at compile time (`#[cfg(feature = "voice-embedded")]` resolves to `true`).
- `static/app.js`: call `GET /api/voice/token` after auth, write `window.__voiceConfig`. If response has `voice_embedded: true`, the JS object is empty. Refactor existing `feature("stt")` / `feature("tts")` checks in `chat.js:4925-4979` to consider the new field.
- `static/audio.js`: at line 842, use `voiceConfig.wsUrl + ?token=...` when set.
- `static/tts.js`: at lines 5, 267, 459, 5511, use `voiceConfig.httpUrl + /v1/audio/...` when set, with `Authorization: Bearer` header.
- `static/chat.js`: at line 5530 (`probeTtsVoices`), use the same path.
- `config/mod.rs` and a new `config/voice_external.rs`: parse `[voice.external]`. Validate that when the binary is built without `voice-embedded` and the section is missing, startup fails with a clear error. When `voice-embedded` is ON, the section is allowed to be absent (warning).

### New `crates/nagent-voice-service/`
- `src/main.rs`: load `voice.toml`, init tracing, build axum router, serve.
- `src/config.rs`: serde-deserialised config matching the example above; reuse the existing `SttConfig` / `TtsConfig` / `LimitsConfig` / `RateLimitConfig` types via a thin re-export shim.
- `src/auth.rs`: `Claims { iss, aud, sub, exp, iat }`; `verify_jwt(token, &secret, &cfg) -> Result<Claims, AuthError>`; tower middleware that injects `Claims` into request extensions for handlers that need it, and a separate `verify_ws_token(token, &cfg)` used by the WS upgrade.
- `src/router.rs`: axum router mounting `/ws`, `/v1/audio/speech`, `/v1/audio/voices`, `/healthz`, and a CORS preflight layer.
- `src/ws.rs`: port of `crates/nagent-server/src/stt/ws_handler.rs` and friends, with auth pulled out of query string.
- `src/tts_routes.rs`: port of `crates/nagent-server/src/tts/routes.rs`, with auth pulled from the bearer header.
- `src/setup_espeak_data_dir`: copy of the same helper from `app.rs:612-660` so the piper feature still finds `espeak-ng-data/`.

To minimise code drift, the voice service's WS and TTS modules can be implemented
as `pub fn mount(router, state)` taking the same `SttState` / `TtsState` types
constructed locally. The two binaries then both call into identical code paths;
`cfg(feature = "voice-embedded")` on the main server simply doesn't compile the
in-process call.

---

## Endpoints — new contract

### Main server
- `GET /api/voice/token` (new, auth = existing session cookie)
  - 200: `{ voice_embedded: bool, ws_url?: string, http_url?: string, token?: string, expires_at?: u64 }`
  - When `voice_embedded = true`, all optional fields are `null`.
  - When `voice_embedded = false`, all fields are populated. If the section is misconfigured, the handler returns 503 with a clear error and the page falls back to disabled state.
- `GET /api/features` (changed)
  - New field: `voice_embedded: bool` at the top of the response.

### Voice service
- `GET /healthz` — returns 200 if the process is up.
- `GET /ws` (auth via `?token=`) — same wire as today.
- `POST /v1/audio/speech` (auth via `Authorization: Bearer`) — same wire as today.
- `GET /v1/audio/voices` (auth via `Authorization: Bearer`) — same wire as today.
- `OPTIONS *` — CORS preflight.

---

## Makefile

Existing `make run*` targets stay unchanged (voice-embedded default ON).
Add:
- `make run-ext` — `cargo run -p nagent-server --release --no-default-features --features all-agents` (no voice-embedded, no real-backend, no tts). Validates the "main server without voice" build path.
- `make voice-run` — `cargo run -p nagent-voice-service --release --features real-backend,tts` (CPU).
- `make voice-run-cuda` / `voice-run-hipblas` / `voice-run-vulkan` — mirror the GPU variants.
- `make voice-run-mock` — no real-backend, no tts (both mock).
- `make voice-build` — release build of the voice service.

---

## Tests

- `crates/nagent-server`: existing tests pass unchanged in embedded mode (default).
- Add `crates/nagent-server/tests/voice_external.rs` (gated on
  `--no-default-features` + `voice-external-test`): spin up a fake
  voice service on a random port with a known shared secret, start the
  main server with `--features voice-external-test` + the same secret,
  drive `/api/voice/token` from a session-authenticated test client,
  verify the JWT, hit the fake service, verify the contract.
- Add `crates/nagent-voice-service/tests/integration.rs`:
  - Mint a valid JWT with the same secret, hit `/v1/audio/voices` (mock
    engine), expect 200 + JSON.
  - Send no token / wrong audience / expired token, expect 401.
  - Hit `/v1/audio/speech` with valid token + mock engine, expect
    200 + `audio/wav`.
- Manual smoke: start `make voice-run-mock` + `make run`, open
  `http://localhost:8080/`, talk into the mic, hear the synthesized
  reply. Then start `make run-ext` and verify the chat UI gracefully
  reports voice unavailable.

---

## Rollout

1. Land the change behind `voice-embedded = true` (default). No behaviour change.
2. Operators who want the split:
   - Generate a shared secret: `openssl rand -hex 32`.
   - Add `[voice.external]` to the main server config with the secret.
   - Write `voice.toml` for the voice service with the same secret and
     the model paths they used to put in `[stt]` / `[tts]`.
   - Build with `make voice-build` (or one of the GPU variants).
   - Restart the main server with `make run-ext` (no voice-embedded feature).
3. Existing single-host deployments do nothing.

---

## Out of scope (call out in PR description)

- Docker image / docker-compose for the voice service.
- Kustomize/Helm manifests for split deployment.
- JWKS / asymmetric keys / secret rotation tooling.
- Per-user rate-limit persistence across voice-service restarts (in-memory only).
- `make voice-smoke` and CI workflow for the voice service.
- Refactoring the `SttState` / `TtsState` into a shared library crate (the
  existing crate split already gives us the libraries; the state wrappers
  are duplicated for now).

---

## Risks and mitigations

- **JWT clock skew**: 30s leeway on `exp` / `nbf` / `iat` in the voice service
  (configurable). Documented in `voice-service.toml.example`.
- **Token in WS query string**: visible to any HTTP proxy in front of the
  voice service. Mitigation: enforce HTTPS in deployment; document that
  operators must TLS-terminate in front of the voice service. Future work
  can move the token to `Sec-WebSocket-Protocol` if a deployment actually
  needs that.
- **Static shared secret operational burden**: acknowledged. v1 is a single
  secret; a future PR can move to a key list with `kid` headers.
- **Code drift between main server and voice service**: the WS handler and
  TTS routes are duplicated. A follow-up can extract them into a shared
  library (`nagent-voice-router` or similar). Out of scope here.
- **Test fixtures referencing `/ws` and `/v1/audio/*` on the main server**:
  these continue to work in embedded mode (default). No fixture changes
  required.
