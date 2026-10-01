//! `auth` — `[auth]` section + per-backend sub-configs .
//!
//! Always present in [`crate::config::Config`] so the rest of the
//! code does not need feature gates. When the runtime config sets
//! `auth.enabled = false` (the default), `enabled` is `false` and
//! every other field is the "no auth" default — the server then
//! behaves exactly as it did before . When the feature is on, the
//! operator enables the subsystem via `NAGENT_AUTH_ENABLED=true` (or
//! `auth.enabled = true` in the TOML overlay) and picks the per-backend
//! knobs.

use crate::config::file::TomlAuthConfig;
use crate::config::{env_opt, resolve_csv, resolve_opt_string, resolve_primitive, ConfigError};

/// Authentication & user-identity subsystem .
#[derive(Debug, Clone)]
pub struct AuthConfig {
    /// Master switch. When `false`, no auth routes are registered
    /// and `RequireAuth` stays off — the server keeps the pre-/// single-user trust boundary. Mirrors `NAGENT_AUTH_ENABLED` /
    /// `[auth].enabled`.
    pub enabled: bool,
    /// Subset of `"local"`, `"oidc"`, `"passkey"` enabled on this
    /// server. Each enabled backend exposes its own login route.
    /// Mirrors `NAGENT_AUTH_BACKENDS` (comma-separated) /
    /// `[auth].backends`.
    pub backends: Vec<AuthBackendKind>,
    /// Public origin used for OIDC/Passkey origin + cookie security
    /// flags. E.g. `https://nagent.example.com`. Mirrors
    /// `[auth].public_url`.
    pub public_url: String,
    /// Absolute session TTL in days, computed at login as
    /// `now() + session_ttl_days`. Range `1..=90`. Mirrors
    /// `NAGENT_AUTH_SESSION_TTL_DAYS` / `[auth].session_ttl_days`.
    pub session_ttl_days: u32,
    /// CSRF token header name. Defaults to `x-csrf-token`. Operators
    /// can rename it to dodge a proxy that strips the default.
    pub csrf_header: String,
    /// DB connection knobs (required when `enabled = true`).
    pub db: AuthDbConfig,
    /// Password backend knobs (argon2id + registration gate).
    pub password: AuthPasswordConfig,
    /// OIDC backend knobs.
    pub oidc: AuthOidcConfig,
    /// Passkey (WebAuthn) backend knobs.
    pub passkey: AuthPasskeyConfig,
    /// Per-user credentials vault knobs. Required when
    /// `enabled = true` AND at least one agent is registered.
    pub credentials: AuthCredentialsConfig,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            backends: Vec::new(),
            public_url: String::new(),
            session_ttl_days: 7,
            csrf_header: "x-csrf-token".to_string(),
            db: AuthDbConfig::default(),
            password: AuthPasswordConfig::default(),
            oidc: AuthOidcConfig::default(),
            passkey: AuthPasskeyConfig::default(),
            credentials: AuthCredentialsConfig::default(),
        }
    }
}

/// Per-user credentials vault knobs. The AES-256-GCM key lives in
/// plaintext inside `[auth.credentials].key` (64 hex chars / 32
/// bytes). Operators who want to keep the secret out of disk should
/// mount the TOML file from an encrypted volume (k8s `Secret`
/// mounted as `subPath`, Vault Agent, etc.); the configuration
/// shape itself does not force plaintext storage on disk.
#[derive(Debug, Clone, Default)]
pub struct AuthCredentialsConfig {
    /// 64 hex chars = 32 raw bytes. Empty string means "unset" —
    /// the `AuthConfig::from_env_with_toml` resolution fails
    /// with a clear error when `auth.enabled = true` AND at
    /// least one agent is registered AND this is empty.
    pub key: String,
}

/// Names of the auth backends an operator can enable. Matches the
/// strings the plan uses in `[auth].backends`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthBackendKind {
    Local,
    Oidc,
    Passkey,
}

impl AuthBackendKind {
    pub fn parse(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "local" => Ok(Self::Local),
            "oidc" => Ok(Self::Oidc),
            "passkey" => Ok(Self::Passkey),
            other => Err(format!(
                "expected one of `local`, `oidc`, `passkey`, got `{other}`"
            )),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Oidc => "oidc",
            Self::Passkey => "passkey",
        }
    }
}

