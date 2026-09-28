//! `stt-server auth …` — operator CLI for the auth subsystem.
//!
//! Runs against the same `[auth.db]` configuration the server uses,
//! without needing the HTTP server to be running. Designed for
//! first-install bootstrap (`create-admin`) and recovery
//! (`delete-user`) over SSH.
//!
//! Hand-rolled flag parsing (matches `CliArgs::parse` style — no
//! `clap` dep). The commands refuse to start when a PID file at
//! `data/server.pid` exists AND points at a live process, unless
//! the operator passes `--force`. The PID file is written by
//! `main` and removed on a clean shutdown (signal handler).
//!
//! Argparse grammar (one command per invocation):
//!
//! ```text
//! stt-server auth create-admin --email <email> [--password <pwd> | --from-stdin] [--force]
//! stt-server auth list-users  [--provider local|oidc|passkey] [--force]
//! stt-server auth delete-user --email <email> [--yes] [--force]
//! ```

use crate::auth::error::AuthError;
use crate::config::{CliArgs, Config};
use std::path::PathBuf;
use std::process::ExitCode;

/// Top-level dispatch: called from `main` when `argv[1] == "auth"`.
/// Returns `Ok(exit_code)` so the binary returns the right status
/// to the shell on each subcommand.
///
/// `args` is the full argv after the binary name. It MAY include
/// global flags (`--config FOO`) before the literal `auth` token
/// when the operator used `stt-server --config FOO auth …`, so
/// we split out the global flags first then strip the `auth`
/// literal before looking at the subcommand name.
pub async fn run_auth_cli(args: Vec<String>) -> Result<ExitCode, anyhow::Error> {
    let (cli_args, rest) = split_global_flags(args);
    // Strip the `auth` literal if present.
    let mut rest = rest;
    if rest.first().map(|s| s.as_str()) == Some("auth") {
        rest.remove(0);
    }
    let mut iter = rest.into_iter();
    let sub = iter.next().unwrap_or_default();
    let result = match sub.as_str() {
        "create-admin" => create_admin(iter.collect(), &cli_args).await,
        "list-users" => list_users(iter.collect(), &cli_args).await,
        "delete-user" => delete_user(iter.collect(), &cli_args).await,
        "help" | "--help" | "-h" | "" => {
            print_help();
            return Ok(ExitCode::from(0));
        }
        other => {
            eprintln!("error: unknown auth subcommand: {other:?}");
            eprintln!("(run `stt-server auth help` for usage)");
            return Ok(ExitCode::from(2));
        }
    };
    result
}

/// Extract the `--config <path>` flag pair (if any) from the auth
/// CLI argv. Returns the parsed [`CliArgs`] plus the remainder
/// without the global flags — the subcommand-specific parsers
/// then operate on a clean slice.
fn split_global_flags(args: Vec<String>) -> (crate::config::CliArgs, Vec<String>) {
    let mut cli = crate::config::CliArgs::default();
    let mut rest = Vec::with_capacity(args.len());
    let mut iter = args.into_iter();
    while let Some(a) = iter.next() {
        if a == "--config" {
            if let Some(v) = iter.next() {
                cli.config = Some(std::path::PathBuf::from(v));
            } else {
                eprintln!("error: --config requires a path argument");
                std::process::exit(2);
            }
        } else if let Some(v) = a.strip_prefix("--config=") {
            cli.config = Some(std::path::PathBuf::from(v));
        } else {
            rest.push(a);
        }
    }
    (cli, rest)
}

