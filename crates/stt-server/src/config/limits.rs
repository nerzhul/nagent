//! `limits` — `[server.limits]` section: inbound WebSocket frame limits.
//!
//! See [`crate::stt::ws_handler::handle_inbound`] for the validation
//! that consumes these knobs.

use crate::config::file::TomlLimitsConfig;
use crate::config::{env_opt, resolve_primitive, ConfigError};

/// Limits applied to inbound WebSocket frames.
#[derive(Debug, Clone)]
pub struct LimitsConfig {
    /// Maximum number of PCM Float32 samples accepted in a single
    /// `AudioFrame`. Whisper's hard cap is 30 s of audio at 16 kHz
    /// (= 480 000 samples); anything longer is rejected with
    /// `ErrorCode::INVALID_FRAME`.
    pub max_audio_frame_samples: usize,
    /// Only `16_000` Hz audio is supported (whisper's required input
    /// rate). A different value is rejected with
    /// `ErrorCode::INVALID_FRAME`.
    pub required_sample_rate: u32,
    /// Maximum length of an accepted ISO 639-1 language hint, in
    /// bytes. Two-letter codes plus a region suffix (`pt-BR`,
    /// `zh-CN`) never exceed a handful of bytes; a 4 KiB string is
    /// almost certainly an attack.
    pub max_language_hint_bytes: usize,
}

impl Default for LimitsConfig {
    fn default() -> Self {
        // 30 seconds at 16 kHz mono Float32.
        const DEFAULT_MAX_AUDIO_FRAME_SAMPLES: usize = 30 * 16_000;
        Self {
            max_audio_frame_samples: DEFAULT_MAX_AUDIO_FRAME_SAMPLES,
            required_sample_rate: 16_000,
            max_language_hint_bytes: 16,
        }
    }
}

impl LimitsConfig {
    pub fn from_env_with_toml(toml: Option<&TomlLimitsConfig>) -> Result<Self, ConfigError> {
        let defaults = Self::default();
        let toml = toml.cloned().unwrap_or_default();
        Ok(Self {
            max_audio_frame_samples: resolve_primitive(
                env_opt("MAX_AUDIO_FRAME_SAMPLES").as_deref(),
                toml.max_audio_frame_samples,
                defaults.max_audio_frame_samples,
                "MAX_AUDIO_FRAME_SAMPLES",
            )?,
            required_sample_rate: resolve_primitive(
                env_opt("REQUIRED_SAMPLE_RATE").as_deref(),
                toml.required_sample_rate,
                defaults.required_sample_rate,
                "REQUIRED_SAMPLE_RATE",
            )?,
            max_language_hint_bytes: resolve_primitive(
                env_opt("MAX_LANGUAGE_HINT_BYTES").as_deref(),
                toml.max_language_hint_bytes,
                defaults.max_language_hint_bytes,
                "MAX_LANGUAGE_HINT_BYTES",
            )?,
        })
    }
}
