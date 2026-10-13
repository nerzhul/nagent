//! `stt-server migrate …` — operator CLI for the auth DB migration
//! state.
//!
//! Argv parsing is driven by `clap` derive (plan
//! `1792262220000-clap-derive-migration`). Subcommands are variants
//! of [`MigrateAction`]; the `compute_target` math and the
//! migration-status JSON rendering are unchanged.
//!
//! Subcommands:
//!
//! ```text
//! stt-server migrate up                    [--force]
//! stt-server migrate down [--steps N | --to VERSION] [--yes] [--force]
//! stt-server migrate status [--json]       [--force]
//! ```
//!
//! `down` is destructive and requires `--yes` to skip the printed
//! plan check. `up` is idempotent (sqlx is the source of truth) so
//! it never asks for confirmation.
//!
//! Unlike [`crate::cli::auth::open_store`], this module does NOT
//! call `migrate_up` after connect — every subcommand uses the
//! migration primitives directly so an operator can inspect
//! `status` against a freshly-created DB without immediately
//! applying the schema.

use std::path::Path;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use nagent_db::{MigrationRow, MigrationStatus};

use crate::cli::shared::{check_running_server, ensure_sqlite_parent_dir};
use crate::config::Config;

/// Top-level `migrate` subcommand.
#[derive(Debug, Parser)]
#[command(
    about = "Operator CLI for auth DB migrations",
    long_about = "Operator CLI for auth DB migrations.\n\
                  Runs against the same `[auth.db]` configuration the server uses, \
                  without booting the HTTP server."
)]
pub struct MigrateCmd {
    #[command(subcommand)]
    pub action: MigrateAction,
}

#[derive(Debug, Subcommand)]
pub enum MigrateAction {
    /// Apply any pending migrations. Idempotent; safe to re-run.
    Up {
        #[arg(long)]
        force: bool,
    },
    /// Reverse migrations. DESTRUCTIVE — requires `--yes`.
    Down {
        /// Number of most-recent migrations to revert. Mutually
        /// exclusive with `--to`.
        #[arg(long, conflicts_with = "to")]
        steps: Option<usize>,
        /// Revert down to (but keeping) this VERSION.
        #[arg(long)]
        to: Option<i64>,
        #[arg(long)]
        yes: bool,
        #[arg(long)]
        force: bool,
    },
    /// List applied + pending migrations.
    Status {
        /// Emit a stable JSON shape instead of the human table.
        #[arg(long)]
        json: bool,
        #[arg(long)]
        force: bool,
    },
}

/// Dispatch the parsed [`MigrateCmd`].
pub async fn run(cmd: MigrateCmd, config: Option<&Path>) -> Result<ExitCode, anyhow::Error> {
    match cmd.action {
        MigrateAction::Up { force } => up(force, config).await,
        MigrateAction::Down {
            steps,
            to,
            yes,
            force,
        } => down(steps, to, yes, force, config).await,
        MigrateAction::Status { json, force } => status(json, force, config).await,
    }
}

/// Connect to the auth DB WITHOUT running migrations. Each
/// `migrate` subcommand then drives the migration primitives
/// directly so the `status` view reflects what is on disk, not
/// what would have been applied by `connect()`.
async fn connect_store(cfg: &Config) -> Result<nagent_db::Db, anyhow::Error> {
    ensure_sqlite_parent_dir(&cfg.auth.db.url)?;
    let opts: nagent_db::DbOptions = (&cfg.auth).into();
    nagent_db::Db::connect(&opts)
        .await
        .map_err(|e| anyhow::anyhow!("auth DB connect failed: {e}"))
}

async fn up(force: bool, config: Option<&Path>) -> Result<ExitCode, anyhow::Error> {
    check_running_server(force)?;
    let cfg = load_config(config)?;

    if !cfg.auth.enabled {
        return Err(anyhow::anyhow!(
            "auth is not enabled in the active configuration; \
             set [auth].enabled = true (or NAGENT_AUTH_ENABLED=true)"
        ));
    }

    let store = connect_store(&cfg).await?;
    // Diff before so we can report what we actually applied. Idempotent
    // — sqlx skips already-applied migrations on the second call.
    let before = store.migration_status().await?;
    store.migrate().await.map_err(map_db_err)?;
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

/// Parsed-shape of the `migrate down` subcommand. Mirrors the
/// clap-derived [`MigrateAction::Down`] variant so
/// [`compute_target`] stays free of clap types.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct DownOpts {
    pub steps: Option<usize>,
    pub to: Option<i64>,
    pub yes: bool,
    pub force: bool,
}

impl From<&MigrateAction> for DownOpts {
    fn from(action: &MigrateAction) -> Self {
        match action {
            MigrateAction::Down {
                steps,
                to,
                yes,
                force,
            } => Self {
                steps: *steps,
                to: *to,
                yes: *yes,
                force: *force,
            },
            _ => Self::default(),
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn down(
    steps: Option<usize>,
    to: Option<i64>,
    yes: bool,
    force: bool,
    config: Option<&Path>,
) -> Result<ExitCode, anyhow::Error> {
    let opts = DownOpts {
        steps,
        to,
        yes,
        force,
    };
    check_running_server(opts.force)?;
    let cfg = load_config(config)?;

    if !cfg.auth.enabled {
        return Err(anyhow::anyhow!(
            "auth is not enabled in the active configuration; \
             there is no auth DB to migrate."
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

    store.revert_to(target).await.map_err(map_db_err)?;

    println!(
        "reverted {} migration(s); target version = {}",
        reverting.len(),
        target
    );
    Ok(ExitCode::from(0))
}

async fn status(json: bool, force: bool, config: Option<&Path>) -> Result<ExitCode, anyhow::Error> {
    check_running_server(force)?;
    let cfg = load_config(config)?;

    if !cfg.auth.enabled {
        return Err(anyhow::anyhow!(
            "auth is not enabled in the active configuration; \
             there is no auth DB to inspect."
        ));
    }

    let store = connect_store(&cfg).await?;
    let status = store.migration_status().await?;

    if json {
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

/// Map a `nagent_db::Error` to the CLI's `anyhow` error type.
fn map_db_err(e: nagent_db::Error) -> anyhow::Error {
    anyhow::anyhow!("{e}")
}

/// Load `Config` from an optional explicit `--config PATH`.
fn load_config(path: Option<&Path>) -> Result<Config, anyhow::Error> {
    Config::load(path).map_err(|e| anyhow::anyhow!("{e}"))
}

#[cfg(test)]
mod tests {
    //! Pure-logic tests for [`compute_target`] and [`DownOpts`].
    //! Parser-shape tests now drive clap directly via
    //! `MigrateAction::try_parse_from`; see
    //! [`crate::cli::snapshot_tests`].
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
