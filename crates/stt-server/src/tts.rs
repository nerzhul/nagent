//! Local text-to-speech (Piper ONNX) engine.
//!
//! The HTTP-facing route lives in [`crate::lib::build_router`] (same
//! envelope as the other `/v1/*` endpoints — CORS allow-list, per-IP
//! rate limit, security headers). This module owns:
//!
//! - The [`Synthesizer`] trait that abstracts the underlying engine so
//!   the HTTP integration tests can swap in [`MockSynthesizer`] and
//!   avoid pulling `piper-rs` (which requires espeak-ng C library +
//!   headers at build time and a real `.onnx` voice at runtime).
//! - The [`PiperSynthesizer`] wrapper around `piper-rs` (only compiled
//!   when the `tts` cargo feature is on — see `Cargo.toml`).
//! - The [`TtsEngine`] facade that owns the [`Synthesizer`], discovers
//!   voice files in `model_dir`, resolves language→voice, enforces the
//!   `max_input_chars` cap, and serialises PCM 16-bit mono WAV bytes
//!   through `hound` for the HTTP response body.
//!
//! NB: piper-rs's API is still pre-1.0. The call site
//! (`PiperSynthesizer::synth`) is the only place that touches the
//! crate, so a future API break is a single-file edit.

use std::io::Cursor;
use std::sync::Arc;

use hound::{SampleFormat, WavSpec, WavWriter};
use tracing::{info, warn};

use crate::config::TtsConfig;

#[cfg(feature = "tts")]
use std::collections::HashMap;
#[cfg(feature = "tts")]
use std::path::{Path, PathBuf};
#[cfg(feature = "tts")]
use std::sync::RwLock;

// ---------------------------------------------------------------------------
// Public error type
// ---------------------------------------------------------------------------

/// Errors produced by [`TtsEngine::synth_wav`] and friends.
#[derive(Debug, thiserror::Error)]
pub enum TtsError {
    /// Request body had zero characters.
    #[error("input is empty")]
    EmptyInput,
    /// Request body exceeded `TtsConfig::max_input_chars`. Defends
    /// against pathological LLM responses streaming one giant
    /// paragraph in a single chunk.
    #[error("input too long: {0} chars (max {1})")]
    InputTooLong(usize, usize),
    /// The supplied `voice` (or the resolved default for the language)
    /// is not in `model_dir`. Surfaced as 404 by the HTTP handler.
    #[error("voice not found: {0}")]
    VoiceNotFound(String),
    /// Underlying synthesizer (piper-rs or the mock) failed.
    /// Surfaced as 500 by the HTTP handler — the message is safe to
    /// log and forward because we don't include model internals.
    #[error("synthesizer error: {0}")]
    Synth(String),
    /// WAV writer (hound) failed — should be unreachable in practice
    /// because we write into an in-memory `Vec<u8>`, but reported for
    /// completeness.
    #[error("wav writer: {0}")]
    Wav(String),
}

impl From<hound::Error> for TtsError {
    fn from(e: hound::Error) -> Self {
        TtsError::Wav(e.to_string())
    }
}

// ---------------------------------------------------------------------------
// Voice metadata
// ---------------------------------------------------------------------------

/// Public voice metadata exposed to the browser via `GET /v1/audio/voices`
/// so the UI's voice selectors can only show voices actually present in
/// `model_dir`. The sample rate is what Piper reported for the loaded
/// `.onnx` checkpoint; the WAV bytes we emit are always at this rate
/// (the browser resamples on playback).
#[derive(Debug, Clone)]
pub struct VoiceMeta {
    /// Voice id, derived from the `.onnx.json` file stem
    /// (`en_US-lessac-medium` for `en_US-lessac-medium.onnx.json`).
    pub id: String,
    /// ISO 639-1 language code as reported by the model's
    /// `language.code` field, when present. `None` when the config did
    /// not declare one (rare for upstream voices but happens with
    /// some fine-tunes).
    pub language: Option<String>,
    /// Sample rate in Hz as reported by the model. Piper voices are
    /// typically 22 050 Hz mono; some are 16 000 Hz.
    pub sample_rate: u32,
}

// ---------------------------------------------------------------------------
// Synthesizer trait
// ---------------------------------------------------------------------------

/// Raw PCM output of a synthesizer, ready to be wrapped in a WAV
/// container. Samples are mono, normalized to `[-1.0, 1.0]`.
#[derive(Debug, Clone)]
pub struct SynthOutput {
    pub samples: Vec<f32>,
    pub sample_rate: u32,
}

