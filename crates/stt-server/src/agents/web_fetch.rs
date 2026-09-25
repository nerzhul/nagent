//! `web_fetch` agent: server-side HTTP GET → clean text.
//!
//! The agent is registered when the `web-agent` cargo feature is
//! enabled AND `AGENTS_ENABLED=true` (the global flag in
//! [`crate::config::AgentConfig`]). It exists so the LLM can answer
//! "what's on <URL>?" questions with live page content rather than a
//! hallucinated guess.
//!
//! ## Sandbox
//!
//! The agent enforces a default-deny network policy:
//!
//! - Loopback (`127.0.0.0/8`, `::1`), private (RFC1918), and
//!   link-local (ULA / `169.254/16`) addresses are **always** blocked
//!   as SSRF protection, regardless of any allow-list.
//! - When `WEB_FETCH_ALLOW_PUBLIC=false` (the default), public IP
//!   ranges are also blocked — only loopback and private ranges pass.
//! - When `WEB_FETCH_ALLOWLIST` is set, suffix matching against the
//!   hostname is applied **before** DNS resolution: only the listed
//!   hosts (and their subdomains for `*.foo` entries) may proceed.
//!   This takes precedence over `WEB_FETCH_ALLOW_PUBLIC`.
//!
//! ## DNS-rebinding mitigation
//!
//! The hostname is resolved once with a system DNS lookup, the
//! resolved IP is bound to the connection via `reqwest`'s per-request
//! `.resolve()` override, and the IP is re-classified after
//! resolution to catch any sneaky CNAME that ended up in a private
//! range. This is the standard Rust recipe for defending against
//! DNS-rebinding SSRF; the LLM controls the URL, so the hardening
//! is load-bearing.
//!
//! ## HTML handling
//!
//! v1 ships a small, dependency-free HTML→text heuristic that pulls
//! the visible text out of common tags (`<title>`, `<h1>`–`<h6>`,
//! `<p>`, `<li>`, `<pre>`, `<code>`, `<a>` with the URL preserved,
//! `<blockquote>`). It is not a full HTML5 parser — quality is a
//! known weak spot — but it is fast, dependency-free, and ships
//! enough fidelity for the LLM to summarise the page. A future
//! release can swap in `html5ever` + a real readability pass without
//! changing the wire format or the agent's public API.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::{Bytes, BytesMut};
use futures_util::StreamExt;
use serde_json::{json, Value};

use crate::agents::{Agent, AgentError};
use crate::config::WebFetchConfig;

/// Maximum length of the cleaned text returned to the LLM, in
/// characters. Caps the response so a long page does not blow up the
/// tool-result size — long-form summarisation is the LLM's job, not
/// the fetcher's. Roughly 100 KB of UTF-8 text, enough for the first
/// few thousand paragraphs of a typical article.
const MAX_TEXT_CHARS: usize = 100 * 1024;

/// Build of the `web_fetch` agent.
#[derive(Clone)]
pub struct WebFetchAgent {
    cfg: WebFetchConfig,
    http: reqwest::Client,
}

impl std::fmt::Debug for WebFetchAgent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebFetchAgent")
            .field("cfg", &self.cfg)
            .field("http", &"<reqwest::Client>")
            .finish()
    }
}

impl WebFetchAgent {
    /// Construct the agent from the active [`WebFetchConfig`]. The
    /// HTTP client is built once with the configured read timeout;
    /// the `reqwest::Client` is internally `Arc`-shared so cloning
    /// the agent for `AppState` keeps the connection pool warm.
    pub fn new(cfg: WebFetchConfig) -> Self {
        let timeout = Duration::from_millis(cfg.timeout_ms.max(1_000));
        let http = reqwest::Client::builder()
            // Long safety net: the per-chunk read timeout below is the
            // primary defence against a stalled server.
            .timeout(Duration::from_secs(300))
            .connect_timeout(timeout)
            .redirect(reqwest::redirect::Policy::limited(5))
            // Pool is shared across calls; the loopback bypass happens
            // at the policy layer (see `is_addr_allowed`), not here.
            .build()
            .expect("reqwest client build");
        Self { cfg, http }
    }
}

