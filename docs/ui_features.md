# nagent UI features

This document inventories the user-visible features of the nagent web UI.
It is meant as a single reference for engineers and reviewers to know what
the frontend currently offers, where each feature lives in the code, and
how features relate to one another. It is descriptive, not a tutorial.

The UI is served as static assets from `crates/stt-server/src/static/` and
loaded by a single `index.html` page. There is no build step, no bundler,
and no npm install at runtime: every JS module, CSS file, and third-party
library is shipped as a plain file and served by the Rust static handler
(`crates/stt-server/src/static_assets.rs`). The page bootstraps two
top-level modes — **Transcript** and **Discussion** — only one of which is
visible at a time.

## 1. Top-level structure

### 1.1 Modes (Transcript / Discussion)

- A `role="tablist"` toggle in the header switches between the two views
  (`#mode-transcript-btn`, `#mode-discussion-btn`).
- The active mode is persisted in `localStorage` under `nagent.mode`
  (`crates/stt-server/src/static/mode.js`); the toggle is rehydrated before
  listeners are wired so the first paint already reflects the saved mode.
- A `modechange` `CustomEvent` is dispatched on `document` whenever the
  mode flips; `chat.js` listens for it to lazily hydrate the model list
  and history on first entry to Discussion.
- A read-only handle `globalThis.__nagentMode.current()` is exported for
  tests and debugging.

### 1.2 Header and version display

- The header (`crates/stt-server/src/static/index.html`) shows the app
  title, a subtitle, and two version pills:
  - `backend <code id="backend-version">` populated from `GET /api/version`.
  - `frontend <code id="frontend-version-self">` populated from
    `/static/version.txt`.
- An **update banner** (`#update-banner`) becomes visible when the server's
  frontend version no longer matches the version served to the current
  tab (`app.js` `maybeShowUpdateBanner`). It offers a Reload button and
  a Dismiss button; a `VERSION_POLL_MS = 30_000` interval re-checks the
  server version.

### 1.3 Shared voice oscilloscope

- A single `<section id="voice-graph-shared">` at the top of `<body>`
  hosts a `<canvas>` waveform and a `speech probability` level bar.
  Both Transcript and Discussion modes reuse the same DOM node
  (`AudioCapture._setGraphVisible` toggles `.is-hidden`), so a single
  Record click in either view drives the same oscilloscope.
- Drawing, frame buffering, and the speech-probability meter are
  implemented by `createScope` in `crates/stt-server/src/static/audio.js`.

## 2. Audio capture pipeline (shared)

The audio stack is identical in both modes. It is implemented in
`crates/stt-server/src/static/audio.js` and exposed as the `AudioCapture`
class. `app.js` (Transcript) and `chat.js` (Discussion) each instantiate
their own `AudioCapture` against their own DOM, sharing only the
underlying Silero VAD.

Pipeline:

```
MicVAD (on-device Silero, vendored) → WebSocket(/ws) → server-side Whisper
```

Concrete behaviour:

- **VAD singleton**: `ensureVad()` lazily constructs one `MicVAD` for the
  page lifetime using `model: "v6"` and assets from
  `/static/vendor/vad/`. The model is a ~3 MB download; instantiating
  twice is avoided by caching the construction promise.
- **VAD configuration**: `positiveSpeechThreshold: 0.6`,
  `negativeSpeechThreshold: 0.4`, `minSpeechFrames: 6`,
  `preSpeechPadFrames: 1`, `postSpeechPadFrames: 3`. ORT is configured
  with `logLevel: "error"`, `wasm.proxy = false`, `wasm.numThreads = 1`.
- **Cross-mode dispatch**: a module-level `activeRecorder` pointer
  routes `onFrameProcessed` / `onSpeechEnd` callbacks to whichever
  `AudioCapture` is currently recording. This works around the fact that
  MicVAD accepts its callbacks once at construction time and offers no
  swap API.
- **Cross-mode exclusivity**: every `AudioCapture` registers itself in
  `allRecorders`; clicking Record in one mode tears down any other
  instance before starting, so only one microphone stream is ever open.
- **Wire protocol**: a postcard-style binary codec mirrored from
  `stt-proto` lives in `audio.js` (`Tag`, `writeVarint`, `writeString`,
  `encodeAudioFrame`, `decodePayload`, etc.). The server returns
  `PartialTranscript`, `FinalTranscript`, `BackendInfo`, and `Error`
  tags.
