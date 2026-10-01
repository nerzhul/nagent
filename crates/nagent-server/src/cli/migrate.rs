//! `stt-server migrate …` — operator CLI for the auth DB migration
//! state.
//!
//! Mirrors [`crate::cli::auth`] style: hand-rolled flag parsing
//! (no `clap` dep), `--config FOO` global flag, `--force` PID-file
//! override, `--help` / `-h` per subcommand. Runs against the same
//! `[auth.db]` configuration the server uses, without needing the
//! HTTP server to be running.
//!
//! Subcommands:
//!
//! ```text
//! stt-server migrate up                    [--force]
//! stt-server migrate down [--steps N | --to VERSION] [--yes] [--force]
//! stt-server migrate status [--json]       [--force]
//! stt-server migrate help
//! ```
//!
//! `down` is destructive and requires `--yes` to skip the printed
//! plan check. `up` is idempotent (sqlx is the source of truth) so
//! it never asks for confirmation.
//!
//! Unlike `crate::cli::auth::open_store()`, this module does NOT call
//! `migrate_up` after connect — every subcommand uses the migration
//! primitives directly so an operator can inspect `status` against
//! a freshly-created DB without immediately applying the schema.

use std::path::PathBuf;
use std::process::ExitCode;

use crate::config::{CliArgs, Config};
use nagent_db::{MigrationRow, MigrationStatus};

/// Top-level dispatch: called from `main` when `argv[1] == "migrate"`.
/// Returns `Ok(exit_code)` so the binary returns the right status to
/// the shell on each subcommand.
///
/// `args` is the full argv after the binary name. The dispatch walks
/// the full argv (skipping the binary name) so `--config FOO migrate
/// status` routes correctly even when the operator puts the global
/// flag before the subcommand — same trick the `auth` CLI uses.
pub async fn run_migrate_cli(args: Vec<String>) -> Result<ExitCode, anyhow::Error> {
    let (cli_args, rest) = split_global_flags(args);
    let mut rest = rest;
    if rest.first().map(|s| s.as_str()) == Some("migrate") {
        rest.remove(0);
    }
    let mut iter = rest.into_iter();
    let sub = iter.next().unwrap_or_default();
    let result = match sub.as_str() {
        // `help` comes BEFORE `down` in the dispatch order — the
        // string `"help"` would otherwise parse cleanly as a VERSION
        // argument if we let it reach `parse_down`.
        "help" | "--help" | "-h" | "" => {
            print_help();
            return Ok(ExitCode::from(0));
        }
        "up" => up(iter.collect(), &cli_args).await,
        "down" => down(iter.collect(), &cli_args).await,
        "status" => status(iter.collect(), &cli_args).await,
        other => {
            eprintln!("error: unknown migrate subcommand: {other:?}");
            eprintln!("(run `stt-server migrate help` for usage)");
            return Ok(ExitCode::from(2));
        }
    };
    result
}

/// Extract the `--config <path>` flag pair (if any) from the migrate
/// CLI argv. Returns the parsed [`CliArgs`] plus the remainder
/// without the global flags — the subcommand-specific parsers then
/// operate on a clean slice. Same shape as
/// [`crate::cli::auth::split_global_flags`]; duplicated rather than
/// shared because the `auth` module's helper is private and there
/// are only two namespaces today.
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

/// Connect to the auth DB WITHOUT running migrations. Each `migrate`
/// subcommand then drives the migration primitives directly so the
/// `status` view reflects what is on disk, not what would have been
/// applied by `connect()`.
async fn connect_store(cfg: &Config) -> Result<nagent_db::Db, anyhow::Error> {
    crate::cli::auth::ensure_sqlite_parent_dir(&cfg.auth.db.url)?;
    let opts: nagent_db::DbOptions = (&cfg.auth).into();
    nagent_db::Db::connect(&opts)
        .await
        .map_err(|e| anyhow::anyhow!("auth DB connect failed: {e}"))
}

/// PID-file + `--force` guard shared by every subcommand. Mirrors the
/// [`crate::cli::auth::check_running_server`] helper but is wired up
/// here independently (the `auth` CLI's check is private and the two
/// namespaces have no reason to share this code path).
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

