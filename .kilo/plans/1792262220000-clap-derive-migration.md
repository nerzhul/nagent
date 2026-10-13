# Implement clap derive for the operator CLI

## Goal

Replace every hand-rolled argv parser in `nagent-server` with `clap = "4"`
(derive) so the operator CLI surface is one source of truth: per-subcommand
structs own their flags, `Cli::parse()` replaces the brittle pre-dispatch in
`main.rs`, and `--help` / unknown-arg errors come from the crate. This is
catalogue item **#1** (`1791919024803-crate-swap-catalogue.md`) — biggest safe
win in the catalogue.

## Scope

**In scope.** Every argv parser + every manual help printer inside
`crates/nagent-server/src/cli/*` and `crates/nagent-server/src/main.rs`,
plus the `CliArgs` type itself in `crates/nagent-server/src/config/server.rs`.

**Out of scope.** The runtime knobs behind the parsed flags (auth DB,
PID-file guard, migration primitives, documents purge, etc.) are
unchanged. This plan only touches the parser surface.

## Behaviour to preserve (acceptance criteria)

1. `--config PATH` and `--config=PATH` both accepted; global flag can be
   placed before or after the subcommand name (existing callsite
   patterns):
   - `nagent-server auth list-users`
   - `nagent-server --config /etc/nagent.toml auth list-users`
   - `nagent-server --config=/etc/nagent.toml auth list-users`
2. PID-file guard (`data/server.pid` + `kill -0 <pid>`) stays identical
   for every subcommand. `--force` opt-out preserved.
3. `auth create-admin` requires `--email` and exactly one of
   `--password` / `--from-stdin`.
4. `auth list-users --provider <local|oidc|passkey>` is an optional
   filter.
5. `auth delete-user --email` + optional `--yes`; refuses when the
   deletion would leave zero local users.
6. `migrate up [--force]`; `migrate down [--steps N | --to VERSION]
   [--yes] [--force]` (the two numeric flags are mutually exclusive);
   `migrate status [--json] [--force]`; `migrate help`.
7. `documents purge --older-than <DURATION> [--dry-run] [--force]`.
   The `<DURATION>` format stays the project's `humantime`-style
   parser (`Nd|Nh|Nm|Ns`, bare integer = seconds, case-insensitive).
8. Exit codes: `0` on success, `1` on user abort (delete-user prompt),
   `2` on parse error (clap default — matches the existing hand-rolled
   behaviour).
9. Help printers stay verbatim or strictly equivalent in content. The
   catalog commits to `--help` snapshot tests per AGENTS.md §5.
