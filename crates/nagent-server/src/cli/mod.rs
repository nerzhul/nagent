//! `cli/` — operator-facing subcommands (`auth`, `migrate`, `documents`).
//!
//! Driven by `clap` derive (plan `1792262220000-clap-derive-migration`);
//! the top-level [`Cli`] parser owns the global `--config PATH` flag
//! and dispatches to one of the three subcommand namespaces via
//! [`Command`]. Each namespace holds its own `#[derive(Parser)]`
//! argument structs so the surface stays declarative and `--help` /
//! unknown-flag handling come from clap itself.
//!
//! Subcommands:
//!
//! - [`auth`] — `stt-server auth {create-admin,list-users,delete-user}`.
//! - [`migrate`] — `stt-server migrate {up,down,status,help}`. Drives
//! the auth DB migration state machine without booting the HTTP
//! server.
//! - [`documents`] — `stt-server documents purge ...`. Operates
//! against the document cache + DB rows without booting the HTTP
//! server.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

pub mod auth;
pub mod documents;
pub mod migrate;
pub mod shared;

#[cfg(test)]
mod snapshots;

/// Top-level operator CLI. Parses the global `--config PATH` flag
/// plus an optional subcommand. When no subcommand is given
/// (`stt-server --config FOO` with nothing else), the binary falls
/// through to the server boot path in [`crate::main`].
#[derive(Debug, Parser)]
#[command(
    name = "stt-server",
    bin_name = "stt-server",
    version,
    about = "STT pipeline with optional LLM proxy and chat agents",
    long_about = "STT pipeline with optional LLM proxy and chat agents.\n\
                  Without a subcommand the binary boots the HTTP/WebSocket server; \
                  the three subcommands (`auth`, `migrate`, `documents`) operate \
                  against the same configuration file without booting the server."
)]
pub struct Cli {
    /// Path to an optional TOML configuration file. When set,
    /// [`crate::Config::load`] reads it and overlays it under the
    /// env-vars (env > TOML > default). When unset, the layered
    /// discovery (`/etc/nagent/config.toml` then
    /// `$XDG_CONFIG_HOME/nagent/config.toml`) runs.
    #[arg(long, value_name = "PATH", global = true)]
    pub config: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Operator CLI for the auth subsystem.
    Auth(auth::AuthCmd),
    /// Operator CLI for the auth DB migration state machine.
    Migrate(migrate::MigrateCmd),
    /// Operator CLI for the documents subsystem.
    Documents(documents::DocumentsCmd),
}
