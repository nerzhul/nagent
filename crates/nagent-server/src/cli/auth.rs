//! `stt-server auth …` — operator CLI for the auth subsystem.
//!
//! Runs against the same `[auth.db]` configuration the server uses,
//! without needing the HTTP server to be running. Designed for
//! first-install bootstrap (`create-admin`) and recovery
//! (`delete-user`) over SSH.
//!
//! Argv parsing is driven by `clap` derive (plan
//! `1792262220000-clap-derive-migration`). Each subcommand owns a
//! `#[derive(Parser)]` variant of [`AuthAction`]; the binary's
//! `--help` / unknown-flag errors come from clap itself.
//!
//! Subcommands:
//!
//! ```text
//! stt-server auth create-admin --email <email> [--password <pwd> | --from-stdin] [--force]
//! stt-server auth list-users  [--provider local|oidc|passkey] [--force]
//! stt-server auth delete-user --email <email> [--yes] [--force]
//! ```

use std::path::Path;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

use crate::cli::shared::{check_running_server, ensure_sqlite_parent_dir};
use crate::config::Config;

/// Top-level `auth` subcommand. Holds a single sub-action so
/// clap parses `stt-server auth <action> ...` cleanly.
#[derive(Debug, Parser)]
#[command(
    about = "Operator CLI for the auth subsystem",
    long_about = "Operator CLI for the auth subsystem.\n\
                  Runs against the same `[auth.db]` configuration the server uses, \
                  without booting the HTTP server."
)]
pub struct AuthCmd {
    #[command(subcommand)]
    pub action: AuthAction,
}

#[derive(Debug, Subcommand)]
pub enum AuthAction {
    /// Create the first local admin user. Refuses if the email
    /// already exists; bypasses `password.allow_registration`.
    CreateAdmin {
        #[arg(long)]
        email: String,
        /// Read the password from the next argv slot instead of
        /// stdin. Mutually exclusive with `--from-stdin`.
        #[arg(long, conflicts_with = "from_stdin")]
        password: Option<String>,
        /// Read the password from stdin (one line) so it never
        /// touches argv / shell history.
        #[arg(long)]
        from_stdin: bool,
        #[arg(long)]
        force: bool,
    },
    /// Tab-separated list of users (`id<TAB>email<TAB>provider<TAB>created_at`).
    ListUsers {
        /// Restrict to one provider kind.
        #[arg(long, value_name = "KIND")]
        provider: Option<String>,
        #[arg(long)]
        force: bool,
    },
    /// Remove a user + cascade to their sessions + passkeys.
    DeleteUser {
        #[arg(long)]
        email: String,
        #[arg(long)]
        yes: bool,
        #[arg(long)]
        force: bool,
    },
}

/// Dispatch the parsed [`AuthCmd`]. Returns `Ok(exit_code)` so
/// the binary returns the right status to the shell on each
/// subcommand.
pub async fn run(cmd: AuthCmd, config: Option<&Path>) -> Result<ExitCode, anyhow::Error> {
    match cmd.action {
        AuthAction::CreateAdmin {
            email,
            password,
            from_stdin,
            force,
        } => create_admin(email, password, from_stdin, force, config).await,
        AuthAction::ListUsers { provider, force } => list_users(provider, force, config).await,
        AuthAction::DeleteUser { email, yes, force } => {
            delete_user(email, yes, force, config).await
        }
    }
}

/// Connect + migrate the auth DB, ensuring the sqlite parent
/// directory exists first. Used by every CLI subcommand that
/// touches the DB so the operator does not have to `mkdir -p
/// ./data/` before running the very first command.
async fn open_store(cfg: &Config) -> Result<nagent_db::Db, anyhow::Error> {
    ensure_sqlite_parent_dir(&cfg.auth.db.url)?;
    let opts: nagent_db::DbOptions = (&cfg.auth).into();
    let db = nagent_db::Db::connect(&opts)
        .await
        .map_err(|e| anyhow::anyhow!("auth DB connect failed: {e}"))?;
    db.migrate()
        .await
        .map_err(|e| anyhow::anyhow!("auth migrations failed: {e}"))?;
    Ok(db)
}

