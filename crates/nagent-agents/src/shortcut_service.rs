//! `shortcut` integration entry in the per-user service catalogue.
//!
//! `ServiceRegistry` is the catalogue the chat UI iterates to
//! render the "Add integration" form (`/api/integrations`); the
//! `ServiceDef` below describes the fields the Shortcut
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
//! - `show_dashboard` (Checkbox) — gates the "Stories" dashboard tab.
//! - `default_team` (Text, JSON `{"id","name"}`) — the team
//!   `shortcut_create_story` targets by default (`group_id`).
//! - `story_templates` (Text, JSON array) — the per-user story
//!   shortcuts applied through `shortcut_create_story`'s
//!   `template` argument.
//!
//! Per `docs/architecture.md` §2.6, `ServiceDef` is plain data so
//! the file lives in the `nagent-agents` crate; the runtime
//! behaviour (probe endpoint, audit rows) is wired in
//! `nagent-server` behind the same `shortcut-agent` cargo feature
//! as the chat agents.

use crate::services::{
    FieldDef, FieldKind, ServiceDef, ECHO_ON_EDIT_NON_PASSWORD, ECHO_ON_EDIT_PASSWORD,
};

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
        FieldDef {
            key: "show_dashboard",
            label: "Show my stories dashboard",
            kind: FieldKind::Checkbox,
            required: false,
            help: Some(
                "Adds a \"Stories\" tab to the top menu: a pure-API list of your Shortcut \
                 stories (never goes through the LLM) with a click-to-read detail view. \
                 Requires the API token above.",
            ),
            placeholder: None,
            options: &[],
            // Not a secret: the saved on/off state is echoed back
            // so the checkbox stays ticked when the form reopens.
            echo_on_edit: ECHO_ON_EDIT_NON_PASSWORD,
        },
        FieldDef {
            key: "default_team",
            label: "Default team for new stories",
            kind: FieldKind::Text,
            required: false,
            help: Some(
                "Team that `shortcut_create_story` targets when the chat request does not \
                 name one. Pick it in the dropdown below (loaded live from your workspace \
                 via `GET /teams`); the form stores the team UUID and the chat tool sends \
                 it as `group_id` when the story is created.",
            ),
            placeholder: Some("— picked from the team list —"),
            options: &[],
            // Not a secret: the saved team echoes back so the
            // dropdown re-selects it when the form reopens. The row
            // is still AES-256-GCM encrypted at rest in the vault.
            echo_on_edit: ECHO_ON_EDIT_NON_PASSWORD,
        },
        FieldDef {
            key: "story_templates",
            label: "Story shortcuts (templates)",
            kind: FieldKind::Text,
            required: false,
            help: Some(
                "JSON array of story templates the chat can apply through \
                 `shortcut_create_story`'s `template` argument, e.g. \
                 `[{\"name\":\"bug\",\"story_type\":\"bug\",\"labels\":[\"bug\"],\
                 \"description_template\":\"## Steps\\n{details}\"}]`. `name` is the \
                 shortcut key, `story_type` is feature|bug|chore, `labels` a string array, \
                 and `description_template` may use `{name}` / `{details}` placeholders.",
            ),
            placeholder: Some("[{\"name\":\"bug\",\"story_type\":\"bug\",\"labels\":[\"bug\"]}]"),
            options: &[],
            // Not a secret: the saved JSON echoes back so the editor
            // reopens with the templates intact.
            echo_on_edit: ECHO_ON_EDIT_NON_PASSWORD,
        },
    ],
    docs_url: Some("https://github.com/nagent/nagent/blob/main/docs/integrations/shortcut.md"),
};
