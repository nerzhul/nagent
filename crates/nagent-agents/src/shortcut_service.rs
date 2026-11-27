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
//!   **Fallback**: when the user connects via OAuth (the
//!   "Connect with Shortcut" button, `/api/auth/login/shortcut/*`),
//!   the `access_token` row below takes precedence and `api_token`
//!   is unused.
//! - `access_token` / `refresh_token` (Password) — the OAuth 2.0
//!   token pair written by the `/api/auth/login/shortcut/callback`
//!   handler and refreshed in place by the `shortcut_*` tools on
//!   expiry / 401. Sent as `Authorization: Bearer …`. All six OAuth
//!   rows are listed in `managed_fields`: they are collected by the
//!   redirect flow, never typed in the form, so the integrations UI
//!   hides their inputs and shows the connect button + a read-only
//!   status instead.
//! - `token_expires_at` (Text, RFC 3339) — wall-clock expiry of the
//!   current access token; drives the proactive refresh.
//! - `token_scope` (Text) — granted scopes (informational).
//! - `permission_id` (Text) — the agent app's member id in the
//!   workspace, returned by the OAuth token endpoint.
//! - `auth_mode` (Text) — `"oauth"` when the OAuth rows are set,
//!   `"api_token"` otherwise; display-only, written at connect /
//!   save time.
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
//! behaviour (OAuth routes, probe endpoint, audit rows) is wired
//! in `nagent-server` behind the same `shortcut-agent` cargo
//! feature as the chat agents.

use crate::services::{
    FieldDef, FieldKind, ServiceDef, ECHO_ON_EDIT_NON_PASSWORD, ECHO_ON_EDIT_PASSWORD,
};

/// `shortcut` integration entry in the per-user service catalogue.
///
/// Two auth modes share this service: the OAuth token pair written
/// by `/api/auth/login/shortcut/callback` (preferred when present),
/// or the `api_token` supplied by the user on the integrations form
/// and validated by the setup probe
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
            label: "API token (fallback)",
            kind: FieldKind::Password,
            required: false,
            help: Some(
                "Classic Shortcut API token, generated at \
                 app.shortcut.com/settings/account/api-tokens. Used only when no OAuth \
                 access token is saved (i.e. when you did not use \"Connect with Shortcut\"). \
                 Validated by the probe before saving. Stored AES-256-GCM encrypted at rest; \
                 never logged or displayed.",
            ),
            placeholder: Some("token from app.shortcut.com/settings/account/api-tokens"),
            options: &[],
            echo_on_edit: ECHO_ON_EDIT_PASSWORD,
        },
        FieldDef {
            key: "access_token",
            label: "OAuth access token",
            kind: FieldKind::Password,
            required: false,
            help: Some(
                "OAuth 2.0 access token written by the \"Connect with Shortcut\" flow \
                 (/api/auth/login/shortcut/callback). Sent as `Authorization: Bearer …`. \
                 Refreshed in place by the shortcut_* tools when expired. Stored \
                 AES-256-GCM encrypted at rest; never logged or displayed.",
            ),
            placeholder: None,
            options: &[],
            echo_on_edit: ECHO_ON_EDIT_PASSWORD,
        },
        FieldDef {
            key: "refresh_token",
            label: "OAuth refresh token",
            kind: FieldKind::Password,
            required: false,
            help: Some(
                "Written by the OAuth callback; used to mint a fresh access token when the \
                 stored one expires. Stored AES-256-GCM encrypted at rest; never logged or \
                 displayed.",
            ),
            placeholder: None,
            options: &[],
            echo_on_edit: ECHO_ON_EDIT_PASSWORD,
        },
        FieldDef {
            key: "token_expires_at",
            label: "Access token expiry (RFC 3339)",
            kind: FieldKind::Text,
            required: false,
            help: Some(
                "Wall-clock expiry of the current OAuth access token, written at connect / \
                 refresh time. Display-only for the user.",
            ),
            placeholder: None,
            options: &[],
            echo_on_edit: ECHO_ON_EDIT_NON_PASSWORD,
        },
        FieldDef {
            key: "token_scope",
            label: "Granted scopes",
            kind: FieldKind::Text,
            required: false,
            help: Some("Scopes returned by the OAuth token endpoint (informational)."),
            placeholder: None,
            options: &[],
            echo_on_edit: ECHO_ON_EDIT_NON_PASSWORD,
        },
        FieldDef {
            key: "permission_id",
            label: "Agent app member id (permission_id)",
            kind: FieldKind::Text,
            required: false,
            help: Some(
                "The agent app's member id in your workspace, returned by the Shortcut \
                 OAuth token endpoint at connect time. Informational.",
            ),
            placeholder: None,
            options: &[],
            echo_on_edit: ECHO_ON_EDIT_NON_PASSWORD,
        },
        FieldDef {
            key: "auth_mode",
            label: "Auth mode",
            kind: FieldKind::Text,
            required: false,
            help: Some(
                "`oauth` when the OAuth rows above are set (preferred), `api_token` when \
                 only the classic token is saved.",
            ),
            placeholder: None,
            options: &[],
            echo_on_edit: ECHO_ON_EDIT_NON_PASSWORD,
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
    // The OAuth rows are written by the `/api/auth/login/shortcut/callback`
    // redirect (never typed in the form), so the UI hides their inputs and
    // shows the "Connect with Shortcut" status instead.
    managed_fields: &[
        "access_token",
        "refresh_token",
        "token_expires_at",
        "token_scope",
        "permission_id",
        "auth_mode",
    ],
    docs_url: Some("https://github.com/nagent/nagent/blob/main/docs/integrations/shortcut.md"),
};