#[allow(clippy::too_many_arguments)]
async fn create_admin(
    email: String,
    password: Option<String>,
    from_stdin: bool,
    force: bool,
    config: Option<&Path>,
) -> Result<ExitCode, anyhow::Error> {
    check_running_server(force)?;
    let cfg = load_config(config)?;
    if !cfg.auth.enabled {
        return Err(anyhow::anyhow!(
            "auth is not enabled in the active configuration; \
             set [auth].enabled = true (or NAGENT_AUTH_ENABLED=true)"
        ));
    }
    let password = if from_stdin {
        read_password_from_stdin()?
    } else {
        password.ok_or_else(|| anyhow::anyhow!("either --password or --from-stdin is required"))?
    };

    if password.len() < cfg.auth.password.min_password_length {
        return Err(anyhow::anyhow!(
            "password must be at least {} characters",
            cfg.auth.password.min_password_length
        ));
    }

    let db = open_store(&cfg).await?;

    if let Some(existing) = db.admin().users.get_by_email(&email).await? {
        return Err(anyhow::anyhow!(
            "a user with email {:?} already exists (id = {})",
            existing.email,
            existing.id
        ));
    }

    // Plan R1a: route the CLI create-admin hash through
    // `spawn_blocking` so the CLI process does not freeze on a
    // tens-of-MiB argon2 run (single-threaded CLIs are especially
    // sensitive).
    let cfg_snapshot = cfg.clone();
    let password_for_blocking = password.clone();
    let hash = tokio::task::spawn_blocking(move || {
        crate::auth::password::hash_password(
            &password_for_blocking,
            cfg_snapshot.auth.password.argon2_memory_kib,
            cfg_snapshot.auth.password.argon2_iterations,
            cfg_snapshot.auth.password.argon2_parallelism,
        )
    })
    .await
    .map_err(|e| anyhow::anyhow!("hash_password join: {e}"))?
    .map_err(|e| anyhow::anyhow!("hash_password failed: {e}"))?;

    let user_id = db
        .admin()
        .users
        .create(&email, &email, "local", Some(&hash))
        .await
        .map_err(|e| anyhow::anyhow!("create_user failed: {e}"))?;
    db.admin().events.record(nagent_db::NewAuthEvent::auth(
        Some(user_id),
        "create_admin",
        "local",
    ));

    println!("{user_id}");
    Ok(ExitCode::from(0))
}

/// Read one line from stdin, trimming the trailing newline. Used
/// for `--from-stdin` so the password never goes through argv /
/// shell history.
fn read_password_from_stdin() -> Result<String, anyhow::Error> {
    use std::io::Read;
    let mut s = String::new();
    std::io::stdin().read_to_string(&mut s)?;
    Ok(s.trim_end_matches(['\n', '\r']).to_string())
}

async fn list_users(
    provider: Option<String>,
    force: bool,
    config: Option<&Path>,
) -> Result<ExitCode, anyhow::Error> {
    check_running_server(force)?;
    let cfg = load_config(config)?;
    let db = open_store(&cfg).await?;
    let users = db.admin().users.list(provider.as_deref()).await?;
    for u in users {
        println!(
            "{}\t{}\t{}\t{}",
            u.id,
            u.email,
            u.provider,
            u.created_at.to_rfc3339(),
        );
    }
    Ok(ExitCode::from(0))
}

async fn delete_user(
    email: String,
    yes: bool,
    force: bool,
    config: Option<&Path>,
) -> Result<ExitCode, anyhow::Error> {
    check_running_server(force)?;
    let cfg = load_config(config)?;
    let db = open_store(&cfg).await?;
    let target = db
        .admin()
        .users
        .get_by_email(&email)
        .await?
        .ok_or_else(|| anyhow::anyhow!("no user with email {email:?}"))?;

    // Belt + braces: refuse if it would leave zero local users and
    // auth is enabled. The role check follows; for now, a hard
    // "1 local user minimum" guard is enough to prevent the
    // lockout class.
    if target.provider == "local" && cfg.auth.enabled {
        let local_count = db.admin().users.count_by_provider("local").await?;
        if local_count <= 1 {
            return Err(anyhow::anyhow!(
                "refusing to delete the last local user while auth is enabled; \
                 create another local user first to avoid lockout"
            ));
        }
    }

    if !yes {
        eprint!(
            "delete user {} ({}, {})? [y/N] ",
            target.id, target.email, target.provider
        );
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        if !line.trim().eq_ignore_ascii_case("y") {
            eprintln!("aborted");
            return Ok(ExitCode::from(1));
        }
    }

    db.admin().users.delete(target.id).await?;
    println!("deleted {}", target.id);
    Ok(ExitCode::from(0))
}

/// Load `Config` from an optional explicit `--config PATH`. When
/// `None`, [`Config::load`] falls back to the layered
/// `/etc/nagent/config.toml` + `$XDG_CONFIG_HOME/nagent/config.toml`
/// discovery.
fn load_config(path: Option<&Path>) -> Result<Config, anyhow::Error> {
    Config::load(path).map_err(|e| anyhow::anyhow!("{e}"))
}
