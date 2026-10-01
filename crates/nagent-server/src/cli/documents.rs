//! `stt-server documents …` — operator CLI for the documents
//! subsystem.
//!
//! Runs against the same `[auth.db]` configuration the server uses
//! (the documents table lives in the auth DB so the CLI can sweep
//! rows + files from a single connection). Designed for the
//! recovery path: an operator who needs to free disk space, drop
//! stale uploads, or verify the periodic sweep's TTL.
//!
//! Argparse grammar (one command per invocation):
//!
//! ```text
//! stt-server documents purge --older-than <DURATION> [--dry-run] [--force]
//! ```
//!
//! `--older-than` accepts the same `humantime`-style strings the
//! rest of the project uses (`30d`, `12h`, `90s`, `5m`); the
//! minimum unit is seconds.

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use crate::config::{CliArgs, Config};

/// Top-level dispatch: called from `main` when `argv` contains
/// `documents`. Mirrors the `auth` / `migrate` dispatchers.
pub async fn run_documents_cli(args: Vec<String>) -> Result<ExitCode, anyhow::Error> {
    let (cli_args, rest) = split_global_flags(args);
    let mut rest = rest;
    if rest.first().map(|s| s.as_str()) == Some("documents") {
        rest.remove(0);
    }
    let mut iter = rest.into_iter();
    let sub = iter.next().unwrap_or_default();
    let result = match sub.as_str() {
        "purge" => purge(iter.collect(), &cli_args).await,
        "help" | "--help" | "-h" | "" => {
            print_help();
            return Ok(ExitCode::from(0));
        }
        other => {
            eprintln!("error: unknown documents subcommand: {other:?}");
            eprintln!("(run `stt-server documents help` for usage)");
            return Ok(ExitCode::from(2));
        }
    };
    result
}

/// Extract `--config <path>` (and `--config=...`) from the args
/// vector so the subcommand parsers see a clean slice. Same
/// trick the `auth` and `migrate` CLIs use; duplicated rather than
/// shared because the helper is private and there are only three
/// namespaces today.
fn split_global_flags(args: Vec<String>) -> (CliArgs, Vec<String>) {
    let mut cli = CliArgs::default();
    let mut rest = Vec::with_capacity(args.len());
    let mut iter = args.into_iter();
    while let Some(a) = iter.next() {
        if a == "--config" {
            if let Some(v) = iter.next() {
                cli.config = Some(PathBuf::from(v));
            } else {
                eprintln!("error: --config requires a path argument");
                std::process::exit(2);
            }
        } else if let Some(v) = a.strip_prefix("--config=") {
            cli.config = Some(PathBuf::from(v));
        } else {
            rest.push(a);
        }
    }
    (cli, rest)
}

fn print_help() {
    println!("stt-server documents — operator CLI for the documents subsystem");
    println!();
    println!("USAGE:");
    println!("    stt-server documents <subcommand> [options]");
    println!();
    println!("SUBCOMMANDS:");
    println!("    purge        Unlink files + delete rows whose created_at +");
    println!("                 [documents].default_ttl_days is older than the");
    println!("                 --older-than threshold. Honours expires_at when");
    println!("                 set.");
    println!();
    println!("GLOBAL OPTIONS:");
    println!("    --config PATH     Load config from this TOML file.");
    println!("    --force           Run even when data/server.pid points at a live process.");
    println!("    --older-than D    Required for `purge`. e.g. 30d, 12h, 5m, 90s.");
    println!("    --dry-run         List matching rows without touching the DB or disk.");
}

#[derive(Debug)]
struct PurgeOpts {
    older_than: Option<Duration>,
    dry_run: bool,
    force: bool,
}