/// Backend-agnostic interface so the HTTP route and the unit tests
/// don't depend on piper-rs (which requires espeak-ng C headers +
/// libclang at build time and a real voice file at runtime).
pub trait Synthesizer: Send + Sync {
    /// Synthesise `text` with `voice_id`. The voice id must have been
    /// advertised by [`Synthesizer::voices`] on a prior call; behaviour
    /// for unknown ids is implementation-defined (the real Piper
    /// backend returns [`TtsError::Synth`], the mock returns
    /// [`TtsError::VoiceNotFound`]).
    fn synth(&self, text: &str, voice_id: &str) -> Result<SynthOutput, TtsError>;

    /// Voices this synthesizer can serve. The returned slice is stable
    /// for the lifetime of the synthesizer; the HTTP route reads it on
    /// each `GET /v1/audio/voices` to surface the list to the browser.
    fn voices(&self) -> &[VoiceMeta];

    /// Optional per-call override of the speaker's `length_scale`
    /// (Piper convention: `>1.0` = slower, `<1.0` = faster). Backends
    /// that don't support it (the mock) return `Ok` without effect.
    fn set_length_scale(&self, _scale: f32) -> Result<(), TtsError> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Mock synthesizer (tests + CI without espeak-ng)
// ---------------------------------------------------------------------------

/// Test double that produces a short sine-wave clip proportional to the
/// input length, plus the requested voice metadata. Used by the unit
/// tests in this module and by the HTTP integration tests in
/// `tests/tts.rs` — never instantiated in production.
#[derive(Debug)]
pub struct MockSynthesizer {
    voices: Vec<VoiceMeta>,
}

impl MockSynthesizer {
    /// Build a mock with one voice per `(id, language)` pair, all at
    /// `sample_rate` Hz. Pass `22050` to match the canonical Piper
    /// output.
    pub fn new(voices: Vec<(String, Option<String>)>, sample_rate: u32) -> Self {
        Self {
            voices: voices
                .into_iter()
                .map(|(id, language)| VoiceMeta {
                    id,
                    language,
                    sample_rate,
                })
                .collect(),
        }
    }
}

impl Synthesizer for MockSynthesizer {
    fn synth(&self, text: &str, voice_id: &str) -> Result<SynthOutput, TtsError> {
        if text.is_empty() {
            return Err(TtsError::EmptyInput);
        }
        let voice = self
            .voices
            .iter()
            .find(|v| v.id == voice_id)
            .ok_or_else(|| TtsError::VoiceNotFound(voice_id.to_string()))?;
        // ~0.05 s of 440 Hz mono sine per character of input, clamped to
        // [0.05 s, 1.0 s] so tests stay snappy and the WAV file is
        // always non-trivial in size.
        let secs = (text.chars().count() as f32 * 0.05).clamp(0.05, 1.0);
        let n = (voice.sample_rate as f32 * secs) as usize;
        let mut samples = Vec::with_capacity(n);
        for i in 0..n {
            let t = i as f32 / voice.sample_rate as f32;
            // Half-amplitude so 16-bit quantisation doesn't clip.
            samples.push(0.5 * (2.0 * std::f32::consts::PI * 440.0 * t).sin());
        }
        Ok(SynthOutput {
            samples,
            sample_rate: voice.sample_rate,
        })
    }

