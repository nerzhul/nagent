# nagent UI features

This document inventories the user-visible features of the nagent web UI.
It is meant as a single reference for engineers and reviewers to know what
the frontend currently offers, where each feature lives in the code, and
how features relate to one another. It is descriptive, not a tutorial.

The UI is served as static assets from `crates/nagent-server/src/static/` and
loaded by a single `index.html` page. There is no build step, no bundler,
and no npm install at runtime: every JS module, CSS file, and third-party
library is shipped as a plain file and served by the Rust static handler
(`crates/nagent-server/src/static_assets.rs`). The page bootstraps two
top-level modes — **Transcript** and **Discussion** — only one of which is
visible at a time. When the operator enables `auth.enabled = true` on the
server, the UI is also gated by a **global portal** so anonymous
visitors never see the chat/voice controls at all — see §8.

## 1. Top-level structure

### 1.1 Modes (Transcript / Discussion)

- A `role="tablist"` toggle in the header switches between the two views
  (`#mode-transcript-btn`, `#mode-discussion-btn`).
- The active mode is persisted in `localStorage` under `nagent.mode`
  (`crates/nagent-server/src/static/mode.js`); the toggle is rehydrated before
  listeners are wired so the first paint already reflects the saved mode.
- A `modechange` `CustomEvent` is dispatched on `document` whenever the
  mode flips; `chat.js` listens for it to lazily hydrate the model list
  and history on first entry to Discussion.
- A read-only handle `globalThis.__nagentMode.current()` is exported for
  tests and debugging.

### 1.2 Header and version display

- The header (`crates/nagent-server/src/static/index.html`) shows the app
  title, a subtitle, and two version pills:
  - `backend <code id="backend-version">` populated from `GET /api/version`.
  - `frontend <code id="frontend-version-self">` populated from
    `/static/version.txt`.
- An **update banner** (`#update-banner`) becomes visible when the server's
  frontend version no longer matches the version served to the current
  tab (`app.js` `maybeShowUpdateBanner`). It offers a Reload button and
  a Dismiss button; a `VERSION_POLL_MS = 30_000` interval re-checks the
  server version.

### 1.3 Voice oscilloscope (shared widget, per-mode placement)

The voice oscilloscope — a waveform canvas + a speech-probability
level bar — is a **reusable widget, not a single shared DOM node**.
Its drawing, frame buffering, and level-meter logic live in
`createScope` in `crates/nagent-server/src/static/audio.js`, and both
modes drive it through their own `AudioCapture` instance. What
changes between modes is *where the widget is mounted in the DOM*:

- **Transcript mode**: the oscillo is mounted at the top of the
  Transcript view, above the controls. It appears as soon as the
  user clicks Record and disappears when recording stops. The CSS
  owns the opacity / height transition.
- **Discussion mode**: the oscillo is mounted **inline inside the
  conversation**, rendered as a voice bubble inside `#chat-messages`
  (see §4.10). It follows the same visual lifecycle (appear on
  Record, disappear on stop) but lives among the chat messages
  rather than at the top of the page.

The shared-code contract means a future mode (or a re-skin) can
mount a third instance of the widget without touching `createScope`
or `AudioCapture` — only the DOM anchor changes.

## 2. Audio capture pipeline (shared)

The audio stack is identical in both modes. It is implemented in
`crates/nagent-server/src/static/audio.js` and exposed as the `AudioCapture`
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

Implemented by `crates/nagent-server/src/static/app.js`. The DOM lives under
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

Implemented by `crates/nagent-server/src/static/chat.js` (~2,800 lines).
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
  collapsed whitespace, trailing ellipsis when truncated). New
  sessions start with the `DEFAULT_TITLE = "New chat"` placeholder
  until `deriveTitle` produces the real title.

### 4.2 Header / model picker

- `#chat-model` populated from `feature("llm_models")`, which is
  populated server-side from the upstream `/v1/models` query inside
  the `GET /api/features` response (see `crates/nagent-server/src/http/features.rs`
  and `crates/nagent-server/src/llm/proxy.rs::fetch_upstream_model_list`).
  The frontend never calls `/v1/models` directly — that route is
  kept for direct callers (`curl`, SDKs) but the chat UI uses the
  feature-flag payload exclusively. The server bounds the upstream
  fetch with `UPSTREAM_MODELS_TIMEOUT` (3s) and collapses to
  `[default_model]` on any failure so the dropdown is never empty.
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

