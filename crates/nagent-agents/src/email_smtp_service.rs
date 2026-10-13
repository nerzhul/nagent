//! `email_smtp` integration entry in the per-user service catalogue.
//!
//! Companion to `email_imap_service.rs`. The SMTP `ServiceDef` carries
//! the same `host` / `port` / `tls_mode` / `username` / `password`
//! shape as IMAP plus one extra required field, `from_address`, that
//! gates the `MAIL FROM:` envelope — the LLM never picks the sender
//! on its own, the chat agent always reads the per-user saved value
//! from the vault and the server validates it with a one-shot SMTP
//! probe at save time (cached in the `email_allowed_from` table per
//! migration `0013_email_allowed_from.sql`).
//!
//! Edit-mode echo policy mirrors the IMAP form: `host`, `port`,
//! `tls_mode`, and `from_address` are echoed back on edit so the form
//! re-fills cleanly; `username` and `password` are masked at the kind
//! level and `username` is additionally opted out via
//! `echo_on_edit = false`.

use crate::services::{
    FieldDef, FieldKind, SelectOption, ServiceDef, ECHO_ON_EDIT_NON_PASSWORD, ECHO_ON_EDIT_PASSWORD,
};

/// Closed enumeration of SMTP `tls_mode` values. Mirrors the
/// IMAP form (same three tokens, slightly different help
/// text because the SMTP wire path is the inverse of IMAP's:
/// the client opens a plain connection and the server
/// advertises STARTTLS as a capability).
const SMTP_TLS_MODE_OPTIONS: &[SelectOption] = &[
    SelectOption {
        value: "implicit",
        label: "Implicit TLS (port 465, SMTPS, recommended)",
    },
    SelectOption {
        value: "starttls",
        label: "STARTTLS (port 587, plain → upgrade before AUTH)",
    },
    SelectOption {
        value: "none",
        label: "None (clear-text — refused for port 25; relay-only)",
    },
];

/// `email_smtp` integration entry in the per-user service catalogue.
///
/// v1 ships one LLM tool that uses the saved SMTP credentials
/// (`email_send`); the agents always pull the `from_address` from the
/// per-user vault, never from the LLM's argument JSON. The
/// `recipient_allowlist` (operator-set, default = the user's own
/// address) is the second gate on outbound mail; see
/// `docs/integrations/email.md` for the full threat model.
pub const EMAIL_SMTP_SERVICE: ServiceDef = ServiceDef {
    id: "email_smtp",
    display_name: "Email (SMTP)",
    icon: "✉️",
    fields: &[
        FieldDef {
            key: "host",
            label: "SMTP server hostname",
            kind: FieldKind::Text,
            required: true,
            help: Some(
                "SMTP submission / relay hostname. Must be in the operator-configured \
                 `[email].allowlist`.",
            ),
            placeholder: Some("smtp.example.com"),
            options: &[],
            echo_on_edit: ECHO_ON_EDIT_NON_PASSWORD,
        },
        FieldDef {
            key: "tls_mode",
            label: "TLS mode",
            kind: FieldKind::Select,
            required: true,
            help: Some(
                "Implicit TLS (port 465) is the recommended posture; STARTTLS upgrades a plain \
                 connection to TLS before AUTH. Clear-text (`none`) is refused for port 25 by \
                 default; the operator must set `[email].allow_cleartext_smtp = true` for it \
                 to be accepted. Changing the TLS mode auto-fills the canonical port below \
                 (465 / 587) so you don't have to remember it; a custom port is preserved.",
            ),
            placeholder: None,
            options: SMTP_TLS_MODE_OPTIONS,
            echo_on_edit: ECHO_ON_EDIT_NON_PASSWORD,
        },
        FieldDef {
            key: "port",
            label: "SMTP TCP port",
            kind: FieldKind::Number,
            required: true,
            help: Some(
                "465 for implicit TLS / SMTPS, 587 for STARTTLS submission, 25 refused \
                 (relay-only). Auto-fills when you change the TLS mode above.",
            ),
            placeholder: Some("465"),
            options: &[],
            echo_on_edit: ECHO_ON_EDIT_NON_PASSWORD,
        },
        FieldDef {
            key: "username",
            label: "Username",
            kind: FieldKind::Text,
            required: true,
            help: Some("SMTP AUTH login name (often the full email address)."),
            placeholder: None,
            options: &[],
            // `username` is credential-adjacent — opted out of
            // edit-mode echo (see the IMAP form for the rationale).
            echo_on_edit: false,
        },
        FieldDef {
            key: "password",
            label: "App password / token",
            kind: FieldKind::Password,
            required: true,
            help: Some(
                "Use an app-specific token where possible. Stored AES-256-GCM encrypted at rest \
                 under the `email_smtp` service id.",
            ),
            placeholder: None,
            options: &[],
            echo_on_edit: ECHO_ON_EDIT_PASSWORD,
        },
        FieldDef {
            key: "from_address",
            label: "From address",
            kind: FieldKind::Text,
            required: true,
            help: Some(
                "Single allowed sender. Validated by `MAIL FROM:<address>` against the SMTP \
                 probe at save time; cached in `email_allowed_from`. The LLM never picks the \
                 sender — the chat agent always reads this saved value.",
            ),
            placeholder: Some("alice@example.com"),
            options: &[],
            echo_on_edit: ECHO_ON_EDIT_NON_PASSWORD,
        },
    ],
    docs_url: Some("https://github.com/nagent/nagent/blob/main/docs/integrations/email.md"),
};