/// Best-effort `mkdir -p` for the parent of a sqlite file:// URL.
/// Mirrors the same helper in `auth::boot::ensure_sqlite_parent_dir`
/// so the CLI works on a fresh host even when the operator pointed
/// it at a relative path under `./data/`. The server's `auto_bootstrap`
/// already does this — we duplicate the logic here rather than
/// export the boot helper because the CLI should not need to know
/// about the boot internals.
fn ensure_sqlite_parent_dir(url: &str) -> Result<(), anyhow::Error> {
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

/// Connect + migrate the auth DB, ensuring the sqlite parent
/// directory exists first. Used by every CLI subcommand that
/// touches the DB so the operator does not have to `mkdir -p
/// ./data/` before running the very first command.
async fn open_store(
    cfg: &crate::config::Config,
) -> Result<crate::auth::store::AuthStore, anyhow::Error> {
    ensure_sqlite_parent_dir(&cfg.auth.db.url)?;
    let store = crate::auth::store::AuthStore::connect(&cfg.auth)
        .await
        .map_err(|e| anyhow::anyhow!("auth DB connect failed: {e}"))?;
    store
        .migrate()
        .await
        .map_err(|e| anyhow::anyhow!("auth migrations failed: {e}"))?;
    Ok(store)
}

fn print_help() {
    println!("stt-server auth — operator CLI for the auth subsystem");
    println!();
    println!("USAGE:");
    println!("    stt-server auth <subcommand> [options]");
    println!();
    println!("SUBCOMMANDS:");
    println!("    create-admin  Create the first local admin user.");
    println!("                  Refuses if the email already exists. Bypasses");
    println!("                  password.allow_registration. Reads the password");
    println!("                  from --password <pwd> or --from-stdin (one line).");
    println!();
    println!("    list-users    Tab-separated list: id<TAB>email<TAB>provider<TAB>created_at.");
    println!("                  Optional --provider local|oidc|passkey prefix filter.");
    println!();
    println!("    delete-user   Remove a user + cascade to their sessions + passkeys.");
    println!("                  Refuses if it would leave zero local users AND");
    println!("                  auth.enabled. Pass --yes to skip the interactive prompt.");
    println!();
    println!("GLOBAL OPTIONS:");
    println!("    --force       Run even when a server PID file is present (data/server.pid).");
}

#[derive(Debug, Default)]
#[allow(dead_code)]
struct CommonOpts {
    force: bool,
}

#[allow(dead_code)]
fn parse_common(args: &[String]) -> (CommonOpts, Vec<String>) {
    let mut opts = CommonOpts::default();
    let mut rest = Vec::new();
    for a in args {
        if a == "--force" {
            opts.force = true;
        } else {
            rest.push(a.clone());
        }
    }
    (opts, rest)
}

/// Refuse to run if the server PID file exists AND points at a live
/// process AND `--force` was not supplied.
fn check_running_server(force: bool) -> Result<(), anyhow::Error> {
    let pid_path = pid_file_path();
    if !pid_path.exists() {
        return Ok(());
    }
    let pid = match std::fs::read_to_string(&pid_path) {
        Ok(s) => s.trim().to_string(),
        Err(e) => {
            return Err(anyhow::anyhow!(
                "could not read PID file at {}: {e}",
                pid_path.display()
            ))
        }
    };
    let pid_num: u64 = match pid.parse() {
        Ok(n) => n,
        Err(_) => {
            // Stale PID file — silently allow the CLI to proceed.
            return Ok(());
        }
    };
    if !force {
        // `kill -0 <pid>` is the unix way to check whether a pid
        // is alive without actually sending a signal. We run it
        // via `nix`-free stdlib by spawning `kill` itself; the
        // exit status 0 = alive, nonzero = gone.
        let status = std::process::Command::new("kill")
            .arg("-0")
            .arg(pid_num.to_string())
            .status();
        match status {
            Ok(s) if s.success() => {
                return Err(anyhow::anyhow!(
                    "a server is already running (PID {} from {}); pass --force to override",
                    pid_num,
                    pid_path.display()
                ));
            }
            _ => {
                // PID gone (process exited without removing the
                // file). Silently allow the CLI to proceed.
            }
        }
    }
    Ok(())
}

fn pid_file_path() -> PathBuf {
    // Path is relative to the current working directory to match
    // `data/` everywhere else in the project. A future PR may move
    // this to a configurable knob if the operator deploys with a
    // non-standard layout.
    PathBuf::from("data").join("server.pid")
}

// ---- create-admin ----------------------------------------------------------

#[derive(Debug, Default)]
struct CreateAdminOpts {
    email: Option<String>,
    password: Option<String>,
    from_stdin: bool,
    force: bool,
}

fn parse_create_admin(args: Vec<String>) -> Result<CreateAdminOpts, anyhow::Error> {
    let mut o = CreateAdminOpts::default();
    let mut iter = args.into_iter();
    while let Some(a) = iter.next() {
        match a.as_str() {
            "--email" => {
                o.email = Some(
                    iter.next()
                        .ok_or_else(|| anyhow::anyhow!("--email requires a value"))?,
                );
            }
            "--password" => {
                o.password = Some(
                    iter.next()
                        .ok_or_else(|| anyhow::anyhow!("--password requires a value"))?,
                );
            }
            "--from-stdin" => o.from_stdin = true,
            "--force" => o.force = true,
            "--help" | "-h" => {
                println!(
                    "stt-server auth create-admin --email <email> \
                     [--password <pwd> | --from-stdin] [--force]"
                );
                std::process::exit(0);
            }
            other => return Err(anyhow::anyhow!("unknown create-admin flag: {other:?}")),
        }
    }
    Ok(o)
}

async fn create_admin(args: Vec<String>, cli: &CliArgs) -> Result<ExitCode, anyhow::Error> {
    let opts = parse_create_admin(args)?;
    check_running_server(opts.force)?;

    let cfg = Config::load(cli)?;
    if !cfg.auth.enabled {
        return Err(anyhow::anyhow!(
            "auth is not enabled in the active configuration; set [auth].enabled = true (or NAGENT_AUTH_ENABLED=true)"
        ));
    }
    let email = opts
        .email
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("--email is required"))?;
    let password = if opts.from_stdin {
        read_password_from_stdin()?
    } else {
        opts.password
            .ok_or_else(|| anyhow::anyhow!("either --password or --from-stdin is required"))?
    };

    if password.len() < cfg.auth.password.min_password_length {
        return Err(anyhow::anyhow!(
            "password must be at least {} characters",
            cfg.auth.password.min_password_length
        ));
    }

    let store = open_store(&cfg).await?;

    if let Some(existing) = store.get_user_by_email(email).await? {
        return Err(anyhow::anyhow!(
            "a user with email {:?} already exists (id = {})",
            existing.email,
            existing.id
        ));
    }

    let hash = crate::auth::password::hash_password(
        &password,
        cfg.auth.password.argon2_memory_kib,
        cfg.auth.password.argon2_iterations,
        cfg.auth.password.argon2_parallelism,
    )
    .map_err(|e| anyhow::anyhow!("hash_password failed: {e}"))?;

    let user_id = store
        .create_user(email, email, "local", Some(&hash))
        .await
        .map_err(|e| anyhow::anyhow!("create_user failed: {e}"))?;
    store.record_event(crate::auth::store::NewAuthEvent {
        user_id: Some(user_id),
        kind: "create_admin".into(),
        provider: "local".into(),
        ip: None,
        user_agent: None,
    });

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

// ---- list-users ------------------------------------------------------------

#[derive(Debug, Default)]
struct ListUsersOpts {
    provider: Option<String>,
    force: bool,
}

fn parse_list_users(args: Vec<String>) -> Result<ListUsersOpts, anyhow::Error> {
    let mut o = ListUsersOpts::default();
    let mut iter = args.into_iter();
    while let Some(a) = iter.next() {
        match a.as_str() {
            "--provider" => {
                o.provider = Some(
                    iter.next()
                        .ok_or_else(|| anyhow::anyhow!("--provider requires a value"))?,
                );
            }
            "--force" => o.force = true,
            "--help" | "-h" => {
                println!("stt-server auth list-users [--provider local|oidc|passkey] [--force]");
                std::process::exit(0);
            }
            other => return Err(anyhow::anyhow!("unknown list-users flag: {other:?}")),
        }
    }
    Ok(o)
}

async fn list_users(args: Vec<String>, cli: &CliArgs) -> Result<ExitCode, anyhow::Error> {
    let opts = parse_list_users(args)?;
    check_running_server(opts.force)?;
    let cfg = Config::load(cli)?;
    let store = open_store(&cfg).await?;
    let users = store.list_users(opts.provider.as_deref()).await?;
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

// ---- delete-user -----------------------------------------------------------

#[derive(Debug, Default)]
struct DeleteUserOpts {
    email: Option<String>,
    yes: bool,
    force: bool,
}

fn parse_delete_user(args: Vec<String>) -> Result<DeleteUserOpts, anyhow::Error> {
    let mut o = DeleteUserOpts::default();
    let mut iter = args.into_iter();
    while let Some(a) = iter.next() {
        match a.as_str() {
            "--email" => {
                o.email = Some(
                    iter.next()
                        .ok_or_else(|| anyhow::anyhow!("--email requires a value"))?,
                );
            }
            "--yes" => o.yes = true,
            "--force" => o.force = true,
            "--help" | "-h" => {
                println!("stt-server auth delete-user --email <email> [--yes] [--force]");
                std::process::exit(0);
            }
            other => return Err(anyhow::anyhow!("unknown delete-user flag: {other:?}")),
        }
    }
    Ok(o)
}

async fn delete_user(args: Vec<String>, cli: &CliArgs) -> Result<ExitCode, anyhow::Error> {
    let opts = parse_delete_user(args)?;
    check_running_server(opts.force)?;
    let cfg = Config::load(cli)?;
    let store = open_store(&cfg).await?;
    let email = opts
        .email
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("--email is required"))?;
    let target = store
        .get_user_by_email(email)
        .await?
        .ok_or_else(|| anyhow::anyhow!("no user with email {email:?}"))?;

    // Belt + braces: refuse if it would leave zero local users and
    // auth is enabled. PR2 adds the role check; for now, a hard
    // "1 local user minimum" guard is enough to prevent the
    // lockout class.
    if target.provider == "local" && cfg.auth.enabled {
        let local_count = store.count_users_by_provider("local").await?;
        if local_count <= 1 {
            return Err(anyhow::anyhow!(
                "refusing to delete the last local user while auth is enabled; \
                 create another local user first to avoid lockout"
            ));
        }
    }

    if !opts.yes {
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

    store.delete_user(target.id).await?;
    println!("deleted {}", target.id);
    Ok(ExitCode::from(0))
}

// Helper so callers can keep `common` parsed in one place even
// when a subcommand doesn't need it.
#[allow(dead_code)]
fn _silence_parse_common(_o: CommonOpts) {}

// Silence `AuthError` not being used directly in the CLI module
// (it's the auth subsystem's error type, and the CLI propagates it
// via `anyhow`).
#[allow(dead_code)]
fn _silence_auth_error(_e: AuthError) {}
