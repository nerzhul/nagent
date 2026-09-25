// nagent — shared audio capture pipeline.
//
// Both Transcript and Discussion modes use the same audio stack:
//   MicVAD (on-device Silero) → WebSocket(/ws) → server-side Whisper.
//
// `AudioCapture` wraps the entire pipeline and exposes a small
// `start()` / `stop()` surface so each mode can wire its own UI on
// top. The VAD *instance* is shared (only one microphone stream can
// be open at a time anyway); the WebSocket and recording state are
// per-instance, so the two modes have fully independent sessions.
//
// Auto-stop on view hide: if `containerEl` becomes `hidden` (e.g. the
// user switches modes), the capture pauses itself. This is per-mode
// cleanup with zero cross-module coordination.
//
// Auto-stop on silence: while recording, a watchdog fires
// `_stop()` after `INACTIVITY_TIMEOUT_MS` of frames where the VAD
// reports no voice activity (prob below `INACTIVITY_SPEECH_THRESHOLD`).
// The watchdog is gated on an optional checkbox so the user can opt
// out for long dictation sessions.

const { MicVAD } = globalThis.vad;

const INACTIVITY_TIMEOUT_MS = 5_000;
const INACTIVITY_SPEECH_THRESHOLD = 0.4; // matches negativeSpeechThreshold
const INACTIVITY_TICK_MS = 1_000;

// ---- Shared VAD singleton ---------------------------------------------------
//
// The Silero model is a ~3 MB download; we do not want to fetch it
// twice when the user toggles between Transcript and Discussion.
// `ensureVad` is idempotent: the second caller gets the same instance.
let sharedVad = null;
let sharedVadPromise = null;

function ensureVad(opts) {
  if (sharedVad) return Promise.resolve(sharedVad);
  if (sharedVadPromise) return sharedVadPromise;
  sharedVadPromise = MicVAD.new(opts).then((v) => {
    sharedVad = v;
    sharedVadPromise = null;
    return v;
  });
  return sharedVadPromise;
}

// ---- Wire protocol (must match stt-proto) ----------------------------------
//
// Mirrored from the original `app.js` codec so the JS side stays
// self-contained (no transitive `postcard` package is available for
// the browser).

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

function writeVarint(out, n) {
  while (n >= 0x80) {
    out.push((n & 0x7f) | 0x80);
    n = Math.floor(n / 2 ** 7);
  }
  out.push(n & 0x7f);
}

function readVarint(view, state) {
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
  state.offset = end;
  return new TextDecoder("utf-8").decode(new Uint8Array(view.buffer, start, len));
}

function writeBool(out, v) { out.push(v ? 1 : 0); }
function readBool(view, state) { return view.getUint8(state.offset++) !== 0; }

function writeOptionString(out, opt) {
  if (opt == null) { out.push(0); return; }
  out.push(1);
  writeString(out, opt);
}

function readOptionString(view, state) {
  return readBool(view, state) ? readString(view, state) : null;
}

function writeVecF32(out, arr) {
  writeVarint(out, arr.length);
  const view = new DataView(new ArrayBuffer(arr.length * 4));
  for (let i = 0; i < arr.length; i++) view.setFloat32(i * 4, arr[i], true);
  for (let i = 0; i < arr.length * 4; i++) out.push(view.getUint8(i));
}

function writeSegment(out, seg) {
  writeString(out, seg.text);
  writeVarint(out, seg.t0_ms);
  writeVarint(out, seg.t1_ms);
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

function encodeAudioFrame(samples) {
  const buf = [Tag.Audio];
  writeVecF32(buf, samples);
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
      const text = readString(view, state);
      readVarint(view, state);
      readVarint(view, state);
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
    default:
      return { kind: "unknown", tag };
  }
}

// ---- Voice graph -----------------------------------------------------------
//
// Inline oscilloscope + level bar. Used by both modes; the canvas and
// level elements are passed in via the `AudioCapture` config.

