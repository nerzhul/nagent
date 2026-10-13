//! IMAP/SMTP email agent family (plan 1791566269874).
//!
//! v1 ships five LLM-callable agents and two setup-only HTTP probe
//! endpoints:
//!
//! - LLM tools: `email_list_inbox`, `email_get_message`,
//!   `email_search`, `email_send`, `email_mark_read`.
//! - Probes: `POST /api/integrations/email_imap/probe` and
//!   `POST /api/integrations/email_smtp/probe`.
//!
//! Per-call connection model: every agent invocation opens its own
//! IMAP or SMTP connection, runs `LOGIN` / `AUTHENTICATE`, executes
//! the requested command, and closes the connection. No persistent
//! state lives on the server. A v2 follow-up can layer a per-user
//! pool (keyed on `user_id`) without changing the capability trait
//! or any agent code; see plan §5 for the rationale.
//!
//! ## Hardening
//!
//! The same posture as the CalDAV and X agents:
//!
//! - Per-service hostname allow-list (`[email].allowlist` on the
//!   server side; `host_matches_allowlist` reused).
//! - TLS posture — STARTTLS mandatory on submission, IMAPS-only by
//!   default for read.
//! - `email_send` is double-gated: server-side
//!   `recipient_allowlist` plus the inline approval card on every
//!   invocation (no session-override for outbound mail).
//! - One audit row per call with kind `email_<tool>_<outcome>` —
//!   host + outcome + at most a 256-byte message-id hash; no PII.
//!
//! ## Capability traits
//!
//! The wire clients (`client_imap`, `client_smtp`) live inside the
//! `nagent-agents` crate. The server-side `EmailImap` / `EmailSmtp`
//! adapters (built per-call from the per-user vault) implement the
//! [`EmailImap`] / [`EmailSmtp`] capability traits declared here.
//! Tests can swap in a mock impl without standing up a real IMAP
//! server.
//!
//! ## Per-process impl slot
//!
//! The descriptor table constructs the five agents at boot time
//! but the `EmailImap` / `EmailSmtp` impls are only built on the
//! server side (they read from the runtime config + the per-user
//! vault). The crate solves this with two process-wide
//! `OnceCell<Arc<dyn …>>` slots — [`set_email_imap_impl`] and
//! [`set_email_smtp_impl`] — populated by `nagent-server` after
//! the config + secret resolver are wired. The agent
//! constructor reads the slots at invoke time (NOT build time)
//! so the descriptor table can stay boot-time only.
//!
//! If a build runs without first calling the setters, the agents
//! return a clear "email capability not wired" error and the
//! operator gets the same diagnostic they had in v1.

use std::sync::Arc;
use std::sync::OnceLock;

use async_trait::async_trait;
use serde::Serialize;
use uuid::Uuid;

use crate::agents::AgentError;

pub mod client_imap;
pub mod client_smtp;
pub mod get_message;
pub mod list_inbox;
pub mod mark_read;
pub mod parse;
pub mod search;
pub mod send;

pub use client_imap::{fetch_to_summary, ImapClient, ImapTlsMode};
pub use client_smtp::{build_message, mail_from_probe, send_message, SmtpClient, SmtpTlsMode};

/// One folder / mailbox name advertised by the IMAP server. Returned
/// by the IMAP probe so the user picks which folder the chat agents
/// will read by default. v1 always defaults the read agents to
/// `INBOX`; this struct is for the future `email_list_folders` setup
/// path.
#[derive(Debug, Clone, Serialize)]
pub struct MailboxSummary {
    /// Server-side name (UTF-7 IMAP modified if the server uses
    /// the legacy encoding; UTF-8 otherwise). Used verbatim by
    /// the wire client.
    pub name: String,
    /// `\\Inbox`, `\\Sent`, `\\Drafts`, `\\Trash`, `\\Archive`,
    /// `\\All`, `\\Flagged` when the server advertises
    /// `SPECIAL-USE` (RFC 6154). `None` otherwise.
    pub special_use: Option<String>,
    /// Number of messages currently in the mailbox. `-1` when
    /// the server did not advertise an `EXISTS` count for this
    /// folder (some servers return it lazily).
    pub exists: i64,
}

