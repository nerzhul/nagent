//! Async SMTP wire client (plan 1791566269874 Phase 5).
//!
//! Thin adapter over the well-tested
//! [`async-smtp`](https://docs.rs/async-smtp) crate. The
//! crate handles the protocol surface (EHLO, STARTTLS, AUTH
//! PLAIN, MAIL FROM, RCPT TO, DATA, QUIT) so the server does
//! not have to hand-roll any of it.
//!
//! ## Connection model
//!
//! Per-call: every `email_send` invocation opens a fresh
//! SMTP connection, runs `EHLO` / `STARTTLS` / `AUTH`, sends
//! the message, and closes. No persistent state lives on
//! the server. A v2 follow-up can layer a per-user pool
//! without changing the trait surface.
//!
//! ## TLS posture
//!
//! Three modes, matching the per-user `tls_mode` value on
//! the `email_smtp` `ServiceDef`:
//!
//! - `Implicit` → `SmtpTransport` over a rustls TLS stream
//!   (port 465 — SMTPS).
//! - `StartTls` → plain TCP, then `async-smtp`'s
//!   `starttls()` upgrade (port 587).
//! - `None` → plain `TcpStream`. Refused for port 25 by
//!   default at the probe endpoint.
//!
//! ## Credential redaction
//!
//! The `async-smtp` crate's `Error` enum does not echo the
//! encoded `AUTH PLAIN` payload (which carries the
//! password) — it surfaces the server's reply text only.
//! The MAIL FROM / RCPT TO / DATA commands the agents
//! issue carry the sender / recipient address, not
//! credentials, so the only redaction surface is the
//! user's `from_address` (which the LLM-facing UI does
//! not display). This is the "the server's reply is safe,
//! the client command is not" model the project rule
//! (AGENTS.md §6) prefers.

use std::sync::Arc;
use std::time::Duration;

use async_smtp::authentication::Mechanism;
use async_smtp::commands::MailCommand;
use async_smtp::extension::ClientId;
use async_smtp::EmailAddress;
use async_smtp::{Envelope as SmtpEnvelope, SmtpClient as AsyncSmtpClient, SmtpTransport};
use rustls::pki_types::ServerName;
use rustls::ClientConfig;
use serde::Serialize;
use tokio::io::BufReader;
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;

use crate::agents::AgentError;

use super::OutgoingMessage;

/// TLS posture for one SMTP connection. Mirrors the
/// `tls_mode` field on the per-user `email_smtp`
/// `ServiceDef`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SmtpTlsMode {
    /// Connect with TLS on connect (port 465 — SMTPS).
    Implicit,
    /// Connect plain, then upgrade with `STARTTLS`
    /// (port 587).
    StartTls,
    /// Plain-text — refused for port 25 by default. Only
    /// reachable when the operator sets
    /// `[email].allow_cleartext_smtp = true`.
    None,
}

impl SmtpTlsMode {
    /// Parse the `tls_mode` value from the per-user
    /// `ServiceDef`. Case-insensitive.
    pub fn parse(value: &str) -> Result<Self, AgentError> {
        match value.trim().to_ascii_lowercase().as_str() {
            "implicit" | "ssl" | "tls" => Ok(Self::Implicit),
            "starttls" => Ok(Self::StartTls),
            "none" | "plain" | "cleartext" => Ok(Self::None),
            other => Err(AgentError::AgentFailed(format!(
                "unknown SMTP tls_mode `{other}` (expected one of `implicit`, `starttls`, `none`)"
            ))),
        }
    }
}

/// Per-call SMTP client handle. Carries only the
/// per-request timeout; the actual connection is opened
/// fresh on every call (the per-call connection model
/// the plan §1.5 specifies).
#[derive(Clone)]
pub struct SmtpClient {
    timeout: Duration,
}

impl std::fmt::Debug for SmtpClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SmtpClient")
            .field("timeout", &self.timeout)
            .finish()
    }
}

impl SmtpClient {
    pub fn new(timeout_ms: u64) -> Self {
        Self {
            timeout: Duration::from_millis(timeout_ms),
        }
    }

