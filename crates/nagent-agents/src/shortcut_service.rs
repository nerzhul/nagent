//! `shortcut` integration entry in the per-user service catalogue.
//!
//! `ServiceRegistry` is the catalogue the chat UI iterates to
//! render the "Add integration" form (`/api/integrations`); the
//! `ServiceDef` below describes the two fields the Shortcut
//! connector stores under the `shortcut` service id.
//!
//! - `api_token` (Password) — a Shortcut API token generated at
//!   <https://app.shortcut.com/settings/account/api-tokens>.
//!   Stored AES-256-GCM encrypted at rest; never logged, never
//!   echoed back, never surfaced in an error string (AGENTS.md §6).
//! - `username` (Text) — the Shortcut `mention_name`, filled in by
//!   the setup probe (`POST /api/integrations/shortcut/probe`) so
//!   the chat UI can say "Connected as @jane". Credential-
//!   adjacent: `echo_on_edit = false` so the saved value is
//!   not re-displayed on every form open.
//!
//! Per `docs/architecture.md` §2.6, `ServiceDef` is plain data so
//! the file lives in the `nagent-agents` crate; the runtime
//! behaviour (probe endpoint, audit rows) is wired in
//! `nagent-server` behind the same `shortcut-agent` cargo feature
//! as the chat agents.

use crate::services::{FieldDef, FieldKind, ServiceDef, ECHO_ON_EDIT_PASSWORD};

/// `shortcut` integration entry in the per-user service catalogue.
///
/// The `api_token` is supplied by the user on the integrations
/// form and validated by the setup probe
/// (`POST /api/integrations/shortcut/probe`), which calls
/// `GET /members` and matches the `username` handle against the
/// returned member list; the resolved `mention_name` is stored into
/// the `username` field.
pub const SHORTCUT_SERVICE: ServiceDef = ServiceDef {
    id: "shortcut",
    display_name: "Shortcut (app.shortcut.com)",
    icon: "🎯",
    fields: &[
        FieldDef {
            key: "api_token",
            label: "API token",
            kind: FieldKind::Password,
            required: true,
            help: Some(
                "Shortcut API token, generated at \
                 app.shortcut.com/settings/account/api-tokens. Validated by the probe before \
                 saving. Stored AES-256-GCM encrypted at rest; never logged or displayed.",
            ),
            placeholder: Some("token from app.shortcut.com/settings/account/api-tokens"),
            options: &[],
            echo_on_edit: ECHO_ON_EDIT_PASSWORD,
        },
        FieldDef {
            key: "username",
            label: "Shortcut mention name",
            kind: FieldKind::Text,
            required: true,
            help: Some(
                "Your Shortcut `mention_name` (without @), filled automatically by the probe \
                 after the token is validated. Used only for display (\"Connected as @jane\") \
                 and to ground `owner:` search queries.",
            ),
            placeholder: Some("jane"),
            options: &[],
            // Credential-adjacent: not echoed back on edit (same
            // posture as the GitHub `username` field).
            echo_on_edit: false,
        },
    ],
    docs_url: Some("https://github.com/nagent/nagent/blob/main/docs/integrations/shortcut.md"),
};
