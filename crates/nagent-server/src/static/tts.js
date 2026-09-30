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

// Find the earliest flush boundary in `buffer`, in priority order:
//
//   1. Sentence terminator `[.!?]+` followed by whitespace -- the
//      most common case, always a sentence boundary.
//   2. Paragraph break `\n\n+` (two or more newlines) -- explicit
//      paragraph break.
//   3. Newline followed by an uppercase letter, digit, or opening
//      punctuation (`[A-ZÀ-ÖØ-Ý\d«»"'(\[]`) -- lookahead at the
//      next line; if it starts a new sentence, flush at the `\n`.
//
// Returns `{ idx, kind }` where `idx` is the position AT WHICH the
// caller should split (the boundary character IS included in the
// left side, never in the right). Returns `null` when no boundary
// is found and the buffer should keep growing.
function findBoundary(buffer) {
  var sent = /[.!?]+[\s\u00a0]+/.exec(buffer);
  if (sent) {
    return { idx: sent.index + sent[0].length, kind: 'sentence' };
  }
  var para = /\n\n+/.exec(buffer);
  if (para) {
    return { idx: para.index + para[0].length, kind: 'paragraph' };
  }
  // Lookahead: `\n` followed by uppercase / digit / opening punct.
  // The `\n` itself is consumed (length 1) so the resulting phrase
  // carries the trailing `\n` -- the next call sees the new
  // sentence's first char at position 0 and won't re-flush on the
  // same boundary.
  var newline = /\n(?=[A-ZÀ-ÖØ-Ý\d«»"'(\[])/u.exec(buffer);
  if (newline) {
    return { idx: newline.index + 1, kind: 'newline-caps' };
  }
  return null;
}

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
 * Split `buffer` on the earliest flush boundary. Returns
 * `{ rest, phrase }`: `phrase` is the complete sentence (suitable
 * to send for synthesis after `phraseForTts`) and `rest` is what
 * stays in the buffer for the next call.
 *
 * If no boundary exists, `phrase` is `null` and `rest` is the
 * whole buffer (capped at `MAX_BUFFER_CHARS`, after which the
 * buffer is force-emitted regardless).
 */
function splitSentence(buffer) {
  if (!buffer) return { rest: '', phrase: null };
  var boundary = findBoundary(buffer);
  if (boundary) {
    return {
      phrase: buffer.slice(0, boundary.idx),
      rest: buffer.slice(boundary.idx),
      kind: boundary.kind,
    };
  }
  if (buffer.length >= MAX_BUFFER_CHARS) {
    // No terminator for a while -- flush the whole buffer to avoid
    // unbounded growth. The browser will say it slightly awkwardly
    // (mid-clause) but never loses words.
    return { phrase: buffer, rest: '' };
  }
  return { rest: buffer, phrase: null };
}

/**
 * Normalise a phrase for the Piper TTS pipeline:
 *
 *   - Collapse runs of whitespace (including newlines and tabs) into
 *     a single space. espeak-ng treats `\n` as a paragraph break (an
 *     audible silence), which sounds awkward when the newline appears
 *     mid-sentence as a result of streaming-token boundaries.
 *   - Trim leading / trailing whitespace.
 *
 * The visible bubble still renders the original markdown via
 * `marked.parse` + `DOMPurify`; only the audio path gets this
 * normalised variant.
 */
function phraseForTts(phrase) {
  return phrase.replace(/\s+/g, ' ').trim();
}

  function TtsPlayer(settings) {
    this._settings = settings; // { endpoint, voice, speed }
    this._buffer = '';
    this._inflight = null; // legacy single-controller field; kept
                            // null under the new queue scheme
    this._audioCtx = null;
    this._scheduledEnd = 0; // last scheduled source.endTime on the ctx
    this._activeSource = null;
    this._playPromise = null;
    this._stopped = false;
    // Parallel-fetch + FIFO-playback queue. Every `_enqueue`
    // assigns the next monotonic `idx`, starts its own fetch, and
    // does NOT abort any in-flight fetches -- the previous design
    // aborted phrase N when phrase N+1 was enqueued, which made
    // phrases 1..N-1 disappear. Now all fetches run in parallel and
    // `_playReady` schedules them in strict enqueue order, draining
    // any out-of-order completions as they arrive.
    this._nextIdx = 0;
    this._nextToPlay = 0;
    this._pending = new Map(); // idx -> AudioBuffer (decoded, waiting)
    this._inflights = new Map(); // idx -> AbortController (for stopAll)
    // Bumped by `stopAll`. Each `_enqueue` captures the current
    // value and the callback checks it before scheduling, so a
    // fetch that survived the abort (race: server already responded
    // when we tried to abort) doesn't sneak a phrase from a
    // previous stream into the current one.
    this._streamVersion = 0;
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
   * `_enqueue` normalises whitespace internally so we just hand the
   * raw buffer over and let it decide whether there's anything
   * worth synthesising.
   */
  TtsPlayer.prototype.flush = function () {
    var tail = this._buffer;
    this._buffer = '';
    if (tail.trim()) this._enqueue(tail);
  };

  TtsPlayer.prototype._enqueue = function (phrase) {
    if (this._stopped) return;
    // Normalise whitespace before sending to Piper (see
    // `phraseForTts` rationale). The `\n` carried over from the
    // lookahead boundary would otherwise insert an unwanted
    // paragraph pause mid-sentence.
    phrase = phraseForTts(phrase);
    if (!phrase) return;
    var self = this;
    var idx = this._nextIdx++;
    // Capture the stream version at fetch start. If the callback
    // resolves after `stopAll` advanced the version, the phrase
    // belongs to the aborted stream and is dropped.
    var version = this._streamVersion;
    var ctrl = new AbortController();
    this._inflights.set(idx, ctrl);
    var body = {
      input: phrase,
      voice: this._voiceFor(),
      speed: this._settings.speed,
    };
    fetch(this._settings.endpoint, {
      method: 'POST',
      // `RequireAuth` rejects every state-changing request that
      // does not carry the per-session CSRF token. `csrfHeaders()`
      // returns `undefined` when the session is gone, which the
      // spread below silently drops.
      headers: { 'Content-Type': 'application/json', ...window.nagentAuth?.csrfHeaders() },
      body: JSON.stringify(body),
      signal: ctrl.signal,
    })
      .then(function (resp) {
        if (!resp.ok) {
          // 4xx/5xx -> log and drop the chunk; the next sentence may
          // succeed (e.g. transient 503 on a slow first inference).
          console.warn('TTS HTTP', resp.status, resp.statusText);
          return null;
        }
        return resp.arrayBuffer();
      })
      .then(function (buf) {
        if (!buf) return null;
        return decodeWav(self._ensureContext(), buf);
      })
      .then(function (audioBuf) {
        if (!audioBuf) return;
        // Drop the phrase if a `stopAll` happened while the fetch was
        // in flight (the version counter advanced, so this fetch
        // belongs to an aborted stream).
        if (self._streamVersion !== version) return;
        self._playReady(idx, audioBuf);
      })
      .catch(function (err) {
        if (err && err.name === 'AbortError') return;
        console.warn('TTS fetch failed:', err);
      })
      .finally(function () {
        self._inflights.delete(idx);
      });
  };

  /**
   * Schedule `audioBuf` at position `idx` in the playback queue.
   * Out-of-order completions are stashed in `_pending` until the
   * head of the queue catches up; in-order completions schedule
   * immediately and drain any subsequent idxs that already arrived.
   */
  TtsPlayer.prototype._playReady = function (idx, audioBuf) {
    if (this._stopped) return;
    if (idx === this._nextToPlay) {
      this._schedule(audioBuf);
      this._nextToPlay++;
      // Drain any later idxs that already arrived while we were
      // waiting on this one. `_pending` is keyed by idx, so we walk
      // forward until we hit a gap.
      while (this._pending.has(this._nextToPlay)) {
        var next = this._pending.get(this._nextToPlay);
        this._pending.delete(this._nextToPlay);
        this._schedule(next);
        this._nextToPlay++;
      }
    } else if (idx > this._nextToPlay) {
      // Out-of-order: stash and wait for the queue head to catch up.
      this._pending.set(idx, audioBuf);
    }
    // idx < _nextToPlay means the chunk arrived after we'd already
    // moved on (e.g. `_stopped` toggled). Drop it silently.
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
    // Hold a reference to the live source so `stopAll()` can
    // interrupt it mid-chunk. We deliberately do NOT call `.stop()`
    // on the previous source -- each `AudioBufferSourceNode` plays
    // its buffer to the end on its own, and stopping the previous
    // one would chop it off mid-word every time a new chunk arrives,
    // leaving only the last phrase audible.
    this._activeSource = src;
    src.onended = function () {
      // Clear the reference only if this is still the latest source;
      // older onended callbacks fire after the queue has moved on.
      if (self._activeSource === src) self._activeSource = null;
    };
    var self = this;
  };

  /**
   * Stop everything: cancel in-flight fetches, stop the active
   * AudioBufferSourceNode mid-playback, clear the sentence buffer.
   * Idempotent.
   */
  TtsPlayer.prototype.stopAll = function () {
    this._stopped = true;
    // Bump the stream version so any fetch that survives the abort
    // (server already responded when we tried to cancel) drops its
    // phrase on arrival.
    this._streamVersion++;
    this._buffer = '';
    // Cancel every parallel fetch (each phrase had its own
    // AbortController under the new queue scheme).
    this._inflights.forEach(function (ctrl) {
      try { ctrl.abort(); } catch (_) {}
    });
    this._inflights.clear();
    this._pending.clear();
    if (this._activeSource) {
      try {
        this._activeSource.stop();
      } catch (_) {}
      this._activeSource = null;
    }
    this._scheduledEnd = 0;
    // Reset the queue head so the next stream starts a fresh idx
    // sequence; otherwise leftover idxs from the aborted stream
    // would be expected to arrive forever and block playback.
    this._nextIdx = 0;
    this._nextToPlay = 0;
    // Resume playback for the next stream -- the audio context stays
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
   * Single-shot speech path: fetch the whole `text` in one HTTP
   * round-trip, decode the returned WAV, queue it on the Web Audio
   * context, and resolve when playback ends. Used by the per-message
   * "Replay" button (vs the streaming `feed`/`flush` path used by
   * autoplay).
   *
   * `opts`:
   *   - voice (string): voice id sent to /v1/audio/speech.
   *   - speed (number): Piper length_scale (>1 slower, <1 faster).
   *   - onEnd (function): invoked when playback ends naturally or
   *     via `stopAll()`. Not called on fetch/decode errors (those
   *     reject the returned promise instead).
   *
   * Cancels any in-flight `feed()` stream by calling `stopAll()`
   * first, then awaits one microtask so the previous fetch's
   * AbortController settles before we start a new one. This avoids
   * the race where two overlapping fetches would land on the same
   * AudioContext queue out of order.
   */
  TtsPlayer.prototype.speak = function (text, opts) {
    var self = this;
    opts = opts || {};
    // Normalise whitespace: espeak-ng treats `\n` as a paragraph
    // break (audible silence) so a replay of a multi-line bubble
    // would otherwise pause at every newline. `phraseForTts` also
    // trims leading / trailing whitespace.
    text = phraseForTts(text);
    if (!text) return Promise.reject(new Error("speak: empty text"));
    this.stopAll();
    return new Promise(function (resolve, reject) {
      // Wait one microtask so the previous fetch's abort settles
      // before we open a new connection. Without this, the old
      // AbortController from the streaming autoplay could race
      // with the replay and steal a chunk.
      Promise.resolve().then(function () {
        var ctrl = new AbortController();
        self._inflight = ctrl;
        var body = {
          input: text,
          voice: opts.voice || self._settings.voice,
          speed: opts.speed != null ? opts.speed : self._settings.speed,
        };
        fetch(self._settings.endpoint, {
          method: "POST",
          // `RequireAuth` rejects every state-changing request that
          // does not carry the per-session CSRF token. `csrfHeaders()`
          // returns `undefined` when the session is gone, which the
          // spread below silently drops.
          headers: { "Content-Type": "application/json", ...window.nagentAuth?.csrfHeaders() },
          body: JSON.stringify(body),
          signal: ctrl.signal,
        })
          .then(function (resp) {
            if (!resp.ok) {
              return Promise.reject(
                new Error("HTTP " + resp.status + " " + resp.statusText)
              );
            }
            return resp.arrayBuffer();
          })
          .then(function (buf) {
            return decodeWav(self._ensureContext(), buf);
          })
          .then(function (audioBuf) {
            if (self._stopped) {
              // Replay was cancelled between fetch and decode;
              // resolve silently rather than scheduling.
              return null;
            }
            return new Promise(function (res) {
              self._schedule(audioBuf);
              // Wait for the AudioBufferSourceNode to finish.
              // `_schedule` doesn't expose the source it created
              // so we poll `activeSource()` until it stops, or
              // listen to the `onended` callback by wrapping.
              var onEnd = function () {
                if (typeof opts.onEnd === "function") opts.onEnd();
                res(null);
              };
              // Wrap activeSource's onended: replace the existing
              // handler installed by `_schedule`.
              var src = self.activeSource();
              if (src) src.onended = onEnd;
              else onEnd(); // already finished
            });
          })
          .then(function () {
            resolve();
          })
          .catch(function (err) {
            if (err && err.name === "AbortError") {
              resolve();
              return;
            }
            reject(err);
          })
          .finally(function () {
            if (self._inflight === ctrl) self._inflight = null;
          });
      });
    });
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
  // Named export matching `chat.js`'s `import { NagentTts } from
  // '/static/tts.js'`. `chat.js` calls `NagentTts.create(settings)`
  // so the surface is a namespace object with `create` as its main
  // entry point. Also re-export the helpers (`splitSentence`,
  // `decodeWav`, `phraseForTts`) for ad-hoc debugging in devtools.
  export const NagentTts = {
    create,
    _splitSentence: splitSentence,
    _decodeWav: decodeWav,
    _phraseForTts: phraseForTts,
  };
