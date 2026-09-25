// nagent STT — frontend logic.
//
// Wire protocol (binary WebSocket frames):
//   tag 0x01: AudioFrame        (client -> server)  payload: Vec<f32>
//   tag 0x10: StartSession      (client -> server)  payload: { lang_hint, sample_rate }
//   tag 0x11: StopSession       (client -> server)  payload: ()
//   tag 0x12: Config            (client -> server)  payload: { language, translate }
//   tag 0x20: PartialTranscript (server -> client)
//   tag 0x21: FinalTranscript   (server -> client)
//   tag 0x30: Error             (server -> client)
//   tag 0x31: BackendInfo       (server -> client)  payload: { model_id, gpu_backend }
//
// Each frame is `[tag_byte, ..postcard_bytes]`. The tag byte is the **only**
// type discriminator: the variant struct follows directly. (Earlier
// versions also emitted a postcard `Payload` enum variant index, which
// shifted every field on the JS side — see the `wire_format_is_tag_then_inner_only`
// test in `stt-proto` for the byte-level assertion.)
//
// The session ID is generated server-side at upgrade time and never travels
// on the wire, which is what guarantees per-session isolation.
//
// `@ricky0123/vad-web` ships a UMD bundle, not an ESM module, so we read
// `MicVAD` off the `window.vad` global that the bundle sets up. The
// <script> tags in index.html must load onnxruntime-web first and the VAD
// bundle second.

const { MicVAD } = globalThis.vad;

// ---- DOM --------------------------------------------------------------------

const $ = (id) => document.getElementById(id);
const recordBtn   = $("record-btn");
const langSelect  = $("lang-select");
const translateCk = $("translate-check");
const downloadBtn = $("download-btn");
const statusEl    = $("status");
const backendEl   = $("backend-info");
const listEl      = $("transcript-list");
const scopeCanvas = $("voice-graph-canvas");
const scopeLevel  = $("voice-graph-level");
const scopeCtx    = scopeCanvas.getContext("2d");

// ---- Protocol tags (must match stt-proto) -----------------------------------

const Tag = Object.freeze({
  Audio:            0x01,
  StartSession:     0x10,
  StopSession:      0x11,
  Config:           0x12,
  PartialTranscript: 0x20,
  FinalTranscript:  0x21,
  Error:            0x30,
  BackendInfo:      0x31,
});

// ---- Minimal postcard-compatible codec --------------------------------------
//
// We can't pull `postcard` directly into the browser without a bundler, so we
// implement just the subset of `postcard` we need for the few payload types
// above. postcard's varint encoding is used for lengths; struct fields are
// serialised in declaration order with no implicit length prefix.
//
// Supported primitive encodings:
//   - u8 / i8     : 1 byte
//   - u16 / i16   : 2 bytes little-endian
//   - u32 / i32   : 4 bytes little-endian
//   - u64 / i64   : 8 bytes little-endian
//   - f32 / f64   : IEEE 754 little-endian
//   - bool        : 1 byte (0 or 1)
//   - Option<T>   : 1 byte tag (0 = None, 1 = Some), then T
//   - Vec<T>      : varint length, then T elements
//   - String      : varint byte length, then UTF-8 bytes
//   - structs     : fields concatenated in declaration order

function writeVarint(out, n) {
  // Unsigned LEB128.
  while (n >= 0x80) {
    out.push((n & 0x7f) | 0x80);
    n = Math.floor(n / 2 ** 7);
  }
  out.push(n & 0x7f);
}

function readVarint(view, state) {
  // Read LEB128 up to 10 bytes; returns the integer and bumps the offset.
  let result = 0n;
  let shift = 0n;
  let bytes = 0;
  while (true) {
    if (state.offset >= view.byteLength) throw new Error("truncated varint");
    const b = view.getUint8(state.offset++);
    result |= BigInt(b & 0x7f) << shift;
    bytes++;
    if ((b & 0x80) === 0) break;
    shift += 7n;
    if (bytes >= 10) throw new Error("varint too long");
  }
  return Number(result);
}

function writeString(out, s) {
  const bytes = new TextEncoder().encode(s);
  writeVarint(out, bytes.length);
  for (const b of bytes) out.push(b);
}

function readString(view, state) {
  const len = readVarint(view, state);
  const start = state.offset;
  const end = start + len;
  if (end > view.byteLength) throw new Error("truncated string");
  const dec = new TextDecoder("utf-8");
  state.offset = end;
  return dec.decode(new Uint8Array(view.buffer, start, len));
}

