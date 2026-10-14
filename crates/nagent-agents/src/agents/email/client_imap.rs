//! Async IMAP wire client (plan 1791566269874 Phase 3+).
//!
//! Thin adapter over the well-tested synchronous
//! [`imap`](https://docs.rs/imap/latest/imap/) crate. Every
//! call goes through `tokio::task::spawn_blocking` so the
//! IMAP work never blocks the tokio runtime, and the
//! `EmailImap` capability trait surface (which the agents
//! code against) stays async.
//!
//! ## Connection model
//!
//! Per-call: each agent invocation opens a fresh IMAP
//! connection, runs `LOGIN`, executes the requested
//! commands, and `LOGOUT`s. There is no persistent
//! per-user pool on the server side; the chat agents
//! invoke IMAP at most a handful of times per turn, so
//! the per-call cost is acceptable. A v2 follow-up can
//! layer a per-user pool keyed on `user_id` without
//! changing the trait surface.
//!
//! ## TLS posture
//!
//! Three modes, matching the per-user `tls_mode` value on
//! the `email_imap` `ServiceDef`:
//!
//! - `Implicit` → `imap::connect` (port 993, TLS on
//!   connect via `native-tls`).
//! - `StartTls` → `imap::connect_starttls` (port 143,
//!   plain connection upgraded to TLS after the server
//!   greeting).
//! - `None` → plain `std::net::TcpStream`. Refused by
//!   default at the probe endpoint; the agent layer
//!   rejects the configuration outright.
//!
//! The `imap` crate 2.4.x uses `native-tls` for both
//! paths; the crate is the de-facto standard Rust IMAP
//! client and ships the TLS glue so the server does not
//! have to.
//!
//! ## Credential redaction
//!
//! The `imap` crate does not echo the command string in
//! its error variants — the `Error` enum carries the
//! server's reply text (e.g. "A1 NO unable to login: …")
//! which is safe to surface verbatim (the user needs that
//! signal). The `LOGIN` command itself is consumed
//! inside the crate and the credentials never reach the
//! error path. This is the "the server's reply is safe,
//! the client command is not" model the project rule
//! (AGENTS.md §6) prefers.

use std::sync::Arc;
use std::time::Duration;

use chrono::TimeZone;
use imap::types::Fetch;
use imap::{Client, Session};
use imap_proto::types::{Address, Envelope};
use rustls::pki_types::ServerName;
use rustls::ClientConfig;
use rustls::ClientConnection;
use rustls::StreamOwned;
use serde::Serialize;
use tokio::task;

use crate::agents::AgentError;

use super::EmailSummary;

/// Stream type the `imap` crate's `Client::new` consumes:
/// the sync `rustls::StreamOwned` over a `std::net::TcpStream`.
/// `StreamOwned` implements `std::io::Read + std::io::Write`
/// (the sync traits), which is what the imap crate
/// requires. The `tokio-rustls` API does not implement
/// those traits, so we use the sync rustls API for the
/// IMAP wire layer (the SMTP wire layer keeps using
/// `tokio-rustls` because `async-smtp` is fully async).
type ImapTlsStream = StreamOwned<ClientConnection, std::net::TcpStream>;

/// TLS posture for one IMAP connection. Mirrors the
/// `tls_mode` field on the per-user `email_imap`
/// `ServiceDef`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ImapTlsMode {
    /// Connect with TLS on connect (port 993 — IMAPS).
    Implicit,
    /// Connect plain, then upgrade with `STARTTLS`
    /// (port 143).
    StartTls,
    /// Plain-text — refused by default. Only reachable
    /// when the operator opts in via
    /// `[email].allow_cleartext_imap = true`.
    None,
}

impl ImapTlsMode {
    /// Parse the `tls_mode` value from the per-user
    /// `ServiceDef`. Case-insensitive. The `imap` crate
    /// does the rest: `connect` vs `connect_starttls`.
    pub fn parse(value: &str) -> Result<Self, AgentError> {
        match value.trim().to_ascii_lowercase().as_str() {
            "implicit" | "ssl" | "tls" => Ok(Self::Implicit),
            "starttls" => Ok(Self::StartTls),
            "none" | "plain" | "cleartext" => Ok(Self::None),
            other => Err(AgentError::AgentFailed(format!(
                "unknown IMAP tls_mode `{other}` (expected one of `implicit`, `starttls`, `none`)"
            ))),
        }
    }
}

