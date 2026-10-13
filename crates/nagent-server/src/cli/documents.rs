//! `stt-server documents …` — operator CLI for the documents
//! subsystem.
//!
//! Argv parsing is driven by `clap` derive (plan
//! `1792262220000-clap-derive-migration`). The `--older-than`
//! `DURATION` knob is parsed through clap's `value_parser!` so the
//! CLI accepts the same `humantime`-style strings (`30d`, `12h`,
//! `90s`, `5m`, `2592000`) the rest of the project uses.
//!
//! Runs against the same `[auth.db]` configuration the server
//! uses (the documents table lives in the auth DB so the CLI can
//! sweep rows + files from a single connection). Designed for the
//! recovery path: an operator who needs to free disk space, drop
//! stale uploads, or verify the periodic sweep's TTL.
//!
//! Subcommands:
//!
//! ```text
//! stt-server documents purge --older-than <DURATION> [--dry-run] [--force]
//! ```

use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, Subcommand};

use crate::cli::shared::{check_running_server, ensure_sqlite_parent_dir};
use crate::config::Config;

/// Top-level `documents` subcommand.
#[derive(Debug, Parser)]
#[command(
    about = "Operator CLI for the documents subsystem",
    long_about = "Operator CLI for the documents subsystem.\n\
                  Sweeps documents whose `created_at + [documents].default_ttl_days` \
                  is older than the `--older-than` threshold."
)]
pub struct DocumentsCmd {
    #[command(subcommand)]
    pub action: DocumentsAction,
}

#[derive(Debug, Subcommand)]
pub enum DocumentsAction {
    /// Unlink files + delete rows whose age exceeds the threshold.
    Purge {
        /// Age threshold; rows older than this are candidates for
        /// removal. Examples: `30d`, `12h`, `5m`, `90s`, or a bare
        /// number of seconds. Required unless `--dry-run` is set,
        /// in which case it defaults to 1s.
        #[arg(long, value_name = "older_than", value_parser = humantime_value_parser, required_unless_present = "dry_run")]
        older_than: Option<Duration>,
        /// List matching rows without touching the DB or disk.
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        force: bool,
    },
}

/// Dispatch the parsed [`DocumentsCmd`].
pub async fn run(cmd: DocumentsCmd, config: Option<&Path>) -> Result<ExitCode, anyhow::Error> {
    match cmd.action {
        DocumentsAction::Purge {
            older_than,
            dry_run,
            force,
        } => purge(older_than, dry_run, force, config).await,
    }
}

/// clap `value_parser` adapter: parses a `humantime`-style string
/// (`30d`, `12h`, `5m`, `90s`, `2592000s`) into a
/// [`std::time::Duration`]. Bare integers are also accepted and
/// treated as seconds — that wire contract predates humantime and
/// is documented in the README. `std::time::Duration` itself does
/// not implement `FromStr` in a form clap recognises, so the
/// `value_parser!` macro cannot infer it; humantime's free function
/// is the recommended shim.
fn humantime_value_parser(s: &str) -> Result<Duration, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("duration must not be empty".into());
    }
    // Bare integer → seconds. humantime requires a unit suffix
    // (`30sec`, `30s`, `30ms`, …); the project previously accepted
    // bare numbers for convenience (`30d` and `2592000` both valid).
    let needs_seconds_suffix =
        s.parse::<u64>().is_ok() && !s.chars().any(|c| c.is_ascii_alphabetic());
    let s_for_humantime = if needs_seconds_suffix {
        format!("{s}s")
    } else {
        s.to_string()
    };
    humantime::parse_duration(&s_for_humantime).map_err(|e| e.to_string())
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
    ensure_sqlite_parent_dir(&cfg.auth.db.url)?;
    let db_opts: nagent_db::DbOptions = (&cfg.auth).into();
    let db = nagent_db::Db::connect(&db_opts)
        .await
        .map_err(|e| anyhow::anyhow!("auth DB connect failed: {e}"))?;
    let doc_store = crate::documents::DocumentStore::new(
        db,
        cfg.documents.max_extracted_chars,
        cfg.documents.max_pages_per_call,
        cfg.documents.max_page_chars_per_call,
        cfg.documents.cache_dir.clone(),
        cfg.documents.pdf_extract_concurrency,
        cfg.documents.pdf_extract_timeout_secs,
    );
    Ok((doc_store, cfg.documents.cache_dir.clone()))
}

async fn purge(
    older_than: Option<Duration>,
    dry_run: bool,
    force: bool,
    config: Option<&Path>,
) -> Result<ExitCode, anyhow::Error> {
    if older_than.is_none() && !dry_run {
        // `--older-than` is required for a real run. For
        // `--dry-run` we accept a missing value and default to 1s
        // so an operator can quickly inspect "what would a 1s sweep
        // remove right now" without remembering the exact duration
        // syntax.
        return Err(anyhow::anyhow!(
            "--older-than <DURATION> is required (e.g. 30d, 12h, 5m, 90s)"
        ));
    }
    let cfg = load_config(config)?;
    check_running_server(force)?;

    let (store, cache_dir) = connect_store(&cfg).await?;
    let ttl = older_than.unwrap_or_else(|| Duration::from_secs(1));

    if dry_run {
        // Inspect-only path: list the rows the sweep WOULD remove.
        // Does not touch the DB or the disk.
        let rows = store
            .db()
            .admin()
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

/// Load `Config` from an optional explicit `--config PATH`.
fn load_config(path: Option<&Path>) -> Result<Config, anyhow::Error> {
    Config::load(path).map_err(|e| anyhow::anyhow!("{e}"))
}

#[cfg(test)]
mod tests {
    //! Pure-logic tests for the purge guard. Parser-shape tests
    //! now drive clap directly via
    //! `DocumentsAction::try_parse_from`; see
    //! [`crate::cli::snapshot_tests`].
    use super::*;
    use clap::Parser;

    #[test]
    fn purge_requires_older_than_unless_dry_run() {
        // Default (no flags) must surface a clear error so a
        // misclicked `purge` doesn't silently no-op.
        let err = DocumentsCmd::try_parse_from(["documents", "purge"]).unwrap_err();
        assert!(
            err.to_string().contains("--older-than"),
            "expected '--older-than' in error, got: {err}"
        );
    }

    #[test]
    fn purge_accepts_older_than_dry_run_force() {
        let cmd = DocumentsCmd::try_parse_from([
            "documents",
            "purge",
            "--older-than",
            "30d",
            "--dry-run",
            "--force",
        ])
        .expect("parse must succeed");
        match cmd.action {
            DocumentsAction::Purge {
                older_than,
                dry_run,
                force,
            } => {
                assert_eq!(older_than, Some(Duration::from_secs(30 * 86_400)));
                assert!(dry_run);
                assert!(force);
            }
        }
    }
}
