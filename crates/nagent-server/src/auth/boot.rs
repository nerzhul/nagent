//! Auto-bootstrap for the auth subsystem.
//!
//! Called from `main.rs` after `Config::load`. The function:
//!
//! 1. Connects to the auth DB and runs the migrations.
//! 2. **SQLite only**: when the `users` table is empty, mints a
//! random 24-char password for the first local admin and
//! inserts it. The plaintext is logged once at `WARN` level.
//! Postgres deployments skip this — admins must be created via
//! the CLI to avoid silent privilege grants on a shared cluster.
//! 3. Returns the connected `AuthStore` so the rest of the
//! application (CLI subcommands running in-process; HTTP routes
//! for in-process CLI subcommands or hand it to the HTTP layer).
//!
//! The auto-bootstrap is a deliberate ergonomic for local-dev
//! sqlite: a fresh `make run` produces a working admin without
//! requiring a separate `auth create-admin` invocation. Operators
//! who do not want auto-bootstrap can pre-seed the DB via the CLI
//! before starting the server (the auto-bootstrap skips when
//! `count_users_by_provider("local") >= 1`).

use std::sync::Arc;
use std::time::Duration;

use rand::RngCore;

use crate::auth::error::AuthError;
use crate::auth::password;
use crate::config::Config;
use nagent_db::NewAuthEvent;

/// Default bootstrap email when the env var is not set. The
/// `@localhost` domain matches the in-process dev setup; operators
/// can override via `NAGENT_AUTH_BOOTSTRAP_EMAIL`.
const DEFAULT_BOOTSTRAP_EMAIL: &str = "admin@localhost";

/// Default password length for the auto-generated admin. 24 chars
/// gives ~143 bits of entropy which is well past OWASP 2025
/// minimums for human-chosen passwords; the auto-generated one is
/// not human-chosen but we keep the length generous to make copy-paste
/// less error-prone.
const BOOTSTRAP_PASSWORD_LEN: usize = 24;

/// Run the auth DB bootstrap. Connects, migrates, and (for sqlite)
/// auto-creates the first local admin if the users table is
/// empty. Returns the connected store so the caller can use it
/// for in-process CLI subcommands or hand it to the HTTP layer.
///
/// Returns `Ok(None)` when `auth.enabled = false` (the subsystem
/// is silently skipped). Returns `Ok(Some(store))` on success.
/// Errors are fatal — the caller should refuse to boot the server.
pub async fn auto_bootstrap(cfg: &Arc<Config>) -> Result<Option<nagent_db::Db>, anyhow::Error> {
    if !cfg.auth.enabled {
        tracing::info!("auth subsystem disabled (auth.enabled = false); skipping bootstrap");
        return Ok(None);
    }

    tracing::info!(
        backend = %cfg.auth.db.backend,
        url = %redact_url(&cfg.auth.db.url),
        "auth subsystem enabled; connecting to auth DB"
    );

    // Make sure the parent directory exists for sqlite file:// URLs
    // — sqlx's `mode=rwc` opens the file but does NOT create the
    // directory. Operators running sqlite against `~/.local/share/...`
    // would otherwise hit "unable to open database file" on the
    // first run of a fresh host.
    ensure_sqlite_parent_dir(&cfg.auth.db.url)?;

    let opts: nagent_db::DbOptions = (&cfg.auth).into();
    let store = nagent_db::Db::connect(&opts)
        .await
        .map_err(|e| anyhow::anyhow!("auth DB connect failed: {e}"))?;
    // Boot-time migration is gated on `[auth.db].auto_migrate` (env
    // `NAGENT_AUTH_DB_AUTO_MIGRATE`). When false the operator is
    // expected to run `stt-server migrate up` separately — useful for
    // init containers in Kubernetes or pre-deploy hooks in CI. The
    // CLI subcommands (`auth create-admin`, etc.) do NOT honour this
    // gate: they always connect + migrate via
    // `crate::cli::auth::open_store` so a one-shot admin creation never
    // fails on an unmigrated DB.
    if cfg.auth.db.auto_migrate {
        store
            .migrate()
            .await
            .map_err(|e| anyhow::anyhow!("auth migrations failed: {e}"))?;
        tracing::info!("auth migrations applied (auto)");
    } else {
        tracing::warn!(
            backend = %cfg.auth.db.backend,
            "auth.db.auto_migrate = false; skipping boot-time migrations. \
             Run `stt-server migrate up` before starting the server."
        );
    }

    // SQLite-only auto-bootstrap: a fresh sqlite install starts with
    // an empty users table. Generate a strong random password, hash
    // it, and insert the admin. The plaintext is logged once at
    // WARN level so it shows up regardless of the operator's log
    // filter (most RUST_LOG defaults are at INFO so WARN is the
    // right level for a one-time secret).
    //
    // Skip when `auto_migrate = false` — the `users` table may not
    // exist yet (the operator is expected to run `stt-server
    // migrate up` themselves before boot) so the count query would
    // error out and the admin insert would fail. Operators in this
    // mode create the first admin via `auth create-admin` after
    // running the migrations.
    if cfg.auth.db.backend == "sqlite" && cfg.auth.db.auto_migrate {
        let local_count = store
            .users
            .count_by_provider("local")
            .await
            .map_err(|e| anyhow::anyhow!("count local users: {e}"))?;
        if local_count == 0 {
            bootstrap_first_admin(&store, cfg).await?;
        }
    } else if cfg.auth.db.backend == "sqlite" {
        tracing::warn!(
            "auth.db.auto_migrate = false; skipping first-admin auto-bootstrap. \
             After running `stt-server migrate up`, create the first admin with \
             `stt-server auth create-admin`."
        );
    } else {
        tracing::info!(
            "auth DB is postgres-backed; skipping auto-bootstrap (use `stt-server auth create-admin` instead)"
        );
    }

    Ok(Some(store))
}

