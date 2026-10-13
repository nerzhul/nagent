//! Snapshot tests for the operator CLI parser surface (plan
//! `1792262220000-clap-derive-migration`, AGENTS.md §5). Each
//! test asserts the exact clap error / help rendering the binary
//! produces so a future clap upgrade or surface change forces a
//! visible diff. Pure-logic tests (e.g. `compute_target`,
//! `humantime` parsing) stay next to the code they exercise.

use clap::Parser;
use nagent_server::cli::{auth::AuthCmd, documents::DocumentsCmd, migrate::MigrateCmd, Cli};

// === Cli top-level ====================================================

#[test]
fn cli_unknown_flag_is_rejected() {
    let err = Cli::try_parse_from(["stt-server", "--bogus"]).unwrap_err();
    let rendered = err.to_string();
    assert!(
        rendered.contains("unexpected argument") || rendered.contains("--bogus"),
        "expected clap's unexpected-argument wording, got: {rendered}"
    );
}

#[test]
fn cli_config_requires_value() {
    let err = Cli::try_parse_from(["stt-server", "--config"]).unwrap_err();
    let rendered = err.to_string();
    assert!(
        rendered.contains("--config") && rendered.contains("value"),
        "expected clap's missing-value wording, got: {rendered}"
    );
}

#[test]
fn cli_accepts_global_config_before_subcommand() {
    let cli = Cli::try_parse_from([
        "stt-server",
        "--config",
        "/etc/nagent.toml",
        "auth",
        "list-users",
    ])
    .expect("clap must accept --config before the subcommand");
    assert_eq!(
        cli.config.as_deref(),
        Some(std::path::Path::new("/etc/nagent.toml"))
    );
    assert!(matches!(
        cli.command,
        Some(nagent_server::cli::Command::Auth(_))
    ));
}

// === auth =============================================================

#[test]
fn auth_create_admin_requires_email() {
    let err = AuthCmd::try_parse_from(["auth", "create-admin"]).unwrap_err();
    assert!(
        err.to_string().contains("--email"),
        "expected '--email' in error, got: {err}"
    );
}

#[test]
fn auth_create_admin_rejects_password_and_from_stdin() {
    // clap enforces `conflicts_with` between `--password` and
    // `--from-stdin`; a regression that drops the constraint
    // would let the operator pass both and produce ambiguous
    // behaviour.
    let err = AuthCmd::try_parse_from([
        "auth",
        "create-admin",
        "--email",
        "ops@example.com",
        "--password",
        "hunter2",
        "--from-stdin",
    ])
    .unwrap_err();
    assert!(
        err.to_string().contains("cannot be used with"),
        "expected 'cannot be used with' error, got: {err}"
    );
}

#[test]
fn auth_list_users_accepts_provider_filter() {
    let cmd = AuthCmd::try_parse_from(["auth", "list-users", "--provider", "local"])
        .expect("parse must succeed");
    match cmd.action {
        nagent_server::cli::auth::AuthAction::ListUsers { provider, .. } => {
            assert_eq!(provider.as_deref(), Some("local"));
        }
        other => panic!("expected ListUsers, got {other:?}"),
    }
}

#[test]
fn auth_delete_user_requires_email() {
    let err = AuthCmd::try_parse_from(["auth", "delete-user"]).unwrap_err();
    assert!(
        err.to_string().contains("--email"),
        "expected '--email' in error, got: {err}"
    );
}

// === migrate ==========================================================

#[test]
fn migrate_up_accepts_force() {
    let cmd = MigrateCmd::try_parse_from(["migrate", "up", "--force"]).expect("parse must succeed");
    match cmd.action {
        nagent_server::cli::migrate::MigrateAction::Up { force } => assert!(force),
        other => panic!("expected Up, got {other:?}"),
    }
}

#[test]
fn migrate_down_accepts_steps_to_yes_force() {
    let cmd = MigrateCmd::try_parse_from(["migrate", "down", "--steps", "2", "--yes", "--force"])
        .expect("parse must succeed");
    match cmd.action {
        nagent_server::cli::migrate::MigrateAction::Down {
            steps,
            to,
            yes,
            force,
        } => {
            assert_eq!(steps, Some(2));
            assert_eq!(to, None);
            assert!(yes);
            assert!(force);
        }
        other => panic!("expected Down, got {other:?}"),
    }
}

#[test]
fn migrate_down_accepts_to_version() {
    let cmd = MigrateCmd::try_parse_from(["migrate", "down", "--to", "1", "--yes"])
        .expect("parse must succeed");
    match cmd.action {
        nagent_server::cli::migrate::MigrateAction::Down { steps, to, yes, .. } => {
            assert_eq!(steps, None);
            assert_eq!(to, Some(1));
            assert!(yes);
        }
        other => panic!("expected Down, got {other:?}"),
    }
}

#[test]
fn migrate_down_rejects_steps_and_to_together() {
    let err =
        MigrateCmd::try_parse_from(["migrate", "down", "--steps", "2", "--to", "1"]).unwrap_err();
    assert!(
        err.to_string().contains("cannot be used with"),
        "expected 'cannot be used with' error, got: {err}"
    );
}

#[test]
fn migrate_status_accepts_json_and_force() {
    let cmd = MigrateCmd::try_parse_from(["migrate", "status", "--json", "--force"])
        .expect("parse must succeed");
    match cmd.action {
        nagent_server::cli::migrate::MigrateAction::Status { json, force } => {
            assert!(json);
            assert!(force);
        }
        other => panic!("expected Status, got {other:?}"),
    }
}

// === documents ========================================================

#[test]
fn documents_purge_requires_older_than() {
    let err = DocumentsCmd::try_parse_from(["documents", "purge"]).unwrap_err();
    assert!(
        err.to_string().contains("--older-than"),
        "expected '--older-than' in error, got: {err}"
    );
}

#[test]
fn documents_purge_accepts_humantime_durations() {
    for input in ["30d", "12h", "5m", "90s", "2592000"] {
        let cmd = DocumentsCmd::try_parse_from(["documents", "purge", "--older-than", input])
            .unwrap_or_else(|e| panic!("{input}: {e}"));
        match cmd.action {
            nagent_server::cli::documents::DocumentsAction::Purge { older_than, .. } => {
                assert!(
                    older_than.is_some(),
                    "{input}: older_than must be Some after a valid humantime value"
                );
            }
        }
    }
}

#[test]
fn documents_purge_rejects_non_humantime_duration() {
    let err =
        DocumentsCmd::try_parse_from(["documents", "purge", "--older-than", "30x"]).unwrap_err();
    assert!(
        err.to_string().contains("invalid") || err.to_string().contains("--older-than"),
        "expected an error mentioning the bad duration, got: {err}"
    );
}