function writeBool(out, v) { out.push(v ? 1 : 0); }

function readBool(view, state) {
  if (state.offset >= view.byteLength) throw new Error("truncated bool");
  return view.getUint8(state.offset++) !== 0;
}

function writeVecF32(out, arr) {
  writeVarint(out, arr.length);
  const view = new DataView(new ArrayBuffer(arr.length * 4));
  for (let i = 0; i < arr.length; i++) view.setFloat32(i * 4, arr[i], true);
  for (let i = 0; i < arr.length * 4; i++) out.push(view.getUint8(i));
}

function readVecF32(view, state) {
  const len = readVarint(view, state);
  const start = state.offset;
  const end = start + len * 4;
  if (end > view.byteLength) throw new Error("truncated vec<f32>");
  const out = new Float32Array(len);
  for (let i = 0; i < len; i++) {
    out[i] = view.getFloat32(start + i * 4, true);
  }
  state.offset = end;
  return Array.from(out);
}

function writeOptionString(out, opt) {
  if (opt == null) { out.push(0); return; }
  out.push(1);
  writeString(out, opt);
}

function readOptionString(view, state) {
  const tag = readBool(view, state);
  if (!tag) return null;
  return readString(view, state);
}

function writeSegment(out, seg) {
  writeString(out, seg.text);
  writeVarint(out, seg.t0_ms);
  writeVarint(out, seg.t1_ms);
  // f32 little-endian
  const fbuf = new ArrayBuffer(4);
  new DataView(fbuf).setFloat32(0, seg.no_speech_prob, true);
  for (let i = 0; i < 4; i++) out.push(new Uint8Array(fbuf)[i]);
}

function readSegment(view, state) {
  const text = readString(view, state);
  const t0 = readVarint(view, state);
  const t1 = readVarint(view, state);
  if (state.offset + 4 > view.byteLength) throw new Error("truncated segment f32");
  const no_speech_prob = view.getFloat32(state.offset, true);
  state.offset += 4;
  return { text, t0_ms: t0, t1_ms: t1, no_speech_prob };
}

function encodeAudioFrame(samples) {
  const buf = [Tag.Audio];
  writeVecF32(buf, samples);
  return new Uint8Array(buf);
}

function encodeStart(lang, sampleRate) {
  const buf = [Tag.StartSession];
  writeOptionString(buf, lang);
  writeVarint(buf, sampleRate);
  return new Uint8Array(buf);
}

function encodeStop() {
  return new Uint8Array([Tag.StopSession]);
}

function encodeConfig(language, translate) {
  const buf = [Tag.Config];
  writeOptionString(buf, language);
  writeBool(buf, translate);
  return new Uint8Array(buf);
}

function decodePayload(tag, view, state) {
  switch (tag) {
    case Tag.FinalTranscript: {
      const text = readString(view, state);
      const segCount = readVarint(view, state);
      const segments = [];
      for (let i = 0; i < segCount; i++) segments.push(readSegment(view, state));
      const lang = readString(view, state);
      return { kind: "final", text, segments, lang };
    }
    case Tag.PartialTranscript: {
      // Server emitted an incremental, non-final transcript. Decode enough
      // to keep the cursor aligned but don't display it in the transcript
      // list (it will be replaced by the eventual FinalTranscript).
      const text = readString(view, state);
      readVarint(view, state); // t0_ms
      readVarint(view, state); // t1_ms
      const lang = readString(view, state);
      return { kind: "partial", text, lang };
    }
    case Tag.Error: {
      const code = readVarint(view, state);
      const message = readString(view, state);
      return { kind: "error", code, message };
    }
    case Tag.BackendInfo: {
      const model_id = readString(view, state);
      const gpu_backend = readString(view, state);
      return { kind: "backend", model_id, gpu_backend };
    }
    default: {
      // Unknown tag — surface a warning to the dev console but don't pollute
      // the transcript list with a noisy error line.
      console.warn(`ignoring unknown server tag 0x${tag.toString(16)}`);
      return { kind: "unknown", tag };
    }
  }
}

// ---- UI state ---------------------------------------------------------------

const state = {
  ws: null,
  vad: null,
  vadReady: false,
  mediaStream: null,
  transcript: [], // { text, lang, ts }
  recording: false,
};

// ---- Helpers ----------------------------------------------------------------

function setStatus(text, cls) {
  statusEl.textContent = text;
  statusEl.className = "status " + cls;
}

