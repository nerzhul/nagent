// Local TTS player for the discussion view.
//
// Loaded as an ES module by `chat.js` via `import { NagentTts } from
// '/static/tts.js'`. Each phrase boundary triggers a
// `fetch('/v1/audio/speech', { method:'POST', body: JSON.stringify({input, voice, speed}) })`
// round-trip; the returned WAV bytes are decoded into an AudioBuffer
// and queued on a single Web Audio context. Chunks play gapless
// because each `AudioBufferSourceNode.start(t)` is given an absolute
// timestamp = `max(currentTime, prevEndTime)`.
//
// Autoplay policy: the AudioContext is created lazily on the first
// call to `ensureContext()`, which must be triggered from a
// user-gesture handler (the `#chat-tts-check` toggle or the
// `#chat-tts-test` button). Once unlocked it stays unlocked for the
// rest of the session.
//
// No external dependencies — `decodeWav` parses a standard RIFF /
// PCM-16 mono header directly so we don't pull a WAV library just
// for one call site.

'use strict';

// Maximum length of the sentence buffer before we force a flush,
// so an LLM that streams a 50 KB comma-separated clause does not
  // hold it forever.
  var MAX_BUFFER_CHARS = 500;

  // Regex matching sentence terminators followed by whitespace, or
  // paragraph breaks. Captures the terminator so the split keeps it
  // attached to the previous sentence.
  var SPLIT_REGEX = /([.!?]+[\s\u00a0]+|\n{2,})/;

  /**
   * Decode a 16-bit PCM mono WAV blob into an `AudioBuffer`.
   *
   * We only support the format Piper writes (RIFF / WAVE, fmt chunk
   * with `format=1` PCM, 1 channel, 16-bit, sample rate declared in
   * the fmt chunk). Any other shape raises — we never want to feed
   * garbage into the AudioContext.
   *
   * @param {AudioContext} audioCtx
   * @param {ArrayBuffer} arrayBuffer
   * @returns {Promise<AudioBuffer>}
   */
  function decodeWav(audioCtx, arrayBuffer) {
    var view = new DataView(arrayBuffer);
    // RIFF header: "RIFF" <size:u32> "WAVE"
    if (view.getUint32(0, true) !== 0x46464952 /* "RIFF" LE */) {
      throw new Error('decodeWav: missing RIFF magic');
    }
    if (view.getUint32(8, true) !== 0x45564157 /* "WAVE" LE */) {
      throw new Error('decodeWav: missing WAVE magic');
    }
    // Walk chunks until we find `fmt ` and `data`.
    var offset = 12;
    var fmt = null;
    var dataOffset = -1;
    var dataLen = 0;
    while (offset + 8 <= view.byteLength) {
      var chunkId = view.getUint32(offset, true);
      var chunkSize = view.getUint32(offset + 4, true);
      var chunkBody = offset + 8;
      if (chunkId === 0x20746d66 /* "fmt " LE */) {
        fmt = {
          format: view.getUint16(chunkBody, true),
          channels: view.getUint16(chunkBody + 2, true),
          sampleRate: view.getUint32(chunkBody + 4, true),
          bitsPerSample: view.getUint16(chunkBody + 14, true),
        };
      } else if (chunkId === 0x61746164 /* "data" LE */) {
        dataOffset = chunkBody;
        dataLen = chunkSize;
        break;
      }
      // Chunks are word-aligned.
      offset = chunkBody + chunkSize + (chunkSize & 1);
    }
    if (!fmt) throw new Error('decodeWav: missing fmt chunk');
    if (dataOffset < 0) throw new Error('decodeWav: missing data chunk');
    if (fmt.format !== 1) {
      throw new Error('decodeWav: only PCM (format=1) supported, got ' + fmt.format);
    }
    if (fmt.channels !== 1) {
      throw new Error('decodeWav: only mono supported, got ' + fmt.channels);
    }
    if (fmt.bitsPerSample !== 16) {
      throw new Error('decodeWav: only 16-bit supported, got ' + fmt.bitsPerSample);
    }
    var n = dataLen / 2;
    var buf = audioCtx.createBuffer(1, n, fmt.sampleRate);
    var channel = buf.getChannelData(0);
    for (var i = 0; i < n; i++) {
      var sample = view.getInt16(dataOffset + i * 2, true);
      channel[i] = sample / 32768;
    }
    return Promise.resolve(buf);
  }

  /**
   * Split `buffer` on the first sentence terminator. Returns
   * `{ rest, phrase }`: `phrase` is the complete sentence (suitable
   * to send for synthesis) and `rest` is whatever trailing characters
   * should stay in the buffer for the next call.
   *
   * If no terminator exists, `phrase` is `null` and `rest` is the
   * whole buffer (capped at `MAX_BUFFER_CHARS`, after which the
   * buffer is force-emitted regardless).
   */
  function splitSentence(buffer) {
    if (!buffer) return { rest: '', phrase: null };
    var m = SPLIT_REGEX.exec(buffer);
    if (m) {
      var idx = m.index + m[0].length;
      return {
        phrase: buffer.slice(0, idx),
        rest: buffer.slice(idx),
      };
    }
    if (buffer.length >= MAX_BUFFER_CHARS) {
      // No terminator for a while — flush the whole buffer to avoid
      // unbounded growth. The browser will say it slightly awkwardly
      // (mid-clause) but never loses words.
      return { phrase: buffer, rest: '' };
    }
    return { rest: buffer, phrase: null };
  }

  function TtsPlayer(settings) {
    this._settings = settings; // { endpoint, voice, speed }
    this._buffer = '';
    this._inflight = null; // AbortController for the current fetch
    this._audioCtx = null;
    this._scheduledEnd = 0; // last scheduled source.endTime on the ctx
    this._activeSource = null;
    this._playPromise = null;
    this._stopped = false;
  }

  TtsPlayer.prototype._ensureContext = function () {
    if (this._audioCtx) return this._audioCtx;
    // `AudioContext` may be undefined on very old browsers; we
    // surface a clear error in that case instead of letting the
    // silence confuse the user.
    if (typeof AudioContext === 'undefined') {
      throw new Error('Web Audio API is not available in this browser');
    }
    this._audioCtx = new AudioContext();
    return this._audioCtx;
  };

  TtsPlayer.prototype._voiceFor = function () {
    // Per-call resolution so chat.js can change the voice / lang
    // without us caching a stale value. Cheap: a closure read.
    return this._settings.voiceResolver
      ? this._settings.voiceResolver()
      : this._settings.voice;
  };

  TtsPlayer.prototype.feed = function (delta) {
    if (!delta) return;
    this._buffer += delta;
    var split;
    while ((split = splitSentence(this._buffer)).phrase) {
      this._buffer = split.rest;
      this._enqueue(split.phrase);
    }
  };

  /**
   * Force-emit whatever is currently buffered (called on `[DONE]`).
   * Trims trailing whitespace to avoid synthesising an empty chunk.
   */
  TtsPlayer.prototype.flush = function () {
    var tail = this._buffer.trim();
    this._buffer = '';
    if (tail) this._enqueue(tail);
  };

  TtsPlayer.prototype._enqueue = function (phrase) {
    if (this._stopped) return;
    var self = this;
    // Cancel any in-flight fetch for a previous phrase (only one is
    // allowed at a time so we don't race the server).
    if (this._inflight) {
      try {
        this._inflight.abort();
      } catch (_) {}
      this._inflight = null;
    }
    var ctrl = new AbortController();
    this._inflight = ctrl;
    var body = {
      input: phrase,
      voice: this._voiceFor(),
      speed: this._settings.speed,
    };
    fetch(this._settings.endpoint, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(body),
      signal: ctrl.signal,
    })
      .then(function (resp) {
        if (!resp.ok) {
          // 4xx/5xx → log and drop the chunk; the next sentence may
          // succeed (e.g. transient 503 on a slow first inference).
          console.warn('TTS HTTP', resp.status, resp.statusText);
          return null;
        }
        return resp.arrayBuffer();
      })
      .then(function (buf) {
        if (!buf) return;
        return decodeWav(self._ensureContext(), buf);
      })
      .then(function (audioBuf) {
        if (!audioBuf || self._stopped) return;
        self._schedule(audioBuf);
      })
      .catch(function (err) {
        if (err && err.name === 'AbortError') return;
        console.warn('TTS fetch failed:', err);
      })
      .finally(function () {
        if (self._inflight === ctrl) self._inflight = null;
      });
  };

  TtsPlayer.prototype._schedule = function (audioBuf) {
    var ctx = this._audioCtx;
    if (!ctx) return;
    var src = ctx.createBufferSource();
    src.buffer = audioBuf;
    src.connect(ctx.destination);
    // Gapless playback: each chunk starts at the later of "now" or
    // "right after the previous chunk". `currentTime` is in seconds
    // since the context was created; both operands share the same
    // clock.
    var playAt = Math.max(ctx.currentTime, this._scheduledEnd);
    src.start(playAt);
    this._scheduledEnd = playAt + audioBuf.duration;
    // Keep a reference so `stopAll` can stop() the live source even
    // mid-chunk. (The next scheduled chunk's start time is already
    // in the past after a stop, but we cancel the in-flight fetch in
    // `stopAll` so it never reaches `_schedule` again.)
    if (this._activeSource) {
      // Don't double-stop; `stop` on a finished source is a no-op
      // anyway, but skipping saves a microtask.
      try {
        this._activeSource.stop();
      } catch (_) {}
    }
    this._activeSource = src;
    src.onended = function () {
      // Only clear if this is still the latest source. A new chunk
      // might have replaced us.
      if (self_active(self, src)) self._activeSource = null;
    };
    var self = this;
    function self_active(self, src) {
      return self._activeSource === src;
    }
  };

  /**
   * Stop everything: cancel in-flight fetches, stop the active
   * AudioBufferSourceNode mid-playback, clear the sentence buffer.
   * Idempotent.
   */
  TtsPlayer.prototype.stopAll = function () {
    this._stopped = true;
    this._buffer = '';
    if (this._inflight) {
      try {
        this._inflight.abort();
      } catch (_) {}
      this._inflight = null;
    }
    if (this._activeSource) {
      try {
        this._activeSource.stop();
      } catch (_) {}
      this._activeSource = null;
    }
    this._scheduledEnd = 0;
    // Resume playback for the next stream — the audio context stays
    // alive and unlocked after the first user gesture.
    this._stopped = false;
  };

  /**
   * Decode any pre-existing WAV bytes (used by the `#chat-tts-test`
   * button to play a fixed phrase without involving the LLM). Returns
   * the AudioBuffer; caller is responsible for scheduling.
   */
  TtsPlayer.prototype.decodeExternal = function (arrayBuffer) {
    return decodeWav(this._ensureContext(), arrayBuffer);
  };

  TtsPlayer.prototype.schedule = function (audioBuf) {
    this._schedule(audioBuf);
  };

  TtsPlayer.prototype.activeSource = function () {
    return this._activeSource;
  };

  /**
   * Public factory. `settings` keys:
   *   - endpoint: string (default '/v1/audio/speech')
   *   - voiceResolver: () => string (returns voice id based on lang)
   *   - speed: number (Piper length_scale; >1 slower)
   */
  function create(settings) {
    return new TtsPlayer(settings || {});
  }

  // Named export matching `chat.js`'s `import { NagentTts } from
  // '/static/tts.js'`. `chat.js` calls `NagentTts.create(settings)`
  // so the surface is a namespace object with `create` as its main
  // entry point. Also re-export the helpers (`splitSentence`,
  // `decodeWav`) for ad-hoc debugging in devtools.
  export const NagentTts = {
    create,
    _splitSentence: splitSentence,
    _decodeWav: decodeWav,
  };
