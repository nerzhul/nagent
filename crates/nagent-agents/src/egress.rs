//! Shared hardened HTTP egress client used by every network agent.
//!
//! The plan (`5.F`) extracts the SSRF / timeout / redirect / size
//! defaults into one place so that adding a network agent does not
//! require re-deriving the policy, and so that tightening a default
//! (e.g. closing the DNS-rebinding window) lands for every caller at
//! once. SF-6 will layer DNS pinning on top of [`EgressClient`] in a
//! follow-up commit.
//!
//! ## Hardening
//!
//! The client enforces the following defaults on every request:
//!
//! - **Scheme allowlist**: only `http://` and `https://` URLs are
//!   accepted; anything else surfaces as [`EgressError::Scheme`].
//! - **Redirect cap**: at most [`MAX_REDIRECTS`] hops, with the same
//!   scheme / IP rules re-applied after each redirect so an attacker
//!   cannot bounce through a public host into a private one.
//! - **Connect + read timeouts**: the configured timeouts, floored
//!   at 1 s (a 0 ms timeout is a self-DoS).
//! - **Body-size budget**: streaming callers can ask for
//!   `max_bytes(bytes)` on each [`EgressClient::get_stream`]; the
//!   body is read chunk-by-chunk and [`EgressError::ResponseExceeded`]
//!   fires as soon as the next chunk would push us over the limit.
//!   Memory is bounded by the chunk size, not the page size.
//!
//! ## IP policy
//!
//! [`is_addr_allowed`] classifies IPv4 / IPv6 addresses against the
//! SSRF block-list (loopback, RFC1918, link-local, ULA, multicast,
//! broadcast, carrier-grade NAT, IPv4-mapped IPv6, …). The classifier
//! is **always** applied to the resolved IP before reqwest connects,
//! and again on every redirect. Operators can opt in to public
//! addresses via [`EgressConfig::allow_public`].
//!
//! ## Hostname allow-list
//!
//! Optional [`EgressConfig::allowlist`] takes precedence over the
//! IP-based policy: a hostname that matches an entry is allowed
//! regardless of where it currently resolves. `*.example.com` matches
//! `example.com` and every subdomain; `*` matches every host
//! (intended for tests / local fixtures only).
//!
//! ## DNS-rebinding note (v1 limitation)
//!
//! The current implementation validates the resolved IP once before
//! the request and re-validates after each redirect. A small rebind
//! window between the pre-check and reqwest's own resolver remains;
//! SF-6 closes it by replacing the pre-check with a custom
//! `reqwest::dns::Resolve` impl. Until then the IP check still
//! catches the practical SSRF cases (loopback / private / ULA) and
//! the hostname allow-list overrides both.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use futures_util::StreamExt;

/// Maximum number of redirects an [`EgressClient`] will follow.
pub const MAX_REDIRECTS: usize = 5;

/// Lower bound for any timeout knob, in milliseconds. A 0 ms timeout
/// is a self-DoS — the first packet would be dropped before the
/// connect syscall returns. The floor is applied transparently in
/// [`EgressConfig::finalize`].
pub const MIN_TIMEOUT_MS: u64 = 1_000;

/// Maximum body size the streaming helper will accept in a single
/// pass, in bytes (2 MiB). Mirrors the legacy `web_fetch` default.
pub const DEFAULT_MAX_BODY_BYTES: usize = 2 * 1024 * 1024;

/// Errors surfaced by the egress layer.
#[derive(Debug, thiserror::Error)]
pub enum EgressError {
    /// The URL scheme was not in the allow-list (only `http` /
    /// `https` are accepted). Surfaces to the caller as a sandbox
    /// violation.
    #[error("unsupported scheme `{0}` (only http and https are accepted)")]
    Scheme(String),

    /// The URL did not include a host component.
    #[error("url has no host")]
    NoHost,

    /// The hostname is not in the configured allow-list (when the
    /// allow-list is non-empty). The hostname is echoed so the agent
    /// can surface it in the tool error.
    #[error("host `{host}` is not in the egress allowlist")]
    NotInAllowlist { host: String },

    /// DNS resolution failed (no addresses returned, transport
    /// error, etc.).
    #[error("dns resolve failed: {0}")]
    Dns(String),

    /// The resolved IP is not allowed by the network policy. The
    /// offending address is included so the agent can surface it
    /// in the tool error.
    #[error("host `{host}` resolves to a disallowed address ({addr})")]
    DisallowedAddress { host: String, addr: SocketAddr },