    fn voices(&self) -> &[VoiceMeta] {
        &self.voices
    }
}

// ---------------------------------------------------------------------------
// Piper synthesizer (production)
// ---------------------------------------------------------------------------

/// Wrapper around `piper-rs`. Loads one Piper voice per `<id>.onnx.json`
/// file in `model_dir` on first use (the underlying ONNX session is
/// heavy — ~50 MB of weights — so we lazy-init). Per-call synthesis is
/// serialised by [`TtsEngine`] (see its `synth_lock`) because piper-rs's
/// per-voice `PiperSynthesisConfig` lives behind a `RwLock` and we tweak
/// `length_scale` per request; concurrent requests on the same engine
/// would otherwise see each other's speed overrides.
///
/// Only compiled when the `tts` cargo feature is enabled — without it,
/// [`TtsEngine::load`] returns `Ok(None)` and the `/v1/audio/*` routes
/// are not registered, so the GPU build targets (which don't opt into
/// the feature) avoid the espeak-ng / libclang / libssl-dev build
/// dependencies that `piper-rs` transitively pulls in.
#[cfg(feature = "tts")]
pub struct PiperSynthesizer {
    voices: Vec<VoiceMeta>,
    /// Directory the voices were discovered in. Re-derived here so the
    /// lazy [`PiperSynthesizer::load_voice`] can rebuild the full
    /// `<voice>.onnx.json` path for piper-rs.
    model_dir: PathBuf,
    /// One slot per voice id; the inner `Option` is `Some` after the
    /// first call to that voice, `None` until then.
    models: RwLock<HashMap<String, Arc<piper_rs::Piper>>>,
}

#[cfg(feature = "tts")]
impl PiperSynthesizer {
    /// Scan `model_dir` for `<id>.onnx.json` files, build a
    /// [`VoiceMeta`] for each, and return the lazy-loading wrapper.
    /// Voice entries that fail to parse the config JSON are skipped
    /// with a warning so a single bad file does not break the whole
    /// engine.
    pub fn discover(model_dir: &Path) -> Result<Self, TtsError> {
        if !model_dir.is_dir() {
            return Err(TtsError::Synth(format!(
                "TTS model_dir does not exist or is not a directory: {}",
                model_dir.display()
            )));
        }
        let mut voices = Vec::new();
        let entries = std::fs::read_dir(model_dir).map_err(|e| {
            TtsError::Synth(format!(
                "could not read TTS model_dir {}: {}",
                model_dir.display(),
                e
            ))
        })?;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let stem = match path.file_stem().and_then(|s| s.to_str()) {
                Some(s) => s,
                None => continue,
            };
            // The Piper convention is "<id>.onnx.json" — we want the
            // voice id "<id>". Strip the trailing ".onnx" if present.
            let voice_id = stem.strip_suffix(".onnx").unwrap_or(stem).to_string();
            let onnx_path = path.with_file_name(format!("{stem}.onnx"));
            if !onnx_path.exists() {
                warn!(
                    voice = %voice_id,
                    config = %path.display(),
                    "skipping Piper voice: missing companion .onnx file"
                );
                continue;
            }
            // Parse just the bits of the .onnx.json we need for the
            // voice listing (sample rate, language). The full Piper
            // model is loaded lazily on first synth call so a broken
            // voice does not break the whole boot.
            match Self::parse_voice_meta(&path) {
                Ok(mut meta) => {
                    meta.id = voice_id;
                    voices.push(meta);
                }
                Err(e) => warn!(
                    voice = %voice_id,
                    error = %e,
                    "skipping Piper voice: could not parse .onnx.json"
                ),
            }
        }
        voices.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(Self {
            voices,
            model_dir: model_dir.to_path_buf(),
            models: RwLock::new(HashMap::new()),
        })
    }

    /// Parse the small subset of `<id>.onnx.json` we need for the
    /// voice listing. The full Piper model loads the same file again
    /// later; this is just to keep the boot-time work bounded and to
    /// keep going when a voice file is malformed.
    fn parse_voice_meta(path: &Path) -> Result<VoiceMeta, TtsError> {
        let bytes = std::fs::read(path).map_err(|e| TtsError::Synth(e.to_string()))?;
        #[derive(serde::Deserialize)]
        struct Subset {
            #[serde(default)]
            audio: Option<AudioSubset>,
            #[serde(default)]
            language: Option<LangSubset>,
        }
        #[derive(serde::Deserialize)]
        struct AudioSubset {
            sample_rate: u32,
        }
        #[derive(serde::Deserialize)]
        struct LangSubset {
            code: String,
        }
        let parsed: Subset = serde_json::from_slice(&bytes)
            .map_err(|e| TtsError::Synth(format!("invalid .onnx.json: {e}")))?;
        Ok(VoiceMeta {
            id: String::new(), // filled in by caller
            language: parsed.language.map(|l| l.code),
            sample_rate: parsed.audio.map(|a| a.sample_rate).unwrap_or(22_050),
        })
    }

    /// Build the path to the `<id>.onnx.json` file for a given voice.
    fn voice_config_path(&self, voice_id: &str) -> PathBuf {
        // Re-apply the same stem convention as `discover`: try
        // `<id>.onnx.json` first, fall back to `<id>.json`.
        let with_onnx = self.model_dir.join(format!("{voice_id}.onnx.json"));
        if with_onnx.exists() {
            with_onnx
        } else {
            self.model_dir.join(format!("{voice_id}.json"))
        }
    }