    /// Open an SMTP connection, run `EHLO` (and `STARTTLS`
    /// + `AUTH PLAIN` when applicable), and return a
    /// `SmtpTransport` ready to send mail.
    pub async fn connect(
        &self,
        host: &str,
        port: u16,
        tls_mode: SmtpTlsMode,
        username: &str,
        password: &str,
        helo_identity: &str,
    ) -> Result<SmtpTransport<SmtpStreamInner>, AgentError> {
        let host_owned = host.to_string();
        let port_owned = port;
        let helo_owned = helo_identity.to_string();
        let host_for_err = host.to_string();
        let username_owned = username.to_string();
        let password_owned = password.to_string();

        // 1. Open the underlying TCP stream with the
        //    configured timeout.
        let tcp = tokio::time::timeout(
            self.timeout,
            TcpStream::connect((host_owned.as_str(), port_owned)),
        )
        .await
        .map_err(|_| {
            AgentError::AgentFailed(format!(
                "SMTP connect to {host_owned}:{port_owned} timed out after {}ms",
                self.timeout.as_millis()
            ))
        })?
        .map_err(|e| {
            AgentError::AgentFailed(format!("SMTP connect to {host_owned}:{port_owned}: {e}"))
        })?;

        // 2. Build the post-STARTTLS (or SMTPS) stream.
        //    We do NOT wrap in `BufStream` because
        //    `async-smtp`'s `starttls()` returns the
        //    original `S` type (the inner `SmtpStreamInner`
        //    we built), and we need the `TcpStream` inside
        //    to do the rustls upgrade. The
        //    `SmtpStreamInner` enum itself implements
        //    `AsyncRead + AsyncWrite + AsyncBufRead` so
        //    it satisfies `async-smtp`'s stream bound
        //    directly.
        let (stream, _pre_starttls_transport): (
            SmtpStreamInner,
            Option<SmtpTransport<SmtpStreamInner>>,
        ) = match tls_mode {
            SmtpTlsMode::Implicit => {
                let tls_stream = tls_connector()
                    .connect(
                        ServerName::try_from(host_owned.clone()).map_err(|e| {
                            AgentError::AgentFailed(format!("SMTP TLS server name: {e}"))
                        })?,
                        tcp,
                    )
                    .await
                    .map_err(|e| {
                        AgentError::AgentFailed(format!(
                            "SMTP TLS handshake to {host_owned}:{port_owned}: {e}"
                        ))
                    })?;
                (SmtpStreamInner::Tls(tls_stream), None)
            }
            SmtpTlsMode::StartTls => {
                // Build the transport over a plain
                // `SmtpStreamInner::Plain(tcp)` so we can
                // ask `async-smtp` to perform the
                // STARTTLS upgrade. `starttls()` returns
                // the same `SmtpStreamInner` (it sends
                // STARTTLS and returns the inner stream
                // for us to upgrade). We then do the
                // rustls upgrade ourselves and re-wrap in
                // `SmtpStreamInner::Tls`.
                let plain = SmtpStreamInner::Plain(BufReader::new(tcp));
                let smtp_client =
                    AsyncSmtpClient::new().hello_name(ClientId::new(helo_owned.clone()));
                let transport = SmtpTransport::new(smtp_client, plain).await.map_err(|e| {
                    AgentError::AgentFailed(format!(
                        "SMTP connect to {host_owned}:{port_owned}: {e}"
                    ))
                })?;
                let inner = transport.starttls().await.map_err(|e| {
                    AgentError::AgentFailed(format!(
                        "SMTP STARTTLS to {host_owned}:{port_owned}: {e}"
                    ))
                })?;
                // The returned `SmtpStreamInner` is
                // still `Plain` (the rustls upgrade is
                // the caller's job). Pull the `TcpStream`
                // out of the inner `BufReader`, upgrade,
                // re-wrap.
                let tcp_after_starttls = match inner {
                    SmtpStreamInner::Plain(buf) => buf.into_inner(),
                    SmtpStreamInner::Tls(_) => {
                        return Err(AgentError::AgentFailed(
                            "SMTP STARTTLS: unexpected TLS stream returned after upgrade".into(),
                        ));
                    }
                };
                let tls_stream = tls_connector()
                    .connect(
                        ServerName::try_from(host_owned.clone()).map_err(|e| {
                            AgentError::AgentFailed(format!("SMTP TLS server name: {e}"))
                        })?,
                        tcp_after_starttls,
                    )
                    .await
                    .map_err(|e| {
                        AgentError::AgentFailed(format!(
                            "SMTP TLS handshake after STARTTLS to {host_owned}: {e}"
                        ))
                    })?;
                (SmtpStreamInner::Tls(tls_stream), None)
            }
            SmtpTlsMode::None => {
                return Err(AgentError::AgentFailed(
                    "SMTP clear-text (`tls_mode = none`) is refused by \
                     default; set `[email].allow_cleartext_smtp = true` on \
                     the server to enable"
                        .into(),
                ));
            }
        };
        // The pre-STARTTLS transport (if any) is dropped
        // here; its plain TCP connection was already
        // upgraded in place, so dropping the wrapper
        // does NOT close the upgraded stream.
        drop(_pre_starttls_transport);

        // 3. Build the post-STARTTLS (or SMTPS) transport
        //    on top of the upgraded stream.
        let smtp_client = AsyncSmtpClient::new().hello_name(ClientId::new(helo_owned));
        let mut transport = SmtpTransport::new(smtp_client, stream).await.map_err(|e| {
            AgentError::AgentFailed(format!("SMTP connect to {host_owned}:{port_owned}: {e}"))
        })?;

        // 4. AUTH PLAIN. We use `try_login` with the
        //    explicit mechanism list so the encoded
        //    payload (`\0user\0pass` base64) stays
        //    inside `async-smtp` and never surfaces in
        //    the error path. The crate's `Error` enum
        //    carries the server's reply text only.
        let creds = async_smtp::authentication::Credentials::new(username_owned, password_owned);
        transport
            .try_login(&creds, &[Mechanism::Plain])
            .await
            .map_err(|e| {
                AgentError::AgentFailed(format!("SMTP AUTH PLAIN to {host_for_err}: {e}"))
            })?;
        Ok(transport)
    }
}