/// Best-effort `mkdir -p` for the parent of a sqlite file:// URL.
/// No-op for `file::memory:` / `:memory:` / postgres URLs.
fn ensure_sqlite_parent_dir(url: &str) -> Result<(), anyhow::Error> {
    // sqlx accepts forms like `sqlite::memory:`, `sqlite://:memory:`,
    // `sqlite://./data/auth.db?mode=rwc`, etc. We extract the path
    // part between the scheme and either `?` or end-of-string.
    let path_part = if let Some(rest) = url.strip_prefix("sqlite://") {
        rest.split('?').next().unwrap_or("")
    } else if let Some(rest) = url.strip_prefix("sqlite:") {
        rest.split('?').next().unwrap_or("")
    } else {
        return Ok(());
    };
    if path_part.is_empty() || path_part == ":memory:" {
        return Ok(());
    }
    // Strip leading slashes so an absolute path becomes a real
    // absolute path (sqlite URL semantics: `sqlite:///foo` means
    // `/foo`, not `foo` under the cwd).
    let path = path_part.trim_start_matches('/');
    if path.is_empty() {
        return Ok(());
    }
    let p = std::path::Path::new(path);
    if let Some(parent) = p.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|e| {
                anyhow::anyhow!(
                    "failed to create sqlite parent dir {}: {e}",
                    parent.display()
                )
            })?;
        }
    }
    Ok(())
}