    /// Lazily load (and cache) the Piper model for `voice_id`.
    fn load_voice(
        &self,
        voice_id: &str,
    ) -> Result<Arc<piper_rs::Piper>, TtsError> {
        {
            let cache = self.models.read().expect("piper voice cache poisoned");
            if let Some(model) = cache.get(voice_id) {
                return Ok(Arc::clone(model));
            }
        }
        let mut cache = self.models.write().expect("piper voice cache poisoned");
        // Double-checked: another task may have loaded it while we
        // were upgrading the lock.
        if let Some(model) = cache.get(voice_id) {
            return Ok(Arc::clone(model));
        }
        // Sanity check: the voice id must be in the discovered list.
        if !self.voices.iter().any(|v| v.id == voice_id) {
            return Err(TtsError::VoiceNotFound(voice_id.to_string()));
        }
        let config_path = self.voice_config_path(voice_id);
        let onnx_path = config_path.with_extension("onnx");
        // piper-rs 0.2's constructor takes (model_path, config_path)
        // in that order. We pass the `.onnx` path first and the
        // matching `.onnx.json` second.
        let model = piper_rs::Piper::new(&onnx_path, &config_path).map_err(|e| {
            TtsError::Synth(format!(
                "could not load Piper voice {voice_id} from {}: {e}",
                config_path.display()
            ))
        })?;
        let model = Arc::new(model);
        cache.insert(voice_id.to_string(), Arc::clone(&model));
        Ok(model)
    }
}

#[cfg(feature = "tts")]
impl Synthesizer for PiperSynthesizer {
    fn synth(&self, text: &str, voice_id: &str) -> Result<SynthOutput, TtsError> {
        if text.is_empty() {
            return Err(TtsError::EmptyInput);
        }
        // `synth` is sync because piper-rs is sync and the tests want
        // a sync trait. The HTTP handler runs inside
        // `tokio::task::spawn_blocking` (see the route implementation),
        // which removes the need for a runtime-aware lock here.
        let mut model = self.load_voice(voice_id)?;

        // piper-rs 0.2 bundles phonemisation (espeak-ng) + inference
        // + sample-rate reporting into a single `create` call. The
        // `is_phonemes = false` flag tells piper-rs to run the
        // phoneticiser internally -- failures here usually mean
        // espeak-ng is missing on the host, which we surface
        // explicitly to the operator. We do not pin a `speaker`
        // (Piper voices typically have a single speaker; multi-speaker
        // voices fall back to speaker 0). `length_scale` is plumbed
        // from the per-request `TtsEngine::synth_wav` step (which
        // already held `synth_lock`), so we do not need to re-set it
        // here.
        let (samples, sample_rate) =
            model
                .create(text, false, None, None, None, None)
                .map_err(|e| {
                    let msg = e.to_string();
                    let hint = if msg.to_lowercase().contains("espeak")
                        || msg.to_lowercase().contains("phonem")
                    {
                        " (is `espeak-ng` installed on this host?)"
                    } else {
                        ""
                    };
                    TtsError::Synth(format!("piper inference failed: {msg}{hint}"))
                })?;

        Ok(SynthOutput {
            samples,
            sample_rate,
        })
    }

    fn voices(&self) -> &[VoiceMeta] {
        &self.voices
    }

    /// piper-rs 0.2 takes `length_scale` per-call via `create()`, so
    /// this setter is a no-op (the override is read directly from
    /// [`TtsEngine`] at synth time). Kept on the trait so
    /// [`TtsEngine::synth_wav`] has a single code path regardless of
    /// the backend.
    fn set_length_scale(&self, _scale: f32) -> Result<(), TtsError> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// TtsEngine (public facade used by the HTTP handler + tests)
// ---------------------------------------------------------------------------

/// User-facing TTS engine. Owned by [`crate::AppState`] (one per
/// server), cloneable via `Arc<TtsEngine>` when passed to handlers.
pub struct TtsEngine {
    inner: Arc<dyn Synthesizer>,
    /// Voice id used when the request specifies `lang = "en"` (or any
    /// non-French language) without a voice override.
    default_voice_en: String,
    /// Voice id used when the request specifies `lang = "fr"`.
    default_voice_fr: String,
    /// Default language code used when the request omits one. Stored
    /// so we can map the empty-string hint to a real default without
    /// re-reading the config every request.
    default_lang: String,
    /// Hard cap on `input` length in characters.
    max_input_chars: usize,
    /// Serialises the (set_length_scale + synth) pair per request so
    /// two concurrent requests with different speeds do not race on
    /// piper-rs's internal synthesis-config lock. Held only across the
    /// two calls (~80-200 ms each).
    synth_lock: std::sync::Mutex<()>,
}

impl std::fmt::Debug for TtsEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TtsEngine")
            .field("default_lang", &self.default_lang)
            .field("default_voice_en", &self.default_voice_en)
            .field("default_voice_fr", &self.default_voice_fr)
            .field("voices", &self.inner.voices())
            .finish()
    }
}