const SCOPE_SAMPLES = 2048; // ~128 ms at 16 kHz, ~4 VAD frames

function createScope(canvasEl, levelEl) {
  const ring = new Float32Array(SCOPE_SAMPLES);
  let write = 0;
  let filled = 0;
  let lastProb = 0;
  let rafId = 0;
  // Auto-gain state. The VAD hands us PCM samples in [-1, 1], but
  // real-world speech rarely reaches ±0.5 — a raw `sample * mid`
  // scaling leaves the waveform as a thin sliver around the centre
  // line. We track the recent peak and scale so the loudest sample
  // fills ~85% of the half-height, which makes both quiet and loud
  // audio readable. The gain is smoothed frame-to-frame so the
  // display doesn't pump on every transient.
  let gain = 1;
  let peakEnv = 0;
  const PEAK_ATTACK = 0.4;  // fast rise on loud transients
  const PEAK_DECAY  = 0.05; // slow fall so quiet moments stay readable
  const MAX_GAIN    = 24;   // cap amplification so noise floor doesn't dominate
  const TARGET_FILL = 0.76; // fraction of half-height the peak should reach
  // Cached CSS box dimensions used to detect size changes. Both start
  // at 0 so the very first draw always resizes. We deliberately keep
  // `cssW`/`cssH` valid across hide/show cycles (see `ensureReady`)
  // so the bitmap doesn't snap to 1x1 while the section is collapsed
  // by `.is-hidden { max-height: 0 }`.
  let cssW = 0;
  let cssH = 0;
  let ctx = null;

  function push(frame) {
    if (!frame || frame.length === 0) return;
    for (let i = 0; i < frame.length; i++) {
      ring[write] = frame[i];
      write = (write + 1) % SCOPE_SAMPLES;
    }
    filled = Math.min(SCOPE_SAMPLES, filled + frame.length);
  }

  function reset() {
    ring.fill(0);
    write = 0;
    filled = 0;
    lastProb = 0;
    // Reset the auto-gain so a fresh capture doesn't inherit the
    // previous session's gain envelope.
    gain = 1;
    peakEnv = 0;
    if (levelEl) levelEl.dataset.speaking = "false";
    draw();
  }

  function resizeIfNeeded() {
    if (!canvasEl) return;
    const cssWNow = canvasEl.clientWidth;
    const cssHNow = canvasEl.clientHeight;
    // Skip the resize while the section is collapsed (`.is-hidden`
    // sets `max-height: 0`, so the CSS box collapses to zero).
    // Resizing then would lock the bitmap at 1x1 and stretch it
    // back to a blurry mess on the next show. The cached values
    // stay intact until `ensureReady()` re-invalidates them.
    if (cssWNow === 0 || cssHNow === 0) return;
    if (cssWNow === cssW && cssHNow === cssH && canvasEl.height !== 0) return;
    cssW = cssWNow;
    cssH = cssHNow;
    const dpr = globalThis.devicePixelRatio || 1;
    canvasEl.width = Math.max(1, Math.floor(cssW * dpr));
    canvasEl.height = Math.max(1, Math.floor(cssH * dpr));
    if (ctx) ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
  }

  function draw() {
    rafId = 0;
    if (!canvasEl) return;
    resizeIfNeeded();
    const cssH = canvasEl.clientHeight;
    const cssWNow = canvasEl.clientWidth;
    if (!ctx) ctx = canvasEl.getContext("2d");
    ctx.clearRect(0, 0, cssWNow, cssH);

    const mid = cssH / 2;
    const accent = getCss("--accent");
    const ok = getCss("--ok");
    const border = getCss("--border");
    const speaking = lastProb >= 0.5;
    const stroke = speaking ? ok : accent;

    ctx.strokeStyle = border;
    ctx.lineWidth = 1;
    ctx.beginPath();
    ctx.moveTo(0, mid);
    ctx.lineTo(cssWNow, mid);
    ctx.stroke();

    const n = Math.min(filled, SCOPE_SAMPLES);
    if (n > 0) {
      // Find the peak amplitude in the current buffer so we can
      // scale the display. This is the input to the gain envelope
      // — we update the envelope once per draw (not per sample)
      // because scanning the ring is cheap but doing it per pixel
      // would be wasteful.
      let peak = 0;
      for (let i = 0; i < n; i++) {
        const idx = (write - n + i + SCOPE_SAMPLES) % SCOPE_SAMPLES;
        const a = ring[idx] < 0 ? -ring[idx] : ring[idx];
        if (a > peak) peak = a;
      }
      // Asymmetric envelope: fast attack so loud transients show up
      // immediately, slow decay so quiet moments stay readable. The
      // gain itself is clamped so the noise floor doesn't get
      // amplified into a full-height fuzz when the user goes silent.
      if (peak > peakEnv) {
        peakEnv = peakEnv + (peak - peakEnv) * PEAK_ATTACK;
      } else {
        peakEnv = peakEnv + (peak - peakEnv) * PEAK_DECAY;
      }
      if (peakEnv > 0.005) {
        const target = Math.min(MAX_GAIN, (mid * TARGET_FILL) / peakEnv);
        gain = gain + (target - gain) * 0.2;
      }

      ctx.strokeStyle = stroke;
      ctx.lineWidth = 1.5;
      ctx.beginPath();
      const step = cssWNow / Math.max(1, n - 1);
      for (let i = 0; i < n; i++) {
        const idx = (write - n + i + SCOPE_SAMPLES) % SCOPE_SAMPLES;
        const y = mid - ring[idx] * (mid - 2) * gain;
        const x = i * step;
        if (i === 0) ctx.moveTo(x, y);
        else ctx.lineTo(x, y);
      }
      ctx.stroke();
    }

    if (levelEl) {
      const pct = Math.max(0, Math.min(1, lastProb));
      const bar = levelEl.querySelector(".bar");
      if (bar) bar.style.width = `${(pct * 100).toFixed(1)}%`;
      levelEl.dataset.speaking = speaking ? "true" : "false";
    }
  }

  function scheduleDraw() {
    if (rafId !== 0) return;
    rafId = requestAnimationFrame(draw);
  }

  function setProb(p) { lastProb = p; }

  // Called when the voice-graph section becomes visible again after a
  // hide transition. The CSS box is briefly at zero size during the
  // transition, which can leave the bitmap at a stale (potentially
  // 1x1) size. Invalidating the cached dimensions forces the next
  // `draw()` to reflow the bitmap to the current CSS box.
  function ensureReady() {
    cssW = 0;
    cssH = 0;
    draw();
  }

  // Initial paint so the graph shows a flat line before any audio arrives.
  if (ctx === null && canvasEl) ctx = canvasEl.getContext("2d");
  reset();

  return { push, reset, scheduleDraw, setProb, ensureReady };
}