fn print_help() {
    println!("stt-server migrate — operator CLI for auth DB migrations");
    println!();
    println!("USAGE:");
    println!("    stt-server migrate <subcommand> [options]");
    println!();
    println!("SUBCOMMANDS:");
    println!("    up       Apply any pending migrations. Idempotent; safe to re-run.");
    println!("             Prints either `applied N migrations (...)` or");
    println!("             `no pending migrations` and exits 0.");
    println!();
    println!("    down     Reverse migrations. DESTRUCTIVE — requires --yes.");
    println!("             Default (no flags): revert the latest applied migration.");
    println!("             --steps N: revert the N most recent migrations.");
    println!("             --to VERSION: revert down to (but keeping) VERSION.");
    println!("             Without --yes, prints the plan and exits non-zero.");
    println!();
    println!("    status   List applied + pending migrations as:");
    println!("             VERSION  DESCRIPTION         STATUS");
    println!("             0001     init                applied");
    println!("             0002     credentials        pending");
    println!("             Plus a footer with `highest applied` + `pending count`.");
    println!("             --json emits a stable JSON shape (see README).");
    println!();
    println!("GLOBAL OPTIONS:");
    println!("    --config PATH    Load config from this TOML file (see auth CLI).");
    println!("    --force          Run even when data/server.pid points at a live process.");
}

// ---- up ---------------------------------------------------------------------

#[derive(Debug, Default)]
struct UpOpts {
    force: bool,
}

fn parse_up(args: Vec<String>) -> Result<UpOpts, anyhow::Error> {
    let mut o = UpOpts::default();
    for a in args {
        match a.as_str() {
            "--force" => o.force = true,
            "--help" | "-h" => {
                println!("stt-server migrate up [--force]");
                std::process::exit(0);
            }
            other => return Err(anyhow::anyhow!("unknown migrate up flag: {other:?}")),
        }
    }
    Ok(o)
}

async fn up(args: Vec<String>, cli: &CliArgs) -> Result<ExitCode, anyhow::Error> {
    let opts = parse_up(args)?;
    check_running_server(opts.force)?;
    let cfg = Config::load(cli)?;

    if !cfg.auth.enabled {
        return Err(anyhow::anyhow!(
            "auth is not enabled in the active configuration; set [auth].enabled = true (or NAGENT_AUTH_ENABLED=true)"
        ));
    }

    let store = connect_store(&cfg).await?;
    // Diff before so we can report what we actually applied. Idempotent
    // — sqlx skips already-applied migrations on the second call.
    let before = store.migration_status().await?;
    store.migrate().await.map_err(map_auth_err)?;
    let after = store.migration_status().await?;

    let newly_applied: Vec<&MigrationRow> = after
        .applied
        .iter()
        .filter(|r| match before.highest_applied {
            Some(h) => r.version > h,
            None => true,
        })
        .collect();

    if newly_applied.is_empty() {
        println!("no pending migrations");
    } else {
        let names = newly_applied
            .iter()
            .map(|r| format!("{:04}_{}", r.version, r.description))
            .collect::<Vec<_>>()
            .join(", ");
        println!("applied {} migration(s): {}", newly_applied.len(), names);
    }
    Ok(ExitCode::from(0))
}

// ---- down -------------------------------------------------------------------

#[derive(Debug, Default)]
struct DownOpts {
    steps: Option<usize>,
    to: Option<i64>,
    yes: bool,
    force: bool,
}

fn parse_down(args: Vec<String>) -> Result<DownOpts, anyhow::Error> {
    let mut o = DownOpts::default();
    let mut iter = args.into_iter();
    while let Some(a) = iter.next() {
        match a.as_str() {
            "--steps" => {
                let v = iter
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--steps requires a value"))?;
                let n: usize = v
                    .parse()
                    .map_err(|e| anyhow::anyhow!("invalid --steps value {v:?}: {e}"))?;
                o.steps = Some(n);
            }
            "--to" => {
                let v = iter
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--to requires a value"))?;
                let n: i64 = v
                    .parse()
                    .map_err(|e| anyhow::anyhow!("invalid --to value {v:?}: {e}"))?;
                o.to = Some(n);
            }
            "--yes" => o.yes = true,
            "--force" => o.force = true,
            "--help" | "-h" => {
                println!("stt-server migrate down [--steps N | --to VERSION] [--yes] [--force]");
                std::process::exit(0);
            }
            other => return Err(anyhow::anyhow!("unknown migrate down flag: {other:?}")),
        }
    }
    Ok(o)
}

