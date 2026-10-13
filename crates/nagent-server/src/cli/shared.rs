//! Shared helpers for the operator CLI subcommands. The PID-file
//! guard and sqlite-parent-dir mkdir were previously duplicated
//! across [`crate::cli::auth`], [`crate::cli::migrate`], and
//! [`crate::cli::documents`]; they live here so each subcommand can
//! import them once (plan `1792262220000-clap-derive-migration`).

use std::path::{Path, PathBuf};
use std::process::Command;

/// Path of the PID file written by `crate::main` on a clean
/// boot. Relative to the current working directory to match
/// `data/` everywhere else in the project; a future PR may move
/// this to a configurable knob if the operator deploys with a
/// non-standard layout.
pub fn pid_file_path() -> PathBuf {
    PathBuf::from("data").join("server.pid")
}

/// Refuse to run when `data/server.pid` exists AND points at a
/// live process AND `force` is `false`. Stale PID files (process
/// gone but file remains) are silently treated as "no server" so
/// a crash-recovery operator is not blocked.
pub fn check_running_server(force: bool) -> Result<(), anyhow::Error> {
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
        // Stale PID file → silently allow the CLI to proceed.
        Err(_) => return Ok(()),
    };
    if force {
        return Ok(());
    }
    // `kill -0 <pid>` is the unix way to check whether a pid is
    // alive without sending a signal. Spawning `kill` itself keeps
    // this crate nix-free; exit status 0 = alive, nonzero = gone.
    let status = Command::new("kill")
        .arg("-0")
        .arg(pid_num.to_string())
        .status();
    match status {
        Ok(s) if s.success() => Err(anyhow::anyhow!(
            "a server is already running (PID {} from {}); pass --force to override",
            pid_num,
            pid_path.display()
        )),
        // PID gone (process exited without removing the file):
        // silently allow the CLI to proceed.
        _ => Ok(()),
    }
}

/// Best-effort `mkdir -p` for the parent of a sqlite file:// URL.
/// Mirrors the same helper in
/// `crate::auth::boot::ensure_sqlite_parent_dir` so the CLI works
/// on a fresh host even when the operator pointed it at a
/// relative path under `./data/`. The server's `auto_bootstrap`
/// already does this — we duplicate the logic here rather than
/// export the boot helper because the CLI should not need to
/// know about the boot internals.
pub fn ensure_sqlite_parent_dir(url: &str) -> Result<(), anyhow::Error> {
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
    let p = Path::new(path);
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
