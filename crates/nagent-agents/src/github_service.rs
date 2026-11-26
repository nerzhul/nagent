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
//! - `show_dashboard` (Checkbox) — gates the "PRs" dashboard tab
//!   in the top menu (plan 1795686864347).
//! - `dashboard_repos` (Text) — the per-user confidential repo
//!   list for that dashboard: a JSON array of `"owner/repo"`
//!   strings (cap 50). Managed by the integrations UI through
//!   `POST /api/integrations/github/probe-repo`, which verifies
//!   access with the saved PAT before a repo is accepted. Stored
//!   AES-256-GCM encrypted at rest like every vault field.
//!
//! Per `docs/architecture.md` §2.6, `ServiceDef` is plain data so
//! the file lives in the `nagent-agents` crate; the runtime
//! behaviour (probe endpoint, audit rows) is wired in
//! `nagent-server` behind the same `github-agent` cargo feature
//! as the chat agents.

use crate::services::{
    FieldDef, FieldKind, ServiceDef, ECHO_ON_EDIT_NON_PASSWORD, ECHO_ON_EDIT_PASSWORD,
};

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
        FieldDef {
            key: "show_dashboard",
            label: "Show my PRs dashboard",
            kind: FieldKind::Checkbox,
            required: false,
            help: Some(
                "Adds a \"PRs\" tab to the top menu: a pure-API table of the open pull \
                 requests of the repositories listed below (never goes through the LLM). \
                 Requires the personal access token above.",
            ),
            placeholder: None,
            options: &[],
            // Not a secret: the saved on/off state is echoed back
            // so the checkbox stays ticked when the form reopens.
            echo_on_edit: ECHO_ON_EDIT_NON_PASSWORD,
        },
        FieldDef {
            key: "dashboard_repos",
            label: "Dashboard repositories",
            kind: FieldKind::Text,
            required: false,
            help: Some(
                "Repositories shown in the PRs dashboard, as a JSON array of \
                 `\"owner/repo\"` strings (max 50). Managed with the Add / remove \
                 buttons in the form: adding a repo verifies access with your saved \
                 token first (`POST /api/integrations/github/probe-repo`).",
            ),
            placeholder: Some("[\"octocat/hello-world\"]"),
            options: &[],
            // Repo names are not secrets: the saved JSON echoes back
            // so the chip list re-renders when the form reopens. The
            // row is still AES-256-GCM encrypted at rest in the vault.
            echo_on_edit: ECHO_ON_EDIT_NON_PASSWORD,
        },
    ],
    docs_url: Some("https://github.com/nagent/nagent/blob/main/docs/integrations/github.md"),
};
