//! `cli/` — operator-facing subcommands (`auth`, `migrate`, `documents`).
//!
//! Dispatched from [`crate::main`] BEFORE the `CliArgs` parser so a
//! flag like `--config FOO auth list-users` routes correctly even when
//! the operator puts the global flag before the subcommand.
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

pub mod auth;
pub mod documents;
pub mod migrate;