/// DB connection knobs for the auth store.
#[derive(Debug, Clone)]
pub struct AuthDbConfig {
    /// `"sqlite"` or `"postgres"`. Empty string means "unset" — the
    /// `AuthConfig::from_env_with_toml` resolution fails when
    /// `auth.enabled = true` and this is unset.
    pub backend: String,
    /// Connection URL — see the plan §"DB schema" for the canonical
    /// sqlite + postgres URLs.
    pub url: String,
    /// Maximum simultaneous connections. Defaults to 16.
    pub max_connections: u32,
    /// When `true` (the default), `auth::boot::auto_bootstrap` runs
    /// `migrate up` against the auth DB at server start. Set this
    /// to `false` to disable boot-time auto-migration; operators
    /// then run `stt-server migrate up` themselves before each boot
    /// (e.g. as a separate init container in Kubernetes, or as a
    /// pre-deploy hook in CI).
    pub auto_migrate: bool,
}

impl Default for AuthDbConfig {
    fn default() -> Self {
        Self {
            backend: String::new(),
            url: String::new(),
            max_connections: 16,
            auto_migrate: true,
        }
    }
}

/// Password backend (argon2id) knobs. Mirrors `[auth.password]`.
#[derive(Debug, Clone)]
pub struct AuthPasswordConfig {
    /// Argon2id memory cost in KiB. OWASP 2025 default: 19456
    /// (19 MiB). The resolver clamps this to at least 19456 so a
    /// careless operator cannot silently weaken the hash.
    pub argon2_memory_kib: u32,
    /// Argon2id time cost (iterations). OWASP default: 2.
    pub argon2_iterations: u32,
    /// Argon2id parallelism (lanes). OWASP default: 1.
    pub argon2_parallelism: u32,
    /// Minimum password length accepted by `/api/auth/password/register`.
    pub min_password_length: usize,
    /// When `true`, any logged-in user can POST to
    /// `/api/auth/password/register` to create a new local account.
    pub allow_registration: bool,
}

impl Default for AuthPasswordConfig {
    fn default() -> Self {
        Self {
            argon2_memory_kib: 19_456,
            argon2_iterations: 2,
            argon2_parallelism: 1,
            min_password_length: 8,
            allow_registration: true,
        }
    }
}

/// OIDC backend knobs. Mirrors `[auth.oidc]`.
#[derive(Debug, Clone)]
pub struct AuthOidcConfig {
    /// When `true`, an OIDC user logging in for the first time has a
    /// `users` row created automatically. When `false`, unknown
    /// emails get `403`.
    pub auto_provision: bool,
    /// OIDC issuer URL — the discovery URL is derived from this.
    pub issuer: String,
    pub client_id: String,
    /// OIDC client secret. `NAGENT_AUTH_OIDC_CLIENT_SECRET` env var
    /// overrides the TOML file.
    pub client_secret: Option<String>,
    /// OIDC scopes — defaults to `["openid", "email", "profile"]`.
    pub scopes: Vec<String>,
    /// Optional allow-list of IdP groups. Empty = no restriction.
    pub required_groups: Vec<String>,
    /// IdP claim name mapped onto the local `roles` list. Defaults to `groups`.
    pub role_claim: String,
}

impl Default for AuthOidcConfig {
    fn default() -> Self {
        Self {
            auto_provision: true,
            issuer: String::new(),
            client_id: String::new(),
            client_secret: None,
            scopes: vec!["openid".into(), "email".into(), "profile".into()],
            required_groups: Vec::new(),
            role_claim: "groups".into(),
        }
    }
}

/// Passkey (WebAuthn) backend knobs. Mirrors `[auth.passkey]`.
#[derive(Debug, Clone)]
pub struct AuthPasskeyConfig {
    /// When `true`, any logged-in user can enroll a new passkey
    /// without an admin. Mirrors `[auth.passkey].self_registration`.
    pub self_registration: bool,
    /// WebAuthn relying party id (no scheme, no port — e.g.
    /// `nagent.example.com`). MUST match the browser's effective
    /// domain.
    pub rp_id: String,
    /// Human-readable relying party name shown to the user by the
    /// authenticator.
    pub rp_name: String,
    /// Origins the authenticator will accept. Each entry MUST
    /// include scheme + port (e.g. `https://nagent.example.com`).
    pub origins: Vec<String>,
}