function getCss(name) {
  return getComputedStyle(document.documentElement).getPropertyValue(name).trim() || "#58a6ff";
}

// ---- AudioCapture ----------------------------------------------------------

export class AudioCapture {
  /**
   * @param {Object} cfg
   * @param {HTMLElement} cfg.buttonEl       Record/Stop button.
   * @param {HTMLElement} cfg.statusEl       Status pill (.status).
   * @param {HTMLElement} [cfg.canvasEl]     Voice-graph canvas.
   * @param {HTMLElement} [cfg.levelEl]      Speech-probability level bar.
   * @param {HTMLElement} cfg.containerEl    Parent view; capture auto-stops
   *                                         when this element becomes
   *                                         `hidden`.
   * @param {HTMLElement} [cfg.graphEl]      Voice-graph section element.
   *                                         The capture toggles
   *                                         `.is-hidden` on it based on
   *                                         recording state — visible
   *                                         while idle/connecting/etc.,
   *                                         hidden once a session is
   *                                         active (and back to visible
   *                                         on stop). Pair with a CSS
   *                                         transition on the same
   *                                         class.
   * @param {(text:string, cls:string) => void} [cfg.onStatusChange]
   *                                        Optional callback for status
   *                                        changes. Fires alongside
   *                                        `statusEl` updates so callers
   *                                        can compose multiple sources
   *                                        (e.g. audio + chat streaming).
   * @param {HTMLSelectElement} [cfg.langSelectEl]  Optional language picker.
   * @param {HTMLInputElement} [cfg.translateCheckEl]  Optional "translate to English".
   * @param {HTMLInputElement} [cfg.inactivityCheckEl]  Optional "auto-stop on silence"
   *                                                     toggle. When present, the
   *                                                     inactivity watchdog only
   *                                                     fires while the checkbox
   *                                                     is checked. When absent,
   *                                                     the watchdog is always on.
   * @param {HTMLElement} [cfg.backendInfoEl]  Optional element to render
   *                                           `model=… backend=…` into.
   * @param {(text:string, lang:string, latencyMs:number|undefined) => void} cfg.onFinalTranscript
   * @param {(text:string, lang:string) => void} [cfg.onPartialTranscript]
   * @param {(modelId:string, gpuBackend:string) => void} [cfg.onBackendInfo]
   * @param {(code:number, message:string) => void} [cfg.onError]
   */
  constructor(cfg) {
    this.cfg = cfg;
    this.ws = null;
    this.recording = false;
    this.vadReady = false;
    this.scope = createScope(cfg.canvasEl, cfg.levelEl);
    this.pendingAudioTs = [];
    this.hiddenObserver = null;
    // `_aborted` is set by `_stop()` (and hence by the container-hide
    // observer below) to tell `_onButtonClick` to bail out of its
    // `await` chain instead of e.g. starting the VAD on a view the
    // user has just left. It is reset at the top of each click.
    this._aborted = false;

    // Inactivity watchdog state. `lastSpeechAt` is bumped from the VAD
    // frame callback whenever the speech probability clears the
    // threshold; the watchdog tick compares it against the wall clock
    // and calls `_stop()` if no voice was detected for the configured
    // window. The timer always runs while recording so toggling the
    // opt-out checkbox at runtime takes effect immediately.
    //
    // `inactivityPaused` is an external gate (e.g. set while a slow
    // LLM reply is in flight) and `pendingAudioTs.length` doubles as
    // a built-in gate: while STT frames are still in the server
    // pipeline we must not tear down the session, otherwise the user
    // never receives the FinalTranscript.
    this.inactivityCheckEl = cfg.inactivityCheckEl || null;
    this.inactivityPaused = false;
    this.lastSpeechAt = 0;
    this.inactivityTimerId = 0;

    // Voice-graph visibility: hidden until the user is recording.
    // The CSS owns the transition (opacity + max-height); we just flip
    // the `is-hidden` class on each state transition.
    this._setGraphVisible(false);

    // Set up the button + container observers.
    cfg.buttonEl.addEventListener("click", () => this._onButtonClick());
    if (cfg.langSelectEl) {
      cfg.langSelectEl.addEventListener("change", () => this._sendConfig());
    }
    if (cfg.translateCheckEl) {
      cfg.translateCheckEl.addEventListener("change", () => this._sendConfig());
    }
    if (cfg.containerEl) {
      this.hiddenObserver = new MutationObserver(() => {
        // Always tear down on hide, even when we weren't recording
        // yet (e.g. the user clicked Record and immediately switched
        // tabs while the VAD was still loading). `_stop` is
        // idempotent, so calling it on an already-idle capture is a
        // harmless no-op that still resets UI state to a clean idle.
        if (cfg.containerEl.hidden) this._stop();
      });
      this.hiddenObserver.observe(cfg.containerEl, {
        attributes: true,
        attributeFilter: ["hidden"],
      });
    }

    // Initial paint of the scope.
    this.scope.reset();

    // Re-render on resize.
    if (cfg.canvasEl) {
      globalThis.addEventListener("resize", () => this.scope.scheduleDraw());
    }
  }

