//! `allowed_origins` — `[server].allowed_origins` (plan S-2).
//!
//! Operator-supplied allow-list for the `Origin` and `Host` header
//! checks applied to state-changing routes and WebSocket upgrades.
//! When empty, the allow-list is derived from `[server].bind_addr`
//! (loopback names plus the bind host); see
//! [`crate::http::origin_guard::build_allowed_origins`] for the
//! derivation rules.

use crate::config::file::TomlAllowedOriginsConfig;
use crate::config::{env_opt, resolve_csv, ConfigError};

/// `[server].allowed_origins` knobs.
#[derive(Debug, Clone, Default)]
pub struct AllowedOriginsConfig {
    /// Operator-supplied `Origin` allow-list (in `scheme://host[:port]`
    /// form). Empty by default — see the module-level note on the
    /// derivation rules that take over.
    pub origins: Vec<String>,
}

impl AllowedOriginsConfig {
    pub fn from_env_with_toml(
        toml: Option<&TomlAllowedOriginsConfig>,
    ) -> Result<Self, ConfigError> {
        let toml = toml.cloned().unwrap_or_default();
        let origins = resolve_csv(
            env_opt("NAGENT_ALLOWED_ORIGINS").as_deref(),
            toml.origins,
            Vec::new(),
        );
        Ok(Self { origins })
    }
}