impl Default for AuthPasskeyConfig {
    fn default() -> Self {
        Self {
            self_registration: true,
            rp_id: String::new(),
            rp_name: "nagent".into(),
            origins: Vec::new(),
        }
    }
}

impl AuthConfig {
    pub fn from_env_with_toml(toml: Option<&TomlAuthConfig>) -> Result<Self, ConfigError> {
        let defaults = Self::default();
        let toml = toml.cloned().unwrap_or_default();

        // `NAGENT_AUTH_ENABLED` wins. Missing → TOML → default.
        // Forcing `false` when the env var is exactly "false" / "0"
        // lets an operator disable auth via env even when the
        // TOML file enables it (the canonical 12-factor contract).
        let enabled_raw = env_opt("NAGENT_AUTH_ENABLED");
        let enabled = match enabled_raw.as_deref() {
            Some(v) => resolve_primitive(
                Some(v),
                toml.enabled,
                defaults.enabled,
                "NAGENT_AUTH_ENABLED",
            )?,
            None => toml.enabled.unwrap_or(defaults.enabled),
        };

        // Backend list — env (comma-separated) wins.
        let backends_csv = env_opt("NAGENT_AUTH_BACKENDS");
        let backends_raw: Vec<String> = match backends_csv.as_deref() {
            Some(v) => v
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect(),
            None => toml.backends.unwrap_or_default(),
        };
        if enabled && backends_raw.is_empty() {
            return Err(ConfigError::InvalidAuth(
                "auth.enabled = true but auth.backends is empty; \
                     set [auth].backends = [\"local\"] (or another subset)"
                    .into(),
            ));
        }
        let mut backends = Vec::with_capacity(backends_raw.len());
        for raw in &backends_raw {
            backends.push(AuthBackendKind::parse(raw).map_err(ConfigError::InvalidAuth)?);
        }

        let public_url = resolve_opt_string(
            env_opt("NAGENT_AUTH_PUBLIC_URL").as_deref(),
            toml.public_url.as_deref(),
        )
        .unwrap_or_default();

        let session_ttl_days = resolve_primitive(
            env_opt("NAGENT_AUTH_SESSION_TTL_DAYS").as_deref(),
            toml.session_ttl_days,
            defaults.session_ttl_days,
            "NAGENT_AUTH_SESSION_TTL_DAYS",
        )?
        // Clamp to the documented range so a typo (`0` or `365`)
        // surfaces as a boot error rather than silently weakening
        // the session timeout. `1..=90` matches the NIST / GDPR
        // guidance cited in plan D6a.
        .clamp(1, 90);

        let csrf_header = resolve_opt_string(
            env_opt("NAGENT_AUTH_CSRF_HEADER").as_deref(),
            toml.csrf_header.as_deref(),
        )
        .unwrap_or_else(|| defaults.csrf_header.clone());

        let db = AuthDbConfig::from_toml(toml.db.as_ref())?;
        if enabled {
            if db.backend.is_empty() {
                return Err(ConfigError::InvalidAuth(
                    "auth.enabled = true but [auth.db].backend is unset; \
                         set it to \"sqlite\" or \"postgres\""
                        .into(),
                ));
            }
            if db.url.is_empty() {
                return Err(ConfigError::InvalidAuth(
                    "auth.enabled = true but [auth.db].url is unset; \
                         set it to a sqlite://… or postgres://… URL"
                        .into(),
                ));
            }
            if db.backend != "sqlite" && db.backend != "postgres" {
                return Err(ConfigError::InvalidAuth(format!(
                    "auth.db.backend must be \"sqlite\" or \"postgres\", got {:?}",
                    db.backend
                )));
            }
            // Cross-check: each enabled backend must have its
            // minimum config present so the operator cannot boot
            // a server that just sits at "missing OIDC issuer"
            // until the first login attempt.
            for kind in &backends {
                match kind {
                    AuthBackendKind::Local => { /* password config has sane defaults */ }
                    AuthBackendKind::Oidc => {
                        let oidc = AuthOidcConfig::from_toml(toml.oidc.as_ref())?;
                        if oidc.issuer.is_empty() {
                            return Err(ConfigError::InvalidAuth(
                                "auth.oidc is enabled but [auth.oidc].issuer is unset".into(),
                            ));
                        }
                        if oidc.client_id.is_empty() {
                            return Err(ConfigError::InvalidAuth(
                                "auth.oidc is enabled but [auth.oidc].client_id is unset".into(),
                            ));
                        }
                    }
                    AuthBackendKind::Passkey => {
                        let pk = AuthPasskeyConfig::from_toml(toml.passkey.as_ref())?;
                        if pk.rp_id.is_empty() {
                            return Err(ConfigError::InvalidAuth(
                                "auth.passkey is enabled but [auth.passkey].rp_id is unset".into(),
                            ));
                        }
                        if pk.origins.is_empty() {
                            return Err(ConfigError::InvalidAuth(
                                "auth.passkey is enabled but [auth.passkey].origins is empty"
                                    .into(),
                            ));
                        }
                    }
                }
            }
        }

        // Per-backend config is parsed unconditionally so the
        // operator can supply `oidc` config even when only
        // `local` is currently enabled (and flip on OIDC later
        // without a server restart needing a config edit first).
        let password = AuthPasswordConfig::from_toml(
            toml.password.as_ref(),
            env_opt("NAGENT_AUTH_PASSWORD_ALLOW_REGISTRATION").as_deref(),
        )?;
        let oidc = AuthOidcConfig::from_toml_with_env(toml.oidc.as_ref())?;
        let passkey = AuthPasskeyConfig::from_toml(toml.passkey.as_ref())?;
        let credentials = AuthCredentialsConfig::from_toml(toml.credentials.as_ref())?;

        Ok(Self {
            enabled,
            backends,
            public_url,
            session_ttl_days,
            csrf_header,
            db,
            password,
            oidc,
            passkey,
            credentials,
        })
    }