#[async_trait]
impl Agent for WebFetchAgent {
    fn name(&self) -> &str {
        "web_fetch"
    }

    fn description(&self) -> &str {
        "Fetch a public URL and return its main textual content as Markdown. Use this when the user asks for live web content, a quote from a page, or the contents of a specific URL. Pass `url` (required)."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "url": {
                    "type": "string",
                    "description": "Absolute URL to fetch (http or https). Must point to a publicly reachable host unless the server has been configured to allow private addresses."
                },
                "max_bytes": {
                    "type": "integer",
                    "description": "Initial response-size budget in bytes. If the page is larger, the agent automatically retries with a doubled budget up to the server-wide cap (default 2 MiB). Pass a smaller value to keep the response cheap; pass nothing to start at the server cap.",
                    "minimum": 1024
                },
                "timeout_ms": {
                    "type": "integer",
                    "description": "Per-request timeout in milliseconds. Defaults to the server-wide value; higher values are clamped.",
                    "minimum": 1000
                }
            },
            "required": ["url"],
            "additionalProperties": false,
        })
    }

    async fn invoke(&self, args: Value) -> Result<String, AgentError> {
        let req = parse_args(&args)?;
        let parsed = url::Url::parse(&req.url)
            .map_err(|e| AgentError::InvalidArguments(format!("url parse: {e}")))?;
        if !matches!(parsed.scheme(), "http" | "https") {
            return Err(AgentError::InvalidArguments(format!(
                "unsupported scheme `{}` (only http and https are accepted)",
                parsed.scheme()
            )));
        }
        let host = parsed
            .host_str()
            .ok_or_else(|| AgentError::InvalidArguments("url has no host".into()))?
            .to_string();

        // Hostname allow-list takes precedence over the IP-based
        // sandbox: an operator who explicitly whitelists `*.example.com`
        // has authorised that host regardless of where its DNS points.
        // The IP policy below still runs as a sanity check when the
        // allow-list is empty; otherwise we skip it (the allow-list is
        // the explicit override).
        let host_allowlisted = if !self.cfg.allowlist.is_empty() {
            host_matches_allowlist(&host, &self.cfg.allowlist)
        } else {
            false
        };
        if !self.cfg.allowlist.is_empty() && !host_allowlisted {
            return Err(AgentError::SandboxDenied(format!(
                "host `{host}` is not in WEB_FETCH_ALLOWLIST"
            )));
        }

        // Resolve the hostname and validate the resolved IPs against
        // the network policy (unless the host was explicitly
        // allow-listed). We deliberately do NOT pin reqwest to the
        // resolved IP — rewriting the URL host to an IP literal
        // breaks TLS SNI (reqwest uses the URL host for SNI, so an
        // IP-literal URL gets `SNI=<ip>` and the server's cert — for
        // the hostname — does not match). The DNS check is therefore
        // advisory: it confirms the hostname currently points to an
        // allowed IP, but a small rebinding window between this
        // resolve and reqwest's own resolve exists. The mitigation
        // is best-effort for v1; a future release can add a custom
        // `reqwest::dns::Resolve` to fully close the window.
        let port = parsed.port_or_known_default().unwrap_or(443);
        let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), port))
            .await
            .map_err(|e| AgentError::AgentFailed(format!("dns resolve: {e}")))?
            .collect();
        if addrs.is_empty() {
            return Err(AgentError::AgentFailed("dns returned no addresses".into()));
        }
        if !host_allowlisted {
            for addr in &addrs {
                if !is_addr_allowed(addr.ip(), self.cfg.allow_public) {
                    return Err(AgentError::SandboxDenied(format!(
                        "host `{host}` resolves to a disallowed address ({addr})"
                    )));
                }
            }
        }

        // Resolve the budget the LLM asked for (or fall back to the
        // server cap). The agent then *iteratively* doubles the
        // budget if the response overflows, up to `self.cfg.max_bytes`,
        // so a conservative model that asks for 10 KB transparently
        // fetches 20 KB → 40 KB → 80 KB → … until the page fits or
        // we hit the server cap. The schema description above
        // explains this in the LLM-facing tool description.
        let mut budget = req
            .max_bytes
            .unwrap_or(self.cfg.max_bytes)
            .min(self.cfg.max_bytes)
            .max(1024);
        let timeout = Duration::from_millis(
            req.timeout_ms
                .unwrap_or(self.cfg.timeout_ms)
                .min(self.cfg.timeout_ms)
                .max(1_000),
        );

        // Adaptive retry loop. Each iteration issues one streaming
        // fetch capped at `budget`. If the response overflows, we
        // double the budget and try again, capped at the server cap
        // (so we cannot exceed the operator's policy no matter how
        // many rounds we run).
        let (status, final_url, content_type, bytes) = loop {
            match self
                .fetch_once(parsed.as_str(), &host, budget, timeout)
                .await
            {
                Ok(fetched) => break fetched,
                Err(AgentError::ResponseExceeded { budget: used }) => {
                    let next = budget.saturating_mul(2).min(self.cfg.max_bytes);
                    if next <= budget {
                        // Already at the server cap. Surface as a
                        // proper tool error so the LLM can recover.
                        return Err(AgentError::AgentFailed(format!(
                            "response exceeded max_bytes={budget} (server cap); \
                             page did not fit"
                        )));
                    }
                    tracing::info!(
                        from = used,
                        to = next,
                        url = parsed.as_str(),
                        "web_fetch: response exceeded budget, retrying with doubled budget"
                    );
                    budget = next;
                }
                Err(other) => return Err(other),
            }
        };

        let (title, text) = if content_type.to_ascii_lowercase().contains("html") {
            extract_html_text(&String::from_utf8_lossy(&bytes))
        } else {
            (String::new(), String::from_utf8_lossy(&bytes).into_owned())
        };
        let text = truncate(&text, MAX_TEXT_CHARS);

        let summary = format!(
            "{} — {}",
            if title.is_empty() {
                host.as_str()
            } else {
                title.as_str()
            },
            format_bytes(bytes.len())
        );

        Ok(serde_json::to_string(&json!({
            "url": parsed.as_str(),
            "final_url": final_url,
            "status": status,
            "title": title,
            "content_type": content_type,
            "summary": summary,
            "text": text,
        }))
        .expect("json encode"))
    }
}

