//! `stt` — `[stt]` section: runtime master switch + Whisper
//! model path for the WebSocket STT pipeline.
//!
//! The heavy STT knobs (model path, queue size, idle timeout, …)
//! used to live under `[server]`; the runtime `enabled` switch
//! plus the model path now share the `[stt]` section because
//! they belong to the same subsystem. The env-var names
//! (`STT_ENABLED`, `WHISPER_MODEL_PATH`) are kept stable for
//! back-compat with deployments that pre-date the move.
//!
//! When `enabled = false`:
//! - `AppState.stt` is `None`;
//! - the `/ws` WebSocket upgrade route is not registered;
//! - the `/api/features` JSON exposes `stt: false` so the
//!   frontend hides every STT control (mic button, language
//!   picker, voice oscilloscope, …);
//! - `whisper_model_path` is **ignored** (no model required to
//!   boot a server that never speaks to Whisper).
//!
//! When `enabled = true`:
//! - `whisper_model_path` is required (env var or TOML);
//! - the model is loaded at boot and the `/ws` route is
//!   registered.
//!
//! The actual backend choice (real `whisper-rs` vs in-process
//! mock) is still a compile-time cargo feature
//! (`real-backend` + `stt-core/whisper-rs-{backend|vulkan|cuda|hipblas}`)
//! — this section is the runtime layer on top of that.

use std::path::PathBuf;

use crate::config::file::TomlSttConfig;
use crate::config::{env_opt, resolve_primitive, ConfigError};

/// Runtime configuration of the WebSocket STT pipeline.
#[derive(Debug, Clone)]
pub struct SttConfig {
    /// Master switch for the `/ws` upgrade route. When `false`
    /// the route is not registered and the frontend hides every
    /// STT control.
    pub enabled: bool,
    /// Path to the ggml-format whisper model. Required when
    /// `enabled = true` (resolved from `WHISPER_MODEL_PATH` env
    /// var or `[stt].whisper_model_path` TOML key); ignored
    /// when `enabled = false`.
    pub whisper_model_path: PathBuf,
}

impl SttConfig {
    pub fn from_env_with_toml(toml: Option<&TomlSttConfig>) -> Result<Self, ConfigError> {
        let defaults = Self::default();
        let toml = toml.cloned().unwrap_or_default();
        let enabled = resolve_primitive(
            env_opt("STT_ENABLED").as_deref(),
            toml.enabled,
            defaults.enabled,
            "STT_ENABLED",
        )?;

        // `whisper_model_path` is required only when STT is on.
        // Off → fall back to the default path so a deployment
        // that never speaks to Whisper doesn't trip the boot
        // check. The value is never read in that case.
        let whisper_model_path = if enabled {
            resolve_model_path(
                env_opt("WHISPER_MODEL_PATH").as_deref(),
                toml.whisper_model_path.as_deref(),
            )?
        } else {
            defaults.whisper_model_path.clone()
        };

        Ok(Self {
            enabled,
            whisper_model_path,
        })
    }
}

impl Default for SttConfig {
    fn default() -> Self {
        // STT is on by default so existing deployments that
        // never touched the new flag keep working unchanged.
        // The model path carries a placeholder — `from_env_with_toml`
        // requires it to be set when `enabled = true` and
        // ignores it when `false`, so the default is never
        // actually consumed in either case.
        Self {
            enabled: true,
            whisper_model_path: PathBuf::from("/dev/null/whisper-model.gguf"),
        }
    }
}

/// Resolve the Whisper model path: env > TOML > required.
/// Mirrors the old top-level `resolve_model_path` (which used
/// to live in `config::server`).
pub(crate) fn resolve_model_path(
    env_value: Option<&str>,
    toml_value: Option<&str>,
) -> Result<PathBuf, ConfigError> {
    match env_value {
        Some(v) => Ok(PathBuf::from(v)),
        None => match toml_value {
            Some(v) => Ok(PathBuf::from(v)),
            None => Err(ConfigError::MissingModelPath),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_enabled() {
        // STT is on by default so existing deployments that
        // never touched the new flag keep working unchanged.
        assert!(SttConfig::default().enabled);
    }

    #[test]
    fn toml_disabled_overrides_default() {
        let toml = TomlSttConfig {
            enabled: Some(false),
            whisper_model_path: None,
        };
        let cfg = SttConfig::from_env_with_toml(Some(&toml)).unwrap();
        assert!(!cfg.enabled, "TOML must override the default");
    }

    #[test]
    fn no_toml_with_stt_on_requires_model_path() {
        // The default is `enabled = true`; with no TOML and no
        // env there is no model path, so the resolver must
        // surface `MissingModelPath` (the operator either
        // provides a path or flips `enabled = false`).
        let err = SttConfig::from_env_with_toml(None).unwrap_err();
        assert!(matches!(err, ConfigError::MissingModelPath));
    }

    #[test]
    fn stt_off_does_not_require_model_path() {
        // `[stt].enabled = false` must boot even without a
        // model — the field is ignored in that case.
        let toml = TomlSttConfig {
            enabled: Some(false),
            whisper_model_path: None,
        };
        let cfg = SttConfig::from_env_with_toml(Some(&toml)).unwrap();
        assert!(!cfg.enabled);
    }

    #[test]
    fn stt_on_without_model_path_is_missing_model_path_error() {
        // `[stt].enabled = true` (default) without a model
        // path must surface `MissingModelPath` so a
        // misconfigured deployment fails loudly at boot.
        let toml = TomlSttConfig {
            enabled: None,
            whisper_model_path: None,
        };
        let err = SttConfig::from_env_with_toml(Some(&toml)).unwrap_err();
        assert!(matches!(err, ConfigError::MissingModelPath));
    }
}