  _setGraphVisible(visible) {
    const el = this.cfg.graphEl;
    if (!el) return;
    el.classList.toggle("is-hidden", !visible);
    if (visible) {
      // The CSS box just grew back from the collapsed (max-height: 0)
      // state. Invalidate the scope's cached CSS dimensions so the
      // next `draw()` reflows the bitmap to the real size, instead of
      // displaying a stale 1x1 buffer stretched across the canvas.
      this.scope.ensureReady?.();
    }
  }

  // ---- Inactivity watchdog --------------------------------------------------
  //
  // The watchdog runs once per `INACTIVITY_TICK_MS` while recording.
  // Each tick decides: (a) has the silence window elapsed? and (b) is
  // the opt-out checkbox either missing or checked? If both, stop and
  // surface a distinct status. Otherwise reschedule.
  //
  // We deliberately keep the timer alive when the checkbox is off so
  // that toggling it on mid-session is honored on the next tick
  // without having to restart the capture.

  _isInactivityEnabled() {
    return !this.inactivityCheckEl || !!this.inactivityCheckEl.checked;
  }

  _scheduleInactivityCheck() {
    if (this.inactivityTimerId !== 0) return;
    this.inactivityTimerId = setTimeout(
      () => this._onInactivityTick(),
      INACTIVITY_TICK_MS,
    );
  }

