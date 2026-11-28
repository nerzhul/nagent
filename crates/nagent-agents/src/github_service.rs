//! `github` integration entry in the per-user service catalogue.
//!
//! `ServiceRegistry` is the catalogue the chat UI iterates to
//! render the "Add integration" form (`/api/integrations`); the
//! `ServiceDef` below describes the fields the GitHub connector
//! stores under the `github` service id.
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
//! - `accounts` (Text, managed) — **multi-account** support (plan
//!   1795793328177): a JSON array of
//!   `{"name", "token", "username", "scopes"}` entries holding the
//!   *extra* named GitHub accounts (cap `[agents.github].max_accounts`,
//!   default 8). The `"default"` account is always the
//!   `personal_access_token` above and is never stored in this
//!   array. The blob contains several tokens, so it is a
//!   multi-credential field under AGENTS.md §6: managed **only**
//!   through the dedicated `…/github/accounts` endpoints, never
//!   echoed on edit (`echo_on_edit = false`), never logged, never
//!   formatted into an error string.
//! - `default_account` (Text) — name of the account used when a
//!   `github_*` tool call has no explicit `account` argument.
//!   Defaults to `"default"`. Echoed on edit so the form can
//!   pre-select it.
//! - `repo_accounts` (Text, managed) — JSON map
//!   `"owner/repo" → "<account>"` recording which account can read
//!   each dashboard repo (written by `probe-repo`, read by the
//!   repo-scoped tools and the PR dashboard to skip re-probing).
//!   Managed out of band; never echoed.
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
        FieldDef {
            key: "default_account",
            label: "Default account",
            kind: FieldKind::Text,
            required: false,
            help: Some(
                "Which GitHub account the `github_*` tools use when no explicit \
                 `account` argument is given (plan 1795793328177). Defaults to \
                 `\"default\"` (the personal access token above). The integrations \
                 form renders this as a dropdown fed by `GET \
                 /api/integrations/github/accounts`.",
            ),
            placeholder: Some("default"),
            options: &[],
            // Not a secret: the saved account name echoes back so the
            // selector pre-selects it when the form reopens.
            echo_on_edit: ECHO_ON_EDIT_NON_PASSWORD,
        },
        FieldDef {
            key: "accounts",
            label: "Additional GitHub accounts",
            kind: FieldKind::Text,
            required: false,
            help: Some(
                "Extra named GitHub accounts (JSON array of {name, token, username, \
                 scopes}, max `[agents.github].max_accounts`). Managed ONLY through \
                 the dedicated `GET/POST/DELETE /api/integrations/github/accounts` \
                 endpoints from the Accounts section of this form — never through \
                 the generic credentials PUT, and never echoed back: the blob \
                 contains tokens (AGENTS.md §6).",
            ),
            placeholder: None,
            options: &[],
            // Multi-credential field: NEVER echoed (AGENTS.md §6).
            echo_on_edit: false,
        },
        FieldDef {
            key: "repo_accounts",
            label: "Repo → account map",
            kind: FieldKind::Text,
            required: false,
            help: Some(
                "Internal JSON map `\"owner/repo\" → account name` recording which \
                 account can read each dashboard repo (written by probe-repo, read \
                 by the repo-scoped tools). Managed out of band; never echoed.",
            ),
            placeholder: None,
            options: &[],
            echo_on_edit: false,
        },
    ],
    // `accounts` and `repo_accounts` are written out of band through
    // the dedicated endpoints, so the generic form never renders or
    // submits them (a hidden field would risk the tokens round-
    // tripping through the generic PUT — see plan §1.1).
    managed_fields: &["accounts", "repo_accounts"],
    docs_url: Some("https://github.com/nagent/nagent/blob/main/docs/integrations/github.md"),
};

#[cfg(test)]
mod tests {
    use super::*;

    /// AGENTS.md §6: the `accounts` field is a multi-credential
    /// blob — even if a caller hands `to_summary` the decrypted
    /// plaintext map, the field's `echo_on_edit = false` must keep
    /// `value` at `None` so no token inside the blob can leak into
    /// the GET /api/integrations JSON.
    #[test]
    fn accounts_field_is_never_echoed_even_with_plaintext() {
        let mut plaintext = std::collections::HashMap::new();
        plaintext.insert(
            "accounts".to_string(),
            "[{\"name\":\"client-a\",\"token\":\"ghp_supersecret\"}]".to_string(),
        );
        plaintext.insert("repo_accounts".to_string(), "{}".to_string());
        let summary = GITHUB_SERVICE.to_summary(
            &[
                "personal_access_token".to_string(),
                "username".to_string(),
                "accounts".to_string(),
                "repo_accounts".to_string(),
                "default_account".to_string(),
            ],
            &plaintext,
        );
        for f in &summary.fields {
            if f.key == "accounts" || f.key == "repo_accounts" {
                assert!(
                    f.value.is_none(),
                    "field `{}` must never echo (AGENTS.md §6), got {:?}",
                    f.key,
                    f.value
                );
            }
        }
        let json = serde_json::to_value(&summary).unwrap();
        let text = json.to_string();
        assert!(
            !text.contains("ghp_supersecret"),
            "no token from the accounts blob may appear in the summary JSON"
        );
        // managed fields reach the UI so it hides those inputs.
        assert!(summary.managed_fields.contains(&"accounts"));
        assert!(summary.managed_fields.contains(&"repo_accounts"));
        // default_account is a normal echoed field.
        let da = summary
            .fields
            .iter()
            .find(|f| f.key == "default_account")
            .unwrap();
        assert!(da.value.is_none(), "not in the plaintext map here");
    }
}