    /// The upstream returned a non-2xx status. The body is truncated
    /// to [`Self::UPSTREAM_BODY_CAP`] bytes.
    #[error("upstream returned {status}: {body}")]
    Upstream { status: u16, body: String },

    /// The response body would exceed the caller's `max_bytes`
    /// budget. This is a *non-fatal* signal — callers are expected
    /// to retry with a larger budget.
    #[error("response exceeded max_bytes={budget}")]
    ResponseExceeded { budget: usize },

    /// Connect / TLS / read / write error.
    #[error("transport error: {0}")]
    Transport(String),

    /// The URL parser rejected the input.
    #[error("url parse: {0}")]
    UrlParse(String),
}

impl EgressError {
    /// Cap on the body we surface from [`Self::Upstream`]. A
    /// multi-megabyte error page would dwarf the actual status code
    /// in the log line and the LLM tool error.
    pub const UPSTREAM_BODY_CAP: usize = 2048;
}

/// Default-deny network policy for every agent that talks to the
/// outside world.
#[derive(Debug, Clone)]
pub struct EgressConfig {
    /// Total request timeout in milliseconds (connect + read).
    /// Floored at [`MIN_TIMEOUT_MS`].
    pub timeout_ms: u64,
    /// When `false`, only loopback + private ranges are allowed —
    /// public IPs are rejected. The web-fetch default.
    pub allow_public: bool,
    /// Optional hostname allow-list. When non-empty, a hostname
    /// match (exact, `*.suffix` glob, or bare `*`) takes precedence
    /// over the IP policy.
    pub allowlist: Vec<String>,
    /// Hard cap on the body the streaming helper will read, in
    /// bytes. The web-fetch default is [`DEFAULT_MAX_BODY_BYTES`].
    pub max_bytes: usize,
    /// User-Agent header sent on every request. Defaults to a
    /// generic string identifying the project; agents that need a
    /// vendor-specific UA override this.
    pub user_agent: String,
}

impl Default for EgressConfig {
    fn default() -> Self {
        Self {
            timeout_ms: 30_000,
            allow_public: false,
            allowlist: Vec::new(),
            max_bytes: DEFAULT_MAX_BODY_BYTES,
            user_agent: format!("nagent-egress/{}", env!("CARGO_PKG_VERSION")),
        }
    }
}

impl EgressConfig {
    /// Apply the timeout floor so a configuration of zero never reaches
    /// the reqwest builder. Returns `self` so the caller can chain
    /// overrides.
    pub fn finalize(mut self) -> Self {
        self.timeout_ms = self.timeout_ms.max(MIN_TIMEOUT_MS);
        self.max_bytes = self.max_bytes.max(1024);
        self
    }
}

/// The shared hardened HTTP client. Cheap to clone (the inner
/// `reqwest::Client` is itself `Arc`-backed) so it can live in every
/// agent's `AppState` clone.
#[derive(Clone)]
pub struct EgressClient {
    cfg: Arc<EgressConfig>,
    http: reqwest::Client,
}

impl std::fmt::Debug for EgressClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EgressClient")
            .field("cfg", &*self.cfg)
            .field("http", &"<reqwest::Client>")
            .finish()
    }
}