  _cancelInactivityCheck() {
    if (this.inactivityTimerId !== 0) {
      clearTimeout(this.inactivityTimerId);
      this.inactivityTimerId = 0;
    }
  }

  _onInactivityTick() {
    this.inactivityTimerId = 0;
    if (!this.recording) return;
    const silentForMs = performance.now() - this.lastSpeechAt;
    // Skip the stop when anything is still in flight: the external
    // pause flag covers LLM replies, and `pendingAudioTs` covers STT
    // frames that have been sent but not yet acknowledged with a
    // FinalTranscript. Both conditions would otherwise close the
    // session while the user is still waiting on a response.
    if (this._isInactivityEnabled()
        && !this.inactivityPaused
        && this.pendingAudioTs.length === 0
        && silentForMs >= INACTIVITY_TIMEOUT_MS) {
      this._stop();
      // Override the "idle" status that `_stop` just wrote so the
      // user can tell the watchdog fired rather than a manual stop.
      this.setStatus("auto-stopped (no speech)", "idle");
      return;
    }
    this._scheduleInactivityCheck();
  }

  /**
   * Pause or resume the inactivity watchdog without touching the
   * recording state. Callers that have downstream work in flight
   * (e.g. a slow LLM streaming reply, where the user is silent and
   * the watchdog would otherwise close the session mid-response) can
   * hold the pipeline open until they're done. Re-entrant; safe to
   * call before recording starts or after it stops.
   */
  setInactivityPaused(paused) {
    this.inactivityPaused = !!paused;
  }

  setButtonLabel(label, dataState) {
    this.cfg.buttonEl.dataset.state = dataState;
    // Only write `textContent` when the button has no child elements
    // (Transcript mode's plain text button). The Discussion-mode
    // record button carries inline SVG icons that swap visibility
    // via CSS based on `data-state`; writing `textContent` would
    // wipe the icons out on the first state transition and leave a
    // bare "Stop" label with no icon. `childElementCount` is stable
    // across state changes — the button either ships with icons in
    // the HTML (chat mode) or ships plain (transcript mode), and
    // we never add children at runtime.
    if (this.cfg.buttonEl.childElementCount === 0) {
      this.cfg.buttonEl.textContent = label;
    }
  }

  setStatus(text, cls) {
    if (this.cfg.statusEl) {
      this.cfg.statusEl.textContent = text;
      this.cfg.statusEl.className = "status " + cls;
    }
    this.cfg.onStatusChange?.(text, cls);
  }