fn parse_purge(args: Vec<String>) -> Result<PurgeOpts, anyhow::Error> {
    let mut opts = PurgeOpts {
        older_than: None,
        dry_run: false,
        force: false,
    };
    let mut iter = args.into_iter();
    while let Some(a) = iter.next() {
        match a.as_str() {
            "--older-than" => {
                let v = iter
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--older-than requires a value"))?;
                opts.older_than = Some(parse_duration(&v)?);
            }
            "--dry-run" => opts.dry_run = true,
            "--force" => opts.force = true,
            "--help" | "-h" => {
                println!(
                    "stt-server documents purge --older-than <DURATION> [--dry-run] [--force]"
                );
                std::process::exit(0);
            }
            other => {
                return Err(anyhow::anyhow!("unknown documents purge flag: {other:?}"));
            }
        }
    }
    if opts.older_than.is_none() && !opts.dry_run {
        // --older-than is required for a real run. For `--dry-run`
        // we accept a missing value and default to 1s so an
        // operator can quickly inspect "what would a 1s sweep
        // remove right now" without remembering the exact
        // duration syntax.
        return Err(anyhow::anyhow!(
            "--older-than <DURATION> is required (e.g. 30d, 12h, 5m, 90s)"
        ));
    }
    Ok(opts)
}

/// Parse a humantime-style duration string. Supports `Nd`, `Nh`,
/// `Nm`, `Ns` (case-insensitive). Pure integer values are treated
/// as seconds so the CLI accepts `30d` and `2592000` interchangeably.
fn parse_duration(s: &str) -> Result<Duration, anyhow::Error> {
    let s = s.trim();
    if s.is_empty() {
        return Err(anyhow::anyhow!("duration must not be empty"));
    }
    if let Some(rest) = s.strip_suffix("d").or_else(|| s.strip_suffix("D")) {
        let n: u64 = rest
            .parse()
            .map_err(|e| anyhow::anyhow!("invalid days in `{s}`: {e}"))?;
        return Ok(Duration::from_secs(n * 86_400));
    }
    if let Some(rest) = s.strip_suffix("h").or_else(|| s.strip_suffix("H")) {
        let n: u64 = rest
            .parse()
            .map_err(|e| anyhow::anyhow!("invalid hours in `{s}`: {e}"))?;
        return Ok(Duration::from_secs(n * 3_600));
    }
    if let Some(rest) = s.strip_suffix("m").or_else(|| s.strip_suffix("M")) {
        let n: u64 = rest
            .parse()
            .map_err(|e| anyhow::anyhow!("invalid minutes in `{s}`: {e}"))?;
        return Ok(Duration::from_secs(n * 60));
    }
    if let Some(rest) = s.strip_suffix("s").or_else(|| s.strip_suffix("S")) {
        let n: u64 = rest
            .parse()
            .map_err(|e| anyhow::anyhow!("invalid seconds in `{s}`: {e}"))?;
        return Ok(Duration::from_secs(n));
    }
    // No suffix → seconds.
    let n: u64 = s
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid duration `{s}` (use Nd/Nh/Nm/Ns): {e}"))?;
    Ok(Duration::from_secs(n))
}

/// Connect to the auth DB + build a `DocumentStore`. Mirrors the
/// `migrate` CLI's `connect_store` helper (which uses the same
/// auth DB); duplicated to avoid leaking the auth boot internals.
async fn connect_store(
    cfg: &Config,
) -> Result<(crate::documents::DocumentStore, std::path::PathBuf), anyhow::Error> {
    if !cfg.auth.enabled {
        return Err(anyhow::anyhow!(
            "auth is not enabled in the active configuration; \
             documents require the auth DB to be reachable"
        ));
    }
    crate::cli::auth::ensure_sqlite_parent_dir(&cfg.auth.db.url)?;
    let db_opts: nagent_db::DbOptions = (&cfg.auth).into();
    let db = nagent_db::Db::connect(&db_opts)
        .await
        .map_err(|e| anyhow::anyhow!("auth DB connect failed: {e}"))?;
    let doc_store = crate::documents::DocumentStore::new(
        db,
        cfg.documents.max_extracted_chars,
        cfg.documents.cache_dir.clone(),
    );
    Ok((doc_store, cfg.documents.cache_dir.clone()))
}