function appendLine(text, lang) {
  const empty = listEl.querySelector(".empty-state");
  if (empty) empty.remove();
  const li = document.createElement("li");
  const ts = new Date().toLocaleTimeString();
  if (lang) {
    const langSpan = document.createElement("span");
    langSpan.className = "lang";
    langSpan.textContent = `[${lang}]`;
    li.appendChild(langSpan);
  }
  const tsSpan = document.createElement("span");
  tsSpan.className = "ts";
  tsSpan.textContent = ts;
  li.appendChild(tsSpan);
  li.appendChild(document.createTextNode(text));
  listEl.appendChild(li);
  listEl.scrollTop = listEl.scrollHeight;
  downloadBtn.disabled = false;
  state.transcript.push({ text, lang, ts });
}

function emptyState() {
  if (state.transcript.length === 0) {
    const li = document.createElement("li");
    li.className = "empty-state";
    li.textContent = "No transcript yet. Click Record to start.";
    listEl.appendChild(li);
  }
}

emptyState();

// ---- Live voice graph -------------------------------------------------------
//
// `MicVAD` exposes an `onFrameProcessed(probs, frame)` hook that fires once
// per processed frame (~32 ms, 512 samples at 16 kHz). We use it to push
// the audio samples into a small ring buffer and redraw an oscilloscope on
// the next animation frame. Drawing is throttled to ~60 fps via rAF, so a
// 30 fps mic stream never queues more than one pending frame.
//
// The graph renders a symmetric waveform around a dim centerline. When the
// VAD speech probability crosses `positiveSpeechThreshold` the line flips
// from the accent blue to the "ok" green and the small level bar fills,
// so the user gets the same visual cue the server-side VAD is using.

const SCOPE_SAMPLES = 2048; // 128 ms at 16 kHz; ~4 VAD frames worth
const scopeRing = new Float32Array(SCOPE_SAMPLES);
let scopeWrite = 0;     // next write index into `scopeRing`
let scopeFilled = 0;    // how many samples have ever been pushed (<= SCOPE_SAMPLES)
let scopeLastProb = 0;  // last speech probability (0..1) for color/level
let scopeRafId = 0;     // pending rAF id, 0 = none
let scopeCssW = 0;      // last CSS width in px, used for resize detection

function pushScopeFrame(samples) {
  if (!samples || samples.length === 0) return;
  // Copy in two halves so we never split a sample across the wrap-around.
  let n = samples.length;
  for (let i = 0; i < n; i++) {
    scopeRing[scopeWrite] = samples[i];
    scopeWrite = (scopeWrite + 1) % SCOPE_SAMPLES;
  }
  scopeFilled = Math.min(SCOPE_SAMPLES, scopeFilled + n);
}

function resetScope() {
  scopeRing.fill(0);
  scopeWrite = 0;
  scopeFilled = 0;
  scopeLastProb = 0;
  if (scopeLevel) scopeLevel.dataset.speaking = "false";
  drawScope();
}

function resizeScopeIfNeeded() {
  const cssW = scopeCanvas.clientWidth;
  const cssH = scopeCanvas.clientHeight;
  if (cssW === scopeCssW && scopeCanvas.height !== 0) return;
  scopeCssW = cssW;
  const dpr = globalThis.devicePixelRatio || 1;
  scopeCanvas.width = Math.max(1, Math.floor(cssW * dpr));
  scopeCanvas.height = Math.max(1, Math.floor(cssH * dpr));
  scopeCtx.setTransform(dpr, 0, 0, dpr, 0, 0);
}

