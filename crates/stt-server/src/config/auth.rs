//! `auth` — `[auth]` section + per-backend sub-configs
//! (`password`, `oidc`, `passkey`, `db`, `credentials`).

pub use crate::config::{
    AuthBackendKind, AuthConfig, AuthCredentialsConfig, AuthDbConfig, AuthOidcConfig,
    AuthPasskeyConfig, AuthPasswordConfig,
};
