//! `cli/` — operator-facing subcommands (`auth`, `migrate`, `documents`).
//!
//! Dispatched from [`crate::main`] BEFORE the `CliArgs` parser so a
//! flag like `--config FOO auth list-users` routes correctly even when
//! the operator puts the global flag before the subcommand.

pub mod auth;
pub mod documents;
pub mod migrate;