Folded into the main `<details>` rather than its own nested one.
When `feature("llm") === false` (LLM proxy not wired on this
server) the whole panel is hidden and the `#chat-disabled-notice`
is shown instead (chat.js:1473-1484), so the picker never offers an
unusable list. A reachable upstream that returns no models still
keeps the panel visible — the server falls back to
`[default_model]` and the user sees at least the configured model.

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
- **Integrations drawer** (`#chat-integrations`, hidden until
  `auth.enabled = true` AND the user is logged in): one row per
  registered service with an icon, label, "Configured" /
  "Not configured" status pill, and a Configure / Edit button.
  The Configure / Edit button opens a modal with a `<form>` per
  field (`<input type="password">` for `password` fields,
  `<input type="url">` for `url` fields). PUT sends the values to
  `PUT /api/integrations/:id/credentials` (CSRF-protected); a
  separate Remove button issues `DELETE`. The v1 framework ships
  with an empty registry so the drawer shows "No integrations
  available yet" until follow-up PRs add concrete `ServiceDef`
  entries — the HTTP surface is fully wired in advance.
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

### 4.6 Assistant message rendering

Every assistant turn in `#chat-messages` follows the same rendering
contract, regardless of which model, tools, or widgets are involved.
The assistant bubble is a vertical stack of three optional slots and
one post-stream affordance:

```
┌──────────────────────────────────────────────────────────┐
│ [Tools ▶]            ← collapsed "Tools" row (only if    │
│                       ←  the LLM emitted any tool calls) │
├──────────────────────────────────────────────────────────┤
│ <widget inset>       ← generic widget card (only if the  │
│                       ←  tool emitted a renderable one)  │
├──────────────────────────────────────────────────────────┤
│ Markdown prose…      ← the LLM reply body                │
│                                                          │
│                                       🔊  ← TTS replay   │
└──────────────────────────────────────────────────────────┘
```

The slots above are filled according to the rules below. They are
mutually independent — a turn with no tools has no tools row, a turn
with a tool that has no widget renderer has no widget inset, and so on.

#### 4.6.1 Loading state

- While the response is streaming, the assistant bubble shows a
  `chat-loader` (three bouncing dots, `role="status"`,
  `aria-label="Loading response"`) in place of the prose body.
- The loader is appended via `appendChild` (not `innerHTML =`) so the
  per-bubble replay button that `appendBubble` already attached is
  not destroyed when the loader is later removed.
- On the first SSE delta the loader is removed and the prose body
  starts accumulating. If the LLM emits a `tool_call` first, the
  loader is replaced by the tools row instead.

#### 4.6.2 Tool row (when tools were used)

- **Position**: the tools row lives *inside* the assistant bubble, *above*
  the widget inset and the prose body, so the visual order matches the
  reading order: tool call → tool result widget → LLM prose.
- **Disclosure**: by default the row is collapsed into a small,
  discreet per-tool caption. The `<summary>` is built from
  `<icon> <name> <status>` (`chat.js:939-950`); the user sees e.g.
  "🔎 web_fetch ✓ <summary>" once the tool completes. The native
  `<details>` chevron provides the expand / collapse arrow.
- **During a tool run**: the row is open so progress is visible
  (running spinner, live status text). On tool success the wrapper
  stays open until `streamReply`'s `finally` block runs and the
  bubble is finalised (chat.js:1982-1984, which also drops
  `chat-message--streaming`); on tool error `resolveToolBubble`
  collapses the `<details>` eagerly (chat.js:1068-1069) so a failed
  tool trace does not stay expanded in front of the error prose.
- **Multiple tools**: each tool call gets its own entry inside the
  same collapsible region. Order matches the order in which the LLM
  emitted `tool_call` SSE events.
- **Implementation**: the row is a `<details class="chat-message__tool-usage">`
  with a `<summary>` carrying the "Tools" caption; the `<details>`'s
  native open / close semantics provide the arrow affordance. The
  inner body is a stack of `.chat-tool-bubble` rows built by
  `appendToolBubble` (one per tool call).
