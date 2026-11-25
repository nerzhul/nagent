//! `github` integration entry in the per-user service catalogue.
//!
//! `ServiceRegistry` is the catalogue the chat UI iterates to
//! render the "Add integration" form (`/api/integrations`); the
//! `ServiceDef` below describes the two fields the GitHub
//! connector stores under the `github` service id.
//!
//! - `personal_access_token` (Password) — a GitHub personal
//!   access token (classic PAT with `read:user` + `repo` scopes,
//!   or a fine-grained PAT). Stored AES-256-GCM encrypted at
//!   rest; never logged, never echoed back, never surfaced in
//!   an error string (AGENTS.md §6).
//! - `username` (Text) — the GitHub `login`, filled in by the
//!   setup probe (`POST /api/integrations/github/probe`) so the
//!   chat UI can say "Connected as @octocat". Credential-
//!   adjacent: `echo_on_edit = false` so the saved value is
//!   not re-displayed on every form open.
//!
//! Per `docs/architecture.md` §2.6, `ServiceDef` is plain data so
//! the file lives in the `nagent-agents` crate; the runtime
//! behaviour (probe endpoint, audit rows) is wired in
//! `nagent-server` behind the same `github-agent` cargo feature
//! as the chat agents.

use crate::services::{FieldDef, FieldKind, ServiceDef, ECHO_ON_EDIT_PASSWORD};

/// `github` integration entry in the per-user service catalogue.
///
/// The `personal_access_token` is supplied by the user on the
/// integrations form and validated by the setup probe
/// (`POST /api/integrations/github/probe`), which calls
/// `GET /user` and stores the returned `login` into the
/// `username` field.
pub const GITHUB_SERVICE: ServiceDef = ServiceDef {
    id: "github",
    display_name: "GitHub",
    icon: "🐙",
    fields: &[
        FieldDef {
            key: "personal_access_token",
            label: "Personal access token",
            kind: FieldKind::Password,
            required: true,
            help: Some(
                "GitHub personal access token. Classic PAT with `read:user` + `repo` scopes \
                 (for private repos), or a fine-grained PAT. Validated by the probe before \
                 saving. Stored AES-256-GCM encrypted at rest; never logged or displayed.",
            ),
            placeholder: Some("ghp_… or github_pat_…"),
            options: &[],
            echo_on_edit: ECHO_ON_EDIT_PASSWORD,
        },
        FieldDef {
            key: "username",
            label: "GitHub username (login)",
            kind: FieldKind::Text,
            required: true,
            help: Some(
                "Your GitHub `login`, filled automatically by the probe after the token is \
                 validated. Used only for display (\"Connected as @octocat\").",
            ),
            placeholder: Some("octocat"),
            options: &[],
            // Credential-adjacent: not echoed back on edit (same
            // posture as the CalDAV / email `username` fields).
            echo_on_edit: false,
        },
    ],
    docs_url: Some("https://github.com/nagent/nagent/blob/main/docs/integrations/github.md"),
};
