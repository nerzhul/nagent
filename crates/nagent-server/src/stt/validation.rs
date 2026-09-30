//! Inbound-frame validation.
//!
//! Every `StartSession`, `Config`, and `AudioFrame` payload coming
//! from a WebSocket client is run through the checks in this module
//! before the server does any work with it. The goal is to reject
//! obvious misuse (wrong sample rate, oversized audio buffer, bogus
//! language code) at the edge instead of letting a single bad client
//! poison the inference queue or leak arbitrary strings into the
//! `LANGUAGE_CACHE` inside `stt-core`.
//!
//! Each variant of [`FrameError`] carries an [`stt_proto::error_code`]
//! value so the WS handler can turn it into a stable wire error
//! without re-mapping anything by hand.

use stt_proto::error_code;

use crate::config::LimitsConfig;

/// Closed allow-list of ISO 639-1 (and a few BCP-47) language codes
/// the server will forward to Whisper. Mirrors the dropdown options in
/// `index.html`. Kept small on purpose: each entry is also interned in
/// `LANGUAGE_CACHE` for the lifetime of the process, so a permissive
/// list would leak a few bytes per *unique* attacker-supplied code.
///
/// Codes are stored lowercased; comparison is case-insensitive.
pub const ALLOWED_LANG_HINTS: &[&str] = &[
    "en", "fr", "es", "de", "it", "pt", "ja", "zh", "zh-cn", "zh-tw", "pt-br",
];

/// Outcome of validating one inbound WS payload.
#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("StartSession: sample_rate must be {expected}, got {got}")]
    InvalidSampleRate { expected: u32, got: u32 },

    #[error("StartSession: language hint exceeds {max} bytes (got {got})")]
    LanguageHintTooLong { max: usize, got: usize },

    #[error("StartSession: unknown language hint `{0}`")]
    UnknownLanguageHint(String),

    #[error("AudioFrame: {got} samples exceeds limit of {max}")]
    AudioFrameTooLarge { max: usize, got: usize },

    #[error("Config: language hint exceeds {max} bytes (got {got})")]
    ConfigLanguageHintTooLong { max: usize, got: usize },

    #[error("Config: unknown language hint `{0}`")]
    ConfigUnknownLanguageHint(String),
}

impl FrameError {
    /// Stable error code matching `stt_proto::error_code`.
    pub fn code(&self) -> u16 {
        error_code::INVALID_FRAME
    }

    /// Validate a `StartSession` payload.
    pub fn validate_start(
        limits: &LimitsConfig,
        sample_rate: u32,
        lang_hint: Option<&str>,
    ) -> Result<(), FrameError> {
        if sample_rate != limits.required_sample_rate {
            return Err(FrameError::InvalidSampleRate {
                expected: limits.required_sample_rate,
                got: sample_rate,
            });
        }
        if let Some(lang) = lang_hint {
            let len = lang.len();
            if len > limits.max_language_hint_bytes {
                return Err(FrameError::LanguageHintTooLong {
                    max: limits.max_language_hint_bytes,
                    got: len,
                });
            }
            if !is_allowed_lang(lang) {
                return Err(FrameError::UnknownLanguageHint(lang.to_string()));
            }
        }
        Ok(())
    }

    /// Validate a `Config` payload.
    pub fn validate_config(
        limits: &LimitsConfig,
        language: Option<&str>,
    ) -> Result<(), FrameError> {
        if let Some(lang) = language {
            let len = lang.len();
            if len > limits.max_language_hint_bytes {
                return Err(FrameError::ConfigLanguageHintTooLong {
                    max: limits.max_language_hint_bytes,
                    got: len,
                });
            }
            if !is_allowed_lang(lang) {
                return Err(FrameError::ConfigUnknownLanguageHint(lang.to_string()));
            }
        }
        Ok(())
    }

