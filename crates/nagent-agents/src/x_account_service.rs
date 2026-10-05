//! `x_account` integration entry in the per-user service catalogue.
//!
//! `ServiceRegistry` is the catalogue the chat UI iterates to
//! render the "Add integration" form (`/api/integrations`); the
//! `ServiceDef` below describes the six fields the X OAuth dance
//! stores under the `x_account` service id.
//!
//! All fields are written by the OAuth callback handler
//! (`/api/auth/login/x/callback`) and refreshed in place by the
//! `x_timeline` agent itself (locked decision in plan 1790695073418):
//! when X returns 401, the agent POSTs to `/2/oauth2/token` and
//! writes the new `access_token` + `refresh_token` + `token_expires_at`
//! back to the vault. The display-only fields (`x_user_id`,
//! `x_screen_name`, `token_scope`) are set once at connect time and
//! survive every refresh round-trip.
//!
//! - `access_token` (Password) — OAuth access_token (~120-char opaque).
//! - `refresh_token` (Password) — OAuth refresh_token (optional but
//!   recommended).
//! - `token_scope` (Text) — space-separated scopes from
//!   `/oauth2/token`.
//! - `x_user_id` (Text) — numeric X user id, needed to build the
//!   timeline URL.
//! - `x_screen_name` (Text) — `@handle`, surfaced in the chat UI
//!   ("Connecté en tant que @naval").
//! - `token_expires_at` (Text) — RFC 3339 timestamp derived from
//!   `expires_in` + issued_at; the agent refreshes when the value is
//!   past or within 60 s.
//!
//! Per `docs/architecture.md` §2.6, `ServiceDef` is plain data so the
//! file lives in the `nagent-agents` crate; the runtime behaviour
//! (OAuth callback, refresh) is wired in `nagent-server` behind the
//! same `x-agent` cargo feature as the chat agent.

use crate::services::{
    FieldDef, FieldKind, ServiceDef, ECHO_ON_EDIT_NON_PASSWORD, ECHO_ON_EDIT_PASSWORD,
};

/// `x_account` integration entry in the per-user service catalogue.
///
/// All fields are written by the OAuth callback at
/// `/api/auth/login/x/callback` (or refreshed in place by the agent
/// on 401). The chat UI masks `access_token` + `refresh_token` and
/// surfaces the other four fields plaintext in
/// `GET /api/integrations/:id`.
pub const X_ACCOUNT_SERVICE: ServiceDef = ServiceDef {
    id: "x_account",
    display_name: "X (Twitter)",
    icon: "𝕏",
    fields: &[
        FieldDef {
            key: "access_token",
            label: "OAuth access token",
            kind: FieldKind::Password,
            required: true,
            help: Some(
                "OAuth 2.0 bearer token issued by the X callback. Refreshed in place by the \
                 `x_timeline` agent on 401. Stored AES-256-GCM encrypted at rest.",
            ),
            placeholder: None,
            echo_on_edit: ECHO_ON_EDIT_PASSWORD,
        },
        FieldDef {
            key: "refresh_token",
            label: "OAuth refresh token",
            kind: FieldKind::Password,
            required: false,
            help: Some(
                "Optional but recommended. Used by the `x_timeline` agent to mint a fresh \
                 access token when the stored one is expired. Stored AES-256-GCM encrypted at rest.",
            ),
            placeholder: None,
            echo_on_edit: ECHO_ON_EDIT_PASSWORD,
        },
        FieldDef {
            key: "token_scope",
            label: "Granted scopes",
            kind: FieldKind::Text,
            required: false,
            help: Some(
                "Space-separated scopes returned by `/oauth2/token` (informational; the agent \
                 does not branch on this).",
            ),
            placeholder: None,
            echo_on_edit: ECHO_ON_EDIT_NON_PASSWORD,
        },
        FieldDef {
            key: "x_user_id",
            label: "X user id",
            kind: FieldKind::Text,
            required: true,
            help: Some(
                "Numeric X user id resolved via `/2/users/me` during the OAuth callback. \
                 Required to build the timeline URL.",
            ),
            placeholder: None,
            echo_on_edit: ECHO_ON_EDIT_NON_PASSWORD,
        },
        FieldDef {
            key: "x_screen_name",
            label: "X handle",
            kind: FieldKind::Text,
            required: true,
            help: Some("@handle surfaced in the chat UI (\"Connecté en tant que @naval\")."),
            placeholder: None,
            echo_on_edit: ECHO_ON_EDIT_NON_PASSWORD,
        },
        FieldDef {
            key: "token_expires_at",
            label: "Access token expiry (RFC 3339)",
            kind: FieldKind::Text,
            required: true,
            help: Some(
                "Wall-clock expiry of the current access token, derived from `expires_in` at \
                 callback time. The agent refreshes when this is past or within 60 s.",
            ),
            placeholder: None,
            echo_on_edit: ECHO_ON_EDIT_NON_PASSWORD,
        },
    ],
    docs_url: Some("https://github.com/nagent/nagent/blob/main/docs/integrations/x.md"),
};