  async _onButtonClick() {
    if (this.recording) {
      this._stop();
      return;
    }
    // Fresh attempt — clear any abort flag left by a previous
    // container-hide so the awaits below can run to completion.
    this._aborted = false;
    this.cfg.buttonEl.disabled = true;
    this.setButtonLabel("Loading…", "loading");
    this._setGraphVisible(true);
    try {
      this.setStatus("Loading speech model…", "loading");
      await this._ensureVad();
      // The user may have switched tabs while the VAD was loading;
      // bail out before we start the audio pipeline on a hidden view.
      if (this._aborted) return;
      await this._connect();
      if (this._aborted) return;
      sharedVad.start();
      this.recording = true;
      // Seed the inactivity timer with `now` so a brand-new session
      // gets the full timeout window before the watchdog fires.
      this.lastSpeechAt = performance.now();
      this._scheduleInactivityCheck();
      this.setButtonLabel("Stop", "recording");
      this.setStatus("recording", "recording");
    } catch (e) {
      // If we were aborted (e.g. the container was hidden), `_stop`
      // already put the UI back into a clean idle state — don't
      // overwrite that with an error and a hidden graph flip.
      if (this._aborted) return;
      this.setStatus(`error: ${e?.message || e}`, "error");
      console.error(e);
      this.setButtonLabel("Record", "idle");
      this._setGraphVisible(false);
    } finally {
      this.cfg.buttonEl.disabled = false;
    }
  }

  _stop() {
    // Flag any in-flight `_onButtonClick` setup so it bails out before
    // starting the VAD or opening a WebSocket on a view the user has
    // just left. Reset at the top of the next click attempt.
    this._aborted = true;
    this._cancelInactivityCheck();
    try { sharedVad?.pause(); } catch {}
    try {
      if (this.ws && this.ws.readyState === WebSocket.OPEN) {
        this.ws.send(encodeStop());
      }
    } catch {}
    try { this.ws?.close(); } catch {}
    this.recording = false;
    this.pendingAudioTs.length = 0;
    this.setButtonLabel("Record", "idle");
    this.setStatus("idle", "idle");
    this.scope.reset();
    this._setGraphVisible(false);
  }

  /**
   * Public, idempotent stop. Callers (e.g. the Discussion-mode chat)
   * use this to tear down the audio session as soon as a transcript
   * has been routed into a request, since keeping the mic open while
   * the LLM responds is wasteful. Safe to call when already idle.
   */
  stop() {
    this._stop();
  }

  /**
   * Public toggle. Starts recording when idle, stops when active.
   * Mirrors clicking the record button so external triggers (keyboard
   * shortcuts, programmatic activation) go through the same
   * validation / state-machine as a user click — in particular the
   * container-hidden guard and the `_aborted` cleanup at the top of
   * a fresh attempt.
   */
  async toggle() {
    if (this.recording) {
      this._stop();
      return;
    }
    await this._onButtonClick();
  }

  /**
   * Whether the capture is currently recording. Used by the chat UI
   * to decide whether a keyboard shortcut should start or stop.
   */
  isRecording() {
    return this.recording;
  }

  async _ensureVad() {
    if (this.vadReady) return;
    const VAD_DIST = "/static/vendor/vad/";
    const ORT_DIST = "/static/vendor/vad/";
    await ensureVad({
      model: "v6",
      baseAssetPath: VAD_DIST,
      onnxWASMBasePath: ORT_DIST,
      onFrameProcessed: (probs, frame) => {
        const prob =
          typeof probs === "number"
            ? probs
            : (probs && (probs.isSpeech || probs.speechProb || 0)) || 0;
        this.scope.setProb(prob);
        this.scope.push(frame);
        this.scope.scheduleDraw();
        // Reset the inactivity watchdog on any frame that crosses the
        // speech-probability threshold. Using the lower
        // negativeSpeechThreshold keeps short vocalizations from being
        // swallowed by `minSpeechFrames` debouncing.
        if (this.recording && prob >= INACTIVITY_SPEECH_THRESHOLD) {
          this.lastSpeechAt = performance.now();
        }
      },
      onSpeechStart: () => {},
      onSpeechEnd: (audioFloat32) => {
        this.pendingAudioTs.push(performance.now());
        this._sendFrame(encodeAudioFrame(audioFloat32));
      },
      positiveSpeechThreshold: 0.6,
      negativeSpeechThreshold: 0.4,
      minSpeechFrames: 6,
      preSpeechPadFrames: 1,
      postSpeechPadFrames: 3,
      ortConfig: (ort) => {
        ort.env.logLevel = "error";
        ort.env.wasm.proxy = false;
        ort.env.wasm.numThreads = 1;
      },
    });
    this.vadReady = true;
  }