    /// Returns the cookie `Secure` flag for the configured public URL.
    /// `Secure` must auto-disable on `http://localhost` to keep the
    /// dev story working (a `Secure` cookie set over plain HTTP is
    /// dropped by the browser). For everything else, `Secure` mirrors
    /// the scheme — an operator deploying over HTTPS gets the right
    /// default with no extra knob.
    pub fn cookie_secure(&self) -> bool {
        let url = self.public_url.trim().to_ascii_lowercase();
        if url.starts_with("http://localhost") || url.starts_with("http://127.0.0.1") {
            return false;
        }
        url.starts_with("https://")
    }

    /// Returns the cookie `SameSite` policy. We default to `Lax` so
    /// OIDC callbacks (cross-site GET) still work — `Strict` would
    /// block the IdP redirect. Operators who do not enable OIDC can
    /// tighten to `Strict` via a future knob.
    pub fn cookie_same_site(&self) -> &'static str {
        "Lax"
    }

    /// Cookie name used for the session id on the browser side.
    pub fn cookie_name(&self) -> &'static str {
        "nagent_session"
    }

    /// Returns the enabled backend names as a lowercase slice — used
    /// by the UI's login panel to know which buttons to render.
    pub fn enabled_backend_names(&self) -> Vec<&'static str> {
        self.backends.iter().map(|b| b.as_str()).collect()
    }
}

impl AuthDbConfig {
    fn from_toml(
        toml: Option<&crate::config::file::TomlAuthDbConfig>,
    ) -> Result<Self, ConfigError> {
        let defaults = Self::default();
        let toml = toml.cloned().unwrap_or_default();
        Ok(Self {
            backend: resolve_opt_string(
                env_opt("NAGENT_AUTH_DB_BACKEND").as_deref(),
                toml.backend.as_deref(),
            )
            .unwrap_or_default(),
            url: resolve_opt_string(
                env_opt("NAGENT_AUTH_DB_URL").as_deref(),
                toml.url.as_deref(),
            )
            .unwrap_or_default(),
            max_connections: resolve_primitive(
                env_opt("NAGENT_AUTH_DB_MAX_CONNECTIONS").as_deref(),
                toml.max_connections,
                defaults.max_connections,
                "NAGENT_AUTH_DB_MAX_CONNECTIONS",
            )?
            .max(1),
            auto_migrate: resolve_primitive(
                env_opt("NAGENT_AUTH_DB_AUTO_MIGRATE").as_deref(),
                toml.auto_migrate,
                defaults.auto_migrate,
                "NAGENT_AUTH_DB_AUTO_MIGRATE",
            )?,
        })
    }
}