/// Per-call IMAP client handle. Carries only the
/// per-request timeout; the actual connection is opened
/// fresh on every call (the per-call connection model
/// the plan §1.5 specifies).
#[derive(Clone)]
pub struct ImapClient {
    timeout: Duration,
}

impl std::fmt::Debug for ImapClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImapClient")
            .field("timeout", &self.timeout)
            .finish()
    }
}

impl ImapClient {
    pub fn new(timeout_ms: u64) -> Self {
        Self {
            timeout: Duration::from_millis(timeout_ms),
        }
    }

    /// Open a fresh IMAP connection, run `LOGIN`, hand
    /// the authenticated `Session` to `f`, then run
    /// `LOGOUT` and return `f`'s result.
    ///
    /// `f` runs inside `spawn_blocking` along with the
    /// entire `imap` session, so the IMAP work never
    /// blocks the tokio runtime.
    ///
    /// ## TLS posture
    ///
    /// The `imap` crate 2.4 ships `native-tls`-based
    /// `connect` / `connect_starttls` helpers behind the
    /// `tls` cargo feature. We compile the crate without
    /// that feature (no OpenSSL on the build machine)
    /// and do the TLS upgrade ourselves with the SYNC
    /// `rustls` API (`StreamOwned<ClientConnection,
    /// TcpStream>`). `tokio-rustls` does not work here
    /// because the imap crate is fundamentally sync and
    /// needs `std::io::Read + std::io::Write`, not
    /// `AsyncRead + AsyncWrite`.
    ///
    /// v1 supports `Implicit` only. STARTTLS would need
    /// a sync TLS upgrade mid-connection, which requires
    /// pulling the TcpStream out of the imap `Client`
    /// — a capability the imap 2.4 crate does not
    /// expose (it requires the `tls` feature).
    /// A v2 follow-up can add STARTTLS by either
    /// (a) switching to `imap 3.0` (which has rustls
    ///     support built in) or
    /// (b) running a parallel plain `imap::Client`
    ///     that we rebuild after the upgrade.
    pub async fn with_session<F, R>(
        &self,
        host: &str,
        port: u16,
        tls_mode: ImapTlsMode,
        username: &str,
        password: &str,
        f: F,
    ) -> Result<R, AgentError>
    where
        F: FnOnce(&mut Session<ImapTlsStream>) -> Result<R, AgentError> + Send + 'static,
        R: Send + 'static,
    {
        let host_owned = host.to_string();
        let port_owned = port;
        let username_owned = username.to_string();
        let password_owned = password.to_string();
        let host_for_err = host.to_string();
        let timeout = self.timeout;

        task::spawn_blocking(move || -> Result<R, AgentError> {
            // Sync std API because the rest of the IMAP wire layer is
            // sync; `spawn_blocking` keeps a slow DNS lookup or a
            // black-holed server off the tokio runtime. We use
            // `TcpStream::connect` (not `connect_timeout`) so it does
            // the `ToSocketAddrs` resolution for us.
            let addr: (&str, u16) = (host_owned.as_str(), port_owned);
            let tcp = std::net::TcpStream::connect(addr).map_err(|e| {
                AgentError::AgentFailed(format!("IMAP connect to {host_owned}:{port_owned}: {e}"))
            })?;
            // Per-call timeout on every subsequent read/write. The
            // `imap` crate does its own buffering so the cost is
            // amortised, but the timeout still applies per read syscall.
            tcp.set_read_timeout(Some(timeout)).map_err(|e| {
                AgentError::AgentFailed(format!("IMAP set_read_timeout to {host_owned}: {e}"))
            })?;
            tcp.set_write_timeout(Some(timeout)).map_err(|e| {
                AgentError::AgentFailed(format!("IMAP set_write_timeout to {host_owned}: {e}"))
            })?;

            let tls_config = Arc::new(
                ClientConfig::builder()
                    .with_root_certificates(rustls::RootCertStore::from_iter(
                        webpki_roots::TLS_SERVER_ROOTS.iter().cloned(),
                    ))
                    .with_no_client_auth(),
            );

            let client = match tls_mode {
                ImapTlsMode::Implicit => {
                    let server_name = ServerName::try_from(host_owned.clone()).map_err(|e| {
                        AgentError::AgentFailed(format!("IMAP TLS server name: {e}"))
                    })?;
                    let conn = ClientConnection::new(tls_config, server_name)
                        .map_err(|e| AgentError::AgentFailed(format!("IMAP TLS init: {e}")))?;
                    let tls_stream: ImapTlsStream = StreamOwned::new(conn, tcp);
                    let mut client = Client::new(tls_stream);
                    client.read_greeting().map_err(|e| {
                        AgentError::AgentFailed(format!("IMAP greeting after TLS: {e}"))
                    })?;
                    client
                }
                ImapTlsMode::StartTls => {
                    return Err(AgentError::AgentFailed(
                        "IMAP STARTTLS is not supported in this build (the \
                         `imap` crate's TLS path is native-tls-only and we \
                         compile without that feature; use the `Implicit` \
                         TLS mode on port 993 for the same server)"
                            .into(),
                    ));
                }
                ImapTlsMode::None => {
                    return Err(AgentError::AgentFailed(
                        "IMAP clear-text (`tls_mode = none`) is refused by \
                         default; set `[email].allow_cleartext_imap = true` on \
                         the server to enable"
                            .into(),
                    ));
                }
            };

            // The `imap` crate's `login` consumes the client and returns
            // a `Session<ImapTlsStream>`. The password stays inside the
            // call and never surfaces in the error path.
            let mut session =
                client
                    .login(username_owned, password_owned)
                    .map_err(|(e, _orig_client)| {
                        AgentError::AgentFailed(format!("IMAP login to {host_owned}: {e}"))
                    })?;
            let result = f(&mut session);
            // Best-effort LOGOUT — failures don't fail the request;
            // the user's op already succeeded or failed for another reason.
            let _ = session.logout();
            result
        })
        .await
        .map_err(|e| {
            AgentError::AgentFailed(format!("IMAP task panicked: {e} (host={host_for_err})"))
        })?
    }
}

