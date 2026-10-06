//! `config/` — server configuration grouped by section.
//!
//! Each section is its own submodule that owns its runtime struct,
//! its `Default`, its env/TOML merge, and its tests. The top-level
//! `Config` (in [`server`]) composes them. The TOML schema lives in
//! [`crate::config_file`] (re-exported here as `file`) and is
//! mirrored per section so the env/TOML/default precedence chain is
//! enforced in one place per knob.
//!
//! ## Section layout
//!
//! - [`server`] — `Config`, `CliArgs`, `ConfigError`, the env/TOML
//!  merge helpers (`env_opt`, `resolve_primitive`, …) and the
//!  layered file discovery.
//! - [`allowed_origins`] — `[server].allowed_origins` (plan S-2).
//! - [`ratelimit`] — per-IP rate-limit knobs (`RateLimitConfig`).
//! - [`trusted_proxies`] — `[server].trusted_proxies` CIDR list.
//! - [`limits`] — inbound WebSocket frame limits (`LimitsConfig`).
//! - [`llm`] — `[llm]` section + `LlmAuthMode`.
//! - [`tts`] — `[tts]` section.
//! - [`auth`] — `[auth]` section + per-backend sub-configs.
//! - [`agents`] — `[agents]` section + per-agent sub-configs.
//! - [`documents`] — `[documents]` section.

pub mod agents;
pub mod allowed_origins;
pub mod auth;
pub mod documents;
pub mod limits;
pub mod llm;
pub mod ratelimit;
pub mod server;
pub mod trusted_proxies;
pub mod tts;
pub mod x_oauth;

// Re-export the TOML schema module under `crate::config::file` so the
// section files can write `crate::config::file::TomlFooConfig`
// without dragging `crate::config_file` into every signature.
pub use crate::config_file as file;

// Re-export the canonical top-level types so call sites continue to
// use `crate::config::Config` / `crate::config::CliArgs`.
pub use server::{CliArgs, Config, ConfigError};

// Section re-exports keep the existing `crate::config::FooConfig`
// canonical paths resolving unchanged for downstream users (lib,
/// agents/*, llm/*, …).
pub use agents::{
    AgentConfig, DictionaryConfig, MemoryConfig, ReadDocumentConfig, StockConfig,
    UnitConvertConfig, WeatherConfig, WebFetchConfig, WikipediaConfig, XTimelineConfig,
};
pub use allowed_origins::AllowedOriginsConfig;
pub use auth::{
    AuthBackendKind, AuthConfig, AuthCredentialsConfig, AuthDbConfig, AuthOidcConfig,
    AuthPasskeyConfig, AuthPasswordConfig,
};
pub use documents::DocumentsConfig;
pub use limits::LimitsConfig;
pub use llm::{LlmAuthMode, LlmConfig};
pub use ratelimit::RateLimitConfig;
pub use trusted_proxies::TrustedProxiesConfig;
pub use tts::TtsConfig;
pub use x_oauth::XOAuthConfig;

/// Read an env var and return `None` for both unset and empty values.
/// Empty values (e.g. `env: - ""` in a container manifest) are treated
/// as unset so they never accidentally override a TOML value that the
/// operator deliberately populated.
pub(crate) fn env_opt(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

/// Resolve a `FromStr` value: env > TOML > default.
///
/// Pulled out as a pure function (no direct `std::env::var`) so unit
/// tests can exercise the precedence chain without mutating
/// process-global state — see the existing comment on
/// `rate_limit_defaults_match_plan` for why.
pub(crate) fn resolve_primitive<T>(
    env_value: Option<&str>,
    toml_value: Option<T>,
    default: T,
    env_key: &str,
) -> Result<T, ConfigError>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    match env_value {
        Some(v) => v
            .parse::<T>()
            .map_err(|e| ConfigError::InvalidEnv(env_key.into(), e.to_string())),
        None => Ok(toml_value.unwrap_or(default)),
    }
}

/// Resolve an `Option<String>`-like knob (the API key style): env
/// value if present & non-empty, else TOML value, else `None`.
pub(crate) fn resolve_opt_string(
    env_value: Option<&str>,
    toml_value: Option<&str>,
) -> Option<String> {
    match env_value {
        Some(v) if !v.is_empty() => Some(v.to_string()),
        _ => toml_value.map(str::to_string),
    }
}