  _wsUrl() {
    const proto = location.protocol === "https:" ? "wss:" : "ws:";
    return `${proto}//${location.host}/ws`;
  }

  async _connect() {
    return new Promise((resolve, reject) => {
      this.setStatus("connecting", "connecting");
      const ws = new WebSocket(this._wsUrl());
      ws.binaryType = "arraybuffer";
      ws.addEventListener("open", () => {
        ws.send(encodeStart(this._currentLang(), 16000));
      });
      ws.addEventListener("message", (ev) => this._onWsMessage(ev.data, resolve));
      ws.addEventListener("close", () => this._onWsClose());
      ws.addEventListener("error", () => reject(new Error("ws error")));
      this.ws = ws;
    });
  }

  _sendFrame(bytes) {
    if (this.ws && this.ws.readyState === WebSocket.OPEN) {
      this.ws.send(bytes);
    }
  }

  _sendConfig() {
    if (this.ws && this.ws.readyState === WebSocket.OPEN) {
      this.ws.send(encodeConfig(this._currentLang(), this._currentTranslate()));
    }
  }

  _currentLang() {
    if (!this.cfg.langSelectEl) return null;
    return this.cfg.langSelectEl.value || null;
  }

  _currentTranslate() {
    return !!this.cfg.translateCheckEl?.checked;
  }

  _onWsMessage(data, onBackendInfo) {
    if (!(data instanceof ArrayBuffer)) return;
    const view = new DataView(data);
    const tag = view.getUint8(0);
    const payloadView = new DataView(
      data, 1, data.byteLength - 1,
    );
    const s = { offset: 0 };
    let p;
    try {
      p = decodePayload(tag, payloadView, s);
    } catch (e) {
      this.cfg.onError?.(0, `client decode error: ${e.message}`);
      return;
    }
    if (p.kind === "backend") {
      if (this.cfg.backendInfoEl) {
        this.cfg.backendInfoEl.textContent = `model=${p.model_id} backend=${p.gpu_backend}`;
      }
      this.cfg.onBackendInfo?.(p.model_id, p.gpu_backend);
      onBackendInfo?.();
    } else if (p.kind === "final") {
      const sentAt = this.pendingAudioTs.shift();
      const latencyMs = typeof sentAt === "number" ? performance.now() - sentAt : undefined;
      this.cfg.onFinalTranscript?.(p.text, p.lang, latencyMs);
    } else if (p.kind === "partial") {
      this.cfg.onPartialTranscript?.(p.text, p.lang);
    } else if (p.kind === "error") {
      this.cfg.onError?.(p.code, p.message);
    }
    // "unknown" is intentionally discarded; see the original app.js.
  }

  _onWsClose() {
    this.ws = null;
    this.pendingAudioTs.length = 0;
    if (!this.recording) {
      this.setStatus("idle", "idle");
      this._setGraphVisible(false);
      return;
    }
    // Mid-recording drop: treat as a stop, same policy as the original
    // app.js — reset UI so the user can re-click Record.
    this.recording = false;
    try { sharedVad?.pause(); } catch {}
    this.setButtonLabel("Record", "idle");
    this.setStatus("disconnected", "error");
    this.scope.reset();
    this._setGraphVisible(false);
  }
}
