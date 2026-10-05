//! `caldav` integration entry in the per-user service catalogue.
//!
//! `ServiceRegistry` is the catalogue the chat UI iterates to
//! render the "Add integration" form (`/api/integrations`); the
//! `ServiceDef` below describes the three fields the CalDAV
//! connector needs:
//!
//! - `url` — HTTPS URL of the **calendar collection** (not the
//!   principal URL), chosen by the user during the
//!   probe-then-pick setup flow. The probe endpoint
//!   (`POST /api/integrations/caldav/probe-calendars`) authenticates
//!   with the supplied credentials, runs a `PROPFIND` against the
//!   supplied principal URL, and lets the user pick the calendar
//!   they want. The picked `href` becomes the saved `url`.
//! - `username` — CalDAV principal or basic-auth username.
//! - `password` — App-specific token where possible. Stored
//!   AES-256-GCM encrypted at rest in the existing per-user
//!   credentials vault.
//!
//! Per `docs/architecture.md` §2.6, `ServiceDef` is plain data so
//! the file lives in the `nagent-agents` crate; the runtime
//! behaviour (probe endpoint, audit row) is wired in
//! `nagent-server` behind the same `caldav-agent` cargo feature
//! as the chat agents.

use crate::services::{
    FieldDef, FieldKind, ServiceDef, ECHO_ON_EDIT_NON_PASSWORD, ECHO_ON_EDIT_PASSWORD,
};

/// `caldav` integration entry in the per-user service catalogue.
///
/// v1 ships a single calendar per user (picked during the
/// connector setup flow). Multi-calendar discovery through
/// `PROPFIND` happens once at setup time; the runtime agents
/// (`caldav_list_events` / `caldav_get_event` / `caldav_create_event`)
/// always read the saved `url` as the calendar collection URL.
///
/// Edit-mode echo policy: the `url` is echoed back on edit (so the
/// user can verify which calendar they picked during setup) but
/// `username` is NOT echoed — the username is a credential-adjacent
/// identifier and re-displaying it offers no UX benefit (the user
/// already has it open in their password manager) while it would
/// surface it in browser history / screen-share / shoulder-surf
/// contexts every time the form is opened. `password` is masked by
/// the kind-level guard regardless.
pub const CALDAV_SERVICE: ServiceDef = ServiceDef {
    id: "caldav",
    display_name: "CalDAV (Nextcloud, Radicale, Fastmail, iCloud, …)",
    icon: "📅",
    fields: &[
        FieldDef {
            key: "url",
            label: "Calendar collection URL",
            kind: FieldKind::Url,
            required: true,
            help: Some(
                "HTTPS URL of the calendar collection chosen during setup. Use the 'Discover' \
                 button on this form to probe the server and pick the calendar you want — only \
                 the selected calendar's URL is stored here.",
            ),
            placeholder: Some(
                "https://nextcloud.example.com/remote.php/dav/calendars/alice/personal/",
            ),
            echo_on_edit: ECHO_ON_EDIT_NON_PASSWORD,
        },
        FieldDef {
            key: "username",
            label: "Username",
            kind: FieldKind::Text,
            required: true,
            help: Some("CalDAV principal or basic-auth username."),
            placeholder: None,
            // `username` is credential-adjacent — opted out of
            // edit-mode echo so it is not re-displayed every time
            // the form is opened (see module-level doc comment).
            echo_on_edit: false,
        },
        FieldDef {
            key: "password",
            label: "App password / token",
            kind: FieldKind::Password,
            required: true,
            help: Some(
                "Use an app-specific token where possible (Nextcloud → Settings → Security → \
                 App passwords). Stored AES-256-GCM encrypted at rest.",
            ),
            placeholder: None,
            echo_on_edit: ECHO_ON_EDIT_PASSWORD,
        },
    ],
    docs_url: Some("https://github.com/nagent/nagent/blob/main/docs/integrations/caldav.md"),
};