10. The `CliArgs` type stays `pub use`-re-exported from
    `nagent_server::` (the catalog's "out of scope" list protects
    `CliArgs::parse`, but we keep the type as a stable DTO so
    downstream consumers don't break).

## Design

### `Cargo.toml` (workspace) addition

```toml
[workspace.dependencies]
# `derive` powers the `#[derive(Parser)]` / `#[derive(Subcommand)]`
# surface used by `crates/nagent-server/src/cli/*`; `wrap_help`
# wraps `--help` output at the terminal width.
clap = { version = "4", features = ["derive", "wrap_help"] }
```

### `crates/nagent-server/Cargo.toml` dep

```toml
clap = { workspace = true }
```

### `crates/nagent-server/src/main.rs` — collapse the dispatch

The pre-parser dispatcher (lines 22-56) is removed: clap now owns
subcommand detection and the global `--config` flag. The new `main`:

```rust
let cli = Cli::parse();
match cli.command {
    Some(Command::Auth(c))    => return auth_cli::run(c, cli.config.as_ref()).await,
    Some(Command::Migrate(c)) => return migrate_cli::run(c, cli.config.as_ref()).await,
    Some(Command::Documents(c)) => return documents_cli::run(c, cli.config.as_ref()).await,
    None => { /* fall through to server boot */ }
}
```

`CliArgs` keeps its old shape (`{ pub config: Option<PathBuf> }`) and
remains re-exported from `nagent_server::` so the catalog's "out of
scope" promise holds. It is no longer parsed by hand; the public
`CliArgs::parse()` method is **removed** (it was a free function
sitting on a single field type, so removing it does not affect any
crate-public API outside this binary).

### `crates/nagent-server/src/config/server.rs`

Replace `impl CliArgs` with a one-liner trait import if needed; the
type shrinks to:

```rust
#[derive(Debug, Default, Clone, Parser)]
pub struct CliArgs {
    /// Path to the optional TOML configuration file.
    #[arg(long, value_name = "PATH", global = true)]
    pub config: Option<PathBuf>,
}
```

Actually, `CliArgs` will lose its `Parser` derive (only `Cli` needs
that); it stays `Default + Clone + Debug` as a plain DTO, and the new
top-level `Cli` in `main.rs` (or a new `cli::top_level::Cli` module)
holds the `#[derive(Parser)]` surface. The existing
`pub use config::{CliArgs, Config, ConfigError}` re-export is
preserved.

### `crates/nagent-server/src/cli/mod.rs`

Becomes the home of the top-level `Cli` + `Command` enum (so the
existing `pub use` paths stay stable):

```rust
pub mod auth;
pub mod documents;
pub mod migrate;
pub mod shared;

#[derive(Parser, Debug)]
#[command(name = "stt-server", bin_name = "stt-server",
          about = "STT pipeline with optional LLM proxy and chat agents",
          long_about = "...")]
pub struct Cli {
    #[arg(long, value_name = "PATH", global = true)]
    pub config: Option<PathBuf>,
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    Auth(AuthCmd),
    Migrate(MigrateCmd),
    Documents(DocumentsCmd),
}
```

### `crates/nagent-server/src/cli/shared.rs` (new)

Holds the helpers currently duplicated 3× across `auth.rs` /
`migrate.rs` / `documents.rs`:

- `pub fn pid_file_path() -> PathBuf` (currently private to auth.rs:237).
- `pub fn check_running_server(force: bool) -> Result<(), anyhow::Error>`
  (replaces the 3× copy).
- `pub fn ensure_sqlite_parent_dir(url: &str) -> Result<...>` is
  **already** `pub` in `cli::auth` (auth.rs:100) and re-used from
  `cli::migrate` / `cli::documents`; move it here so all three import
  from `cli::shared` instead of `cli::auth`. (No public-API change —
  `cli::auth::ensure_sqlite_parent_dir` keeps its path via a
  `pub use shared::ensure_sqlite_parent_dir;` alias.)

### `crates/nagent-server/src/cli/auth.rs`

`split_global_flags` is deleted. `print_help()` is deleted (clap owns
it). The three `parse_*` functions collapse to:

```rust
#[derive(Parser, Debug)]
pub struct AuthCmd { #[command(subcommand)] pub action: AuthAction }

#[derive(Subcommand, Debug)]
pub enum AuthAction {
    CreateAdmin {
        #[arg(long)] email: String,
        #[arg(long, conflicts_with = "from_stdin")] password: Option<String>,
        #[arg(long)] from_stdin: bool,
        #[arg(long)] force: bool,
    },
    ListUsers {
        #[arg(long)] provider: Option<String>,
        #[arg(long)] force: bool,
    },
    DeleteUser {
        #[arg(long)] email: String,
        #[arg(long)] yes: bool,
        #[arg(long)] force: bool,
    },
}

pub async fn run(cmd: AuthCmd, config: Option<&PathBuf>) -> Result<ExitCode, anyhow::Error> {
    match cmd.action {
        AuthAction::CreateAdmin { email, password, from_stdin, force } => {
            check_running_server(force)?;
            // ...existing body, sans `args: Vec<String>` parameter
        }
        AuthAction::ListUsers { .. } => { ... }
        AuthAction::DeleteUser { .. } => { ... }
    }
}
```

The `_silence_*` placeholder hacks at the bottom of `auth.rs` are
deleted (they were `#[allow(dead_code)]` workarounds for unused
imports that are no longer needed once we stop importing the unused
types).

### `crates/nagent-server/src/cli/migrate.rs`

Same shape as `auth.rs`:

```rust
#[derive(Parser, Debug)]
pub struct MigrateCmd { #[command(subcommand)] pub action: MigrateAction }

#[derive(Subcommand, Debug)]
pub enum MigrateAction {
    Up { #[arg(long)] force: bool },
    Down {
        #[arg(long, conflicts_with = "to")] steps: Option<usize>,
        #[arg(long)] to: Option<i64>,
        #[arg(long)] yes: bool,
        #[arg(long)] force: bool,
    },
    Status {
        #[arg(long)] json: bool,
        #[arg(long)] force: bool,
    },
}
```

The 12 unit tests in the `mod tests` block are rewritten to drive the
clap parser via `try_parse_from(argv)` (clap's test-friendly entry
point). The `compute_target_*` tests are **kept verbatim** — they
exercise pure logic on `DownOpts`/`MigrationStatus`, not the parser.
For consistency with clap-derived output, a new test-only struct
`DownOptsTest` mirrors the old field shape but is built from clap's
parsed output:

```rust
let cli = MigrateAction::try_parse_from(["migrate", "down",
    "--steps", "2", "--yes", "--force"]).unwrap();
let down = match cli { MigrateAction::Down { steps, to, yes, force } => DownOpts { steps, to, yes, force }, _ => unreachable!() };
```

The `cli_args_parse_space_and_equals_forms` test in
`config/mod.rs:302-332` is replaced by a test that drives clap
directly: `Cli::try_parse_from(["--config", "/etc/nagent.toml"])`.

### `crates/nagent-server/src/cli/documents.rs`

Same as above. The `parse_duration` helper is **kept** and re-exported
unchanged — clap's built-in humantime parser produces a different
`Duration` error shape and the existing `parse_duration_rejects_*`
tests rely on the existing strings.

```rust
#[derive(Parser, Debug)]
pub struct DocumentsCmd { #[command(subcommand)] pub action: DocumentsAction }

#[derive(Subcommand, Debug)]
pub enum DocumentsAction {
    Purge {
        #[arg(long, value_parser = parse_duration_or_unset)]
        older_than: Option<Duration>,
        #[arg(long)] dry_run: bool,
        #[arg(long)] force: bool,
    },
}
```

`parse_duration_or_unset` is a small `value_parser` wrapper that
returns `Duration` from the existing `parse_duration` helper.

The `parse_purge_requires_older_than` test is replaced by a clap test
that asserts `Cli::try_parse_from(["documents", "purge"])` fails with
"the following required arguments were not provided" — the wording is
clap-native; we add a snapshot test for the rendered error.

## Documentation sync (AGENTS.md §5)

- `README.md:518` — the existing `--force` footnote remains valid.
  No change required (clap's `--help` already shows the flag).
- `README.md:57-60, 226-246` — the `--config <path>` documentation
  remains valid (clap's `--help` shows the same flag, with the same
  env > TOML > default precedence documented elsewhere).
- `crates/nagent-server/src/cli/auth.rs:7-12` — module docstring is
  updated to drop the "hand-rolled flag parsing" claim and the
  "matches `CliArgs::parse` style — no `clap` dep" sentence.
- `crates/nagent-server/src/cli/migrate.rs:3-8` — same update.
- `crates/nagent-server/src/main.rs:15-56` — the comment block
  explaining "Hand-rolled flag parsing… `stt-server auth …` subcommand
  dispatch happens FIRST" is replaced by a one-paragraph comment
  pointing to `cli::Cli` and `cli::Command`.
- `crates/nagent-server/src/config/server.rs:99-103` — the comment on
  `CliArgs` ("Parsed manually (no `clap` dep)") is replaced by a
  reference to `cli::Cli`.

## Test plan

### Unit (rewritten in this PR)

| Test file | Existing tests | Action |
|---|---|---|
| `config/mod.rs` | `cli_args_parse_space_and_equals_forms` | Rewrite via `Cli::try_parse_from`. |
| `cli/migrate.rs` | `parse_down_*`, `parse_up_*`, `parse_status_*` (6 tests) | Rewrite via `MigrateAction::try_parse_from`. |
| `cli/migrate.rs` | `compute_target_*` (6 tests) | **Keep verbatim** — pure-logic on `DownOpts`/`MigrationStatus`. |
| `cli/documents.rs` | `parse_purge_*` (2 tests), `split_global_flags_strips_only_config` (1 test) | Rewrite via `DocumentsAction::try_parse_from` + remove the split test (no longer applicable). |
| `cli/documents.rs` | `parse_duration_*` (2 tests) | **Keep verbatim** — parser logic unchanged. |

### Snapshot tests (new, per AGENTS.md §5)

Add `crates/nagent-server/src/cli/snapshots.rs` (or extend each
module's tests) with `assert_cmd` snapshots for:

- `Cli::try_parse_from([]).unwrap_err().to_string()` for unknown flag
  (e.g. `["--bogus"]`).
- `Cli::try_parse_from(["--bogus"])` rendering → matches clap's
  standard "error: unexpected argument found" message.
- `Cli::try_parse_from(["--config"]).unwrap_err()` rendering →
  matches "error: a value is required for '--config <PATH>'".
- `Cli::try_parse_from(["auth", "--help"])` rendering → covers the
  whole `auth` help tree.
- `Cli::try_parse_from(["migrate", "down", "--steps", "abc"])`
  rendering → covers the numeric-parse error.

These snapshots are committed under
`crates/nagent-server/src/snapshots/cli__*.snap` (or inline strings in
the test). The exact snapshot tool is not material; what matters is
that a `--help` change forces a visible diff.

### Integration tests (unchanged)

`tests/migrate_cli.rs` only hits the data layer (`Db::migrate`,
`Db::migration_status`, `Db::revert_to`). No change required.

## Validation (this PR runs all four)

```
cargo fmt --all
cargo build -p nagent-server --all-targets --all-features
cargo clippy -p nagent-server --all-targets --all-features -- -D warnings
cargo test -p nagent-server --all-features
cargo audit
```

Acceptance: zero warnings, zero failing tests, audit clean.

## Commit

Single commit. Title (≤120 chars):

```
feat(cli): migrate operator argv parsing from hand-rolled to clap derive
```

Body (≤5 sentences, ≤450 chars):

```
Replaces the hand-rolled argv walkers in cli/{auth,migrate,documents}.rs
plus the pre-parser dispatcher in main.rs with clap derive. Preserves
the public CliArgs DTO so downstream consumers are unchanged; --config,
--force, --from-stdin, --provider, --steps/--to, --json, --older-than
all keep their semantics. Errors and --help output now come from clap
with snapshot tests guarding the wire contract.
```

The Co-authored-by trailer follows the AGENTS.md §7 rule.

## LOC budget

Catalogue estimate: ~500–650 LOC saved. Realistic split after the
rewrite:

- `cli/auth.rs` 506 → ~250 LOC (−256)
- `cli/migrate.rs` 649 → ~430 LOC (−219)
- `cli/documents.rs` 359 → ~250 LOC (−109)
- `main.rs` 101 → ~70 LOC (−31)
- `config/server.rs` CliArgs impl 144 → ~10 LOC (−134) [CliArgs parser
  impl + helper function]
- `config/mod.rs` test block ~30 → ~10 LOC (−20)
- New `cli/shared.rs` ~40 LOC (deduplicated PID guard)
- New `cli/snapshots.rs` ~80 LOC (added snapshot coverage)

Net: **~770 LOC deleted**, **~120 LOC added** → **~650 net LOC saved**.
At the upper end of the catalogue's range.