async fn bootstrap_first_admin(store: &nagent_db::Db, cfg: &Config) -> Result<(), anyhow::Error> {
    let email = std::env::var("NAGENT_AUTH_BOOTSTRAP_EMAIL")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_BOOTSTRAP_EMAIL.to_string());
    let password = random_password(BOOTSTRAP_PASSWORD_LEN);

    let hash = password::hash_password(
        &password,
        cfg.auth.password.argon2_memory_kib,
        cfg.auth.password.argon2_iterations,
        cfg.auth.password.argon2_parallelism,
    )
    .map_err(|e| anyhow::anyhow!("hash bootstrap password: {e}"))?;

    let user_id = store
        .users
        .create(&email, &email, "local", Some(&hash))
        .await
        .map_err(|e| anyhow::anyhow!("create bootstrap user: {e}"))?;
    store.events.record(NewAuthEvent::auth(
        Some(user_id),
        "bootstrap_admin",
        "local",
    ));

    // The password is shown EXACTLY ONCE — on a fresh install with
    // an empty `users` table. Two channels:
    //
    // 1. `eprintln!` straight to stderr. Bypasses every tracing
    // filter / formatter / log aggregator on the way to the
    // operator's terminal, journald, docker logs, or systemd
    // journal. The multi-line banner is deliberately loud so
    // it cannot scroll past unnoticed in a long boot log.
    // 2. `tracing::warn!` for log aggregators that ingest the
    // structured fields. WARN is the right level: it shows up
    // on a default `RUST_LOG=info` filter (the most common
    // mistake on a fresh install is to set RUST_LOG=warn or
    // higher, which would silence the WARN; we keep the
    // eprintln! for that case).
    eprintln!(
        "\n\
         ================================================================\n\
         \x20AUTH BOOTSTRAP: created first local admin\n\
         \x20\n\
         \x20  Email:    {email}\n\
         \x20  Password: {password}\n\
         \x20\n\
         \x20  SAVE THIS PASSWORD NOW — it will NOT be shown again.\n\
         \x20  Rotate it with:\n\
         \x20    stt-server auth create-admin --email <other> --from-stdin\n\
         \x20  (then delete this user, or change its password via the\n\
         \x20   HTTP API: POST /api/auth/login/password, then a future\n\
         \x20   password-change endpoint).\n\
         ================================================================\n"
    );
    tracing::warn!(
        user_id = %user_id,
        email = %email,
        password = %password,
        "auth bootstrap: created first local admin — SAVE THIS PASSWORD NOW, it will not be logged again. \
         Rotate it with `stt-server auth create-admin --email <other>` or via the HTTP API \
         (POST /api/auth/login/password then a future password-change endpoint)."
    );
    Ok(())
}

/// Generate a random alphanumeric password. Uses the OS RNG
/// (`rand::rngs::OsRng`) so it is cryptographically secure on
/// every supported platform. The character set excludes `0`, `O`,
/// `I`, `l` so the password is copy-paste friendly.
fn random_password(len: usize) -> String {
    random_password_for_test(len)
}

/// Test-only public wrapper. The implementation lives next to the
/// private version so tests can assert the charset + length without
/// needing to capture `tracing` output.
pub fn random_password_for_test(len: usize) -> String {
    const CHARSET: &[u8] = b"abcdefghijkmnopqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ123456789";
    let mut rng = rand::rngs::OsRng;
    let mut bytes = vec![0u8; len];
    rng.fill_bytes(&mut bytes);
    bytes
        .iter()
        .map(|b| CHARSET[(*b as usize) % CHARSET.len()] as char)
        .collect()
}

/// Small wrapper used by main.rs to format a clean error message
/// on bootstrap failure without pulling in anyhow at the call site.
pub fn format_bootstrap_err(e: &anyhow::Error) -> String {
    format!("auth bootstrap failed: {e}")
}

// Pull `AuthError` into scope so the `?` operator in callers can
// implicitly convert when they `map_err` a `Result<_, AuthError>`
// into an `anyhow::Result`. Currently unused — kept here as a
// deliberate marker so future refactors don't accidentally remove
// the import.
#[allow(dead_code)]
fn _silence_unused(_: AuthError, _: Duration) {}