/// Compute the `target_version` we want to keep applied after the
/// `down` runs. Returns `None` when there is nothing to do (no
/// applied migrations, or `--steps 0`).
fn compute_target(opts: &DownOpts, status: &MigrationStatus) -> Result<Option<i64>, anyhow::Error> {
    let highest = status
        .highest_applied
        .ok_or_else(|| anyhow::anyhow!("no migrations applied; nothing to revert"))?;

    if let Some(to) = opts.to {
        if to > highest {
            return Err(anyhow::anyhow!(
                "--to {to} is greater than highest applied ({highest}); \
                 `down` only rolls back, not forward. Run `migrate up` first."
            ));
        }
        // --to VERSION means "keep VERSION applied". VERSION must be
        // a known migration (sqlx silently ignores targets with no
        // matching migration, so we surface a clearer error here).
        if !status.applied.iter().any(|r| r.version == to) {
            return Err(anyhow::anyhow!(
                "--to {to} is not in the applied set (highest applied = {highest}); \
                 check `migrate status` for the exact version numbers."
            ));
        }
        return Ok(Some(to));
    }

    let n = opts.steps.unwrap_or(1);
    if n == 0 {
        return Ok(None);
    }
    // Compute the version we KEEP applied after `n` reverts.
    // `sorted` is ascending, so the new highest is `sorted[len - n
    // - 1]` (the element just below the n highest entries). When
    // `n >= sorted.len()` sqlx treats `target = 0` as "undo every
    // applied migration" (the loop in `Migrator::undo` filters
    // `version > target`), so we pass 0 in that case rather than a
    // negative index.
    let sorted: Vec<i64> = {
        let mut v: Vec<i64> = status.applied.iter().map(|r| r.version).collect();
        v.sort();
        v
    };
    if n >= sorted.len() {
        return Ok(Some(0));
    }
    Ok(Some(sorted[sorted.len() - n - 1]))
}

async fn down(args: Vec<String>, cli: &CliArgs) -> Result<ExitCode, anyhow::Error> {
    let opts = parse_down(args)?;
    check_running_server(opts.force)?;
    let cfg = Config::load(cli)?;

    if !cfg.auth.enabled {
        return Err(anyhow::anyhow!(
            "auth is not enabled in the active configuration; \
             there is no auth DB to migrate."
        ));
    }
    if opts.steps.is_some() && opts.to.is_some() {
        return Err(anyhow::anyhow!(
            "--steps and --to are mutually exclusive; pick one."
        ));
    }

    let store = connect_store(&cfg).await?;
    let status = store.migration_status().await?;
    let target = match compute_target(&opts, &status)? {
        Some(t) => t,
        None => {
            println!("nothing to revert");
            return Ok(ExitCode::from(0));
        }
    };
    let reverting: Vec<i64> = status
        .applied
        .iter()
        .map(|r| r.version)
        .filter(|v| *v > target)
        .collect();

    if reverting.is_empty() {
        println!("nothing to revert");
        return Ok(ExitCode::from(0));
    }

    // Print the plan and require explicit confirmation.
    eprintln!("migration plan:");
    for v in &reverting {
        let label = status
            .applied
            .iter()
            .find(|r| r.version == *v)
            .map(|r| format!("{:04}_{}", r.version, r.description))
            .unwrap_or_else(|| format!("{v:04}_<unknown>"));
        eprintln!("  - revert {label}");
    }
    eprintln!("keeping migration(s) with version <= {target} applied.");
    eprintln!();
    if !opts.yes {
        eprintln!("refusing to run without --yes; rerun with --yes to apply.");
        return Ok(ExitCode::from(2));
    }

    store.revert_to(target).await.map_err(map_auth_err)?;

    println!(
        "reverted {} migration(s); target version = {}",
        reverting.len(),
        target
    );
    Ok(ExitCode::from(0))
}