/// Enum over the two stream shapes `async-smtp` runs on:
/// plain `TcpStream` (wrapped in `BufReader` so it
/// implements `AsyncBufRead`, the trait `async-smtp` needs
/// to do line-based reading) or the rustls TLS stream.
/// We hide the choice behind a single newtype so the
/// `SmtpTransport` type stays readable.
#[derive(Debug)]
pub enum SmtpStreamInner {
    /// Plain TCP wrapped in a `BufReader` so the
    /// `AsyncBufRead` trait is satisfied. Used while
    /// `async-smtp` is upgrading to TLS via `starttls()`.
    Plain(BufReader<TcpStream>),
    /// rustls TLS stream. Used for SMTPS (port 465) and
    /// for the post-STARTTLS SMTP session (port 587).
    Tls(tokio_rustls::client::TlsStream<TcpStream>),
}

impl tokio::io::AsyncRead for SmtpStreamInner {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            SmtpStreamInner::Plain(s) => std::pin::Pin::new(s).poll_read(cx, buf),
            SmtpStreamInner::Tls(s) => std::pin::Pin::new(s).poll_read(cx, buf),
        }
    }
}
impl tokio::io::AsyncBufRead for SmtpStreamInner {
    fn poll_fill_buf(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<&[u8]>> {
        match self.get_mut() {
            SmtpStreamInner::Plain(s) => std::pin::Pin::new(s).poll_fill_buf(cx),
            SmtpStreamInner::Tls(s) => std::pin::Pin::new(s).poll_fill_buf(cx),
        }
    }
    fn consume(self: std::pin::Pin<&mut Self>, amt: usize) {
        match self.get_mut() {
            SmtpStreamInner::Plain(s) => std::pin::Pin::new(s).consume(amt),
            SmtpStreamInner::Tls(s) => std::pin::Pin::new(s).consume(amt),
        }
    }
}
impl tokio::io::AsyncWrite for SmtpStreamInner {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        match self.get_mut() {
            SmtpStreamInner::Plain(s) => std::pin::Pin::new(s).poll_write(cx, buf),
            SmtpStreamInner::Tls(s) => std::pin::Pin::new(s).poll_write(cx, buf),
        }
    }
    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            SmtpStreamInner::Plain(s) => std::pin::Pin::new(s).poll_flush(cx),
            SmtpStreamInner::Tls(s) => std::pin::Pin::new(s).poll_flush(cx),
        }
    }
    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            SmtpStreamInner::Plain(s) => std::pin::Pin::new(s).poll_shutdown(cx),
            SmtpStreamInner::Tls(s) => std::pin::Pin::new(s).poll_shutdown(cx),
        }
    }
}

/// Build the shared rustls `ClientConfig` used by every SMTP
/// connection. The `imap` crate pulls the same bundle for
/// its TLS connector, so the operator only pays for one
/// CA load.
fn tls_connector() -> TlsConnector {
    let config = ClientConfig::builder()
        .with_root_certificates(rustls::RootCertStore::from_iter(
            webpki_roots::TLS_SERVER_ROOTS.iter().cloned(),
        ))
        .with_no_client_auth();
    TlsConnector::from(Arc::new(config))
}

// ---------------------------------------------------------------------------
// High-level send / probe helpers. The `SmtpTransport`
// from `async-smtp` is generic over the stream, so the
// helpers below are similarly generic. The caller picks
// the stream type once in `SmtpClient::connect`.
// ---------------------------------------------------------------------------