function drawScope() {
  scopeRafId = 0;
  resizeScopeIfNeeded();
  const cssW = scopeCanvas.clientWidth;
  const cssH = scopeCanvas.clientHeight;
  scopeCtx.clearRect(0, 0, cssW, cssH);

  const mid = cssH / 2;
  const accent = getCss("--accent");
  const ok = getCss("--ok");
  const border = getCss("--border");
  const speaking = scopeLastProb >= 0.5;
  const stroke = speaking ? ok : accent;

  // Centerline.
  scopeCtx.strokeStyle = border;
  scopeCtx.lineWidth = 1;
  scopeCtx.beginPath();
  scopeCtx.moveTo(0, mid);
  scopeCtx.lineTo(cssW, mid);
  scopeCtx.stroke();

  // Symmetric waveform: top half mirrors the bottom around `mid`.
  const n = Math.min(scopeFilled, SCOPE_SAMPLES);
  if (n > 0) {
    scopeCtx.strokeStyle = stroke;
    scopeCtx.lineWidth = 1.5;
    scopeCtx.beginPath();
    const step = cssW / Math.max(1, n - 1);
    for (let i = 0; i < n; i++) {
      // Read from ring buffer in chronological order: oldest sample is at
      // (scopeWrite - n) mod SCOPE_SAMPLES, newest is at scopeWrite - 1.
      const idx = (scopeWrite - n + i + SCOPE_SAMPLES) % SCOPE_SAMPLES;
      const s = scopeRing[idx];
      const y = mid - s * (mid - 1); // -1..1 maps to (mid-1)..(mid+1) ≈ full height
      const x = i * step;
      if (i === 0) scopeCtx.moveTo(x, y);
      else scopeCtx.lineTo(x, y);
    }
    scopeCtx.stroke();
  }

  // Level bar fill.
  if (scopeLevel) {
    const pct = Math.max(0, Math.min(1, scopeLastProb));
    scopeLevel.querySelector(".bar").style.width = `${(pct * 100).toFixed(1)}%`;
    scopeLevel.dataset.speaking = speaking ? "true" : "false";
  }
}

function scheduleScopeDraw() {
  if (scopeRafId !== 0) return;
  scopeRafId = requestAnimationFrame(drawScope);
}

function getCss(name) {
  return getComputedStyle(document.documentElement).getPropertyValue(name).trim() || "#58a6ff";
}

// Initial paint so the graph shows a flat line before recording starts.
resetScope();
window.addEventListener("resize", () => {
  scopeCssW = 0; // force resizeScopeIfNeeded to re-measure
  scheduleScopeDraw();
});

// ---- WebSocket lifecycle ----------------------------------------------------

function wsUrl() {
  const proto = location.protocol === "https:" ? "wss:" : "ws:";
  return `${proto}//${location.host}/ws`;
}

function connect() {
  return new Promise((resolve, reject) => {
    setStatus("connecting", "connecting");
    const ws = new WebSocket(wsUrl());
    ws.binaryType = "arraybuffer";
    ws.addEventListener("open", () => {
      ws.send(encodeStart(langSelect.value || null, 16000));
    });
    ws.addEventListener("message", (ev) => onWsMessage(ev.data, resolve));
    ws.addEventListener("close", () => onWsClose());
    ws.addEventListener("error", (ev) => reject(new Error("ws error")));
    state.ws = ws;
  });
}

function onWsMessage(data, onBackendInfo) {
  if (!(data instanceof ArrayBuffer)) return;
  const view = new DataView(data);
  const tag = view.getUint8(0);
  const payloadBuf = new Uint8Array(data, 1);
  const payloadView = new DataView(payloadBuf.buffer, payloadBuf.byteOffset, payloadBuf.byteLength);
  const s = { offset: 0 };
  try {
    const p = decodePayload(tag, payloadView, s);
    if (p.kind === "backend") {
      backendEl.textContent = `model=${p.model_id} backend=${p.gpu_backend}`;
      onBackendInfo();
    } else if (p.kind === "final") {
      if (p.text) appendLine(p.text, p.lang);
    } else if (p.kind === "partial") {
      // Discard: the eventual FinalTranscript supersedes it.
    } else if (p.kind === "error") {
      appendLine(`[error ${p.code}] ${p.message}`, "err");
    }
    // "unknown" was already logged via console.warn by decodePayload.
  } catch (e) {
    appendLine(`[client decode error] ${e.message}`, "err");
  }
}

async function onWsClose() {
  state.ws = null;
  if (!state.recording) {
    // Normal idle close (user clicked Stop, or initial connect failed).
    setStatus("idle", "idle");
    return;
  }
  // The server dropped the connection while we were recording. The VAD
  // would otherwise keep capturing audio and burning CPU on frames we
  // can no longer ship, and the Record button would stay stuck on "Stop"
  // even though no session is open. Treat any close mid-recording as a
  // stop: pause the VAD, clear the recording flag, and put the UI back
  // to a state where the user can re-click Record.
  state.recording = false;
  try {
    await state.vad?.pause();
  } catch {}
  setRecordState("Record", "idle");
  setStatus("disconnected", "error");
  resetScope();
}

function send(bytes) {
  if (state.ws && state.ws.readyState === WebSocket.OPEN) {
    state.ws.send(bytes);
  }
}

// ---- Microphone + VAD -------------------------------------------------------

