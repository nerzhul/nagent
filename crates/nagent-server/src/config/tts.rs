//! `tts` — `[tts]` section: optional server-side Piper TTS engine.
//!
//! When `enabled` is `false` the `/v1/audio/*` routes are simply not
//! registered, so the discussion-mode "Read response aloud" checkbox
//! disappears and the rest of the server is unaffected. The engine is
//! compiled unconditionally and runtime-gated by `TTS_ENABLED`
//! (mirroring the `[llm]` pattern) so disabling TTS does not shrink
//! the binary — it just hides the routes.
//!
//! Voice files are downloaded separately by the operator and stored
//! under `model_dir` as one `<voice>.onnx` + `<voice>.onnx.json` pair
//! per voice. See `scripts/download-piper-voices.sh` and the README
//! "Text-to-Speech (Piper)" section.

use std::path::PathBuf;

use crate::config::file::TomlTtsConfig;
use crate::config::{env_opt, resolve_opt_string, resolve_primitive, ConfigError};

/// Configuration for the optional server-side Piper TTS engine.
#[derive(Debug, Clone)]
pub struct TtsConfig {
    /// Master switch for the `/v1/audio/*` routes.
    pub enabled: bool,
    /// Directory holding the Piper voice checkpoints
    /// (`<voice>.onnx` + `<voice>.onnx.json`). Only consulted when
    /// `enabled = true`; ignored otherwise.
    pub model_dir: PathBuf,
    /// Default voice id used when the request does not specify one
    /// (English-language fallback).
    pub voice_en: String,
    /// Default voice id used when the request does not specify one
    /// (French-language fallback).
    pub voice_fr: String,
    /// Default language code used when the client does not specify
    /// one. `"en"` or `"fr"` map to `voice_en` / `voice_fr`. Any
    /// other value falls back to `voice_en`.
    pub default_lang: String,
    /// Piper `length_scale`: `>1.0` = slower, `<1.0` = faster,
    /// `1.0` = neutral. Per-request `speed` overrides this.
    pub length_scale: f32,
    /// Piper `noise_scale` — controls the variability of the
    /// synthesized audio. Defaults to `0.667` (Piper upstream).
    pub noise_scale: f32,
    /// Piper `noise_w` — controls the variability of the phoneme
    /// durations. Defaults to `0.8` (Piper upstream).
    pub noise_w: f32,
    /// Hard cap on the number of characters accepted in a single
    /// `POST /v1/audio/speech` request body. Defends against
    /// pathological LLM responses that stream a 50 KB paragraph in
    /// one shot.
    pub max_input_chars: usize,
    /// Max concurrent Piper-rs synthesis calls on the bounded
    /// blocking pool (plan R4a). Piper-rs is single-threaded in
    /// practice; `1` is the safe default. `0` falls back to `1`
    /// (NOT half cores — a model in memory only supports one
    /// caller at a time, so the half-core heuristic would
    /// over-allocate permits the engine cannot use).
    pub synth_concurrency: usize,
    /// Max callers queued waiting for a concurrency permit (plan
    /// R1a). `0` disables the queue gate (backwards compatible —
    /// callers wait indefinitely). Saturated callers are answered
    /// with `503 Service Unavailable + Retry-After` so a burst
    /// cannot pile up unbounded.
    pub synth_max_queue: usize,
    /// Maximum wait time for a concurrency permit, in
    /// milliseconds (plan R1a). `0` waits indefinitely (backwards
    /// compatible). Exceeding this surface as `429 Too Many
    /// Requests + Retry-After`.
    pub synth_queue_timeout_ms: u64,
}

impl Default for TtsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            model_dir: PathBuf::from("./models/piper"),
            voice_en: "en_US-lessac-medium".to_string(),
            voice_fr: "fr_FR-upmc-medium".to_string(),
            default_lang: "en".to_string(),
            length_scale: 1.0,
            noise_scale: 0.667,
            noise_w: 0.8,
            max_input_chars: 2_000,
            synth_concurrency: 1,
            // Defaults preserve the historical behaviour (no
            // queue gate, no wait timeout) so existing
            // deployments do not silently change semantics on
            // upgrade. Operators opt in via `[tts].synth_max_queue`
            // / `TTS_SYNTH_QUEUE_TIMEOUT_MS`.
            synth_max_queue: 0,
            synth_queue_timeout_ms: 0,
        }
    }
}