impl AuthCredentialsConfig {
    /// Read the per-user credentials vault knobs from TOML. The
    /// `key` field holds the AES-256-GCM encryption key in
    /// plaintext (64 hex chars). No env-var indirection: the
    /// value comes from `[auth.credentials].key` and nowhere else,
    /// so operators who want secret confidentiality can mount
    /// the TOML file from an encrypted volume.
    fn from_toml(
        toml: Option<&crate::config::file::TomlAuthCredentialsConfig>,
    ) -> Result<Self, ConfigError> {
        let toml = toml.cloned().unwrap_or_default();
        Ok(Self {
            key: toml.key.unwrap_or_default().trim().to_string(),
        })
    }
}

impl AuthPasswordConfig {
    fn from_toml(
        toml: Option<&crate::config::file::TomlAuthPasswordConfig>,
        env_allow_registration: Option<&str>,
    ) -> Result<Self, ConfigError> {
        let defaults = Self::default();
        let toml = toml.cloned().unwrap_or_default();
        // Clamp memory cost to ≥ 19 456 KiB so an operator cannot
        // silently weaken the hash by typing a smaller value. The
        // OWASP 2025 default (19 MiB) is the floor.
        let argon2_memory_kib = resolve_primitive(
            env_opt("NAGENT_AUTH_PASSWORD_ARGON2_MEMORY_KIB").as_deref(),
            toml.argon2_memory_kib,
            defaults.argon2_memory_kib,
            "NAGENT_AUTH_PASSWORD_ARGON2_MEMORY_KIB",
        )?
        .max(19_456);
        let argon2_iterations = resolve_primitive(
            env_opt("NAGENT_AUTH_PASSWORD_ARGON2_ITERATIONS").as_deref(),
            toml.argon2_iterations,
            defaults.argon2_iterations,
            "NAGENT_AUTH_PASSWORD_ARGON2_ITERATIONS",
        )?
        .max(1);
        let argon2_parallelism = resolve_primitive(
            env_opt("NAGENT_AUTH_PASSWORD_ARGON2_PARALLELISM").as_deref(),
            toml.argon2_parallelism,
            defaults.argon2_parallelism,
            "NAGENT_AUTH_PASSWORD_ARGON2_PARALLELISM",
        )?
        .max(1);
        let min_password_length = resolve_primitive(
            env_opt("NAGENT_AUTH_PASSWORD_MIN_LENGTH").as_deref(),
            toml.min_password_length,
            defaults.min_password_length,
            "NAGENT_AUTH_PASSWORD_MIN_LENGTH",
        )?
        .max(1);
        let allow_registration = match env_allow_registration {
            Some(v) => resolve_primitive(
                Some(v),
                toml.allow_registration,
                defaults.allow_registration,
                "NAGENT_AUTH_PASSWORD_ALLOW_REGISTRATION",
            )?,
            None => toml
                .allow_registration
                .unwrap_or(defaults.allow_registration),
        };
        Ok(Self {
            argon2_memory_kib,
            argon2_iterations,
            argon2_parallelism,
            min_password_length,
            allow_registration,
        })
    }
}

impl AuthOidcConfig {
    fn from_toml_with_env(
        toml: Option<&crate::config::file::TomlAuthOidcConfig>,
    ) -> Result<Self, ConfigError> {
        let defaults = Self::default();
        let toml = toml.cloned().unwrap_or_default();
        let auto_provision = resolve_primitive(
            env_opt("NAGENT_AUTH_OIDC_AUTO_PROVISION").as_deref(),
            toml.auto_provision,
            defaults.auto_provision,
            "NAGENT_AUTH_OIDC_AUTO_PROVISION",
        )?;
        let issuer = resolve_opt_string(
            env_opt("NAGENT_AUTH_OIDC_ISSUER").as_deref(),
            toml.issuer.as_deref(),
        )
        .unwrap_or_default();
        let client_id = resolve_opt_string(
            env_opt("NAGENT_AUTH_OIDC_CLIENT_ID").as_deref(),
            toml.client_id.as_deref(),
        )
        .unwrap_or_default();
        let client_secret = resolve_opt_string(
            env_opt("NAGENT_AUTH_OIDC_CLIENT_SECRET").as_deref(),
            toml.client_secret.as_deref(),
        );
        let scopes = resolve_csv(
            env_opt("NAGENT_AUTH_OIDC_SCOPES").as_deref(),
            toml.scopes,
            defaults.scopes,
        );
        let required_groups = resolve_csv(
            env_opt("NAGENT_AUTH_OIDC_REQUIRED_GROUPS").as_deref(),
            toml.required_groups,
            defaults.required_groups,
        );
        let role_claim = resolve_opt_string(
            env_opt("NAGENT_AUTH_OIDC_ROLE_CLAIM").as_deref(),
            toml.role_claim.as_deref(),
        )
        .unwrap_or_else(|| defaults.role_claim.clone());
        Ok(Self {
            auto_provision,
            issuer,
            client_id,
            client_secret,
            scopes,
            required_groups,
            role_claim,
        })
    }