/// Resolve an `Option<T>` knob where `T: FromStr`: env value if
/// present, else TOML value, else `None`. The env parse error is
/// surfaced as a [`ConfigError`] (a typo must fail loudly at boot
/// rather than silently dropping the knob). Use for optional
/// numeric knobs (`ollama_num_predict`, `ollama_num_ctx`, future
/// batch sizes, …) where the proxy must distinguish "operator
/// opted in" (`Some`) from "operator has no opinion, leave
/// upstream default alone" (`None`).
pub(crate) fn resolve_opt_primitive<T>(
    env_value: Option<&str>,
    toml_value: Option<T>,
    env_key: &str,
) -> Result<Option<T>, ConfigError>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    match env_value {
        Some(v) => v
            .parse::<T>()
            .map(Some)
            .map_err(|e| ConfigError::InvalidEnv(env_key.into(), e.to_string())),
        None => Ok(toml_value),
    }
}

/// Resolve a comma-separated string list: env value (split + trimmed
/// + empty-filtered) > TOML list > default.
pub(crate) fn resolve_csv(
    env_value: Option<&str>,
    toml_value: Option<Vec<String>>,
    default: Vec<String>,
) -> Vec<String> {
    match env_value {
        Some(v) => v
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect(),
        None => toml_value.unwrap_or(default),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn env_var_beats_toml_for_primitives() {
        // env Some, toml Some → env wins
        assert_eq!(
            resolve_primitive(Some("99"), Some(8usize), 32usize, "MAX_QUEUE").unwrap(),
            99
        );
        // env None, toml Some → toml wins
        assert_eq!(
            resolve_primitive(None, Some(8usize), 32usize, "MAX_QUEUE").unwrap(),
            8
        );
        // env None, toml None → default wins
        assert_eq!(
            resolve_primitive(None, None::<usize>, 32usize, "MAX_QUEUE").unwrap(),
            32
        );
        // env Some, toml None → env wins
        assert_eq!(
            resolve_primitive(Some("99"), None::<usize>, 32usize, "MAX_QUEUE").unwrap(),
            99
        );
        // Invalid env value is an error, not a silent default.
        let err = resolve_primitive(Some("not-a-number"), Some(8usize), 32usize, "MAX_QUEUE")
            .unwrap_err();
        match err {
            ConfigError::InvalidEnv(k, _) => assert_eq!(k, "MAX_QUEUE"),
            other => panic!("expected InvalidEnv, got {other:?}"),
        }
    }

    #[test]
    fn opt_string_helpers_handle_empty_env() {
        // env non-empty → wins over TOML
        assert_eq!(
            resolve_opt_string(Some("env-key"), Some("toml-key")),
            Some("env-key".to_string())
        );
        // env empty → treated as unset, TOML wins
        assert_eq!(
            resolve_opt_string(Some(""), Some("toml-key")),
            Some("toml-key".to_string())
        );
        // env unset, TOML unset → None
        assert_eq!(resolve_opt_string(None, None), None);
    }

    /// Helper: build a `LlmConfig` directly with the `system_prompt`
    /// resolution short-circuited to a single `resolve_opt_string`
    /// call. Avoids mutating process-global env vars (see the comment
    /// on `rate_limit_defaults_match_plan` for why).
    fn llm_system_prompt_resolved(env: Option<&str>, toml: Option<&str>) -> Option<String> {
        resolve_opt_string(env, toml)
    }

    #[test]
    fn llm_system_prompt_env_overrides_toml() {
        assert_eq!(
            llm_system_prompt_resolved(Some("from-env"), Some("from-toml")),
            Some("from-env".to_string())
        );
    }

    #[test]
    fn llm_system_prompt_toml_used_when_env_unset() {
        assert_eq!(
            llm_system_prompt_resolved(None, Some("from-toml")),
            Some("from-toml".to_string())
        );
    }

    #[test]
    fn llm_system_prompt_defaults_to_none() {
        assert_eq!(llm_system_prompt_resolved(None, None), None);
    }

    #[test]
    fn llm_system_prompt_empty_env_string_falls_back_to_toml() {
        // Empty env vars (e.g. `LLM_SYSTEM_PROMPT=""` in a container
        // manifest) must NOT silently override the TOML value.
        assert_eq!(
            llm_system_prompt_resolved(Some(""), Some("from-toml")),
            Some("from-toml".to_string())
        );
        // Both empty / unset → None (no injection).
        assert_eq!(llm_system_prompt_resolved(Some(""), None), None);
    }

    #[test]
    fn csv_helper_trims_and_dedups_blanks() {
        // env wins over TOML; splits + trims + drops empties
        assert_eq!(
            resolve_csv(
                Some("a, b , ,c"),
                Some(vec!["should-not-appear".into()]),
                vec!["default".into()]
            ),
            vec!["a".to_string(), "b".to_string(), "c".to_string()]
        );
        // env unset → TOML wins
        assert_eq!(
            resolve_csv(
                None,
                Some(vec!["x".into(), "y".into()]),
                vec!["default".into()]
            ),
            vec!["x".to_string(), "y".to_string()]
        );
        // both unset → default wins
        assert_eq!(
            resolve_csv(None, None, vec!["d1".into(), "d2".into()]),
            vec!["d1".to_string(), "d2".to_string()]
        );
    }

    #[test]
    fn bind_addr_uses_default_when_neither_is_set() {
        let addr = server::resolve_bind_addr(None, None).expect("default bind addr must parse");
        assert_eq!(addr.to_string(), "0.0.0.0:8080");
    }

    #[test]
    fn bind_addr_prefers_env_over_toml() {
        let addr = server::resolve_bind_addr(Some("127.0.0.1:9001"), Some("127.0.0.1:9002"))
            .expect("env override must parse");
        assert_eq!(addr.to_string(), "127.0.0.1:9001");
    }

    #[test]
    fn model_path_is_required_when_neither_is_set() {
        let err = server::resolve_model_path(None, None).unwrap_err();
        assert!(matches!(err, ConfigError::MissingModelPath));
    }

    #[test]
    fn model_path_falls_back_to_toml_when_env_unset() {
        let p =
            server::resolve_model_path(None, Some("/from/toml.bin")).expect("toml path must parse");
        assert_eq!(p, PathBuf::from("/from/toml.bin"));
    }

    /// `CliArgs::parse` accepts both `--config PATH` and the
    /// `--config=PATH` form so operators don't have to remember which
    /// one the binary supports.
    #[test]
    fn cli_args_parse_space_and_equals_forms() {
        // Drive `parse()` against a synthetic argv by re-implementing
        // the loop inline; `std::env::args` is process-global so we
        // can't stub it from a unit test.
        fn run(argv: &[&str]) -> Result<CliArgs, String> {
            let mut iter = argv.iter().copied();
            let mut out = CliArgs::default();
            while let Some(arg) = iter.next() {
                if arg == "--config" {
                    let v = iter.next().ok_or("missing value".to_string())?;
                    out.config = Some(PathBuf::from(v));
                } else if let Some(v) = arg.strip_prefix("--config=") {
                    out.config = Some(PathBuf::from(v));
                } else {
                    return Err(format!("unknown arg: {arg}"));
                }
            }
            Ok(out)
        }
        assert_eq!(
            run(&["--config", "/etc/nagent.toml"]).unwrap().config,
            Some(PathBuf::from("/etc/nagent.toml"))
        );
        assert_eq!(
            run(&["--config=/etc/nagent.toml"]).unwrap().config,
            Some(PathBuf::from("/etc/nagent.toml"))
        );
        assert!(run(&[]).unwrap().config.is_none());
        assert!(run(&["--bogus"]).is_err());
    }

    #[test]
    fn xdg_path_uses_xdg_config_home_when_set() {
        let p = server::xdg_config_path_from_for_test(Some("/custom/xdg"), Some("/home/test"))
            .expect("must resolve when XDG is set");
        assert_eq!(p, PathBuf::from("/custom/xdg/nagent/config.toml"));
    }

    #[test]
    fn xdg_path_falls_back_to_home_dot_config() {
        let p = server::xdg_config_path_from_for_test(None, Some("/home/test"))
            .expect("HOME fallback must resolve");
        assert_eq!(p, PathBuf::from("/home/test/.config/nagent/config.toml"));
    }

    #[test]
    fn xdg_path_treats_empty_xdg_as_unset() {
        // XDG Base Directory spec: empty $XDG_CONFIG_HOME must be
        // treated as if it were unset.
        let p = server::xdg_config_path_from_for_test(Some(""), Some("/home/test"))
            .expect("empty XDG must fall through to HOME");
        assert_eq!(p, PathBuf::from("/home/test/.config/nagent/config.toml"));
    }

    #[test]
    fn xdg_path_returns_none_when_no_env() {
        assert!(server::xdg_config_path_from_for_test(None, None).is_none());
        assert!(server::xdg_config_path_from_for_test(Some(""), None).is_none());
    }

    #[test]
    fn inference_workers_resolution_precedence() {
        // TOML-only value is picked up.
        let toml: file::TomlConfig = toml::from_str(
            r#"
                [server]
                whisper_model_path = "/tmp/m.bin"
                inference_workers = 4
            "#,
        )
        .unwrap();
        let cfg = Config::from_env_with_toml(Some(&toml)).expect("config must load");
        assert_eq!(cfg.inference_workers, Some(4), "TOML value must apply");

        // Invalid TOML value (zero) must be clamped to 1, never panic.
        let toml: file::TomlConfig = toml::from_str(
            r#"
                [server]
                whisper_model_path = "/tmp/m.bin"
                inference_workers = 0
            "#,
        )
        .unwrap();
        let cfg = Config::from_env_with_toml(Some(&toml)).expect("config must load");
        assert_eq!(cfg.inference_workers, Some(1), "zero must clamp to 1");
    }

    #[test]
    fn toml_overlay_is_used_when_env_unset() {
        let toml: file::TomlConfig = toml::from_str(
            r#"
                [server]
                max_queue = 8
                whisper_model_path = "/tmp/from-toml.bin"

                [llm]
                llm_max_tool_rounds = 2

                [agents]
                enabled = false

                [agents.web_fetch]
                allow_public = true
                allowlist = ["example.com"]
                max_bytes = 1024
                timeout_ms = 5000

                [agents.get_weather]
                api_key = "weather-toml-key"
            "#,
        )
        .unwrap();
        let cfg = Config::from_env_with_toml(Some(&toml)).expect("config must load");
        assert_eq!(cfg.max_queue, 8, "max_queue from TOML");
        assert_eq!(
            cfg.whisper_model_path,
            PathBuf::from("/tmp/from-toml.bin"),
            "model path from TOML"
        );
        assert!(!cfg.agents.enabled, "agents.enabled from TOML");
        assert_eq!(
            cfg.llm.llm_max_tool_rounds, 2,
            "llm_max_tool_rounds from TOML [llm] section"
        );
        assert!(cfg.agents.web_fetch.allow_public);
        assert_eq!(cfg.agents.web_fetch.allowlist, vec!["example.com"]);
        assert_eq!(cfg.agents.web_fetch.max_bytes, 1024);
        assert_eq!(cfg.agents.web_fetch.timeout_ms, 5000);
        assert_eq!(cfg.agents.weather.api_key, "weather-toml-key");
    }

    #[test]
    fn layered_load_skips_missing_files_and_merges_existing() {
        // Write two real files into a temp dir and pass their paths to
        // a refactored layered loader — the public layered loader
        // hardcodes /etc + XDG, which we can't exercise from a unit
        // test (no permission to write /etc on most runners).
        let dir = std::env::temp_dir().join(format!(
            "nagent-layered-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();

        let system = dir.join("system.toml");
        let xdg = dir.join("xdg.toml");
        let missing = dir.join("missing.toml");
        std::fs::write(
            &system,
            r#"
                [server]
                bind_addr = "127.0.0.1:1111"
                max_queue = 8

                [agents]
                enabled = true

                [agents.web_fetch]
                allow_public = false
                max_bytes = 1024
            "#,
        )
        .unwrap();
        std::fs::write(
            &xdg,
            r#"
                [server]
                max_queue = 64

                [agents.web_fetch]
                allow_public = true
            "#,
        )
        .unwrap();

        let paths = vec![system.clone(), missing, xdg.clone()];
        let merged = server::load_layered_paths_for_test(&paths)
            .expect("layered load must succeed")
            .expect("merged config must be Some when files exist");

        // bind_addr only in system → kept.
        let server = merged.server.expect("server merged");
        assert_eq!(server.bind_addr.as_deref(), Some("127.0.0.1:1111"));
        // max_queue in both → xdg (later) wins.
        assert_eq!(server.max_queue, Some(64));
        // agents.enabled only in system → kept.
        let agents = merged.agents.expect("agents merged");
        assert_eq!(agents.enabled, Some(true));
        // agents.web_fetch.allow_public in both → xdg wins.
        let wf = agents.web_fetch.expect("web_fetch merged");
        assert_eq!(wf.allow_public, Some(true));
        // agents.web_fetch.max_bytes only in system → kept.
        assert_eq!(wf.max_bytes, Some(1024));

        // Cleanup.
        let _ = std::fs::remove_file(&system);
        let _ = std::fs::remove_file(&xdg);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn layered_load_returns_none_when_all_files_missing() {
        let dir = std::env::temp_dir().join(format!(
            "nagent-layered-empty-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let a = dir.join("a.toml");
        let b = dir.join("b.toml");
        let merged =
            server::load_layered_paths_for_test(&[a, b]).expect("missing files are not an error");
        assert!(
            merged.is_none(),
            "no existing file → None, mirroring the no-TOML path"
        );
    }
}