    /// Validate an `AudioFrame` payload.
    pub fn validate_audio(limits: &LimitsConfig, samples: &[f32]) -> Result<(), FrameError> {
        if samples.len() > limits.max_audio_frame_samples {
            return Err(FrameError::AudioFrameTooLarge {
                max: limits.max_audio_frame_samples,
                got: samples.len(),
            });
        }
        Ok(())
    }
}

/// Normalise a language hint to lower-case and return `true` if it
/// appears in [`ALLOWED_LANG_HINTS`].
fn is_allowed_lang(lang: &str) -> bool {
    let lower = lang.to_ascii_lowercase();
    ALLOWED_LANG_HINTS.iter().any(|allowed| *allowed == lower)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn defaults() -> LimitsConfig {
        LimitsConfig::default()
    }

    #[test]
    fn start_accepts_expected_sample_rate() {
        assert!(FrameError::validate_start(&defaults(), 16_000, None).is_ok());
    }

    #[test]
    fn start_rejects_other_sample_rates() {
        for bad in [8_000u32, 22_050, 44_100, 48_000] {
            let err = FrameError::validate_start(&defaults(), bad, None).unwrap_err();
            assert!(matches!(err, FrameError::InvalidSampleRate { got, .. } if got == bad));
        }
    }

    #[test]
    fn start_accepts_known_language_hints_case_insensitive() {
        for lang in ["en", "EN", "En", "fr", "zh-cn", "pt-BR", "ja"] {
            assert!(
                FrameError::validate_start(&defaults(), 16_000, Some(lang)).is_ok(),
                "{lang} should be accepted"
            );
        }
    }

    #[test]
    fn start_rejects_unknown_language_hints() {
        let err = FrameError::validate_start(&defaults(), 16_000, Some("xx")).unwrap_err();
        assert!(matches!(err, FrameError::UnknownLanguageHint(s) if s == "xx"));
    }

    #[test]
    fn start_rejects_overlong_language_hints() {
        let huge = "a".repeat(64);
        let err = FrameError::validate_start(&defaults(), 16_000, Some(&huge)).unwrap_err();
        assert!(matches!(err, FrameError::LanguageHintTooLong { .. }));
    }

    #[test]
    fn audio_rejects_oversized_frames() {
        let limits = defaults();
        let ok = vec![0.0_f32; limits.max_audio_frame_samples];
        assert!(FrameError::validate_audio(&limits, &ok).is_ok());
        let too_big = vec![0.0_f32; limits.max_audio_frame_samples + 1];
        let err = FrameError::validate_audio(&limits, &too_big).unwrap_err();
        assert!(matches!(err, FrameError::AudioFrameTooLarge { .. }));
    }

    #[test]
    fn audio_accepts_empty_frame() {
        // An empty buffer is harmless — whisper will return an empty
        // transcript. The size cap is an upper bound, not a floor.
        assert!(FrameError::validate_audio(&defaults(), &[]).is_ok());
    }

    #[test]
    fn config_rejects_unknown_language_hints() {
        let err = FrameError::validate_config(&defaults(), Some("klingon")).unwrap_err();
        assert!(matches!(err, FrameError::ConfigUnknownLanguageHint(s) if s == "klingon"));
    }

    #[test]
    fn config_accepts_known_language_hint() {
        assert!(FrameError::validate_config(&defaults(), Some("en")).is_ok());
    }

    #[test]
    fn all_frame_errors_map_to_invalid_frame_code() {
        let cases: Vec<FrameError> = vec![
            FrameError::InvalidSampleRate {
                expected: 16_000,
                got: 8_000,
            },
            FrameError::LanguageHintTooLong { max: 16, got: 32 },
            FrameError::UnknownLanguageHint("xx".into()),
            FrameError::AudioFrameTooLarge { max: 1, got: 2 },
            FrameError::ConfigLanguageHintTooLong { max: 16, got: 32 },
            FrameError::ConfigUnknownLanguageHint("xx".into()),
        ];
        for err in cases {
            assert_eq!(err.code(), error_code::INVALID_FRAME);
        }
    }
}