async function ensureVad() {
  if (state.vadReady) return;
  setStatus("loading vad…", "connecting");

  // The VAD model, the AudioWorklet bundle, the ORT WASM blob, and the
  // ORT `.mjs` worker all need to live next to `bundle.min.js`. The
  // `.mjs` worker resolves its companion `.wasm` via `import.meta.url`
  // (its own directory), and the ORT runtime expects the two files to
  // be siblings. Everything is colocated under `/static/vendor/vad/`.
  const VAD_DIST = "/static/vendor/vad/";
  const ORT_DIST = "/static/vendor/vad/";

  state.vad = await MicVAD.new({
    model: "v6",
    baseAssetPath: VAD_DIST,
    onnxWASMBasePath: ORT_DIST,
    onFrameProcessed: (probs, frame) => {
      // Frame is a Float32Array of 16 kHz mono PCM samples (~512 per call).
      // `probs` is whatever the Silero model returned: a bare number in v6
      // and an `{ isSpeech }` object in earlier shapes, so accept both.
      const prob =
        typeof probs === "number"
          ? probs
          : (probs && (probs.isSpeech || probs.speechProb || 0)) || 0;
      scopeLastProb = prob;
      pushScopeFrame(frame);
      scheduleScopeDraw();
    },
    onSpeechStart: () => {
      // no-op: server only cares about completed segments
    },
    onSpeechEnd: (audioFloat32) => {
      // audioFloat32 is already 16 kHz mono PCM Float32 from the VAD.
      send(encodeAudioFrame(audioFloat32));
    },
    positiveSpeechThreshold: 0.6,
    negativeSpeechThreshold: 0.4,
    minSpeechFrames: 6,
    preSpeechPadFrames: 1,
    postSpeechPadFrames: 3,
    ortConfig: (ort) => {
      ort.env.logLevel = "error";
      // Force ORT onto the non-threaded factory so it does not try to
      // spawn the Emscripten pthread worker (whose URL would otherwise
      // resolve to a non-existent `.worker.js` / `.mjs`).
      ort.env.wasm.proxy = false;
      ort.env.wasm.numThreads = 1;
    },
  });
  state.vadReady = true;
}

// ---- Buttons ----------------------------------------------------------------
//
// `record-btn` is a single toggle. While idle, clicking it kicks off the
// VAD load (which may request microphone permission and download the
// Silero ONNX model on first use), opens the WebSocket, and starts the
// VAD. While recording, clicking it stops the VAD, sends StopSession to
// the server, closes the WS, and returns to idle.
//
// During the initial VAD load the button is disabled so a fast double
// click can't kick off two parallel initialisations.

function setRecordState(label, stateName) {
  recordBtn.textContent = label;
  recordBtn.dataset.state = stateName;
}

recordBtn.addEventListener("click", async () => {
  if (state.recording) {
    // Stop path.
    try {
      await state.vad?.pause();
    } catch {}
    try {
      send(encodeStop());
      state.ws?.close();
    } catch {}
    state.recording = false;
    setRecordState("Record", "idle");
    setStatus("idle", "idle");
    resetScope();
    return;
  }

  // Start path: disable the button until the VAD is ready so the user
  // can't double-click and queue two initialisations.
  recordBtn.disabled = true;
  setRecordState("Loading…", "loading");
  try {
    setStatus("requesting mic…", "connecting");
    await ensureVad();
    await connect();
    state.vad.start();
    state.recording = true;
    setRecordState("Stop", "recording");
    setStatus("recording", "recording");
  } catch (e) {
    setStatus(`error: ${e.message}`, "error");
    console.error(e);
    setRecordState("Record", "idle");
  } finally {
    recordBtn.disabled = false;
  }
});

langSelect.addEventListener("change", () => {
  send(encodeConfig(langSelect.value || null, translateCk.checked));
});

translateCk.addEventListener("change", () => {
  send(encodeConfig(langSelect.value || null, translateCk.checked));
});

downloadBtn.addEventListener("click", () => {
  const blob = new Blob(
    [state.transcript.map((l) => `${l.ts} [${l.lang || "-"}] ${l.text}`).join("\n")],
    { type: "text/plain" },
  );
  const url = URL.createObjectURL(blob);
  const a = document.createElement("a");
  a.href = url;
  a.download = "transcript.txt";
  a.click();
  setTimeout(() => URL.revokeObjectURL(url), 1000);
});

// No automatic reconnect: a dropped WS is treated as a stop (see
// `onWsClose`) and the user re-clicks Record to start a fresh session.
// This keeps the UI state unambiguous — there is no "reconnecting…"
// pseudo-state to confuse the Record/Stop button.