/// Redact a DB connection URL for safe logging. SQLite URLs are
/// plain file paths (or `:memory:`); we print them as-is so the
/// operator sees exactly where the auth DB lives. Postgres URLs
/// carry the password inline — we replace the password component
/// with `***` so the secret never lands in `journalctl` or
/// `kubectl logs`.
pub fn redact_url(url: &str) -> String {
    // sqlite://host/path?query  or  sqlite:host:path  or  sqlite::memory:
    // — never carries credentials, so return as-is.
    if url.starts_with("sqlite:") {
        return url.to_string();
    }
    // Postgres URLs look like:
    // postgres://user:pwd@host:port/dbname?query
    // Replace the `pwd` component with `***` while keeping user/host/db
    // visible so the operator can confirm which DB the server is
    // talking to without exposing the secret.
    if let Some(scheme_end) = url.find("://") {
        let (scheme, rest) = url.split_at(scheme_end + 3);
        if let Some(at_idx) = rest.find('@') {
            let userinfo = &rest[..at_idx];
            let after_at = &rest[at_idx..];
            // Only redact if there IS a password component. URLs
            // without a password (trust auth, peer auth) return
            // as-is so we don't accidentally introduce a misleading
            // `:` in the log line.
            if let Some((user, _pwd)) = userinfo.split_once(':') {
                return format!("{}{}:***{}", scheme, user, after_at);
            } else {
                return url.to_string();
            }
        }
    }
    url.to_string()
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;

    use crate::config::Config;
    use crate::config::{AuthBackendKind, AuthConfig, AuthDbConfig};

    fn sqlite_config(name: &str) -> Config {
        let dir = std::env::temp_dir().join("nagent-auth-boot-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{name}.db"));
        // Force a fresh DB by removing any prior copy.
        let _ = std::fs::remove_file(&path);
        Config {
            whisper_model_path: PathBuf::from("/tmp/m.bin"),
            bind_addr: "127.0.0.1:0".parse().unwrap(),
            max_queue: 1,
            inference_workers: Some(1),
            session_idle_timeout: std::time::Duration::from_secs(1),
            infer_timeout: std::time::Duration::from_secs(1),
            limits: Default::default(),
            rate_limit: Default::default(),
            trusted_proxies: Default::default(),
            llm: Default::default(),
            agents: Default::default(),
            tts: Default::default(),
            auth: AuthConfig {
                enabled: true,
                backends: vec![AuthBackendKind::Local],
                public_url: "http://127.0.0.1:0".into(),
                session_ttl_days: 7,
                csrf_header: "x-csrf-token".into(),
                db: AuthDbConfig {
                    backend: "sqlite".into(),
                    url: format!("sqlite://{}", path.display()),
                    max_connections: 1,
                    auto_migrate: true,
                },
                password: Default::default(),
                oidc: Default::default(),
                passkey: Default::default(),
                credentials: Default::default(),
            },
            documents: Default::default(),
        }
    }

    #[tokio::test]
    async fn auto_bootstrap_disabled_returns_none_without_db() {
        // `auth.enabled = false` should short-circuit before any
        // connection attempt. A bogus DB URL that would normally
        // fail on `connect()` proves the early return.
        let mut cfg = sqlite_config("disabled");
        cfg.auth.db.url = "sqlite:///this/path/does/not/exist/foo.db?mode=rwc".into();
        cfg.auth.enabled = false;
        let store = crate::auth::boot::auto_bootstrap(&Arc::new(cfg))
            .await
            .expect("disabled must be a no-op");
        assert!(store.is_none());
    }

    #[tokio::test]
    async fn auto_bootstrap_sqlite_creates_first_admin() {
        let cfg = sqlite_config("first-admin");
        let cfg = Arc::new(cfg);
        let store = crate::auth::boot::auto_bootstrap(&cfg)
            .await
            .expect("sqlite bootstrap must succeed")
            .expect("auth.enabled = true so store must be Some");
        // The auto-bootstrap should have created exactly one local user.
        let users = store.users.list(Some("local")).await.unwrap();
        assert_eq!(users.len(), 1, "exactly one local admin");
        let admin = &users[0];
        assert!(admin.password_hash.is_some(), "hash must be present");
    }

    #[tokio::test]
    async fn auto_bootstrap_sqlite_skips_when_admin_exists() {
        // Pre-create one local user, then run bootstrap. It must
        // NOT generate a second admin.
        let cfg = sqlite_config("skip");
        let cfg = Arc::new(cfg.clone());
        // Connect + migrate manually.
        let opts: nagent_db::DbOptions = (&cfg.auth).into();
        let store = nagent_db::Db::connect(&opts).await.unwrap();
        store.migrate().await.unwrap();
        let hash = b"pre-existing-argon2id-blob".to_vec();
        store
            .users
            .create("alice@example.com", "Alice", "local", Some(&hash))
            .await
            .unwrap();
        // Re-run bootstrap. It should not create a second admin.
        crate::auth::boot::auto_bootstrap(&cfg)
            .await
            .expect("bootstrap must succeed");
        let users = store.users.list(Some("local")).await.unwrap();
        assert_eq!(
            users.len(),
            1,
            "bootstrap must skip when a local user already exists"
        );
        assert_eq!(users[0].email, "alice@example.com");
    }

    #[tokio::test]
    async fn auto_bootstrap_skips_migrations_when_auto_migrate_false() {
        // Fresh sqlite DB + `auto_migrate = false`. Boot must still
        // succeed (the DB is opened for the auto-bootstrap admin
        // path) and `_sqlx_migrations` must remain absent so the
        // next `migrate up` correctly reports "all pending".
        let cfg = sqlite_config("no-auto-migrate");
        let mut cfg = cfg;
        cfg.auth.db.auto_migrate = false;
        let cfg = Arc::new(cfg);
        let store = crate::auth::boot::auto_bootstrap(&cfg)
            .await
            .expect("bootstrap must succeed even with auto_migrate = false")
            .expect("auth.enabled = true so store must be Some");

        // `_sqlx_migrations` must NOT exist on the DB. We assert
        // by asking the store for a fresh status — the helper
        // returns the "all pending" sentinel when the table is
        // missing, so the applied set must be empty.
        let status = store.migration_status().await.expect("status");
        assert!(
            status.applied.is_empty(),
            "auto_migrate = false must not apply migrations; got applied = {:?}",
            status.applied
        );
        assert!(
            status.highest_applied.is_none(),
            "fresh DB without auto_migrate must report no applied migrations"
        );
    }

    #[test]
    fn random_password_has_expected_length_and_charset() {
        let p = crate::auth::boot::random_password_for_test(24);
        assert_eq!(p.len(), 24);
        assert!(p.chars().all(|c| c.is_ascii_alphanumeric()));
        // Two consecutive generations must differ.
        let p2 = crate::auth::boot::random_password_for_test(24);
        assert_ne!(p, p2, "OS RNG must produce distinct passwords");
    }

    #[test]
    fn redact_url_passes_sqlite_through() {
        // Sqlite URLs carry no credentials — must round-trip as-is.
        let url = "sqlite:///home/nerzhul/.local/share/nagent/auth.db?mode=rwc";
        assert_eq!(super::redact_url(url), url);
        assert_eq!(super::redact_url("sqlite::memory:"), "sqlite::memory:");
        assert_eq!(
            super::redact_url("sqlite:./data/auth.db"),
            "sqlite:./data/auth.db"
        );
    }

    #[test]
    fn redact_url_redacts_postgres_password() {
        let url = "postgres://nagent:s3cr3t@db.internal:5432/nagent";
        assert_eq!(
            super::redact_url(url),
            "postgres://nagent:***@db.internal:5432/nagent"
        );
        // No password (e.g. trust auth) — must not be touched.
        assert_eq!(
            super::redact_url("postgres://nagent@db.internal/nagent"),
            "postgres://nagent@db.internal/nagent"
        );
    }

    #[test]
    fn redact_url_handles_query_string() {
        let url = "postgres://u:pwd@host:5432/db?sslmode=require&application_name=app";
        assert_eq!(
            super::redact_url(url),
            "postgres://u:***@host:5432/db?sslmode=require&application_name=app"
        );
    }
}