async fn purge(args: Vec<String>, cli: &CliArgs) -> Result<ExitCode, anyhow::Error> {
    let opts = parse_purge(args)?;
    let cfg = Config::load(cli)?;
    check_running_server(opts.force)?;

    let (store, cache_dir) = connect_store(&cfg).await?;
    let ttl = opts.older_than.unwrap_or_else(|| Duration::from_secs(1));

    if opts.dry_run {
        // Inspect-only path: list the rows the sweep WOULD remove.
        // Does not touch the DB or the disk.
        let rows = store
            .db()
            .documents
            .sweep_older_than(ttl)
            .await
            .map_err(|e| anyhow::anyhow!("sweep failed: {e}"))?;
        println!("would purge {} document(s):", rows.len());
        for row in &rows {
            println!(
                "  {} {} ({} bytes, expires_at = none)",
                row.id, row.original_name, row.size_bytes
            );
        }
        return Ok(ExitCode::from(0));
    }

    let removed = crate::documents::purge::purge_older_than(&store, &cache_dir, ttl)
        .await
        .map_err(|e| anyhow::anyhow!("purge failed: {e}"))?;
    println!("purged {removed} document(s) (ttl = {ttl:?})");
    Ok(ExitCode::from(0))
}

/// Refuse to run if the server PID file exists AND points at a
/// live process AND `--force` was not supplied. Mirrors
/// [`crate::cli::migrate::check_running_server`] and
/// [`crate::cli::auth::check_running_server`]; duplicated to
/// keep the three namespaces independent.
fn check_running_server(force: bool) -> Result<(), anyhow::Error> {
    let pid_path = PathBuf::from("data").join("server.pid");
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
        Err(_) => return Ok(()), // stale PID file → allow
    };
    if !force {
        let status = std::process::Command::new("kill")
            .arg("-0")
            .arg(pid_num.to_string())
            .status();
        if let Ok(s) = status {
            if s.success() {
                return Err(anyhow::anyhow!(
                    "a server is already running (PID {} from {}); pass --force to override",
                    pid_num,
                    pid_path.display()
                ));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_purge_requires_older_than() {
        // Default (no flags) must surface a clear error so a
        // misclicked `purge` doesn't silently no-op.
        let err = parse_purge(vec![]).unwrap_err();
        assert!(err.to_string().contains("--older-than"));
    }

    #[test]
    fn parse_purge_accepts_older_than_dry_run_force() {
        let opts = parse_purge(vec![
            "--older-than".into(),
            "30d".into(),
            "--dry-run".into(),
            "--force".into(),
        ])
        .expect("parse_purge");
        assert_eq!(opts.older_than, Some(Duration::from_secs(30 * 86_400)));
        assert!(opts.dry_run);
        assert!(opts.force);
    }

    #[test]
    fn parse_duration_supports_days_hours_minutes_seconds_and_bare() {
        assert_eq!(
            parse_duration("30d").unwrap(),
            Duration::from_secs(30 * 86_400)
        );
        assert_eq!(
            parse_duration("12h").unwrap(),
            Duration::from_secs(12 * 3_600)
        );
        assert_eq!(parse_duration("5m").unwrap(), Duration::from_secs(5 * 60));
        assert_eq!(parse_duration("90s").unwrap(), Duration::from_secs(90));
        // Bare integer = seconds.
        assert_eq!(parse_duration("120").unwrap(), Duration::from_secs(120));
        // Case insensitive.
        assert_eq!(
            parse_duration("7D").unwrap(),
            Duration::from_secs(7 * 86_400)
        );
    }

    #[test]
    fn parse_duration_rejects_garbage() {
        assert!(parse_duration("").is_err());
        assert!(parse_duration("abc").is_err());
        assert!(parse_duration("30x").is_err());
        // Negative values are nonsensical for a TTL.
        assert!(parse_duration("-30s").is_err());
    }

    #[test]
    fn split_global_flags_strips_only_config() {
        let (cli, rest) = split_global_flags(vec![
            "--config".into(),
            "/etc/nagent/config.toml".into(),
            "purge".into(),
            "--older-than".into(),
            "30d".into(),
        ]);
        assert_eq!(
            cli.config.as_deref(),
            Some(std::path::Path::new("/etc/nagent/config.toml"))
        );
        assert_eq!(rest, vec!["purge", "--older-than", "30d"]);
    }
}