/// Summary of one message — the LLM-facing shape returned by
/// `email_list_inbox` and `email_search`. Caps the per-message
/// payload at one envelope row; the LLM is expected to call
/// `email_get_message(uid)` for the body.
#[derive(Debug, Clone, Serialize)]
pub struct EmailSummary {
    /// Stable identifier within the folder. The IMAP UID is
    /// preferred (RFC 3501 §2.3.1.1); the LLM passes this value
    /// back to `email_get_message` / `email_mark_read`.
    pub uid: u32,
    /// Folder / mailbox the message lives in. Echoed back so
    /// the LLM can disambiguate UIDs that are only unique per
    /// folder.
    pub folder: String,
    /// RFC 5322 `From:` header, raw text (display name + address).
    pub from: String,
    /// RFC 5322 `Subject:` header. Empty string when the
    /// server did not provide one.
    pub subject: String,
    /// RFC 5322 `Date:` header, parsed to UTC. `None` when the
    /// server did not advertise a `Date:` header.
    pub date: Option<chrono::DateTime<chrono::Utc>>,
    /// Short plaintext snippet — the first ~200 chars of the
    /// plain-text body, with HTML / quoted-printable stripped.
    /// `None` when the message has no `text/plain` part.
    pub snippet: Option<String>,
    /// RFC 5322 `Message-ID:` header. None when the server did
    /// not advertise one. Surfaced for the audit-row hash so the
    /// LLM can correlate per-call audit rows.
    pub message_id: Option<String>,
    /// True when `\Seen` is set on the message.
    pub seen: bool,
}

/// Full body of one message — returned by `email_get_message`.
#[derive(Debug, Clone, Serialize)]
pub struct EmailMessage {
    /// Same as `EmailSummary::uid`.
    pub uid: u32,
    /// Folder / mailbox the message lives in.
    pub folder: String,
    /// RFC 5322 `From:` header, raw text.
    pub from: String,
    /// RFC 5322 `To:` header(s), comma-separated.
    pub to: String,
    /// RFC 5322 `Cc:` header(s), comma-separated. Empty when
    /// absent.
    pub cc: String,
    /// RFC 5322 `Subject:` header. Empty when absent.
    pub subject: String,
    /// RFC 5322 `Date:` header, parsed to UTC. `None` when
    /// absent.
    pub date: Option<chrono::DateTime<chrono::Utc>>,
    /// RFC 5322 `Message-ID:` header. `None` when absent.
    pub message_id: Option<String>,
    /// Plain-text body, capped at `EmailAgentConfig::max_body_bytes`.
    /// `None` when the message has no `text/plain` part.
    pub body: Option<String>,
    /// Attachment metadata envelope — `filename`, `mime`, `size_bytes`,
    /// and `content_id` for `cid:` references in the body. v1 never
    /// decodes the attachment bytes themselves (the agent refuses
    /// to fetch them — `max_attachment_bytes = 0` in v1).
    pub attachments: Vec<AttachmentMeta>,
}

/// Metadata for one attachment — surfaced by `email_get_message`
/// but never decoded in v1.
#[derive(Debug, Clone, Serialize)]
pub struct AttachmentMeta {
    pub filename: String,
    pub mime: String,
    pub size_bytes: u64,
    pub content_id: Option<String>,
}

/// Outgoing message envelope supplied to [`EmailSmtp::send`].
#[derive(Debug, Clone)]
pub struct OutgoingMessage {
    /// The `from_address` the SMTP probe validated at save time
    /// (the per-user value, never an LLM-supplied one).
    pub from: String,
    /// `To:` recipients — RFC 5322 addresses (no display name).
    pub to: Vec<String>,
    /// `Cc:` recipients. Empty when the LLM did not pass any.
    pub cc: Vec<String>,
    /// `Bcc:` recipients. Empty when the LLM did not pass any.
    pub bcc: Vec<String>,
    /// Subject line.
    pub subject: String,
    /// Plain-text body.
    pub body: String,
    /// `In-Reply-To:` value when threading an existing thread.
    pub in_reply_to: Option<String>,
    /// `References:` value when threading an existing thread.
    pub references: Option<String>,
}

/// Per-call capability for the IMAP wire client. The server-side
/// `EmailImapImpl` builds a fresh `ImapClient` per agent invocation
/// from the per-user vault; the trait is the seam the agents code
/// against so a test can swap in a mock impl.
///
/// All methods are `async_trait`-based; the wire client is `Send +
/// Sync` so the per-request `Arc<dyn EmailImap>` is cheap to clone.
#[async_trait]
pub trait EmailImap: Send + Sync {
    /// Probe-only: list the mailboxes advertised by the server
    /// after `LOGIN`. Used by the setup endpoint so the user can
    /// see what the IMAP server exposes.
    async fn list_mailboxes(
        &self,
        user_id: Uuid,
        config: &ImapConnectConfig,
    ) -> Result<Vec<MailboxSummary>, AgentError>;