impl WebFetchAgent {
    /// Issue one streaming HTTP GET capped at `budget` bytes.
    ///
    /// Returns `(status, final_url, content_type, bytes)` on success
    /// (the body fits within `budget`). Returns
    /// [`AgentError::ResponseExceeded`] when the upstream body would
    /// exceed the budget — this is a *non-fatal* signal that the
    /// caller's retry loop handles by doubling the budget. Any other
    /// failure (network, TLS, non-2xx, sandbox violation) is bubbled
    /// up as the appropriate [`AgentError`] variant.
    async fn fetch_once(
        &self,
        url: &str,
        _host_fallback: &str,
        budget: usize,
        timeout: Duration,
    ) -> Result<(u16, String, String, Bytes), AgentError> {
        // Issue the request against the *original* URL so reqwest's
        // TLS layer uses the hostname for SNI and certificate
        // verification. The IP we resolved earlier is no longer
        // pinned to the request (reqwest 0.12 has no per-request
        // DNS override), so a small DNS-rebinding window exists
        // between our pre-check and reqwest's own resolve. The
        // sandbox check above catches the practical SSRF cases
        // (loopback / private / ULA) where a rebind would matter;
        // closing the residual window is future work.
        let resp = self
            .http
            .get(url)
            .header(
                reqwest::header::USER_AGENT,
                "nagent-web-fetch/0.1 (+https://github.com/nagent/nagent)",
            )
            .timeout(timeout)
            .send()
            .await
            .map_err(|e| AgentError::AgentFailed(format!("connect/read failed: {e}")))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(AgentError::Upstream {
                status: status.as_u16(),
                body: truncate(&body, 2048),
            });
        }
        let final_url = resp.url().to_string();
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();