impl TtsEngine {
    /// Build a `TtsEngine` from config. Returns `Ok(None)` when TTS is
    /// disabled (so the HTTP layer can simply not register the
    /// routes). When enabled but `model_dir` is missing or contains no
    /// voices, returns `Err` so the binary refuses to start with a
    /// misconfigured engine -- the operator wants TTS on, the binary
    /// cannot find the voices; we want a loud boot-time signal rather
    /// than a silent runtime 503 on every request.
    ///
    /// Returns `Ok(None)` with a startup warning when the `tts`
    /// cargo feature is **off** even if `TTS_ENABLED=true`: the
    /// binary has no `piper-rs` compiled in, so we cannot honour
    /// the request. The warning tells the operator to rebuild with
    /// `--features stt-server/tts`.
    pub async fn load(config: &TtsConfig) -> Result<Option<Self>, TtsError> {
        if !config.enabled {
            info!("TTS disabled (TTS_ENABLED=false); /v1/audio/* routes will not be registered");
            return Ok(None);
        }
        #[cfg(not(feature = "tts"))]
        {
            warn!(
                "TTS_ENABLED=true but the server was compiled without the `tts` cargo feature; \
                 rebuild with `--features stt-server/tts` to enable Piper. \
                 /v1/audio/* routes will not be registered."
            );
            Ok(None)
        }
        #[cfg(feature = "tts")]
        {
            let piper = PiperSynthesizer::discover(&config.model_dir)?;
            let count = piper.voices().len();
            if count == 0 {
                warn!(
                    model_dir = %config.model_dir.display(),
                    "TTS enabled but no Piper voices found in model_dir; \
                     /v1/audio/* routes will respond with an error until voices are added"
                );
            } else {
                info!(
                    model_dir = %config.model_dir.display(),
                    voices = count,
                    "Piper TTS engine ready"
                );
            }
            Ok(Some(Self {
                inner: Arc::new(piper),
                default_voice_en: config.voice_en.clone(),
                default_voice_fr: config.voice_fr.clone(),
                default_lang: config.default_lang.clone(),
                max_input_chars: config.max_input_chars,
                synth_lock: std::sync::Mutex::new(()),
            }))
        }
    }

    /// Build a `TtsEngine` from an arbitrary [`Synthesizer`]. Used by
    /// tests (`MockSynthesizer`) and by future code paths that want to
    /// substitute the engine (e.g. a Cloud TTS adapter).
    pub fn from_synth(
        synth: Arc<dyn Synthesizer>,
        voice_en: impl Into<String>,
        voice_fr: impl Into<String>,
        default_lang: impl Into<String>,
        max_input_chars: usize,
    ) -> Self {
        Self {
            inner: synth,
            default_voice_en: voice_en.into(),
            default_voice_fr: voice_fr.into(),
            default_lang: default_lang.into(),
            max_input_chars,
            synth_lock: std::sync::Mutex::new(()),
        }
    }

    /// List of voices the engine can serve. Used by
    /// `GET /v1/audio/voices` to populate the UI's voice selectors.
    pub fn voices(&self) -> &[VoiceMeta] {
        self.inner.voices()
    }

    /// Resolve the default voice id for a given language hint. `"fr"`
    /// (or any language starting with `fr`) maps to `voice_fr`;
    /// everything else — including the empty string — maps to
    /// `voice_en`.
    pub fn default_voice_for(&self, lang: &str) -> String {
        if lang.eq_ignore_ascii_case("fr") || lang.to_ascii_lowercase().starts_with("fr-") {
            self.default_voice_fr.clone()
        } else {
            self.default_voice_en.clone()
        }
    }

    /// Default language hint (`"en"` or `"fr"` typically).
    pub fn default_lang(&self) -> &str {
        &self.default_lang
    }