    /// List the most recent `limit` messages in `folder`,
    /// optionally filtered to messages received after `since`.
    async fn list_inbox(
        &self,
        user_id: Uuid,
        config: &ImapConnectConfig,
        folder: &str,
        since: Option<chrono::DateTime<chrono::Utc>>,
        limit: usize,
    ) -> Result<Vec<EmailSummary>, AgentError>;

    /// Fetch the full body of one message by IMAP UID.
    async fn get_message(
        &self,
        user_id: Uuid,
        config: &ImapConnectConfig,
        folder: &str,
        uid: u32,
    ) -> Result<EmailMessage, AgentError>;

    /// Run a constrained IMAP `SEARCH` against `folder` and return
    /// the matching summaries. The query string is constrained to
    /// a small allow-list of IMAP criteria tokens; see
    /// `parse::validate_search_criteria` for the exact set.
    async fn search(
        &self,
        user_id: Uuid,
        config: &ImapConnectConfig,
        folder: &str,
        criteria: &str,
        limit: usize,
    ) -> Result<Vec<EmailSummary>, AgentError>;

    /// Add or remove the `\Seen` flag on a single message.
    async fn set_flags(
        &self,
        user_id: Uuid,
        config: &ImapConnectConfig,
        folder: &str,
        uid: u32,
        read: bool,
    ) -> Result<(), AgentError>;
}

/// Per-call capability for the SMTP wire client. v1 ships a single
/// method (`send`); a v2 follow-up may add `verify_address` for the
/// SMTP probe to re-check an `email_allowed_from` row.
#[async_trait]
pub trait EmailSmtp: Send + Sync {
    /// Send `message` through the per-user SMTP credentials. The
    /// caller (the `email_send` agent) has already validated the
    /// `to` / `cc` / `bcc` against the operator allow-list and the
    /// `from` against the cached `email_allowed_from` row.
    async fn send(
        &self,
        user_id: Uuid,
        config: &SmtpConnectConfig,
        message: &OutgoingMessage,
    ) -> Result<(), AgentError>;
}

// ---------------------------------------------------------------------------
// Per-process impl slots. The descriptor table builds the
// five agents at boot time without access to the per-user
// secret resolver; the `EmailImapImpl` / `EmailSmtpImpl` are
// populated here, AFTER the config + secret resolver are
// wired, and the agents read them on every invocation.
// ---------------------------------------------------------------------------

/// Process-wide slot for the IMAP capability impl.
static EMAIL_IMAP_IMPL: OnceLock<Arc<dyn EmailImap>> = OnceLock::new();

/// Process-wide slot for the SMTP capability impl.
static EMAIL_SMTP_IMPL: OnceLock<Arc<dyn EmailSmtp>> = OnceLock::new();

/// Install the per-process `EmailImap` impl. Called by
/// `nagent-server` at boot time after the runtime config +
/// per-user secret resolver are wired. Calling this more
/// than once is a wiring bug and the second call panics.
pub fn set_email_imap_impl(impl_: Arc<dyn EmailImap>) {
    if EMAIL_IMAP_IMPL.set(impl_).is_err() {
        panic!("set_email_imap_impl called twice — wiring bug in nagent-server");
    }
}

/// Install the per-process `EmailSmtp` impl. Same contract as
/// [`set_email_imap_impl`].
pub fn set_email_smtp_impl(impl_: Arc<dyn EmailSmtp>) {
    if EMAIL_SMTP_IMPL.set(impl_).is_err() {
        panic!("set_email_smtp_impl called twice — wiring bug in nagent-server");
    }
}

/// Borrow the per-process IMAP impl. Returns `None` if
/// `nagent-server` did not call [`set_email_imap_impl`] at
/// boot — the agent then surfaces a clear "capability not
/// wired" error to the LLM caller.
pub fn email_imap_impl() -> Option<&'static Arc<dyn EmailImap>> {
    EMAIL_IMAP_IMPL.get()
}

/// Borrow the per-process SMTP impl. See [`email_imap_impl`].
pub fn email_smtp_impl() -> Option<&'static Arc<dyn EmailSmtp>> {
    EMAIL_SMTP_IMPL.get()
}