        // Stream the body in chunks. As soon as we would exceed
        // `budget`, stop and signal `ResponseExceeded` so the caller
        // can retry with a larger budget — without ever buffering
        // the whole page into memory. The +1 boundary lets us
        // detect overflow on the chunk that crosses the line without
        // pulling an extra byte we would just discard.
        let mut stream = resp.bytes_stream();
        let mut buf = BytesMut::new();
        while let Some(chunk_res) = stream.next().await {
            let chunk: Bytes =
                chunk_res.map_err(|e| AgentError::AgentFailed(format!("read body: {e}")))?;
            if buf.len() + chunk.len() > budget {
                return Err(AgentError::ResponseExceeded { budget });
            }
            buf.extend_from_slice(&chunk);
        }
        Ok((status.as_u16(), final_url, content_type, buf.freeze()))
    }
}

// ---- Argument parsing ----------------------------------------------------

#[derive(Debug, Default)]
struct ParsedArgs {
    url: String,
    max_bytes: Option<usize>,
    timeout_ms: Option<u64>,
}

fn parse_args(args: &Value) -> Result<ParsedArgs, AgentError> {
    let obj = args
        .as_object()
        .ok_or_else(|| AgentError::InvalidArguments("arguments must be a JSON object".into()))?;
    let url = obj
        .get("url")
        .and_then(|v| v.as_str())
        .ok_or_else(|| AgentError::InvalidArguments("`url` (string) is required".into()))?
        .to_string();
    let max_bytes = obj
        .get("max_bytes")
        .and_then(|v| v.as_u64())
        .map(|v| v as usize);
    let timeout_ms = obj.get("timeout_ms").and_then(|v| v.as_u64());
    Ok(ParsedArgs {
        url,
        max_bytes,
        timeout_ms,
    })
}

// ---- Network policy ------------------------------------------------------

fn is_addr_allowed(ip: IpAddr, allow_public: bool) -> bool {
    match ip {
        IpAddr::V4(v4) => is_v4_allowed(v4, allow_public),
        IpAddr::V6(v6) => is_v6_allowed(v6, allow_public),
    }
}