    /// Synthesize `text` to a 16-bit PCM mono WAV byte vector, ready
    /// for `Content-Type: audio/wav`. `voice_override` wins over
    /// `lang`; `lang` wins over the engine default.
    ///
    /// `speed` is the Piper `length_scale` (>1.0 = slower, <1.0 =
    /// faster). `None` keeps whatever the voice's `.onnx.json`
    /// declared.
    pub fn synth_wav(
        &self,
        text: &str,
        voice_override: Option<&str>,
        lang: Option<&str>,
        speed: Option<f32>,
    ) -> Result<Vec<u8>, TtsError> {
        if text.is_empty() {
            return Err(TtsError::EmptyInput);
        }
        if text.chars().count() > self.max_input_chars {
            return Err(TtsError::InputTooLong(
                text.chars().count(),
                self.max_input_chars,
            ));
        }
        let voice_id = voice_override.map(|s| s.to_string()).unwrap_or_else(|| {
            let hint = lang.unwrap_or(&self.default_lang);
            self.default_voice_for(hint)
        });
        // Validate the voice exists so we can return 404 instead of 500.
        if !self.inner.voices().iter().any(|v| v.id == voice_id) {
            return Err(TtsError::VoiceNotFound(voice_id));
        }
        // Serialise (set_length_scale + synth) so concurrent requests
        // with different speeds do not race on piper-rs's per-voice
        // internal synthesis-config lock. The mock ignores
        // `set_length_scale` so the cost is just one uncontended
        // Mutex acquire per request.
        let _guard = self.synth_lock.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(s) = speed {
            // Best-effort: some backends (the mock) ignore this.
            self.inner.set_length_scale(s)?;
        }
        let output = self.inner.synth(text, &voice_id)?;
        encode_wav(&output)
    }
}

/// Wrap a [`SynthOutput`] in a 16-bit PCM mono WAV. The sample rate
/// comes from the synthesizer (Piper voices report their own rate, the
/// mock hard-codes 22 050). We clamp to `i16` so the WAV stays
/// portable; values are already in `[-1.0, 1.0]` per the contract of
/// [`SynthOutput`].
fn encode_wav(out: &SynthOutput) -> Result<Vec<u8>, TtsError> {
    let spec = WavSpec {
        channels: 1,
        sample_rate: out.sample_rate,
        bits_per_sample: 16,
        sample_format: SampleFormat::Int,
    };
    let mut buf = Cursor::new(Vec::with_capacity(out.samples.len() * 2 + 44));
    {
        let mut writer = WavWriter::new(&mut buf, spec)?;
        for s in &out.samples {
            let clamped = s.clamp(-1.0, 1.0);
            let sample = (clamped * i16::MAX as f32) as i16;
            writer.write_sample(sample)?;
        }
        writer.finalize()?;
    }
    Ok(buf.into_inner())
}

// ---------------------------------------------------------------------------
// HTTP handlers
// ---------------------------------------------------------------------------

use axum::extract::{Json, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};

use crate::AppState;

/// Request body for `POST /v1/audio/speech`. Mirrors the fields the
/// browser sends; everything except `input` is optional.
#[derive(Debug, Deserialize)]
pub struct SpeechRequest {
    /// Text to synthesize. Required. Hard-capped at
    /// `TtsConfig::max_input_chars` (default 2 000).
    pub input: String,
    /// Override the resolved voice id. Wins over `lang`.
    pub voice: Option<String>,
    /// Language hint (`"en"`, `"fr"`, …). Used to pick
    /// `default_voice_en` vs `default_voice_fr` when `voice` is `None`.
    pub lang: Option<String>,
    /// Piper `length_scale` (>1.0 = slower, <1.0 = faster). `None`
    /// keeps the voice's own `.onnx.json` default.
    pub speed: Option<f32>,
}

/// Response body for `GET /v1/audio/voices` — the list of Piper voices
/// discovered in `model_dir`. Returned as JSON so the UI can populate
/// its voice selectors without hard-coding voice ids.
#[derive(Debug, Serialize)]
pub struct VoicesResponse {
    pub voices: Vec<VoiceMetaJson>,
    /// Default voice id used when the request specifies `lang = "en"`
    /// (or any non-French language) without a voice override.
    pub default_voice_en: String,
    /// Default voice id used when the request specifies `lang = "fr"`.
    pub default_voice_fr: String,
    /// Default language code used when the request omits one.
    pub default_lang: String,
}

#[derive(Debug, Serialize)]
pub struct VoiceMetaJson {
    pub id: String,
    pub language: Option<String>,
    pub sample_rate: u32,
}