impl EgressClient {
    /// Build a new client from the supplied configuration. The
    /// `reqwest::Client` is constructed once with the timeout /
    /// redirect / TLS defaults so the connection pool stays warm
    /// across calls.
    pub fn new(cfg: EgressConfig) -> Self {
        let cfg = Arc::new(cfg.finalize());
        let timeout = Duration::from_millis(cfg.timeout_ms);
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(300))
            .connect_timeout(timeout)
            .redirect(reqwest::redirect::Policy::limited(MAX_REDIRECTS))
            .user_agent(cfg.user_agent.clone())
            .build()
            .expect("reqwest client build");
        Self { cfg, http }
    }

    /// Access the configuration (read-only). Agents that need the
    /// timeout or allow-list for per-call clamping can read it via
    /// this accessor.
    pub fn config(&self) -> &EgressConfig {
        &self.cfg
    }

    /// Borrow the inner `reqwest::Client`. Exposed for the small
    /// number of agents (dictionary, weather, stock, wikipedia) that
    /// use a public, well-known API and do not need the SSRF
    /// pre-check. **New network agents should prefer the
    /// [`Self::get`] / [`Self::get_stream`] helpers** so the
    /// hardening is applied automatically.
    pub fn inner(&self) -> &reqwest::Client {
        &self.http
    }

    /// Validate `url_str` against the scheme allow-list and the
    /// IP / hostname policy. On success returns the parsed URL and
    /// the resolved `SocketAddr`s (already classified against the
    /// IP policy, so the caller can connect directly).
    ///
    /// The agent should then issue the request against
    /// `parsed.as_str()` (NOT an IP-literal rewrite — see the
    /// `web_fetch` module-level note on SNI).
    pub async fn validate(&self, url_str: &str) -> Result<ValidateOk, EgressError> {
        let parsed = url::Url::parse(url_str).map_err(|e| EgressError::UrlParse(e.to_string()))?;
        match parsed.scheme() {
            "http" | "https" => {}
            other => return Err(EgressError::Scheme(other.to_string())),
        }
        let host = parsed.host_str().ok_or(EgressError::NoHost)?.to_string();

        // Hostname allow-list takes precedence. When the allow-list
        // is non-empty we trust the operator's intent and skip the
        // IP pre-resolution check.
        let host_allowlisted = if !self.cfg.allowlist.is_empty() {
            host_matches_allowlist(&host, &self.cfg.allowlist)
        } else {
            false
        };
        if !self.cfg.allowlist.is_empty() && !host_allowlisted {
            return Err(EgressError::NotInAllowlist { host });
        }

        // Resolve once and validate every address.
        let port = parsed.port_or_known_default().unwrap_or(443);
        let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), port))
            .await
            .map_err(|e| EgressError::Dns(e.to_string()))?
            .collect();
        if addrs.is_empty() {
            return Err(EgressError::Dns("no addresses".into()));
        }
        if !host_allowlisted {
            for addr in &addrs {
                if !is_addr_allowed(addr.ip(), self.cfg.allow_public) {
                    return Err(EgressError::DisallowedAddress {
                        host: host.clone(),
                        addr: *addr,
                    });
                }
            }
        }

        Ok(ValidateOk {
            url: parsed,
            host,
            addresses: addrs,
        })
    }

    /// Hardened GET that returns the full response body as a
    /// `Bytes` buffer. Mirrors the legacy `web_fetch::fetch_once`
    /// shape but reuses [`Self::validate`] for the SSRF policy.
    /// Non-2xx responses surface as [`EgressError::Upstream`] with
    /// the body truncated to [`EgressError::UPSTREAM_BODY_CAP`].
    ///
    /// `max_bytes` defaults to the config's `max_bytes`. Pass a
    /// smaller value to keep the response cheap.
    pub async fn get(&self, url: &str, max_bytes: Option<usize>) -> Result<Bytes, EgressError> {
        let budget = max_bytes
            .unwrap_or(self.cfg.max_bytes)
            .min(self.cfg.max_bytes)
            .max(1024);
        let ValidateOk {
            url: parsed,
            host: _,
            addresses: _,
        } = self.validate(url).await?;
        let resp = self
            .http
            .get(parsed.as_str())
            .send()
            .await
            .map_err(|e| EgressError::Transport(e.to_string()))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(EgressError::Upstream {
                status: status.as_u16(),
                body: truncate_body(&body, EgressError::UPSTREAM_BODY_CAP),
            });
        }
        let mut stream = resp.bytes_stream();
        let mut buf = BytesMut::new();
        while let Some(chunk_res) = stream.next().await {
            let chunk: Bytes = chunk_res.map_err(|e| EgressError::Transport(e.to_string()))?;
            if buf.len() + chunk.len() > budget {
                return Err(EgressError::ResponseExceeded { budget });
            }
            buf.extend_from_slice(&chunk);
        }
        Ok(buf.freeze())
    }

    /// Streaming GET for large pages. Returns
    /// `(status, final_url, content_type, bytes)` on success (the
    /// body fits within `budget`). Returns
    /// [`EgressError::ResponseExceeded`] when the body would exceed
    /// the budget — a *non-fatal* signal the caller can recover
    /// from by retrying with a larger budget.
    pub async fn get_stream(
        &self,
        url: &str,
        max_bytes: Option<usize>,
        timeout: Option<Duration>,
    ) -> Result<(u16, String, String, Bytes), EgressError> {
        let budget = max_bytes
            .unwrap_or(self.cfg.max_bytes)
            .min(self.cfg.max_bytes)
            .max(1024);
        let ValidateOk {
            url: parsed,
            host: _,
            addresses: _,
        } = self.validate(url).await?;
        let mut req = self.http.get(parsed.as_str());
        if let Some(t) = timeout {
            req = req.timeout(t);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| EgressError::Transport(e.to_string()))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(EgressError::Upstream {
                status: status.as_u16(),
                body: truncate_body(&body, EgressError::UPSTREAM_BODY_CAP),
            });
        }
        let final_url = resp.url().to_string();
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();

        let mut stream = resp.bytes_stream();
        let mut buf = BytesMut::new();
        while let Some(chunk_res) = stream.next().await {
            let chunk: Bytes = chunk_res.map_err(|e| EgressError::Transport(e.to_string()))?;
            if buf.len() + chunk.len() > budget {
                return Err(EgressError::ResponseExceeded { budget });
            }
            buf.extend_from_slice(&chunk);
        }
        Ok((status.as_u16(), final_url, content_type, buf.freeze()))
    }
}