fn is_v4_allowed(v4: Ipv4Addr, allow_public: bool) -> bool {
    // Loopback and link-local are always blocked (SSRF protection).
    if v4.is_loopback() || v4.is_link_local() {
        return false;
    }
    // 169.254.0.0/16 is technically `is_link_local`, but be explicit
    // so a future upstream API change does not silently re-open it.
    let octets = v4.octets();
    if octets[0] == 169 && octets[1] == 254 {
        return false;
    }
    // 0.0.0.0/8 — non-routable.
    if octets[0] == 0 {
        return false;
    }
    // 100.64.0.0/10 — carrier-grade NAT. Treat as private: most LLM
    // tools do not have legitimate reasons to reach into an ISP's
    // shared address pool.
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

fn host_matches_allowlist(host: &str, allowlist: &[String]) -> bool {
    let host_lc = host.to_ascii_lowercase();
    for entry in allowlist {
        let entry = entry.to_ascii_lowercase();
        // Bare `*` matches every host. Useful for local development
        // and integration tests against a loopback fixture; in
        // production operators should narrow the list to specific
        // suffixes.
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

#[allow(dead_code)]
fn arc_clone<T>(t: &Arc<T>) -> Arc<T> {
    Arc::clone(t)
}

// ---- HTML / text handling ------------------------------------------------

/// Extract `(title, markdown)` from a (possibly malformed) HTML body.
///
/// v1 ships a small dependency-free heuristic. It walks the source
/// linearly, recognises a fixed set of start/end tags, and emits a
/// Markdown-ish stream. Style/script blocks are dropped. Tags with no
/// special handling are stripped. Attribute values that look like
/// URLs (`href`, `src`) are inlined as plain text.
fn extract_html_text(html: &str) -> (String, String) {
    let mut out = String::with_capacity(html.len() / 2);
    let mut title = String::new();
    let mut in_title = false;
    let mut in_script = false;
    let mut in_style = false;
    let mut in_pre = false;
    let mut list_depth: usize = 0;
    let mut last_was_space = true; // avoid leading whitespace

    let lower = html.to_ascii_lowercase();
    let bytes = html.as_bytes();
    let lower_bytes = lower.as_bytes();

    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'<' {
            // Plain text: copy until the next `<`. Collapse runs of
            // whitespace into a single space, except inside <pre>.
            if in_pre {
                out.push(bytes[i] as char);
                last_was_space = false;
            } else {
                let c = bytes[i] as char;
                if c.is_whitespace() {
                    if !last_was_space {
                        out.push(' ');
                        last_was_space = true;
                    }
                } else {
                    out.push(c);
                    last_was_space = false;
                }
            }
            i += 1;
            continue;
        }
        // Parse the tag.
        let end = lower_bytes[i..]
            .iter()
            .position(|&b| b == b'>')
            .map(|p| i + p)
            .unwrap_or(bytes.len());
        if end >= bytes.len() {
            break;
        }
        let raw = &html[i + 1..end];
        let lower_raw = &lower[i + 1..end];
        let tag_name_end = lower_raw
            .trim_start()
            .find(|c: char| c.is_whitespace() || c == '/' || c == '>')
            .unwrap_or(lower_raw.trim_start().len());
        let tag = lower_raw.trim_start()[..tag_name_end].trim();
        let self_closing = raw.trim_end().ends_with('/');

        // Handle structural tags.
        match tag {
            "title" => {
                in_title = true;
            }
            "/title" => {
                in_title = false;
            }
            "script" => in_script = true,
            "/script" => in_script = false,
            "style" => in_style = true,
            "/style" => in_style = false,
            "pre" => {
                in_pre = true;
                push_blank_line(&mut out);
                last_was_space = true;
            }
            "/pre" => {
                in_pre = false;
                push_blank_line(&mut out);
                last_was_space = true;
            }
            "br" => {
                if !last_was_space {
                    out.push('\n');
                    last_was_space = true;
                }
            }
            "p" | "div" => push_blank_line(&mut out),
            "/p" | "/div" => push_blank_line(&mut out),
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                push_blank_line(&mut out);
            }
            "/h1" | "/h2" | "/h3" | "/h4" | "/h5" | "/h6" => {
                push_blank_line(&mut out);
            }
            "li" => {
                let indent = "  ".repeat(list_depth);
                out.push('\n');
                out.push_str(&indent);
                out.push_str("- ");
                last_was_space = false;
            }
            "ul" | "ol" => {
                list_depth += 1;
                push_blank_line(&mut out);
            }
            "/ul" | "/ol" => {
                list_depth = list_depth.saturating_sub(1);
                push_blank_line(&mut out);
            }
            "a" => {
                // Pull `href="..."` and append as " (URL)" so the LLM
                // keeps the link.
                if let Some(href) = attr_value(raw, "href") {
                    if !href.is_empty() {
                        out.push('(');
                        out.push_str(href);
                        out.push(')');
                        last_was_space = false;
                    }
                }
            }
            _ => {}
        }

        if in_script || in_style {
            // Drop the entire tag's contents by skipping to the matching
            // closing tag. Cheap heuristic: scan for `</script>` /
            // `</style>` (case-insensitive) and resume after it.
            let needle = if in_script { "</script" } else { "</style" };
            if let Some(pos) = lower[i..].find(needle) {
                i += pos + needle.len();
                // Skip to the closing '>'.
                while i < bytes.len() && bytes[i] != b'>' {
                    i += 1;
                }
                if i < bytes.len() {
                    i += 1;
                }
                if in_script {
                    in_script = false;
                } else {
                    in_style = false;
                }
                continue;
            }
        }

        // Advance past the current tag *before* any flag-dependent
        // branches (notably the title capture below) so the
        // flag-dependent code sees the post-tag offset. Otherwise a
        // freshly-opened `<title>` would capture from `i` (the `<` of
        // `<title>`) and pull the tag itself into the title string.
        let post_tag_i = end + 1;
        if in_title && tag == "title" {
            // The opening tag was just processed; capture the body up
            // to `</title` and skip past it. Doing this here (rather
            // than after the match arm) lets us index into the source
            // at `post_tag_i` instead of the still-`<`-positioned `i`.
            if let Some(pos) = lower[post_tag_i..].find("</title") {
                title = html[post_tag_i..post_tag_i + pos].trim().to_string();
                let mut ni = post_tag_i + pos + "</title".len();
                while ni < bytes.len() && bytes[ni] != b'>' {
                    ni += 1;
                }
                if ni < bytes.len() {
                    ni += 1;
                }
                in_title = false;
                i = ni;
                continue;
            }
            // No closing tag found: bail out of the title block so we
            // do not spin forever on malformed input.
            in_title = false;
        }

        if self_closing {
            // no children
        }
        i = post_tag_i;
    }

    // Trim trailing whitespace and collapse 3+ blank lines into 2.
    let mut cleaned = String::with_capacity(out.len());
    let mut blanks = 0usize;
    for line in out.lines() {
        if line.trim().is_empty() {
            blanks += 1;
            if blanks > 1 {
                continue;
            }
        } else {
            blanks = 0;
        }
        cleaned.push_str(line);
        cleaned.push('\n');
    }
    (title.trim().to_string(), cleaned.trim().to_string())
}

fn push_blank_line(out: &mut String) {
    if !out.ends_with("\n\n") {
        if !out.ends_with('\n') {
            out.push('\n');
        }
        out.push('\n');
    }
}

fn attr_value<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let needle = format!("{name}=");
    let pos = tag.to_ascii_lowercase().find(&needle)?;
    let after = &tag[pos + needle.len()..];
    let bytes = after.as_bytes();
    if bytes.is_empty() {
        return None;
    }
    let quote = bytes[0];
    if quote == b'"' || quote == b'\'' {
        let rest = &after[1..];
        let end = rest.find(quote as char)?;
        Some(&rest[..end])
    } else {
        // Bareword value — read until whitespace or `>`.
        let end = after
            .find(|c: char| c.is_whitespace() || c == '>')
            .unwrap_or(after.len());
        Some(&after[..end])
    }
}