impl From<&VoiceMeta> for VoiceMetaJson {
    fn from(v: &VoiceMeta) -> Self {
        Self {
            id: v.id.clone(),
            language: v.language.clone(),
            sample_rate: v.sample_rate,
        }
    }
}

/// `POST /v1/audio/speech` — synthesise a single chunk of text and
/// return the WAV bytes (`audio/wav`). The actual Piper call runs on
/// the blocking pool because piper-rs is synchronous and a single
/// short-phrase inference can take 80-200 ms.
pub async fn audio_speech(
    State(state): State<Arc<AppState>>,
    Json(req): Json<SpeechRequest>,
) -> Response {
    let Some(tts) = state.tts.as_ref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            [(
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/plain; charset=utf-8"),
            )],
            "TTS disabled on this server (TTS_ENABLED=false)",
        )
            .into_response();
    };
    let engine = Arc::clone(tts);
    // Piper-rs is sync and CPU-bound; run on the blocking pool so we
    // do not stall the tokio runtime on a long inference.
    let join = tokio::task::spawn_blocking(move || {
        engine.synth_wav(
            &req.input,
            req.voice.as_deref(),
            req.lang.as_deref(),
            req.speed,
        )
    })
    .await;
    let result = match join {
        Ok(r) => r,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("TTS worker panicked: {e}"),
            )
                .into_response();
        }
    };
    match result {
        Ok(wav) => {
            let len = wav.len();
            (
                StatusCode::OK,
                [
                    (header::CONTENT_TYPE, HeaderValue::from_static("audio/wav")),
                    (
                        header::CONTENT_LENGTH,
                        HeaderValue::from_str(&len.to_string())
                            .unwrap_or(HeaderValue::from_static("0")),
                    ),
                    // Marker so curl --include output makes the route
                    // obvious in logs / smoke tests.
                    (
                        header::HeaderName::from_static("x-tts-backend"),
                        HeaderValue::from_static("piper"),
                    ),
                ],
                wav,
            )
                .into_response()
        }
        Err(TtsError::EmptyInput) => (
            StatusCode::BAD_REQUEST,
            [(
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/plain; charset=utf-8"),
            )],
            "input is empty",
        )
            .into_response(),
        Err(TtsError::InputTooLong(got, max)) => (
            StatusCode::BAD_REQUEST,
            [(
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/plain; charset=utf-8"),
            )],
            format!("input too long: {got} chars (max {max})"),
        )
            .into_response(),
        Err(TtsError::VoiceNotFound(name)) => (
            StatusCode::NOT_FOUND,
            [(
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/plain; charset=utf-8"),
            )],
            format!("voice not found: {name}"),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            [(
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/plain; charset=utf-8"),
            )],
            format!("TTS failed: {e}"),
        )
            .into_response(),
    }
}

/// `GET /v1/audio/voices` — list the Piper voices discovered in
/// `model_dir`, plus the per-language defaults. Used by the discussion
/// UI's voice selectors so they only ever offer voices that actually
/// exist on disk.
pub async fn audio_voices(State(state): State<Arc<AppState>>) -> Response {
    let Some(tts) = state.tts.as_ref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            [(
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/plain; charset=utf-8"),
            )],
            "TTS disabled on this server (TTS_ENABLED=false)",
        )
            .into_response();
    };
    let body = VoicesResponse {
        voices: tts.voices().iter().map(VoiceMetaJson::from).collect(),
        default_voice_en: tts.default_voice_for("en"),
        default_voice_fr: tts.default_voice_for("fr"),
        default_lang: tts.default_lang().to_string(),
    };
    (StatusCode::OK, Json(body)).into_response()
}