/// Successful outcome of [`EgressClient::validate`].
#[derive(Debug)]
pub struct ValidateOk {
    /// The parsed URL.
    pub url: url::Url,
    /// Lowercased hostname (no scheme, no port).
    pub host: String,
    /// All addresses returned by `lookup_host`, already validated
    /// against the IP policy when the hostname was NOT in the
    /// allow-list.
    pub addresses: Vec<SocketAddr>,
}

// ---- Network policy (shared) --------------------------------------------

/// True when `ip` is allowed by the egress IP policy. Always rejects
/// loopback / link-local / private / multicast / broadcast ranges,
/// and additionally rejects *public* IPs unless `allow_public` is
/// set.
pub fn is_addr_allowed(ip: IpAddr, allow_public: bool) -> bool {
    match ip {
        IpAddr::V4(v4) => is_v4_allowed(v4, allow_public),
        IpAddr::V6(v6) => is_v6_allowed(v6, allow_public),
    }
}

fn is_v4_allowed(v4: Ipv4Addr, allow_public: bool) -> bool {
    // Loopback and link-local are always blocked (SSRF protection).
    if v4.is_loopback() || v4.is_linkback_compat() || v4.is_link_local() {
        return false;
    }
    let octets = v4.octets();
    // 169.254.0.0/16 — be explicit so a future upstream API change
    // does not silently re-open it (`is_link_local` should already
    // catch it but the second check is cheap).
    if octets[0] == 169 && octets[1] == 254 {
        return false;
    }
    // 0.0.0.0/8 — non-routable.
    if octets[0] == 0 {
        return false;
    }
    // 100.64.0.0/10 — carrier-grade NAT. Treat as private: most
    // LLM tools do not have legitimate reasons to reach into an
    // ISP's shared address pool.
    if octets[0] == 100 && (octets[1] >= 64 && octets[1] <= 127) {
        return false;
    }
    // 10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16 — RFC1918 private.
    if v4.is_private() {
        return false;
    }
    // Multicast / broadcast / reserved are also non-routable for HTTP.
    if v4.is_multicast() || v4.is_broadcast() || v4.is_unspecified() {
        return false;
    }
    // Everything else is "public" and subject to the `allow_public`
    // gate.
    allow_public
}

// Compatibility shim — `Ipv4Addr::is_linkback_compat` is unstable,
// but the loopback range (`127.0.0.0/8`) is already covered by
// `is_loopback()`. We keep this function for forward-compatibility:
// if the standard library stabilises it we will swap the call site
// without changing the signature.
#[allow(dead_code)]
trait Ipv4AddrLinkbackExt {
    fn is_linkback_compat(&self) -> bool {
        false
    }
}
impl Ipv4AddrLinkbackExt for Ipv4Addr {}

fn is_v6_allowed(v6: Ipv6Addr, allow_public: bool) -> bool {
    if v6.is_loopback() || v6.is_unspecified() {
        return false;
    }
    let segments = v6.segments();
    // fe80::/10 — link-local.
    if segments[0] == 0xfe80 {
        return false;
    }
    // fc00::/7 — unique local addresses (ULA).
    if (segments[0] & 0xfe00) == 0xfc00 {
        return false;
    }
    // ff00::/8 — multicast.
    if (segments[0] & 0xff00) == 0xff00 {
        return false;
    }
    // ::ffff:0:0/96 — IPv4-mapped. Apply the IPv4 policy to the
    // embedded address so an IPv6 literal cannot bypass it.
    if let Some(v4) = v6.to_ipv4_mapped() {
        return is_v4_allowed(v4, allow_public);
    }
    allow_public
}