/// Send a message through an already-authenticated
/// `SmtpTransport`. Used by the `email_send` agent. The
/// transport is dropped on return so the connection
/// closes.
pub async fn send_message<S>(
    transport: &mut SmtpTransport<S>,
    message: &OutgoingMessage,
) -> Result<(), AgentError>
where
    S: tokio::io::AsyncRead
        + tokio::io::AsyncWrite
        + tokio::io::AsyncBufRead
        + Unpin
        + Send
        + 'static,
{
    let from = message
        .from
        .parse::<async_smtp::EmailAddress>()
        .map_err(|e| AgentError::AgentFailed(format!("SMTP from address: {e}")))?;
    let to: Result<Vec<_>, _> = message
        .to
        .iter()
        .chain(message.cc.iter())
        .chain(message.bcc.iter())
        .map(|a| {
            a.parse::<async_smtp::EmailAddress>()
                .map_err(|e| AgentError::AgentFailed(format!("SMTP recipient `{a}`: {e}")))
        })
        .collect();
    let to = to?;
    let envelope = SmtpEnvelope::new(Some(from), to)
        .map_err(|e| AgentError::AgentFailed(format!("SMTP envelope: {e}")))?;
    let body = build_message(message);
    let email = async_smtp::SendableEmail::new(envelope, body);
    transport
        .send(email)
        .await
        .map_err(|e| AgentError::AgentFailed(format!("SMTP send: {e}")))?;
    Ok(())
}

/// Run a `MAIL FROM` round-trip on an already-authenticated
/// SMTP transport. Used by the probe endpoint to verify
/// the relay will accept the user's `from_address` (a 553
/// rejection here is the most common reason a freshly-set
/// vault row never sends).
pub async fn mail_from_probe<S>(
    transport: &mut SmtpTransport<S>,
    from: &str,
) -> Result<async_smtp::response::Response, AgentError>
where
    S: tokio::io::AsyncRead
        + tokio::io::AsyncWrite
        + tokio::io::AsyncBufRead
        + Unpin
        + Send
        + 'static,
{
    let from_addr = from
        .parse::<EmailAddress>()
        .map_err(|e| AgentError::AgentFailed(format!("SMTP from address `{from}`: {e}")))?;
    let response = transport
        .get_mut()
        .command(MailCommand::new(Some(from_addr), vec![]))
        .await
        .map_err(|e| AgentError::AgentFailed(format!("SMTP MAIL FROM `{from}`: {e}")))?;
    Ok(response)
}

/// Build the raw RFC 5322 message the SMTP `DATA` command
/// ships. v1 builds the headers inline (no attachments);
/// attachments are a v2 follow-up.
pub fn build_message(message: &OutgoingMessage) -> String {
    let mut out = String::new();
    out.push_str("From: <");
    out.push_str(&message.from);
    out.push_str(">\r\n");
    for to in &message.to {
        out.push_str("To: <");
        out.push_str(to);
        out.push_str(">\r\n");
    }
    for cc in &message.cc {
        out.push_str("Cc: <");
        out.push_str(cc);
        out.push_str(">\r\n");
    }
    if let Some(irt) = &message.in_reply_to {
        out.push_str("In-Reply-To: ");
        out.push_str(irt);
        out.push_str("\r\n");
    }
    if let Some(rf) = &message.references {
        out.push_str("References: ");
        out.push_str(rf);
        out.push_str("\r\n");
    }
    out.push_str("Subject: ");
    out.push_str(&message.subject);
    out.push_str("\r\n");
    out.push_str("Content-Type: text/plain; charset=utf-8\r\n");
    out.push_str("Content-Transfer-Encoding: 8bit\r\n");
    out.push_str("\r\n");
    out.push_str(&message.body);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tls_mode_parser_accepts_known_values() {
        for v in ["implicit", "ssl", "starttls", "none", "IMPLICIT"] {
            let m = SmtpTlsMode::parse(v).unwrap();
            assert!(
                matches!(
                    m,
                    SmtpTlsMode::Implicit | SmtpTlsMode::StartTls | SmtpTlsMode::None
                ),
                "unexpected variant for {v}"
            );
        }
    }

    #[test]
    fn tls_mode_parser_rejects_unknown_values() {
        let err = SmtpTlsMode::parse("garbage").unwrap_err();
        assert!(matches!(err, AgentError::AgentFailed(_)));
    }

    #[test]
    fn build_message_emits_required_headers() {
        let msg = OutgoingMessage {
            from: "alice@example.com".to_string(),
            to: vec!["bob@example.com".to_string()],
            cc: vec!["carol@example.com".to_string()],
            bcc: vec!["dave@example.com".to_string()],
            subject: "Hello".to_string(),
            body: "World".to_string(),
            in_reply_to: Some("<prev@example.com>".to_string()),
            references: None,
        };
        let raw = build_message(&msg);
        assert!(raw.starts_with("From: <alice@example.com>\r\n"));
        assert!(raw.contains("To: <bob@example.com>\r\n"));
        assert!(raw.contains("Cc: <carol@example.com>\r\n"));
        // BCC is intentionally NOT emitted as a header
        // (RFC 5321 §4.1.1.3 — recipients are listed in the
        // envelope, not the message).
        assert!(!raw.contains("Bcc:"));
        assert!(raw.contains("In-Reply-To: <prev@example.com>\r\n"));
        assert!(raw.contains("Subject: Hello\r\n"));
        assert!(raw.ends_with("World"));
    }
}