impl TtsConfig {
    pub fn from_env_with_toml(toml: Option<&TomlTtsConfig>) -> Result<Self, ConfigError> {
        let defaults = Self::default();
        let toml = toml.cloned().unwrap_or_default();
        let enabled = resolve_primitive(
            env_opt("TTS_ENABLED").as_deref(),
            toml.enabled,
            defaults.enabled,
            "TTS_ENABLED",
        )?;
        // The model dir is only meaningful when TTS is on. We still
        // resolve it from the env / TOML so misconfigurations show up
        // at boot rather than at the first request, but a default of
        // `./models/piper` keeps first-run harmless.
        let model_dir = resolve_opt_string(
            env_opt("TTS_MODEL_DIR").as_deref(),
            toml.model_dir.as_deref(),
        )
        .map(PathBuf::from)
        .unwrap_or(defaults.model_dir.clone());
        let voice_en =
            resolve_opt_string(env_opt("TTS_VOICE_EN").as_deref(), toml.voice_en.as_deref())
                .unwrap_or_else(|| defaults.voice_en.clone());
        let voice_fr =
            resolve_opt_string(env_opt("TTS_VOICE_FR").as_deref(), toml.voice_fr.as_deref())
                .unwrap_or_else(|| defaults.voice_fr.clone());
        let default_lang = resolve_opt_string(
            env_opt("TTS_DEFAULT_LANG").as_deref(),
            toml.default_lang.as_deref(),
        )
        .unwrap_or_else(|| defaults.default_lang.clone());
        let length_scale = resolve_primitive(
            env_opt("TTS_LENGTH_SCALE").as_deref(),
            toml.length_scale,
            defaults.length_scale,
            "TTS_LENGTH_SCALE",
        )?
        .clamp(0.1, 5.0);
        let noise_scale = resolve_primitive(
            env_opt("TTS_NOISE_SCALE").as_deref(),
            toml.noise_scale,
            defaults.noise_scale,
            "TTS_NOISE_SCALE",
        )?
        .clamp(0.0, 5.0);
        let noise_w = resolve_primitive(
            env_opt("TTS_NOISE_W").as_deref(),
            toml.noise_w,
            defaults.noise_w,
            "TTS_NOISE_W",
        )?
        .clamp(0.0, 5.0);
        let max_input_chars = resolve_primitive(
            env_opt("TTS_MAX_INPUT_CHARS").as_deref(),
            toml.max_input_chars,
            defaults.max_input_chars,
            "TTS_MAX_INPUT_CHARS",
        )?;
        let synth_concurrency = match resolve_primitive(
            env_opt("TTS_SYNTH_CONCURRENCY").as_deref(),
            toml.synth_concurrency,
            defaults.synth_concurrency,
            "TTS_SYNTH_CONCURRENCY",
        )? {
            0 => 1,
            n => n,
        };
        let synth_max_queue = resolve_primitive(
            env_opt("TTS_SYNTH_MAX_QUEUE").as_deref(),
            toml.synth_max_queue,
            defaults.synth_max_queue,
            "TTS_SYNTH_MAX_QUEUE",
        )?;
        let synth_queue_timeout_ms = resolve_primitive(
            env_opt("TTS_SYNTH_QUEUE_TIMEOUT_MS").as_deref(),
            toml.synth_queue_timeout_ms,
            defaults.synth_queue_timeout_ms,
            "TTS_SYNTH_QUEUE_TIMEOUT_MS",
        )?;
        Ok(Self {
            enabled,
            model_dir,
            voice_en,
            voice_fr,
            default_lang,
            length_scale,
            noise_scale,
            noise_w,
            max_input_chars,
            synth_concurrency,
            synth_max_queue,
            synth_queue_timeout_ms,
        })
    }
}

// Plan 4.F: project the server's `TtsConfig` onto the plain
// `TtsSettings` the `nagent-tts` crate consumes. Keeps the crate
// decoupled from our config plumbing so the engine can be split
// into its own inference tier later (plan H) without surgery on
// the config layer.
impl From<&TtsConfig> for nagent_tts::TtsSettings {
    fn from(c: &TtsConfig) -> Self {
        Self {
            enabled: c.enabled,
            model_dir: c.model_dir.clone(),
            voice_en: c.voice_en.clone(),
            voice_fr: c.voice_fr.clone(),
            default_lang: c.default_lang.clone(),
            max_input_chars: c.max_input_chars,
            synth_concurrency: c.synth_concurrency,
            synth_max_queue: c.synth_max_queue,
            synth_queue_timeout_ms: c.synth_queue_timeout_ms,
        }
    }
}