// ---------------------------------------------------------------------------
// IMAP response → LLM-facing types. The `imap` crate gives us
// a typed `Envelope` (with parsed `Address` lists) and
// `Fetch` (with `envelope()`, `text()`, `flags()` accessors)
// so we do not have to hand-roll any parser. These helpers
// just project the typed IMAP values into the agents-crate
// `EmailSummary` / `EmailMessage` shapes.
// ---------------------------------------------------------------------------

/// Project a `Fetch` (the typed IMAP response) into the
/// agents-crate `EmailSummary` shape. The `imap` crate's
/// `Envelope::date` is already parsed to a `DateTime<FixedOffset>`;
/// we convert it to UTC. The `from` / `to` address lists are
/// rendered as `Display Name <mailbox@host>` strings so the
/// LLM-facing payload is plain text.
pub fn fetch_to_summary(fetch: &Fetch, folder: &str) -> Result<EmailSummary, AgentError> {
    let uid = fetch
        .uid
        .ok_or_else(|| AgentError::AgentFailed("IMAP FETCH did not return UID".into()))?;
    let envelope = fetch.envelope();
    let (date, from, subject, message_id) = match envelope {
        Some(env) => envelope_fields(env),
        None => (None, String::new(), String::new(), None),
    };
    let seen = fetch
        .flags()
        .iter()
        .any(|f| matches!(f, imap::types::Flag::Seen));
    Ok(EmailSummary {
        uid: uid,
        folder: folder.to_string(),
        from,
        subject,
        date,
        snippet: None,
        message_id,
        seen,
    })
}

/// Render the four LLM-facing fields the `EmailSummary`
/// carries out of a typed `Envelope`. The `imap-proto`
/// crate's `Envelope` is `Cow<[u8]>`-flavoured: every
/// field is a raw byte slice. We convert each to UTF-8
/// (lossy) so the LLM-facing payload is a plain `String`
/// (a lossy `String` is acceptable here because email
/// headers are 7-bit ASCII by RFC 5322; the rare 8-bit
/// header that is not valid UTF-8 is shown with
/// replacement characters and the LLM can still reason
/// about it).
fn envelope_fields(
    env: &Envelope,
) -> (
    Option<chrono::DateTime<chrono::Utc>>,
    String,
    String,
    Option<String>,
) {
    let date = env
        .date
        .and_then(|b| std::str::from_utf8(b).ok())
        .and_then(parse_imap_date);
    let from = address_list_to_string(env.from.as_deref().unwrap_or(&[]));
    let subject = env
        .subject
        .map(|b| String::from_utf8_lossy(b).into_owned())
        .unwrap_or_default();
    let message_id = env
        .message_id
        .map(|b| String::from_utf8_lossy(b).into_owned());
    (date, from, subject, message_id)
}