/// IMAP connection parameters — the per-call config the agents
/// build from the per-user vault. Lives in the agents crate so the
/// trait can reference it without crossing the crate boundary.
#[derive(Debug, Clone)]
pub struct ImapConnectConfig {
    pub host: String,
    pub port: u16,
    pub tls_mode: ImapTlsMode,
    pub username: String,
    /// The plaintext password. Carried alongside the
    /// other config rather than read at the call site
    /// because the per-user vault returns the
    /// `(host, port, tls_mode, username)` tuple AND the
    /// password as separate values; threading both
    /// through one struct keeps the call site symmetric
    /// with the SMTP variant.
    pub password: String,
}

/// SMTP connection parameters — mirror of [`ImapConnectConfig`] for
/// the submission side.
#[derive(Debug, Clone)]
pub struct SmtpConnectConfig {
    pub host: String,
    pub port: u16,
    pub tls_mode: SmtpTlsMode,
    pub username: String,
    pub password: String,
    pub from_address: String,
}

/// Concrete `EmailImap` impl that holds the per-call `ImapClient`
/// factory. The factory builds a fresh `ImapClient` per call from
/// the per-user vault at request time and tears it down before
/// returning (the per-call connection model).
///
/// The struct is `Arc`ed into the agent at boot time and the agent
/// reaches for it through the `AgentRegistry` post-build wiring
/// path. Tests substitute a different `EmailImap` impl by hand.
#[derive(Clone)]
pub struct EmailImapImpl {
    /// Shared `EmailAgentConfig` (timeout, size caps). Read by the
    /// wire client to enforce per-call budgets.
    pub cfg: Arc<crate::EmailAgentConfig>,
}

impl std::fmt::Debug for EmailImapImpl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EmailImapImpl")
            .field("cfg", &self.cfg)
            .finish()
    }
}

impl EmailImapImpl {
    pub fn new(cfg: Arc<crate::EmailAgentConfig>) -> Self {
        Self { cfg }
    }
}

/// Concrete `EmailSmtp` impl — mirror of [`EmailImapImpl`] for the
/// submission side.
#[derive(Clone)]
pub struct EmailSmtpImpl {
    pub cfg: Arc<crate::EmailAgentConfig>,
}

impl std::fmt::Debug for EmailSmtpImpl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EmailSmtpImpl")
            .field("cfg", &self.cfg)
            .finish()
    }
}

impl EmailSmtpImpl {
    pub fn new(cfg: Arc<crate::EmailAgentConfig>) -> Self {
        Self { cfg }
    }
}

// ---------------------------------------------------------------------------
// Concrete `EmailImap` / `EmailSmtp` impls. Both are
// `Clone + Send + Sync` and own only an `Arc<EmailAgentConfig>`,
// so the per-call cost is one IMAP / SMTP connection + one
// per-request `UserContext::secret` look-up (the latter is
// the only piece of state that crosses the trust boundary).
// ---------------------------------------------------------------------------

#[async_trait]
impl EmailSmtp for EmailSmtpImpl {
    async fn send(
        &self,
        _user_id: Uuid,
        config: &SmtpConnectConfig,
        message: &OutgoingMessage,
    ) -> Result<(), AgentError> {
        let client = SmtpClient::new(self.cfg.timeout_ms);
        let mut transport = client
            .connect(
                &config.host,
                config.port,
                config.tls_mode,
                &config.username,
                &config.password,
                &self.cfg.default_probe_helo,
            )
            .await?;
        // `async-smtp`'s `send` does the full
        // `MAIL FROM` + `RCPT TO` + `DATA` + `QUIT`
        // flow. The `from_address` we hand it is the
        // server-validated one (the probe endpoint
        // already ran `MAIL FROM` against the relay at
        // save time). The `recipient_allowlist` check
        // at the agent layer above this method is the
        // second gate on outbound mail.
        client_smtp::send_message(&mut transport, message).await?;
        Ok(())
    }
}