- **Visibility during tool runs**: while a tool call is in flight, the
  assistant prose body is hidden via the `chat-message--tool-pending`
  class on the bubble. The CSS for that class explicitly preserves
  the tool trace (`details.chat-message__tool-usage`) and any
  widget card (`chat-weather-card`) so progress is never invisible.
  Both the `finally` block in `streamReply` and `resolveToolBubble`
  drop the class on success **and** on tool error so the prose
  becomes visible regardless of which tool ran.

#### 4.6.3 Widget inset

- **Position**: the widget slot is a *sibling* of the tool trace
  `<details class="chat-message__tool-usage">`, NOT a child of it.
  Putting the card inside the `<details>` made it disappear the
  moment the user collapsed the tool summary (the card was the
  visible answer — losing it with the trace defeated the point of
  the widget). The card now sits as a direct child of the assistant
  bubble, immediately after the `<details>`, so the visual stack
  reads: tool trace → weather card → prose body. The hint inserted
  by `finalizeAssistantForToolResult` ("🌤️ Détails ci-dessous.")
  is positioned directly before the card so the contracted view
  reads: tool trace → hint → card → 🔊.
- **Presence**: the slot is shown only when the tool that resolved
  advertises a widget renderer (see §4.7). For tools without a
  renderer the slot is empty and the prose body sits directly below
  the tools row.
- **Idempotency**: `renderWeatherWidget` (chat.js) walks the
  bubble's tracked `_weatherCards` array and removes the
  previously-rendered card before inserting the new one, so a
  session re-hydration racing the live stream never produces two
  cards for the same tool call.
- **Re-mount across markdown re-renders**: the card is a direct
  child of the assistant bubble, so `applyMarkdown`'s per-tick
  `innerHTML = ""` reset wipes it along with the prose. The bubble
  carries a parallel `_weatherCards` array (mirroring
  `_toolUsageEls`) that `applyMarkdown` re-mounts after every
  render — order matches the order the cards were inserted, with
  each card placed immediately before the replay button so it
  stays the last visible element.
- **Weather replace**: on `get_weather` success with non-trivial
  prose, `finalizeAssistantForToolResult` adds the
  `chat-message--weather-replaced` class to the assistant bubble.
  This class is the canonical CSS hook for the "card-as-answer"
  layout — the prose is reduced to the italic hint "🌤️ Détails
  ci-dessous." (inserted directly before the card) while the tool
  trace and the weather card stay visible. Short replies
  (≤ 120 chars, single line) are kept verbatim and the class is not
  applied.

#### 4.6.4 Prose body

- The LLM reply is processed by a Markdown pipeline
  (`renderMarkdown` in `chat.js`):
  0. `normalizeMathDelimiters(text)` rewrites the model-friendly
     `[\n … \n]` and `[\frac{…}]` shorthand into the `\[…\]`
     delimiters KaTeX actually recognises (chat.js:193-222). Without
     this pre-pass the model cannot reliably emit display math
     without escaping every backslash.
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
- During the stream the markdown is re-rendered on each accumulated
  delta so links and math stay current; the final render happens
  once on the full reply.

#### 4.6.5 Finalised state — TTS replay button

- **Visibility gate (two-tier)**:
  - **CSS** (style.css:1682-1684): while the assistant bubble is
    mid-stream, `chat-message--streaming` hides its
    `.chat-message-replay` button via `display: none`. The class is
    added at `streamReply` start and removed in its `finally`.
  - **JS** (chat.js:2331-2336, `refreshReplayButtonVisibility`):
    on every state change the function walks every assistant
    bubble and toggles `btn.hidden` purely from
    `window.__ttsAvailable` — it does **not** look at streaming,
    tool-pending, or weather-replaced state. Tool-pending bubbles
    hide the button structurally (no `.chat-message-replay` is
    attached to the placeholder), not via class.
  - Net effect: the button is hidden only when Piper is unavailable
    on the server (`__ttsAvailable === false`) or while the current
    bubble is actively streaming.
- **Click semantics** (chat.js:2354-2399, `replayMessage`):
  - Idle → speak the full sanitised bubble text in one HTTP
    round-trip (no per-sentence streaming).
  - Clicking the *same* playing button → stop playback.
  - Clicking a *different* bubble's button while another is playing
    → stop the previous one, start the new one.
  - The button auto-enables the master `#chat-tts-check` if it was
    off, so a first-time user does not need to open the Advanced
    panel first.