// ---- status -----------------------------------------------------------------

#[derive(Debug, Default)]
struct StatusOpts {
    json: bool,
    force: bool,
}

fn parse_status(args: Vec<String>) -> Result<StatusOpts, anyhow::Error> {
    let mut o = StatusOpts::default();
    for a in args {
        match a.as_str() {
            "--json" => o.json = true,
            "--force" => o.force = true,
            "--help" | "-h" => {
                println!("stt-server migrate status [--json] [--force]");
                std::process::exit(0);
            }
            other => return Err(anyhow::anyhow!("unknown migrate status flag: {other:?}")),
        }
    }
    Ok(o)
}

async fn status(args: Vec<String>, cli: &CliArgs) -> Result<ExitCode, anyhow::Error> {
    let opts = parse_status(args)?;
    check_running_server(opts.force)?;
    let cfg = Config::load(cli)?;

    if !cfg.auth.enabled {
        return Err(anyhow::anyhow!(
            "auth is not enabled in the active configuration; \
             there is no auth DB to inspect."
        ));
    }

    let store = connect_store(&cfg).await?;
    let status = store.migration_status().await?;

    if opts.json {
        let json = serde_json::json!({
            "applied": status.applied.iter().map(row_to_json).collect::<Vec<_>>(),
            "pending": status.pending.iter().map(row_to_json).collect::<Vec<_>>(),
            "highest_applied": status.highest_applied,
        });
        println!("{}", serde_json::to_string_pretty(&json)?);
    } else {
        print_status_table(&status);
    }
    Ok(ExitCode::from(0))
}

fn row_to_json(r: &MigrationRow) -> serde_json::Value {
    serde_json::json!({
        "version": r.version,
        "description": r.description,
    })
}

fn print_status_table(status: &MigrationStatus) {
    let mut rows: Vec<(i64, &str, &str)> =
        Vec::with_capacity(status.applied.len() + status.pending.len());
    for r in &status.applied {
        rows.push((r.version, &r.description, "applied"));
    }
    for r in &status.pending {
        rows.push((r.version, &r.description, "pending"));
    }
    rows.sort_by_key(|(v, _, _)| *v);

    println!("{:<8} {:<24} STATUS", "VERSION", "DESCRIPTION");
    for (v, desc, state) in &rows {
        println!("{:04}     {:<24} {}", v, desc, state);
    }
    match status.highest_applied {
        Some(h) => println!("\nhighest applied: {:04}", h),
        None => println!("\nhighest applied: <none>"),
    }
    println!("pending: {}", status.pending.len());
}

/// Map the auth subsystem's error into `anyhow` so the CLI can use a
/// Map a `nagent_db::Error` to the CLI's `anyhow` error type.
fn map_auth_err(e: nagent_db::Error) -> anyhow::Error {
    anyhow::anyhow!("{e}")
}

#[cfg(test)]
mod tests {
    //! CLI parsing tests. The data-layer tests (up / status / down
    //! against a real sqlite store) live in `tests/migrate_cli.rs`
    //! so they share the existing integration-test harness; this
    //! module only covers the pure-Rust flag-parsing branches so
    //! they run as fast unit tests inside the crate.
    use super::*;
    use nagent_db::MigrationRow;

    fn status_with(applied: Vec<i64>, pending: Vec<i64>) -> MigrationStatus {
        let applied: Vec<MigrationRow> = applied
            .into_iter()
            .map(|v| MigrationRow {
                version: v,
                description: format!("desc_{v}"),
            })
            .collect();
        let pending: Vec<MigrationRow> = pending
            .into_iter()
            .map(|v| MigrationRow {
                version: v,
                description: format!("desc_{v}"),
            })
            .collect();
        let highest_applied = applied.iter().map(|r| r.version).max();
        MigrationStatus {
            applied,
            pending,
            highest_applied,
        }
    }