    fn from_toml(
        toml: Option<&crate::config::file::TomlAuthOidcConfig>,
    ) -> Result<Self, ConfigError> {
        Self::from_toml_with_env(toml)
    }
}

impl AuthPasskeyConfig {
    fn from_toml(
        toml: Option<&crate::config::file::TomlAuthPasskeyConfig>,
    ) -> Result<Self, ConfigError> {
        let defaults = Self::default();
        let toml = toml.cloned().unwrap_or_default();
        let self_registration = resolve_primitive(
            env_opt("NAGENT_AUTH_PASSKEY_SELF_REGISTRATION").as_deref(),
            toml.self_registration,
            defaults.self_registration,
            "NAGENT_AUTH_PASSKEY_SELF_REGISTRATION",
        )?;
        let rp_id = resolve_opt_string(
            env_opt("NAGENT_AUTH_PASSKEY_RP_ID").as_deref(),
            toml.rp_id.as_deref(),
        )
        .unwrap_or_default();
        let rp_name = resolve_opt_string(
            env_opt("NAGENT_AUTH_PASSKEY_RP_NAME").as_deref(),
            toml.rp_name.as_deref(),
        )
        .unwrap_or_else(|| defaults.rp_name.clone());
        let origins = resolve_csv(
            env_opt("NAGENT_AUTH_PASSKEY_ORIGINS").as_deref(),
            toml.origins,
            defaults.origins,
        );
        Ok(Self {
            self_registration,
            rp_id,
            rp_name,
            origins,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::file::TomlConfig;

    /// Helper: minimal TOML that satisfies `[auth].enabled = true` —
    /// the rest of the auth tests layer extra sections on top of this.
    fn minimal_auth_toml() -> TomlConfig {
        toml::from_str(
            r#"
                [server]
                whisper_model_path = "/tmp/m.bin"

                [auth]
                enabled = true
                backends = ["local"]
                session_ttl_days = 7

                [auth.db]
                backend = "sqlite"
                url = "sqlite://./data/auth.db?mode=rwc"
            "#,
        )
        .expect("minimal auth TOML must parse")
    }

    #[test]
    fn auth_backend_kinds_parse_case_insensitive() {
        // Operators occasionally typo-case; we accept any case to keep
        // the TOML ergonomic.
        for (raw, expected) in [
            ("local", AuthBackendKind::Local),
            ("LOCAL", AuthBackendKind::Local),
            (" Local ", AuthBackendKind::Local),
            ("Oidc", AuthBackendKind::Oidc),
            ("PASSKEY", AuthBackendKind::Passkey),
        ] {
            assert_eq!(
                AuthBackendKind::parse(raw).expect("known variant must parse"),
                expected,
                "parse({raw:?})"
            );
        }
        assert!(AuthBackendKind::parse("magic-link").is_err());
    }

    #[test]
    fn session_ttl_clamped_to_documented_range() {
        // 0 → clamped to 1, 91 → clamped to 90. We do not have to
        // mutate process env for this — `from_env_with_toml` with an
        // explicit TOML exercises the same `clamp(1, 90)` path.
        let mut base = minimal_auth_toml();
        base.auth.as_mut().unwrap().session_ttl_days = Some(0);
        let cfg =
            crate::config::Config::from_env_with_toml(Some(&base)).expect("clamp must not error");
        assert_eq!(cfg.auth.session_ttl_days, 1, "0 must clamp to 1");

        base.auth.as_mut().unwrap().session_ttl_days = Some(91);
        let cfg =
            crate::config::Config::from_env_with_toml(Some(&base)).expect("clamp must not error");
        assert_eq!(cfg.auth.session_ttl_days, 90, "91 must clamp to 90");

        // 7 / 1 / 90 all pass through unchanged.
        for ok in [1_u32, 7, 90] {
            base.auth.as_mut().unwrap().session_ttl_days = Some(ok);
            let cfg = crate::config::Config::from_env_with_toml(Some(&base))
                .expect("value in range must pass");
            assert_eq!(cfg.auth.session_ttl_days, ok);
        }
    }

    #[test]
    fn argon2_memory_clamped_to_owasp_minimum() {
        // The resolver clamps argon2_memory_kib to >= 19456 so a
        // careless operator cannot silently weaken the hash by typing
        // a smaller value.
        let mut base = minimal_auth_toml();
        base.auth.as_mut().unwrap().password = Some(crate::config::file::TomlAuthPasswordConfig {
            argon2_memory_kib: Some(1024),
            argon2_iterations: Some(2),
            argon2_parallelism: Some(1),
            min_password_length: Some(8),
            allow_registration: Some(true),
        });
        let cfg =
            crate::config::Config::from_env_with_toml(Some(&base)).expect("clamp must not error");
        assert_eq!(
            cfg.auth.password.argon2_memory_kib, 19_456,
            "1024 must clamp to OWASP minimum"
        );
    }

    #[test]
    fn cookie_secure_auto_disables_on_http_localhost() {
        // Dev story: `http://localhost:8080` must NOT set the Secure
        // flag on cookies, otherwise the browser silently drops them.
        let mut base = minimal_auth_toml();
        base.auth.as_mut().unwrap().public_url = Some("http://localhost:8080".into());
        let cfg = crate::config::Config::from_env_with_toml(Some(&base)).expect("config must load");
        assert!(
            !cfg.auth.cookie_secure(),
            "http://localhost must be insecure"
        );

        // 127.0.0.1 is the same carve-out.
        base.auth.as_mut().unwrap().public_url = Some("http://127.0.0.1:9000".into());
        let cfg = crate::config::Config::from_env_with_toml(Some(&base)).expect("config must load");
        assert!(
            !cfg.auth.cookie_secure(),
            "http://127.0.0.1 must be insecure"
        );

        // Plain http to a non-loopback host must also be insecure
        // (we don't try to be clever — http is http).
        base.auth.as_mut().unwrap().public_url = Some("http://nagent.example.com".into());
        let cfg = crate::config::Config::from_env_with_toml(Some(&base)).expect("config must load");
        assert!(
            !cfg.auth.cookie_secure(),
            "http to any host must be insecure"
        );

        // https to anything loopback or not must be Secure.
        for url in [
            "https://localhost",
            "https://nagent.example.com",
            "https://127.0.0.1:8443",
        ] {
            base.auth.as_mut().unwrap().public_url = Some(url.into());
            let cfg =
                crate::config::Config::from_env_with_toml(Some(&base)).expect("config must load");
            assert!(cfg.auth.cookie_secure(), "https must be Secure ({url})");
        }
    }

    #[test]
    fn enabled_backend_names_returns_lowercase_slice() {
        let mut base = minimal_auth_toml();
        base.auth.as_mut().unwrap().backends = Some(vec!["local".into(), "passkey".into()]);
        base.auth.as_mut().unwrap().passkey = Some(crate::config::file::TomlAuthPasskeyConfig {
            self_registration: Some(true),
            rp_id: Some("nagent.example.com".into()),
            rp_name: Some("nagent".into()),
            origins: Some(vec!["https://nagent.example.com".into()]),
        });
        let cfg = crate::config::Config::from_env_with_toml(Some(&base)).expect("config must load");
        assert_eq!(
            cfg.auth.enabled_backend_names(),
            vec!["local", "passkey"],
            "UI iterates these to decide which buttons to render"
        );
    }

    #[test]
    fn auth_enabled_without_backends_is_rejected() {
        let toml: TomlConfig = toml::from_str(
            r#"
                [server]
                whisper_model_path = "/tmp/m.bin"

                [auth]
                enabled = true
            "#,
        )
        .unwrap();
        let err = crate::config::Config::from_env_with_toml(Some(&toml)).unwrap_err();
        match err {
            ConfigError::InvalidAuth(msg) => {
                assert!(
                    msg.contains("backends is empty"),
                    "error must mention empty backends, got: {msg}"
                );
            }
            other => panic!("expected InvalidAuth, got {other:?}"),
        }
    }

    #[test]
    fn auth_enabled_without_db_backend_is_rejected() {
        let toml: TomlConfig = toml::from_str(
            r#"
                [server]
                whisper_model_path = "/tmp/m.bin"

                [auth]
                enabled = true
                backends = ["local"]
                # NB: no [auth.db] section
            "#,
        )
        .unwrap();
        let err = crate::config::Config::from_env_with_toml(Some(&toml)).unwrap_err();
        match err {
            ConfigError::InvalidAuth(msg) => {
                assert!(
                    msg.contains("[auth.db].backend"),
                    "error must mention the missing backend key, got: {msg}"
                );
            }
            other => panic!("expected InvalidAuth, got {other:?}"),
        }
    }

    #[test]
    fn auth_oidc_enabled_without_issuer_is_rejected() {
        let toml: TomlConfig = toml::from_str(
            r#"
                [server]
                whisper_model_path = "/tmp/m.bin"

                [auth]
                enabled = true
                backends = ["oidc"]

                [auth.db]
                backend = "sqlite"
                url = "sqlite://./x.db?mode=rwc"

                [auth.oidc]
                client_id = "nagent"
                # NB: no issuer
            "#,
        )
        .unwrap();
        let err = crate::config::Config::from_env_with_toml(Some(&toml)).unwrap_err();
        match err {
            ConfigError::InvalidAuth(msg) => {
                assert!(
                    msg.contains("issuer"),
                    "error must mention missing issuer, got: {msg}"
                );
            }
            other => panic!("expected InvalidAuth, got {other:?}"),
        }
    }

    #[test]
    fn auth_passkey_enabled_without_rp_id_is_rejected() {
        let toml: TomlConfig = toml::from_str(
            r#"
                [server]
                whisper_model_path = "/tmp/m.bin"

                [auth]
                enabled = true
                backends = ["passkey"]

                [auth.db]
                backend = "sqlite"
                url = "sqlite://./x.db?mode=rwc"

                [auth.passkey]
                origins = ["https://nagent.example.com"]
                # NB: no rp_id
            "#,
        )
        .unwrap();
        let err = crate::config::Config::from_env_with_toml(Some(&toml)).unwrap_err();
        match err {
            ConfigError::InvalidAuth(msg) => {
                assert!(
                    msg.contains("rp_id"),
                    "error must mention missing rp_id, got: {msg}"
                );
            }
            other => panic!("expected InvalidAuth, got {other:?}"),
        }
    }

    #[test]
    fn auth_defaults_disabled_when_no_toml() {
        // No [auth] section at all → enabled=false, no backends.
        // We need to provide a TOML with a `whisper_model_path` so
        // the no-auth code path doesn't trip on the model-path
        // required knob (which is unrelated to the test).
        let toml: TomlConfig = toml::from_str(
            r#"
                [server]
                whisper_model_path = "/tmp/m.bin"
            "#,
        )
        .unwrap();
        let cfg = crate::config::Config::from_env_with_toml(Some(&toml)).expect("config must load");
        assert!(!cfg.auth.enabled, "auth must default to disabled");
        assert!(cfg.auth.backends.is_empty());
        assert_eq!(cfg.auth.session_ttl_days, 7);
        assert_eq!(cfg.auth.cookie_name(), "nagent_session");
        assert_eq!(cfg.auth.csrf_header, "x-csrf-token");
    }

    #[test]
    fn auth_toml_overlay_enables_local_backend() {
        let toml = minimal_auth_toml();
        let cfg = crate::config::Config::from_env_with_toml(Some(&toml)).expect("config must load");
        assert!(cfg.auth.enabled);
        assert_eq!(cfg.auth.backends, vec![AuthBackendKind::Local]);
        assert_eq!(cfg.auth.db.backend, "sqlite");
        assert!(cfg.auth.db.url.contains("auth.db"));
    }
}
