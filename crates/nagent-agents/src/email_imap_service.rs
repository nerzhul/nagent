//! `email_imap` integration entry in the per-user service catalogue.
//!
//! `ServiceRegistry` is the catalogue the chat UI iterates to render
//! the "Add integration" form (`/api/integrations`); the `ServiceDef`
//! below describes the five fields the IMAP connector needs (host,
//! port, TLS mode, username, password). The companion `email_smtp`
//! `ServiceDef` is in `email_smtp_service.rs` and reuses the same
//! field shape with one extra `from_address` row.
//!
//! Per `docs/architecture.md` §2.6, `ServiceDef` is plain data so the
//! file lives in the `nagent-agents` crate; the runtime behaviour
//! (probe endpoint, audit row, server-side validation) is wired in
//! `nagent-server` behind the same `email-agent` cargo feature as the
//! chat agents. The two `ServiceDef`s stay distinct (rather than
//! folding IMAP+SMTP into one form) because the IMAP and SMTP
//! endpoints for the same mailbox very often differ
//! (`imap.fastmail.com:993` + `smtp.fastmail.com:465`), and the
//! credentials are not always symmetric (submission sometimes uses
//! an app-specific password while IMAP uses the real one).
//!
//! Edit-mode echo policy: the `host` is echoed back on edit (so the
//! user can verify which IMAP server they picked during setup) but
//! `username` is NOT echoed — it is credential-adjacent and
//! re-displaying it offers no UX benefit. `password` is masked by the
//! kind-level guard regardless. `port` and `tls_mode` are operator-
//! visible knobs and may surface in edit; both are flagged
//! `echo_on_edit: true` so the form re-fills cleanly on a typo fix.

use crate::services::{
    FieldDef, FieldKind, SelectOption, ServiceDef, ECHO_ON_EDIT_NON_PASSWORD, ECHO_ON_EDIT_PASSWORD,
};

/// Closed enumeration of IMAP `tls_mode` values. The user
/// picks one from a `<select>` dropdown (no free-text input)
/// so a typo never reaches the wire layer. The wire format
/// is the `value` of the chosen option; the `agents::email`
/// parser accepts these three tokens plus their case-folded
/// variants and the deprecated `ssl` / `tls` aliases.
const IMAP_TLS_MODE_OPTIONS: &[SelectOption] = &[
    SelectOption {
        value: "implicit",
        label: "Implicit TLS (port 993, recommended)",
    },
    SelectOption {
        value: "starttls",
        label: "STARTTLS (port 143, plain → upgrade before AUTH)",
    },
    SelectOption {
        value: "none",
        label: "None (clear-text — refused unless the operator opts in)",
    },
];

/// `email_imap` integration entry in the per-user service catalogue.
///
/// v1 ships read + flag-set operations on top of IMAP4rev1 + the
/// common extensions. The runtime agents (`email_list_inbox`,
/// `email_get_message`, `email_search`, `email_mark_read`) all read
/// the saved `host` / `port` / `tls_mode` / `username` / `password`
/// from the per-user vault at request time. The SMTP counterpart
/// (`email_smtp`) carries the same field shape plus `from_address`.
pub const EMAIL_IMAP_SERVICE: ServiceDef = ServiceDef {
    id: "email_imap",
    display_name: "Email (IMAP)",
    icon: "📧",
    fields: &[
        FieldDef {
            key: "host",
            label: "IMAP server hostname",
            kind: FieldKind::Text,
            required: true,
            help: Some(
                "IMAP server hostname. Must be in the operator-configured `[email].allowlist` \
                 (defaults to fail-closed — set `EMAIL_ALLOWLIST` before the probe will accept \
                 any call).",
            ),
            placeholder: Some("imap.example.com"),
            options: &[],
            echo_on_edit: ECHO_ON_EDIT_NON_PASSWORD,
        },
        FieldDef {
            key: "tls_mode",
            label: "TLS mode",
            kind: FieldKind::Select,
            required: true,
            help: Some(
                "Implicit TLS (port 993) is the recommended posture; STARTTLS upgrades a plain \
                 connection to TLS before AUTH. Clear-text (`none`) is refused by default — the \
                 operator must set `[email].allow_cleartext_imap = true` for it to be accepted. \
                 Changing the TLS mode auto-fills the canonical port below (993 / 143) so you \
                 don't have to remember it; a custom port is preserved.",
            ),
            placeholder: None,
            options: IMAP_TLS_MODE_OPTIONS,
            echo_on_edit: ECHO_ON_EDIT_NON_PASSWORD,
        },
        FieldDef {
            key: "port",
            label: "IMAP TCP port",
            kind: FieldKind::Number,
            required: true,
            help: Some(
                "993 for implicit TLS, 143 for STARTTLS, custom allowed. Port 25 is refused \
                 (relay-only — there is no IMAP service there). Auto-fills when you change the \
                 TLS mode above.",
            ),
            placeholder: Some("993"),
            options: &[],
            echo_on_edit: ECHO_ON_EDIT_NON_PASSWORD,
        },
        FieldDef {
            key: "username",
            label: "Username",
            kind: FieldKind::Text,
            required: true,
            help: Some("IMAP login name (often the full email address)."),
            placeholder: None,
            options: &[],
            // `username` is credential-adjacent — opted out of
            // edit-mode echo so it is not re-displayed every time
            // the form is opened (see the CalDAV `username` field
            // for the same posture).
            echo_on_edit: false,
        },
        FieldDef {
            key: "password",
            label: "App password / token",
            kind: FieldKind::Password,
            required: true,
            help: Some(
                "Use an app-specific token where possible. Stored AES-256-GCM encrypted at rest \
                 under the `email_imap` service id.",
            ),
            placeholder: None,
            options: &[],
            echo_on_edit: ECHO_ON_EDIT_PASSWORD,
        },
    ],
    docs_url: Some("https://github.com/nagent/nagent/blob/main/docs/integrations/email.md"),
};