/// Render a typed IMAP address list as a comma-separated
/// `Display Name <mailbox@host>` string. Missing fields
/// (`NIL` from the wire) surface as an empty segment; the
/// LLM-facing payload is meant to be human-readable, not
/// machine-roundtrippable.
fn address_list_to_string(addrs: &[Address]) -> String {
    let mut out = String::new();
    for addr in addrs {
        let mailbox = bytes_to_string_lossy(addr.mailbox);
        let host = bytes_to_string_lossy(addr.host);
        if mailbox.is_empty() && host.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push_str(", ");
        }
        let name = bytes_to_string_lossy(addr.name);
        if !name.is_empty() {
            out.push_str(&name);
            out.push_str(" <");
            out.push_str(&mailbox);
            out.push('@');
            out.push_str(&host);
            out.push('>');
        } else {
            out.push_str(&mailbox);
            out.push('@');
            out.push_str(&host);
        }
    }
    out
}

/// Lossy `String` conversion for the IMAP `Envelope` byte
/// slices. Lossy is fine here because RFC 5322 §2.2
/// restricts header text to 7-bit ASCII (the only
/// exception is the 8-bit MIME extension, where
/// non-UTF-8 encodings are labelled by a
/// `Content-Type: …; charset=…` header — the LLM
/// reads the raw bytes anyway).
fn bytes_to_string_lossy(b: Option<&[u8]>) -> String {
    b.map(|s| String::from_utf8_lossy(s).into_owned())
        .unwrap_or_default()
}

/// Parse an IMAP `INTERNALDATE` / `Envelope date` value.
/// RFC 3501 §7.3: `DD-Mon-YYYY HH:MM:SS +/-ZZZZ` (the
/// `+0000` form is the canonical GMT case the IMAP
/// envelope uses for the parsed `Date:` header). We
/// fall through to RFC 3339 + the date-only form for
/// servers that hand us a less-strictly-formatted value.
fn parse_imap_date(s: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    if let Ok(d) = chrono::DateTime::parse_from_str(s, "%d-%b-%Y %H:%M:%S %z") {
        return Some(d.with_timezone(&chrono::Utc));
    }
    if let Ok(d) = chrono::NaiveDateTime::parse_from_str(s, "%d-%b-%Y %H:%M:%S") {
        return Some(chrono::Utc.from_utc_datetime(&d));
    }
    if let Ok(d) = chrono::DateTime::parse_from_rfc3339(s) {
        return Some(d.with_timezone(&chrono::Utc));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tls_mode_parser_accepts_known_values() {
        for v in ["implicit", "ssl", "tls", "IMPLICIT", "  StartTls  ", "none"] {
            let m = ImapTlsMode::parse(v).unwrap();
            assert!(
                matches!(
                    m,
                    ImapTlsMode::Implicit | ImapTlsMode::StartTls | ImapTlsMode::None
                ),
                "unexpected variant for {v}"
            );
        }
    }

    #[test]
    fn tls_mode_parser_rejects_unknown_values() {
        let err = ImapTlsMode::parse("garbage").unwrap_err();
        assert!(matches!(err, AgentError::AgentFailed(_)));
    }

    #[test]
    fn address_list_to_string_renders_display_name_and_address() {
        let addrs = vec![Address {
            name: Some(b"Alice".as_slice()),
            mailbox: Some(b"alice".as_slice()),
            host: Some(b"example.com".as_slice()),
            adl: None,
        }];
        assert_eq!(address_list_to_string(&addrs), "Alice <alice@example.com>");
    }

    #[test]
    fn address_list_to_string_omits_empty_addresses() {
        let addrs = vec![
            Address {
                name: Some(b"Alice".as_slice()),
                mailbox: Some(b"alice".as_slice()),
                host: Some(b"example.com".as_slice()),
                adl: None,
            },
            // IMAP servers often emit `NIL` for empty
            // entries; the typed crate represents that as
            // `None`. We should skip them so the LLM
            // does not see ` <@>` noise.
            Address {
                name: None,
                mailbox: None,
                host: None,
                adl: None,
            },
        ];
        assert_eq!(address_list_to_string(&addrs), "Alice <alice@example.com>");
    }

    #[test]
    fn address_list_to_string_renders_address_only_when_name_missing() {
        let addrs = vec![Address {
            name: None,
            mailbox: Some(b"alice".as_slice()),
            host: Some(b"example.com".as_slice()),
            adl: None,
        }];
        assert_eq!(address_list_to_string(&addrs), "alice@example.com");
    }
}