#[async_trait]
impl EmailImap for EmailImapImpl {
    async fn list_mailboxes(
        &self,
        _user_id: Uuid,
        config: &ImapConnectConfig,
    ) -> Result<Vec<MailboxSummary>, AgentError> {
        let client = ImapClient::new(self.cfg.timeout_ms);
        let host = config.host.clone();
        let port = config.port;
        let tls_mode = config.tls_mode;
        let username = config.username.clone();
        let password = config.password.clone();
        let folder = "*".to_string();
        client
            .with_session(
                &host,
                port,
                tls_mode,
                &username,
                &password,
                move |session| {
                    let names = session
                        .list(Some(""), Some(&folder))
                        .map_err(|e| AgentError::AgentFailed(format!("IMAP LIST: {e}")))?;
                    let out = names
                        .iter()
                        .map(|n| MailboxSummary {
                            name: n.name().to_string(),
                            special_use: None,
                            exists: -1,
                        })
                        .collect();
                    Ok::<_, AgentError>(out)
                },
            )
            .await
    }

    async fn list_inbox(
        &self,
        _user_id: Uuid,
        config: &ImapConnectConfig,
        folder: &str,
        since: Option<chrono::DateTime<chrono::Utc>>,
        limit: usize,
    ) -> Result<Vec<EmailSummary>, AgentError> {
        let client = ImapClient::new(self.cfg.timeout_ms);
        let host = config.host.clone();
        let port = config.port;
        let tls_mode = config.tls_mode;
        let username = config.username.clone();
        let password = config.password.clone();
        let folder_owned = folder.to_string();
        let criteria = match since {
            Some(d) => format!("SINCE {}", d.format("%d-%b-%Y")),
            None => "ALL".to_string(),
        };
        client
            .with_session(
                &host,
                port,
                tls_mode,
                &username,
                &password,
                move |session| {
                    session.select(&folder_owned).map_err(|e| {
                        AgentError::AgentFailed(format!("IMAP SELECT {folder_owned}: {e}"))
                    })?;
                    let uids: Vec<imap::types::Uid> = session
                        .uid_search(&criteria)
                        .map_err(|e| AgentError::AgentFailed(format!("IMAP UID SEARCH: {e}")))?
                        .into_iter()
                        .collect();
                    if uids.is_empty() {
                        return Ok::<_, AgentError>(Vec::new());
                    }
                    let mut uids: Vec<u32> = uids.into_iter().collect();
                    // Newest first (IMAP returns in
                    // mailbox order; we want the
                    // opposite).
                    uids.sort_unstable();
                    uids.reverse();
                    uids.truncate(limit);
                    let uid_set: String = uids
                        .iter()
                        .map(|u| u.to_string())
                        .collect::<Vec<_>>()
                        .join(",");
                    let fetches_zc = session
                        .uid_fetch(&uid_set, "ENVELOPE FLAGS")
                        .map_err(|e| AgentError::AgentFailed(format!("IMAP UID FETCH: {e}")))?;
                    let summaries: Vec<EmailSummary> = fetches_zc
                        .iter()
                        .map(|f| client_imap::fetch_to_summary(f, &folder_owned))
                        .collect::<Result<_, _>>()?;
                    Ok(summaries)
                },
            )
            .await
    }

    async fn get_message(
        &self,
        _user_id: Uuid,
        config: &ImapConnectConfig,
        folder: &str,
        uid: u32,
    ) -> Result<EmailMessage, AgentError> {
        let client = ImapClient::new(self.cfg.timeout_ms);
        let host = config.host.clone();
        let port = config.port;
        let tls_mode = config.tls_mode;
        let username = config.username.clone();
        let password = config.password.clone();
        let folder_owned = folder.to_string();
        let max_body_bytes = self.cfg.max_body_bytes;
        client
            .with_session(
                &host,
                port,
                tls_mode,
                &username,
                &password,
                move |session| {
                    session.select(&folder_owned).map_err(|e| {
                        AgentError::AgentFailed(format!("IMAP SELECT {folder_owned}: {e}"))
                    })?;
                    let uid_set = uid.to_string();
                    // `BODY.PEEK[TEXT]<0.N>` returns the
                    // text body capped at N bytes; the
                    // `.PEEK` form does NOT set \Seen.
                    let query = format!("ENVELOPE BODY.PEEK[TEXT]<0.{}>", max_body_bytes);
                    // The `imap` crate's `uid_fetch`
                    // returns a `ZeroCopy<Vec<Fetch>>` —
                    // references into the response bytes
                    // that are valid for the lifetime of
                    // the `ZeroCopy`. We deref into a
                    // `&[Fetch]` and build the `EmailSummary`
                    // rows while the `ZeroCopy` is still in
                    // scope.
                    let fetches_zc = session
                        .uid_fetch(&uid_set, &query)
                        .map_err(|e| AgentError::AgentFailed(format!("IMAP UID FETCH: {e}")))?;
                    let fetch = fetches_zc.first().ok_or_else(|| {
                        AgentError::AgentFailed(format!("IMAP FETCH {uid} returned no message"))
                    })?;
                    let summary = client_imap::fetch_to_summary(&fetch, &folder_owned)?;
                    let body = fetch
                        .text()
                        .map(|b| String::from_utf8_lossy(b).into_owned());
                    Ok(EmailMessage {
                        uid: summary.uid,
                        folder: summary.folder,
                        from: summary.from,
                        // `email_get_message` does not yet
                        // surface the full `to` / `cc`
                        // list — the LLM-facing payload
                        // relies on `from` + the `text()`
                        // body. A v2 follow-up can wire
                        // `mailparse` to extract the To/Cc
                        // headers from the full RFC 822
                        // body. The v1 `Envelope` from
                        // `imap-proto` carries them but
                        // not all servers populate it.
                        to: String::new(),
                        cc: String::new(),
                        subject: summary.subject,
                        date: summary.date,
                        message_id: summary.message_id,
                        body,
                        attachments: Vec::<AttachmentMeta>::new(),
                    })
                },
            )
            .await
    }