- Clicking the button re-synthesises the bubble's prose through the
  same Piper-backed audio queue as live replies (see §4.9). The
  button is hidden again if the user starts a new turn while the
  replay is playing (`stopOnSend` semantics).

### 4.7 Available widget renderers

The widget inset described in §4.6.3 is filled in by a per-tool
renderer keyed on the tool name. Today the only renderable tool is
`get_weather` (§4.7.1); every other tool result — including
`calculate`, `web_fetch`, `wikipedia`, … — is surfaced through the
generic tool trace only, with no dedicated widget. Adding a new
widget means writing a renderer that appends a card under its
`.chat-tool-bubble` (mirroring the weather card below); the bubble
layout itself does not need to change.

#### 4.7.1 Weather

- **Renderer**: `renderWeatherWidget(parentEl, data, mode)` in `chat.js`.
- **Triggered by**: the `get_weather` tool result. The mode
  (`current`, `daily`, or `hourly`) is auto-detected by
  `detectWeatherMode(data)` from the payload shape.
- **Layout**:
  - Header line: city · as-of timestamp · requested date (for
    forecast modes). Always present so the card is scannable even
    when the upstream forecast array is empty.
  - Hero block (`current` mode only): emoji condition icon,
    temperature, "feels like", wind (direction + cardinal + speed),
    humidity, UV index.
  - Forecast strip (`daily` / `hourly`): day / hour cells with
    labels (`formatDayShort`), mini icon, hi temperature, and
    precipitation probability chip.
- **Timezone handling**: every timestamp in the card uses
  `formatLocalTimestamp` against the location's IANA timezone, not
  the user's local clock.

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

Implemented primarily by `crates/nagent-server/src/static/tts.js` with
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

### 4.10 Inline voice waveform widget (Discussion)

In Discussion mode the voice oscilloscope (§1.3) is not mounted at the
top of the page — it is rendered **inline as a voice bubble inside
`#chat-messages`**, so the live waveform sits in the same scroll
context as the user / assistant turns.

- **DOM anchor**: a dedicated voice-bubble element appended to
  `#chat-messages`. The DOM tree for the widget itself is built by
  the same `createScope` factory used by Transcript mode — only the
  mount point differs.
- **Lifecycle**:
  - Hidden by default (`class="voice-graph is-hidden"`-equivalent on
    the inline wrapper).
  - Inserted / revealed on Record (`AudioCapture._setGraphVisible` or
    the chat-mode equivalent) and removed / re-hidden on stop.
  - During recording it scrolls with the conversation so the user
    can keep typing in `#chat-input` while watching the waveform.
- **Per-mode placement contract**: this is the Discussion-mode
  instance of the shared voice oscilloscope widget. Transcript mode
  still mounts its instance at the top of the transcript view
  (§1.3, §3). The shared-code contract means a future mode can mount
  a third instance without touching `createScope` or `AudioCapture`.
- **Why inline**: anchoring the widget in the conversation makes the
  recording session feel like a chat-native action (the user's voice
  is part of the transcript) rather than a toolbar overlay. It also
  keeps the visible scope close to the user's gaze point — typically
  the chat input — when the chat view has scrolled down.
- **Cross-mode exclusivity is preserved**: clicking Record in either
  mode tears down the other mode's `AudioCapture` first (§2), so the
  inline voice widget in Discussion and the top-mounted widget in
  Transcript never animate at the same time.

## 5. Geolocation (Discussion)

Implemented by `crates/nagent-server/src/static/geolocation.js` and
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
`crates/nagent-server/src/static/vendor/` so the server can serve them
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

`crates/nagent-server/src/static/lang-preselect.js` exports
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
  font stacks) in `crates/nagent-server/src/static/style.css`.
- No light-theme toggle in the current UI; the colour scheme is
  fixed.

## 8. Authentication (global portal)

When the operator sets `auth.enabled = true` on the server, the
UI is gated by a **global portal** with two layers of defence so
that an anonymous visitor never sees — and cannot inspect in
DevTools — the chat/voice controls. The portal is implemented as
a coordination between three files:

- `crates/nagent-server/src/static/index.html` — the HTML shell.
- `crates/nagent-server/src/static/auth.js` — the only script
  loaded directly in the body, owns the probe + login form +
  template mount/unmount lifecycle.
- `crates/nagent-server/src/static/app.js` — runs *inside* the
  cloned template, renders the auth pill, dispatches the
  `nagent:logout` event.

The backend wiring is described in the README §"Authentication";
this section only covers the UI behaviour and the rationale for
the template split.

### 8.1 DOM absence (template split)

The page body has three top-level children:

```html
<body>
  <dialog id="login-modal">…</dialog>           <!-- always in DOM -->
  <div id="app-root"></div>                     <!-- empty mount point -->
  <template id="app-shell-template">
    <header>…</header>                          <!-- chat/voice UI -->
    <main id="view-transcript">…</main>
    <main id="view-discussion">…</main>
    <div id="shortcuts-modal">…</div>
    <script src="/static/mode.js" defer></script>
    <script type="module" src="/static/chat.js"></script>
    <script type="module" src="/static/app.js"></script>
    <script type="module" src="/static/shortcuts.js"></script>
  </template>
  <script type="module" src="/static/auth.js"></script>
</body>
```

The login `<dialog>` sits outside the template because
`auth.js` needs to show it *before* the rest of the UI exists.
Everything else — the header (with the auth pill, the mode
toggle, the version line), both mode views, the shortcuts help
modal, and every UI script other than `auth.js` — lives inside
`<template id="app-shell-template">`. While the template is
inert, its content is **not in the document tree**: a logged-out
visitor inspecting the page in DevTools sees only the empty
mount point and the login dialog. The chat controls, voice
graph, transcript list, and chat input do not exist in the DOM
and cannot be queried, clicked, or leaked via a screenshot tool.

The template is paired with a static-assets test
(`index_html_wraps_app_shell_in_template` in
`tests/static_assets.rs`) that asserts every UI element that
should be hidden from anonymous visitors is positioned *inside*
the template, while the login dialog and the `auth.js` script
tag are positioned *outside*.

### 8.2 Auth probe and mount lifecycle

`auth.js` is the first script to run on the page (it's the
last `<script>` tag in the body, so all preceding markup is
parsed). On module load it:

1. Fetches `GET /api/me` with `cache: "no-store"` and
   `credentials: "same-origin"`. The response status drives the
   rest of the boot:
   - `200` — server returns the `AuthUser` JSON. The user is
     authenticated. `auth.js` stores the user on a private
     `state` object, exposes it via
     `window.nagentAuth.getUser()`, and calls
     `mountShell()` to clone the template into `#app-root`.
   - `404` — auth subsystem not configured on the server (the
     default). The pill is hidden and the pre-PR1 single-user
     trust boundary holds. `auth.js` still clones the template
     so the chat UI is visible to anyone reaching the page.
   - `401` — auth enabled and the visitor is anonymous. `auth.js`
     does **not** clone the template. It opens the login modal
     in *forced* mode (see §8.3) so the user has to authenticate
     before the UI is created. The chat controls are genuinely
     absent from the DOM.
   - Any other status — logged to the console, no mount. (The
     server's `/healthz` endpoint stays reachable without auth
     so ops tooling can probe the server.)
2. Wires the login form (`#login-form`), the Cancel button
   (`#login-cancel`), and the modal's `cancel` event handler
   (§8.3) once, regardless of which boot branch ran.