    #[test]
    fn parse_down_accepts_steps_to_yes_and_force() {
        let opts = parse_down(vec![
            "--steps".into(),
            "2".into(),
            "--yes".into(),
            "--force".into(),
        ])
        .expect("parse_down");
        assert_eq!(opts.steps, Some(2));
        assert_eq!(opts.to, None);
        assert!(opts.yes);
        assert!(opts.force);
    }

    #[test]
    fn parse_down_accepts_to_version() {
        let opts = parse_down(vec!["--to".into(), "1".into(), "--yes".into()]).expect("parse_down");
        assert_eq!(opts.steps, None);
        assert_eq!(opts.to, Some(1));
        assert!(opts.yes);
    }

    #[test]
    fn parse_down_rejects_unknown_flag() {
        let err = parse_down(vec!["--nope".into()]).unwrap_err();
        assert!(err.to_string().contains("unknown migrate down flag"));
    }

    #[test]
    fn parse_down_rejects_missing_steps_value() {
        let err = parse_down(vec!["--steps".into()]).unwrap_err();
        assert!(err.to_string().contains("--steps requires a value"));
    }

    #[test]
    fn parse_down_rejects_non_numeric_steps() {
        let err = parse_down(vec!["--steps".into(), "abc".into()]).unwrap_err();
        assert!(err.to_string().contains("invalid --steps value"));
    }

    #[test]
    fn parse_up_accepts_force() {
        let opts = parse_up(vec!["--force".into()]).expect("parse_up");
        assert!(opts.force);
    }

    #[test]
    fn parse_status_accepts_json_and_force() {
        let opts = parse_status(vec!["--json".into(), "--force".into()]).expect("parse_status");
        assert!(opts.json);
        assert!(opts.force);
    }

    #[test]
    fn compute_target_defaults_to_second_highest() {
        // Default `down` reverts the LATEST migration, so the target
        // we keep applied is the second-highest.
        let s = status_with(vec![1, 2], vec![]);
        let opts = DownOpts::default();
        assert_eq!(compute_target(&opts, &s).unwrap(), Some(1));
    }

    #[test]
    fn compute_target_steps_two_returns_zero_when_all_would_revert() {
        let s = status_with(vec![1, 2], vec![]);
        let opts = DownOpts {
            steps: Some(2),
            ..Default::default()
        };
        assert_eq!(compute_target(&opts, &s).unwrap(), Some(0));
    }

    #[test]
    fn compute_target_to_version_must_be_applied() {
        // `to = 1` is < highest (= 3) so it passes the "forward"
        // check, but the applied set is {2, 3} so version 1 is
        // unknown. The CLI must reject this case explicitly.
        let s = status_with(vec![2, 3], vec![]);
        let opts = DownOpts {
            to: Some(1),
            ..Default::default()
        };
        let err = compute_target(&opts, &s).unwrap_err();
        assert!(
            err.to_string().contains("is not in the applied set"),
            "expected 'is not in the applied set' error, got: {err}"
        );
    }

    #[test]
    fn compute_target_to_greater_than_highest_errors() {
        let s = status_with(vec![1, 2], vec![]);
        let opts = DownOpts {
            to: Some(5),
            ..Default::default()
        };
        let err = compute_target(&opts, &s).unwrap_err();
        assert!(err.to_string().contains("only rolls back, not forward"));
    }

    #[test]
    fn compute_target_errors_when_no_migrations_applied() {
        let s = MigrationStatus::default();
        let opts = DownOpts::default();
        let err = compute_target(&opts, &s).unwrap_err();
        assert!(err.to_string().contains("no migrations applied"));
    }

    #[test]
    fn compute_target_steps_zero_is_noop() {
        let s = status_with(vec![1, 2], vec![]);
        let opts = DownOpts {
            steps: Some(0),
            ..Default::default()
        };
        assert_eq!(compute_target(&opts, &s).unwrap(), None);
    }

    #[test]
    fn compute_target_to_one_keeps_one() {
        let s = status_with(vec![1, 2], vec![]);
        let opts = DownOpts {
            to: Some(1),
            ..Default::default()
        };
        assert_eq!(compute_target(&opts, &s).unwrap(), Some(1));
    }
}