    async fn search(
        &self,
        _user_id: Uuid,
        config: &ImapConnectConfig,
        folder: &str,
        criteria: &str,
        limit: usize,
    ) -> Result<Vec<EmailSummary>, AgentError> {
        let client = ImapClient::new(self.cfg.timeout_ms);
        let host = config.host.clone();
        let port = config.port;
        let tls_mode = config.tls_mode;
        let username = config.username.clone();
        let password = config.password.clone();
        let folder_owned = folder.to_string();
        let criteria_owned = criteria.to_string();
        client
            .with_session(
                &host,
                port,
                tls_mode,
                &username,
                &password,
                move |session| {
                    session.select(&folder_owned).map_err(|e| {
                        AgentError::AgentFailed(format!("IMAP SELECT {folder_owned}: {e}"))
                    })?;
                    let uids: Vec<imap::types::Uid> = session
                        .uid_search(&criteria_owned)
                        .map_err(|e| AgentError::AgentFailed(format!("IMAP UID SEARCH: {e}")))?
                        .into_iter()
                        .collect();
                    if uids.is_empty() {
                        return Ok::<_, AgentError>(Vec::new());
                    }
                    let mut uids: Vec<u32> = uids.into_iter().collect();
                    uids.sort_unstable();
                    uids.truncate(limit);
                    let uid_set: String = uids
                        .iter()
                        .map(|u| u.to_string())
                        .collect::<Vec<_>>()
                        .join(",");
                    let fetches_zc = session
                        .uid_fetch(&uid_set, "ENVELOPE FLAGS")
                        .map_err(|e| AgentError::AgentFailed(format!("IMAP UID FETCH: {e}")))?;
                    let summaries: Vec<EmailSummary> = fetches_zc
                        .iter()
                        .map(|f| client_imap::fetch_to_summary(f, &folder_owned))
                        .collect::<Result<_, _>>()?;
                    Ok(summaries)
                },
            )
            .await
    }

    async fn set_flags(
        &self,
        _user_id: Uuid,
        config: &ImapConnectConfig,
        folder: &str,
        uid: u32,
        read: bool,
    ) -> Result<(), AgentError> {
        let client = ImapClient::new(self.cfg.timeout_ms);
        let host = config.host.clone();
        let port = config.port;
        let tls_mode = config.tls_mode;
        let username = config.username.clone();
        let password = config.password.clone();
        let folder_owned = folder.to_string();
        let op = if read { "+FLAGS" } else { "-FLAGS" };
        let query = format!("{op} (\\Seen)");
        client
            .with_session(
                &host,
                port,
                tls_mode,
                &username,
                &password,
                move |session| {
                    session.select(&folder_owned).map_err(|e| {
                        AgentError::AgentFailed(format!("IMAP SELECT {folder_owned}: {e}"))
                    })?;
                    session.uid_store(uid.to_string(), &query).map_err(|e| {
                        AgentError::AgentFailed(format!("IMAP UID STORE {uid}: {e}"))
                    })?;
                    Ok::<_, AgentError>(())
                },
            )
            .await
    }
}