3. Listens for the `nagent:logout` `CustomEvent` on
   `window`. When the cloned `app.js` dispatches it (because
   the user clicked the auth pill's **Sign out** button),
   `auth.js` clears the mount point with
   `appRoot.replaceChildren()`, marks the user as anonymous,
   and opens the forced modal again — re-creating the
   DOM-absence invariant after every logout.

The `mountShell()` helper is idempotent: if the template is
already cloned into `#app-root` (e.g. the user is already
authenticated and re-runs the probe for any reason), the second
call is a no-op. Conversely `unmountShell()` is unconditional:
it removes every cloned child so the next `mountShell()` call
re-runs the inline scripts cleanly (a fresh closure per clone).

### 8.3 Forced login modal

The login dialog uses the native `<dialog>` element. It is
shown with `dialog.showModal()` so it sits in the browser's
top layer and the rest of the page is inert while it is open.
Two cosmetic layers finish the lockdown:

- **Cancel hidden in forced mode.** When `auth.js` opens the
  modal with `force=true` (the only call site is the 401 boot
  branch and the `nagent:logout` handler), it adds the
  `login-modal--forced` class to the dialog and sets
  `#login-cancel[hidden]`. The CSS rule
  `.login-modal--forced .login-cancel { display: none; }` keeps
  the button out of the layout as well. The "Sign in" submit
  button remains visible and is the only way to dismiss the
  modal.
- **ESC re-opens the dialog.** The dialog's native `cancel`
  event fires when the user presses ESC or clicks the
  backdrop. A listener on `#login-modal` calls
  `e.preventDefault()` and re-opens the dialog with
  `showModal()` + `loginEmailInput.focus()` whenever both the
  `login-modal--forced` class is present *and* the user is
  still anonymous. After a successful login the class is
  removed (in `hideLoginModal()`) and the listener is a no-op.

The modal is hidden in non-forced mode as well — a non-forced
`showLoginModal(false)` call only happens when the user clicks
the auth pill's **Sign in** button (visible only when the
server returns 200 with no user, an edge case for a revoked
session). In that branch the user *can* dismiss the modal
with Cancel or ESC and continue using the app.

### 8.4 Inter-module coordination

The cloned `app.js` does not own authentication state. It reads
the current user from `window.nagentAuth` on boot, renders the
auth pill, and dispatches `nagent:logout` on Sign out. The
exposed object is frozen so the cloned app cannot mutate the
authoritative state:

```js
window.nagentAuth = Object.freeze({
  getUser: () => state.user,
  isProbed: () => state.probed,
  showLoginModal: (force = false) => showLoginModal(force),
  hideLoginModal: () => hideLoginModal(),
});
```

The auth pill is hidden entirely when `authState.probed` is
false (the 404 boot branch). When probed and the user is set,
the pill shows `Logged in as <email>` and a **Sign out**
button. When probed but the user is missing (an unusual
state — typically a 200 with a transient session), the pill
shows **Sign in** and a click delegates to
`window.nagentAuth.showLoginModal(false)`.

`app.js`'s **Sign out** click handler POSTs to
`/api/auth/logout` with the per-session `x-csrf-token` header
(unset on the server cookie name, configurable via
`auth.csrf_header`), then dispatches `nagent:logout` and
returns. The actual shell teardown happens in `auth.js` so
that `app.js` does not have to know about the template
machinery.

### 8.5 Cookie + CSRF model

Authenticated browser clients carry a `nagent_session` cookie
holding a UUID v4. API clients can pass the same id via
`Authorization: Bearer <session-id>`. State-changing browser
requests (POST/PUT/PATCH/DELETE) must also send an
`x-csrf-token` header (configurable via `auth.csrf_header`)
whose value matches the per-session `csrf_token` (32 bytes of
OS RNG, hex-encoded). Bearer requests skip the CSRF check
because the `Authorization` header proves the caller is the
API client itself, not a victim of a CSRF attack.

The cookie `Secure` flag is **auto-disabled on
`http://localhost`/`http://127.0.0.1`** so the dev experience
works out of the box; for every other origin the flag mirrors
the URL scheme. Sessions are absolute (NIST SP 800-63B):
`expires_at = now() + auth.session_ttl_days` and never extends
on activity.

## 9. Out of scope (today)

For clarity, these are **not** features of the current UI even if a
reasonable reader might assume they are:

- No light theme switcher.
- No account / login flow — every setting is stored per-browser in
  `localStorage`; there is no server-side user state. (The backend
  has landed in PR1 but the UI integration is staged — see §8.)
- No multi-user chat: sessions are keyed by `localStorage` and do not
  sync across devices. (Same as above; the auth DB is ready, the
  client-side login panel is staged for PR2.)
- No streaming partial transcripts in the UI for the Transcript view:
  only `FinalTranscript` messages append a line; `PartialTranscript`
  tags are received but not surfaced.
- No drag-and-drop file upload for transcription.
- No export of chat sessions as a downloadable file (only the
  Transcript mode offers `.txt` export).
- No custom system prompt presets library.
- No multi-model concurrent comparison.