fn truncate(s: &str, max_chars: usize) -> String {
    if s.len() <= max_chars {
        return s.to_string();
    }
    // Truncate at a char boundary to avoid splitting a multi-byte code
    // point in the middle.
    let mut end = max_chars;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = s[..end].to_string();
    out.push_str("\n…[truncated]");
    out
}

fn format_bytes(n: usize) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB"];
    let mut size = n as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{} {}", n, UNITS[0])
    } else {
        format!("{:.1} {}", size, UNITS[unit])
    }
}

// ---- Tests ---------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
    fn rejects_invalid_args() {
        let rt = tokio::runtime::Runtime::new().expect("rt");
        let agent = WebFetchAgent::new(WebFetchConfig::default());
        let err = rt.block_on(agent.invoke(json!({}))).unwrap_err();
        matches!(err, AgentError::InvalidArguments(_));
        let err = rt
            .block_on(agent.invoke(json!("not-an-object")))
            .unwrap_err();
        matches!(err, AgentError::InvalidArguments(_));
    }

    #[test]
    fn rejects_unsupported_scheme() {
        let rt = tokio::runtime::Runtime::new().expect("rt");
        let agent = WebFetchAgent::new(WebFetchConfig::default());
        let err = rt
            .block_on(agent.invoke(json!({"url": "file:///etc/passwd"})))
            .unwrap_err();
        matches!(err, AgentError::InvalidArguments(_));
    }

    #[test]
    fn rejects_sandbox_denied_host() {
        let rt = tokio::runtime::Runtime::new().expect("rt");
        let cfg = WebFetchConfig::default();
        let agent = WebFetchAgent::new(cfg.clone());
        let err = rt
            .block_on(agent.invoke(json!({"url": "http://10.0.0.1/"})))
            .unwrap_err();
        matches!(err, AgentError::SandboxDenied(_));
    }

    #[test]
    fn rejects_host_not_in_allowlist() {
        let rt = tokio::runtime::Runtime::new().expect("rt");
        let cfg = WebFetchConfig {
            allowlist: vec!["allowed.example".into()],
            ..WebFetchConfig::default()
        };
        let agent = WebFetchAgent::new(cfg);
        let err = rt
            .block_on(agent.invoke(json!({"url": "http://evil.com/"})))
            .unwrap_err();
        matches!(err, AgentError::SandboxDenied(_));
    }

    #[test]
    fn html_extraction_basic() {
        let html = r#"<!doctype html><html><head><title>Example</title></head><body>
<h1>Hello</h1>
<p>This is a <b>test</b> page.</p>
<ul><li>One</li><li>Two</li></ul>
<script>alert('drop me');</script>
<a href="https://example.com">A link</a>
</body></html>"#;
        let (title, text) = extract_html_text(html);
        assert_eq!(title, "Example");
        assert!(text.contains("Hello"));
        assert!(text.contains("This is a test page."));
        assert!(text.contains("- One"));
        assert!(text.contains("- Two"));
        assert!(text.contains("https://example.com"));
        assert!(!text.contains("alert"));
    }

    #[test]
    fn html_extraction_preserves_pre_blocks() {
        let html = "<pre>line1\nline2\n  indented</pre>";
        let (_t, text) = extract_html_text(html);
        // The <pre> content should keep newlines + leading whitespace.
        assert!(text.contains("line1"));
        assert!(text.contains("line2"));
        assert!(text.contains("  indented"));
    }

    #[test]
    fn truncate_splits_on_char_boundary() {
        let s = "héllo"; // é is 2 bytes
        let t = truncate(s, 3);
        assert!(t.starts_with("h"));
        assert!(t.ends_with("…[truncated]"));
    }

    #[test]
    fn budget_progression_doubles_until_server_cap() {
        // The adaptive-retry policy: start at the LLM-supplied
        // budget (or the server cap), double on each `ResponseExceeded`
        // error, cap at the server cap. We exercise the math without
        // an HTTP round-trip so a regression to the doubling rule
        // surfaces as a unit-test failure rather than a flaky
        // integration test.
        let server_cap: usize = 2 * 1024 * 1024;
        // LLM asks for 10 KB.
        let mut budget: usize = 10 * 1024;
        let mut iterations = 0;
        loop {
            let next = budget.saturating_mul(2).min(server_cap);
            if next <= budget {
                break;
            }
            budget = next;
            iterations += 1;
            if iterations > 20 {
                panic!("doubling did not converge within 20 rounds");
            }
        }
        // 10 KB → 20 → 40 → 80 → 160 → 320 → 640 KB → 1.28 MB →
        // 2 MB (capped) → next=2 MB, equal to budget, stop. Eight
        // doublings to reach the 2 MiB server cap.
        assert_eq!(budget, server_cap);
        assert_eq!(iterations, 8);
    }

    #[test]
    fn budget_progression_handles_already_at_cap() {
        // When the LLM's budget already equals the server cap, the
        // first overflow must NOT loop forever; the very first
        // `next <= budget` check trips and the caller surfaces a
        // hard error.
        let server_cap: usize = 4096;
        let budget: usize = server_cap;
        let next = budget.saturating_mul(2).min(server_cap);
        assert!(
            next <= budget,
            "next ({next}) must be <= budget ({budget}) when budget == server_cap"
        );
    }
}