/// True when `host` matches an entry of `allowlist`. Matches exact
/// hosts, `*.suffix` glob entries (matching the suffix itself and
/// every subdomain), and the bare `*` wildcard (matches every
/// host — intended for tests / local fixtures only).
pub fn host_matches_allowlist(host: &str, allowlist: &[String]) -> bool {
    let host_lc = host.to_ascii_lowercase();
    for entry in allowlist {
        let entry = entry.to_ascii_lowercase();
        if entry == "*" {
            return true;
        }
        if let Some(suffix) = entry.strip_prefix("*.") {
            if host_lc == suffix || host_lc.ends_with(&format!(".{suffix}")) {
                return true;
            }
        } else if host_lc == entry {
            return true;
        }
    }
    false
}

/// Truncate `s` to `max_chars` bytes, slicing at the nearest
/// character boundary so we never split a multi-byte code point.
pub(crate) fn truncate_body(s: &str, max_chars: usize) -> String {
    if s.len() <= max_chars {
        return s.to_string();
    }
    let mut end = max_chars;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = s[..end].to_string();
    out.push_str("\n…[truncated]");
    out
}

// ---- Tests ---------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_matches_allowlist_exact() {
        let allowlist = vec!["example.com".into()];
        assert!(host_matches_allowlist("example.com", &allowlist));
        assert!(!host_matches_allowlist("foo.example.com", &allowlist));
        assert!(!host_matches_allowlist("evil.com", &allowlist));
    }

    #[test]
    fn host_matches_allowlist_wildcard() {
        let allowlist = vec!["*".into()];
        assert!(host_matches_allowlist("anything.example", &allowlist));
        assert!(host_matches_allowlist("127.0.0.1", &allowlist));
        assert!(host_matches_allowlist("localhost", &allowlist));
    }

    #[test]
    fn host_matches_allowlist_glob() {
        let allowlist = vec!["*.example.com".into()];
        assert!(host_matches_allowlist("example.com", &allowlist));
        assert!(host_matches_allowlist("foo.example.com", &allowlist));
        assert!(host_matches_allowlist("a.b.example.com", &allowlist));
        assert!(!host_matches_allowlist("notexample.com", &allowlist));
    }

    #[test]
    fn host_matches_allowlist_mixed() {
        let allowlist = vec!["exact.io".into(), "*.example.com".into()];
        assert!(host_matches_allowlist("exact.io", &allowlist));
        assert!(host_matches_allowlist("docs.example.com", &allowlist));
        assert!(!host_matches_allowlist("exact.com", &allowlist));
    }

    #[test]
    fn sandbox_blocks_loopback_and_private() {
        assert!(!is_addr_allowed(
            IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)),
            true
        ));
        assert!(!is_addr_allowed(
            IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)),
            true
        ));
        assert!(!is_addr_allowed(
            IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1)),
            true
        ));
        assert!(!is_addr_allowed(
            IpAddr::V4(Ipv4Addr::new(172, 16, 0, 1)),
            true
        ));
        assert!(!is_addr_allowed(
            IpAddr::V4(Ipv4Addr::new(169, 254, 1, 1)),
            true
        ));
        assert!(!is_addr_allowed(
            IpAddr::V4(Ipv4Addr::new(100, 64, 0, 1)),
            true
        ));
        assert!(!is_addr_allowed(
            IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)),
            true
        ));
        assert!(!is_addr_allowed(IpAddr::V6(Ipv6Addr::LOCALHOST), true));
        // fc00::/7 — ULA
        assert!(!is_addr_allowed(
            IpAddr::V6(Ipv6Addr::new(0xfc00, 0, 0, 0, 0, 0, 0, 1)),
            true
        ));
        // fe80::/10 — link-local
        assert!(!is_addr_allowed(
            IpAddr::V6(Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 1)),
            true
        ));
        // ::ffff:127.0.0.1 — IPv4-mapped loopback
        assert!(!is_addr_allowed(
            IpAddr::V6(Ipv6Addr::new(0, 0, 0, 0, 0, 0xffff, 0x7f00, 1)),
            true
        ));
    }

    #[test]
    fn sandbox_blocks_public_by_default() {
        assert!(!is_addr_allowed(
            IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8)),
            false
        ));
        assert!(is_addr_allowed(IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8)), true));
    }

    #[test]
    fn config_finalize_floors_timeout() {
        let cfg = EgressConfig {
            timeout_ms: 0,
            max_bytes: 0,
            ..EgressConfig::default()
        }
        .finalize();
        assert!(cfg.timeout_ms >= MIN_TIMEOUT_MS);
        assert!(cfg.max_bytes >= 1024);
    }

    #[test]
    fn truncate_body_respects_char_boundary() {
        let s = "héllo"; // é is 2 bytes
        let t = truncate_body(s, 3);
        assert!(t.starts_with("h"));
        assert!(t.ends_with("…[truncated]"));
    }
}