// ---------------------------------------------------------------------------
// Unit tests (mock-only — never touch piper-rs or espeak-ng)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn mock_engine() -> TtsEngine {
        let synth: Arc<dyn Synthesizer> = Arc::new(MockSynthesizer::new(
            vec![
                ("en_US-lessac-medium".to_string(), Some("en".to_string())),
                ("fr_FR-upmc-medium".to_string(), Some("fr".to_string())),
            ],
            22_050,
        ));
        TtsEngine::from_synth(synth, "en_US-lessac-medium", "fr_FR-upmc-medium", "en", 100)
    }

    #[test]
    fn default_voice_for_lang() {
        let e = mock_engine();
        assert_eq!(e.default_voice_for("en"), "en_US-lessac-medium");
        assert_eq!(e.default_voice_for("fr"), "fr_FR-upmc-medium");
        assert_eq!(e.default_voice_for(""), "en_US-lessac-medium");
        assert_eq!(e.default_voice_for("fr-CA"), "fr_FR-upmc-medium");
    }

    #[test]
    fn empty_input_is_rejected() {
        let e = mock_engine();
        let err = e.synth_wav("", None, None, None).unwrap_err();
        assert!(matches!(err, TtsError::EmptyInput));
    }

    #[test]
    fn oversized_input_is_rejected() {
        let e = mock_engine();
        let big = "x".repeat(200);
        let err = e.synth_wav(&big, None, None, None).unwrap_err();
        assert!(matches!(err, TtsError::InputTooLong(200, 100)));
    }

    #[test]
    fn unknown_voice_override_is_rejected() {
        let e = mock_engine();
        let err = e
            .synth_wav("hello", Some("does-not-exist"), None, None)
            .unwrap_err();
        assert!(matches!(err, TtsError::VoiceNotFound(_)));
    }

    #[test]
    fn synth_returns_valid_wav_header() {
        let e = mock_engine();
        let wav = e.synth_wav("hello", None, None, None).expect("must synth");
        // "RIFF" magic
        assert_eq!(&wav[0..4], b"RIFF");
        // "WAVE" magic at offset 8
        assert_eq!(&wav[8..12], b"WAVE");
        // "fmt " chunk
        assert_eq!(&wav[12..16], b"fmt ");
        // "data" chunk
        assert_eq!(&wav[36..40], b"data");
        // Sample rate (little-endian u32 at offset 24) = 22 050
        let sr = u32::from_le_bytes([wav[24], wav[25], wav[26], wav[27]]);
        assert_eq!(sr, 22_050);
        // Bits per sample (u16 LE at offset 34) = 16
        let bps = u16::from_le_bytes([wav[34], wav[35]]);
        assert_eq!(bps, 16);
        // Channels (u16 LE at offset 22) = 1
        let ch = u16::from_le_bytes([wav[22], wav[23]]);
        assert_eq!(ch, 1);
        // Data length (u32 LE at offset 40) should be > 0 and even
        let data_len = u32::from_le_bytes([wav[40], wav[41], wav[42], wav[43]]) as usize;
        assert!(data_len > 0, "data section must not be empty");
        assert_eq!(data_len % 2, 0, "16-bit samples must be byte-aligned");
        // Re-decode via hound to round-trip the bytes we just emitted.
        let reader = hound::WavReader::new(Cursor::new(&wav)).expect("hound must parse");
        let spec = reader.spec();
        assert_eq!(spec.channels, 1);
        assert_eq!(spec.sample_rate, 22_050);
        assert_eq!(spec.bits_per_sample, 16);
        assert_eq!(spec.sample_format, SampleFormat::Int);
        let n = reader.into_samples::<i16>().count();
        assert_eq!(n, data_len / 2, "sample count must match data section");
    }

    #[test]
    fn mock_synthesizer_rejects_unknown_voice() {
        let synth = MockSynthesizer::new(vec![("a".to_string(), Some("en".to_string()))], 22_050);
        let err = synth.synth("hi", "missing").unwrap_err();
        assert!(matches!(err, TtsError::VoiceNotFound(_)));
    }

    #[test]
    fn mock_synthesizer_rejects_empty_text() {
        let synth = MockSynthesizer::new(vec![("a".to_string(), Some("en".to_string()))], 22_050);
        let err = synth.synth("", "a").unwrap_err();
        assert!(matches!(err, TtsError::EmptyInput));
    }

    /// Piper's `discover` should not panic on a missing directory and
    /// should return a structured error so the boot-time log is
    /// actionable.
    #[cfg(feature = "tts")]
    #[test]
    fn piper_discover_errors_on_missing_dir() {
        let dir = std::env::temp_dir().join(format!(
            "nagent-tts-missing-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let err = PiperSynthesizer::discover(&dir).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("does not exist") || msg.contains("not a directory"),
            "missing-dir error must be clear, got: {msg}"
        );
    }

    /// `discover` returns no voices for an empty directory (a real
    /// directory, just no `.onnx.json` files inside).
    #[cfg(feature = "tts")]
    #[test]
    fn piper_discover_finds_no_voices_in_empty_dir() {
        let dir = std::env::temp_dir().join(format!(
            "nagent-tts-empty-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let p = PiperSynthesizer::discover(&dir).expect("empty dir must parse");
        assert_eq!(p.voices().len(), 0);
        let _ = std::fs::remove_dir(&dir);
    }
}