- **Inactivity watchdog**: while recording, a 5 s timer
  (`INACTIVITY_TIMEOUT_MS = 5_000`) auto-stops the capture if no frame
  crosses `INACTIVITY_SPEECH_THRESHOLD = 0.4`. The timer is gated on
  `#inactivity-check`; Transcript mode persists the toggle in
  `localStorage` under `nagent.audio.inactivityEnabled`.
- **Auto-stop on view hide**: when the owning `containerEl` becomes
  `hidden` (e.g. the user switches modes), the capture pauses itself.
- **Public `stop()`**: `AudioCapture.stop()` is idempotent and is called
  by `chat.js` as soon as the FinalTranscript triggers the LLM request,
  so the mic closes during the model's response.

## 3. Transcript mode

Implemented by `crates/stt-server/src/static/app.js`. The DOM lives under
`<main id="view-transcript">` in `index.html`.

### 3.1 Toolbar controls

- **`#record-btn`**: single toggle button with `data-state` reflecting
  the pipeline state. Idle → "Record"; click → disabled "Loading…"
  during VAD init (prevents double-clicks); running → red "Stop".
- **`#lang-select`**: language picker with the values
  `""` (Auto-detect), `en`, `fr`, `es`, `de`, `it`, `pt`, `ja`, `zh`.
  Preselected from `navigator.languages` on first load by
  `preselectFromBrowser` (see [§7.1](#71-locale-aware-preselection)).
- **`#translate-check`**: translate the transcript to English.
- **`#inactivity-check`** (default checked): auto-stop after 5 s of
  silence. Persisted in `localStorage`.
- **`#download-btn`**: disabled until at least one line exists; downloads
  the full transcript as `transcript.txt` with the format
  `<ts> [<lang>] (<latency>) <text>` per line.
- **`#clear-btn`**: disabled until at least one line exists; pops a
  `confirm()` before wiping the in-memory list.
- **`#status`**: status pill (`idle`, `loading`, `connecting`,
  `listening`, `recording`, etc.) updated by `AudioCapture` callbacks.
- **`#backend-info`**: shows backend hints surfaced by the server
  (e.g. whisper.cpp build flags, GPU backend).

### 3.2 Transcript list

- An ordered list (`<ol id="transcript-list">`) appended in document
  order. Each `<li>` shows the language code in brackets, a local
  timestamp, an optional `latency` chip (audio → FinalTranscript
  round-trip, rendered as `123ms` / `1.2s`), and the recognised text.
- An empty-state row (`"No transcript yet. Click Record to start."`)
  appears when the list is empty and is removed on the first
  appendLine.
- Auto-scrolls to the bottom on each new line.

## 4. Discussion mode (chat)

Implemented by `crates/stt-server/src/static/chat.js` (~2,800 lines).
The DOM lives under `<main id="view-discussion">`. The mode is
fronted by a chat sidebar and a chat-main area.

### 4.1 Sidebar (chat sessions)

- `#chat-new-session` button to start a fresh conversation.
- `#chat-sessions` renders the in-memory session list sorted by
  `updatedAt` desc; each item supports **rename** (inline) and
  **delete**. Clicking an item switches the active session.
- Persistence is delegated to `chat-sessions.js`:
  - `nagent.chat.sessions` → JSON `[{ id, title, createdAt, updatedAt }]`
  - `nagent.chat.active` → active session id
  - `nagent.chat.session.<id>` → JSON message array per session
  - `nagent.chat.history` → legacy single-history payload, migrated into
    a session on first boot then removed.
- `HISTORY_CAP = 200` caps the messages persisted per session.
- `TITLE_MAX = 60` caps the auto-derived title (first user turn,
  collapsed whitespace, trailing ellipsis when truncated).

### 4.2 Header / model picker

- `#chat-model` populated from `GET /v1/models` (lazy-loaded on first
  mode entry and on `modechange` re-entries).
- `#chat-clear` wipes the current session.
- `#chat-status` mirrors the same status-pill semantics as Transcript.
- `#chat-location-pill` (hidden until geolocation is granted and a
  position cached): opens the Advanced panel for location management.

### 4.3 Audio controls (Discussion-specific)

- `#chat-lang-select`: same values as the Transcript picker, preselected
  from the browser locale on first load.
- `#chat-translate-check`: same semantics as the Transcript checkbox.
- `#chat-tts-check` / `#chat-tts-label` (hidden when the server reports
  TTS disabled at boot): toggles read-aloud. Created lazily from a
  user gesture to satisfy the browser's autoplay policy.
- `#chat-record-btn`: SVG mic/stop icons. Title says
  `Start voice recording (Ctrl+Shift+D)`.
- `#chat-backend-info`: backend info chip.

### 4.4 Advanced panel (`<details class="chat-advanced">`)

Folded into the main `<details>` rather than its own nested one:

- **TTS sub-panel** (`#chat-tts-settings`, hidden when read-aloud is
  off):
  - `#chat-tts-voice-en` and `#chat-tts-voice-fr` populated dynamically
    from `GET /v1/audio/voices` (only installed voices are shown).
  - `#chat-tts-speed` slider labelled `0.5x…1.5x`. Piper's
    `length_scale` is inverted (smaller = faster); the displayed value
    is the human-facing multiplier and the wire value is the inverse
    mapping computed in `chat.js`.
  - `#chat-tts-autoplay` (default checked): auto-play the next response.
  - `#chat-tts-stop-on-send` (default checked): cut off the previous
    reply's audio when a new turn starts.
  - `#chat-tts-test`: synthesises a fixed sample phrase and plays it
    through the same Web Audio queue the LLM replies use, without
    involving the LLM proxy.
- **Agents banner** (`#chat-agents-banner`): shown when the server
  advertises tools, naming the agents the model can call.
- **System prompt** (`#chat-system` textarea): appended to the server's
  default system prompt.
- **Temperature** (`#chat-temperature`, `0…2`, step `0.1`, default `0.8`):
  sent in the request body when finite.
- **Geolocation advanced controls** (`#chat-location-advanced`, hidden
  until a position is cached):
  - `#chat-location-toggle`: include my location in LLM context.
  - `#chat-location-refresh` / `#chat-location-forget`: re-acquire or
    drop the cached fix.
  - `#chat-location-status`: compact "captured Xs ago" status line.

### 4.5 Chat input form

- `#chat-input` textarea with the following shortcuts
  (`title` attribute documents them):
  - **Enter**: send the message.
  - **Ctrl+Enter / Shift+Enter**: insert a newline.
  - **Ctrl+Shift+D**: toggle voice recording.
- `#chat-location-share`: opt-in button that calls
  `navigator.geolocation.getCurrentPosition` on first click. Hidden
  once a position is cached (the header pill + Advanced controls take
  over); hidden outright when `window.isSecureContext === false`
  (e.g. plain-HTTP LAN deploy).
- `#chat-record-btn` (mic toggle): shared `AudioCapture` instance
  routed to chat. The mic closes (`audioCapture.stop()`) as soon as
  the FinalTranscript triggers the LLM request; the user can re-click
  Record to send a follow-up.
- `#chat-send`: SVG send icon, submits the form.
- `#chat-stop` (hidden by default): stops an in-flight LLM stream via
  `AbortController`.

### 4.6 Message rendering

- Each turn appends a `<div class="chat-message chat-message--user">`
  or `chat-message--assistant` bubble into `#chat-messages`.
- Assistant bubbles are processed by a Markdown pipeline
  (`renderMarkdown` in `chat.js`):
  1. `marked` parses the accumulated LLM stream into HTML.
  2. `DOMPurify` sanitises the result so a prompt-injection reply
     cannot smuggle `<script>` tags or `onerror=` handlers.
  3. Math delimiters (`$…$`, `$$…$$`) are normalised before parse
     so the model can emit LaTeX without escaping each backslash.
  4. KaTeX `auto-render` is invoked on the rendered DOM with
     `window.renderMathInElement(root, KATEX_RENDER_OPTIONS)` so
     `\frac{5000W}{500W}` becomes a stacked fraction.
  5. `decorateSafeLinks` post-processes `<a>` tags so external links
     get `rel="noopener noreferrer"` and `target="_blank"`.
- A `chat-loader` (three bouncing dots, `role="status"`,
  `aria-label="Loading response"`) is inserted into the assistant
  bubble while waiting for the first token; it is removed (without
  destroying the bubble's `innerHTML`) on the first delta.
- A per-bubble **replay button** (`🔊`) is attached to every assistant
  bubble but stays hidden (`chat-message--streaming`,
  `chat-message--tool-pending`, etc.) until the reply is fully
  rendered and the bubble transitions to a historical state.

### 4.7 Tool bubbles and widgets

When the LLM emits an SSE `tool_call` / `tool_result`, `chat.js`
appends a separate tool bubble (`#chat-message--tool-*`) and routes
the result through `resolveToolBubble`. Concrete examples:

- **Weather widget** (`renderWeatherWidget`): custom card rendered next
  to the tool bubble. Supports three modes — `current`, `daily`,
  `hourly` — chosen by `detectWeatherMode(data)`. The current-mode
  hero block shows an emoji condition icon, temperature, "feels
  like", wind, humidity, and UV; the daily/hourly strips show
  forecast rows with day labels (`formatDayShort`) and timezone-aware
  timestamps (`formatLocalTimestamp`). Idempotent: re-rendering a card
  replaces an existing one in place rather than stacking duplicates.
- **Web-fetch** tool bubble: title + summary link.
- The assistant bubble stays marked `chat-message--tool-pending`
  (with `display:none` on its content) until the tool resolves; the
  `finally` block in `streamReply` and `resolveToolBubble` drop the
  class on both success and error so prose becomes visible regardless
  of which tool ran.

### 4.8 Streaming reply

- `POST /v1/chat/completions` with `stream: true`. The body includes
  the earlier history (filtered to user/assistant), the new user
  turn, and an **ephemeral location block** (see §5) at the head of
  the `messages` array when geolocation is enabled. The location
  block is never persisted to history.
- The stream is read with the `fetch` body reader and a
  `TextDecoder`. SSE events are routed by `event:` name: `tool_call`
  and `tool_result` go to the tool-bubble layer; default `data:`
  events are appended to the assistant bubble.
- `temperature` and `model` are added to the body when finite / set.
- The status pill transitions from "Loading model…" (during the
  Ollama cold start) to "Streaming…" / "Working…" on first delta
  or first tool call.
- On a clean stream the persisted source is the raw markdown; on an
  abort with no tokens or a non-abort error the source is overwritten
  to match the rendered DOM so a page reload reproduces what the
  user saw.

### 4.9 TTS playback

Implemented primarily by `crates/stt-server/src/static/tts.js` with
controls in `chat.js`:

- Settings helpers: `getTtsSettings`, `resolveTtsVoice`,
  `lsGet/lsSet/lsGetBool/lsGetNum` for persistence.
- `sanitizeForTts`: strips markdown, code fences, links, and emoji
  before sending text to Piper; falls back to the raw reply if the
  sanitised body is empty.
- `splitSentence` / `phraseForTts`: phrase-level chunking so a
  long reply starts playing before the LLM finishes streaming.
- `findBoundary`: WAV boundary detection for streamed audio chunks.
- `getOrCreateTtsPlayer`: lazily creates the `AudioContext` (user
  gesture required); `feed` queues new chunks; `stopAll` honours
  `stopOnSend` and aborts.
- `replayMessage(div, btn)`: replays a single historical bubble
  through the same audio queue.
- `refreshReplayButtonVisibility`: hides / shows the per-bubble
  replay button as bubbles transition between streaming, pending,
  and finalised states.

## 5. Geolocation (Discussion)

Implemented by `crates/stt-server/src/static/geolocation.js` and
consumed by `chat.js` / `index.html`. Storage layout:

- `nagent.chat.location` → JSON `{ lat, lon, accuracy, timestamp }`
- `nagent.chat.locationEnabled` → `"true"` / `"false"`

Behaviour:

- **No TTL** on the cached position. Browser permission covers every
  `getCurrentPosition` after opt-in; `refreshLocationOnBoot`
  silently re-fetches on every page load.
- `getLocation()` uses `enableHighAccuracy: false`,
  `maximumAge: 60_000`, `timeout: 10_000`.
- `formatLocationMessage` rounds lat/lon to 4 decimals (~11 m) and
  emits a fixed marker prefix `User's approximate location: …` mirrored
  on the server as `USER_LOCATION_MARKER` so the admin kill-switch
  (`LLM_ALLOW_USER_LOCATION=false`) can strip the block before it
  reaches the upstream model.
- `formatRelativeTime` caps at "1d+" so a multi-day-stale value stays
  short.
- The initial opt-in path is the `#chat-location-share` button in the
  form footer; once granted, the header pill becomes the visible
  affordance and the Advanced controls expose toggle / refresh / forget.

## 6. Vendored assets

All third-party assets are vendored under
`crates/stt-server/src/static/vendor/` so the server can serve them
with no network round-trip and no build step:

- `vad/` — `@ricky0123/vad-web@0.0.31` UMD bundle (depends on
  `window.ort`).
- `ort/` — `onnxruntime-web 1.17.0`.
- `marked/` — `marked` (Markdown parser).
- `sanitize/` — `DOMPurify` (HTML sanitiser).
- `katex/` — KaTeX (math), including `auto-render` and WOFF2 fonts
  under `katex/fonts/`.
- `version.txt` — frontend build identifier polled for the update banner.

`index.html` documents inline why CDN loading was abandoned (jsdelivr
does not serve `ort-wasm-simd-threaded.mjs`, and the AudioWorklet +
worker URL need a same-origin path).

## 7. Cross-cutting concerns

### 7.1 Locale-aware preselection

`crates/stt-server/src/static/lang-preselect.js` exports
`preselectFromBrowser(selectEl)`. It walks `navigator.languages`,
falls back to `navigator.language`, reduces each BCP-47 tag to its
primary language subtag (`fr-FR` → `fr`, `zh-Hans-CN` → `zh`), and
selects the first match in the dropdown. It is a no-op when the
select already has a non-empty value, so explicit persistence always
wins. A `change` event is dispatched after mutation so the audio
pipeline re-reads the new value. Called once from `app.js` and once
from `chat.js` on boot.

### 7.2 Persistence summary

| Key                                | Used by        | Purpose                          |
|------------------------------------|----------------|----------------------------------|
| `nagent.mode`                      | mode.js        | Active mode                      |
| `nagent.audio.inactivityEnabled`   | app.js         | Inactivity checkbox              |
| `nagent.chat.sessions`             | chat-sessions  | Sidebar metadata                 |
| `nagent.chat.active`               | chat-sessions  | Active session id                |
| `nagent.chat.session.<id>`         | chat-sessions  | Per-session message array        |
| `nagent.chat.history`              | chat-sessions  | Legacy payload (one-shot migration) |
| `nagent.chat.location`             | geolocation    | Cached position                  |
| `nagent.chat.locationEnabled`      | geolocation    | Geolocation opt-in flag          |
| TTS settings (keys defined in `chat.js`) | tts/chat  | Voice, speed, autoplay, stopOnSend |

### 7.3 Accessibility

- `role="tablist"` / `role="tab"` on the mode toggle and
  `aria-selected` kept in sync on switch.
- `aria-live="polite"` on `#transcript-list` and `#chat-messages` so
  new lines / bubbles are announced.
- `aria-label` on the mic record buttons, send / stop buttons, and
  the live waveform (`aria-label="Live microphone waveform"`).
- `chat-loader` carries `role="status"` and
  `aria-label="Loading response"` so the bouncing-dots placeholder is
  announced.

### 7.4 Visual theme

- Dark theme by default (`:root` defines `--bg`, `--fg`, `--accent`,
  `--accent-2`, `--error`, `--ok`, `--border`, plus mono and sans
  font stacks) in `crates/stt-server/src/static/style.css`.
- No light-theme toggle in the current UI; the colour scheme is
  fixed.

## 8. Out of scope (today)

For clarity, these are **not** features of the current UI even if a
reasonable reader might assume they are:

- No light theme switcher.
- No account / login flow — every setting is stored per-browser in
  `localStorage`; there is no server-side user state.
- No multi-user chat: sessions are keyed by `localStorage` and do not
  sync across devices.
- No streaming partial transcripts in the UI for the Transcript view:
  only `FinalTranscript` messages append a line; `PartialTranscript`
  tags are received but not surfaced.
- No drag-and-drop file upload for transcription.
- No export of chat sessions as a downloadable file (only the
  Transcript mode offers `.txt` export).
- No custom system prompt presets library.
- No multi-model concurrent comparison.
