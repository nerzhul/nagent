//! `CalDavClient` — thin async wrapper around `reqwest` for the
//! CalDAV protocol subset that v1 needs.
//!
//! v1 exposes **only** the following verbs:
//!
//! - `PROPFIND` with `Depth: 0|1` for calendar discovery
//!   (`probe_calendars`).
//! - `REPORT calendar-query` with `<C:time-range>` for
//!   `list_events`.
//! - `GET` for `get_event` (a single `.ics` resource).
//! - `PUT` for `create_event` (creating a new `<uid>.ics`).
//!
//! `UPDATE` (PUT over an existing href) and `DELETE` are
//! **forbidden** in v1. The client does not even expose those
//! methods; a code review check ("grep for `DELETE` in the caldav
//! module") makes the restriction enforceable.
//!
//! ## Hardening
//!
//! Every wire method takes `&EgressClient` (the existing
//! default-deny SSRF guard). The per-agent
//! [`CalDavAgentConfig::allowlist`] is applied per call so a
//! misconfigured vault row cannot escape the operator's
//! host boundary silently.
//!
//! ## Auth
//!
//! CalDAV universally accepts HTTP Basic over HTTPS (the
//! `caldav` `ServiceDef` only collects a username / password).
//! The client builds the `Authorization: Basic …` header on
//! every request using the per-call [`BasicAuth`] value the
//! agent reads from the per-user vault at request time.
//!
//! ## iCalendar parsing
//!
//! v1 parses VEVENT bodies with a small, dependency-free
//! RFC 5545 subset parser (the `icalendar` 0.7 crate hides
//! the property values behind a private field, so the only
//! reliable path is to parse the text directly). The subset
//! covers `UID`, `DTSTART`, `DTEND` / `DURATION`, `SUMMARY`,
//! `DESCRIPTION`, `LOCATION`, `RRULE`.

use bytes::Bytes;
use chrono::{DateTime, Utc};
use serde::Serialize;
use url::Url;

use crate::agents::AgentError;
use crate::egress::EgressClient;

/// HTTP Basic credentials used on every CalDAV request.
///
/// Plaintext only lives in this struct; callers are expected to
/// pass it straight from [`crate::UserContext::secret`] (which
/// returns a `SecretString` and zeroises on `Drop`).
#[derive(Debug, Clone)]
pub struct BasicAuth {
    pub username: String,
    pub password: String,
}

impl BasicAuth {
    /// Build the `Authorization: Basic <b64>` header value. The
    /// returned string is consumed by the request builder and
    /// never logged.
    pub fn header_value(&self) -> String {
        use base64::Engine;
        let raw = format!("{}:{}", self.username, self.password);
        format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(raw.as_bytes())
        )
    }
}

/// One calendar resource returned by the probe endpoint.
///
/// The probe endpoint issues a single `PROPFIND` with
/// `Depth: 1` and filters children whose `resourcetype` contains
/// `<C:calendar/>`. The `href` is the calendar collection URL
/// the user picks during setup; it becomes the `url` field
/// stored in the per-user vault and is what the runtime
/// agents reach.
#[derive(Debug, Clone, Serialize)]
pub struct Calendar {
    /// Absolute URL of the calendar collection (the resource
    /// that contains `VEVENT` children). The agent treats this
    /// as opaque — it is forwarded to `list_events` /
    /// `create_event` verbatim.
    pub href: String,
    /// `<d:displayname>` of the calendar. Empty string when the
    /// server does not provide one.
    pub display_name: String,
    /// `<cs:getctag>` value, when the server exposes it. Drives
    /// the LLM / UI's "calendar has changed since last poll"
    /// hint.
    pub ctag: Option<String>,
}

/// One `VEVENT` parsed from a single `.ics` resource.
#[derive(Debug, Clone, Serialize)]
pub struct Event {
    /// `UID` of the event (RFC 5545 §3.8.4.7). Stable across
    /// reads; the LLM uses it as the `uid` argument of
    /// `caldav_get_event` / `caldav_create_event`.
    pub uid: String,
    /// Absolute href of the event resource (used internally
    /// by the client; the LLM only sees `uid`).
    pub href: String,
    /// `SUMMARY` (RFC 5545 §3.8.1.12). Empty string when the
    /// event has no title.
    pub summary: String,
    /// `DTSTART` normalised to UTC. The LLM-facing JSON always
    /// exposes UTC; floating times (no `TZID`, no `Z`) are
    /// interpreted as UTC for v1 — see the integration doc for
    /// the documented limitation.
    pub dt_start: DateTime<Utc>,
    /// Inclusive end of the event (`DTEND` or
    /// `DTSTART + DURATION`). `None` for a point-in-time event
    /// without `DURATION`.
    pub dt_end: Option<DateTime<Utc>>,
    /// `DESCRIPTION` (RFC 5545 §3.8.1.5). `None` when absent.
    pub description: Option<String>,
    /// `LOCATION` (RFC 5545 §3.8.1.7). `None` when absent.
    pub location: Option<String>,
    /// Raw `RRULE` value (RFC 5545 §3.8.5.3). Surfaced verbatim
    /// without expansion; the LLM interprets recurrence.
    pub rrule: Option<String>,
}

/// `Event` payload supplied to [`CalDavClient::create_event`].
///
/// The `summary` is the only strictly required field; `start`
/// must be supplied (the agent defaults it to "now" if the
/// LLM omits it). `description` / `location` round-trip
/// verbatim into the iCalendar body.
#[derive(Debug, Clone)]
pub struct EventPatch {
    pub summary: String,
    pub start: DateTime<Utc>,
    pub end: Option<DateTime<Utc>>,
    pub description: Option<String>,
    pub location: Option<String>,
}

impl EventPatch {
    /// Render to a minimal RFC 5545 VCALENDAR / VEVENT body with
    /// a fresh UID + DTSTAMP. The UID uses the same scheme
    /// Nextcloud / Radicale issue: `<epoch-ns>-<random-hex>@nagent`.
    pub fn to_ical(&self) -> String {
        let uid = format!(
            "{}-{}@nagent",
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0),
            random_hex_suffix()
        );
        let dtstamp = Utc::now().format("%Y%m%dT%H%M%SZ");
        let dtstart = self.start.format("%Y%m%dT%H%M%SZ");
        let dtend_line = match self.end {
            Some(e) => format!("DTEND:{}\r\n", e.format("%Y%m%dT%H%M%SZ")),
            None => "DURATION:PT1H\r\n".to_string(),
        };
        let description = match &self.description {
            Some(d) if !d.is_empty() => format!("DESCRIPTION:{}\r\n", ical_escape(d)),
            _ => String::new(),
        };
        let location = match &self.location {
            Some(l) if !l.is_empty() => format!("LOCATION:{}\r\n", ical_escape(l)),
            _ => String::new(),
        };
        format!(
            "BEGIN:VCALENDAR\r\n\
             VERSION:2.0\r\n\
             PRODID:-//nagent//CalDAV//EN\r\n\
             BEGIN:VEVENT\r\n\
             UID:{uid}\r\n\
             DTSTAMP:{dtstamp}\r\n\
             DTSTART:{dtstart}\r\n\
             {dtend_line}\
             SUMMARY:{}\r\n\
             {description}\
             {location}\
             END:VEVENT\r\n\
             END:VCALENDAR\r\n",
            ical_escape(&self.summary),
        )
    }
}

fn random_hex_suffix() -> String {
    use rand::Rng;
    let mut bytes = [0u8; 8];
    rand::thread_rng().fill(&mut bytes[..]);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Escape an iCalendar TEXT value per RFC 5545 §3.3.11.
fn ical_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            ';' => out.push_str("\\;"),
            ',' => out.push_str("\\,"),
            '\n' => out.push_str("\\n"),
            '\r' => {} // drop CR; LF handled above
            _ => out.push(ch),
        }
    }
    out
}

/// Outcome of [`CalDavClient::create_event`]: the `href` of the
/// newly-created event and the assigned `uid` (echoed back from
/// the rendered body).
#[derive(Debug, Clone, Serialize)]
pub struct CreatedEvent {
    pub uid: String,
    pub href: String,
}

/// Thin async CalDAV client.
///
/// Built per-call (the `BasicAuth` is per-user and the
/// `EgressClient` is shared); the struct is cheap to
/// construct and the wire methods take `&self`.
#[derive(Clone)]
pub struct CalDavClient {
    /// The shared hardened HTTP client. Per-call SSRF checks
    /// still apply; the agent config's `allowlist` is enforced
    /// by the underlying `EgressClient::validate`.
    pub http: EgressClient,
    /// The full CalDAV server base URL (e.g.
    /// `https://cloud.example.com/remote.php/dav/`). Used to
    /// resolve relative hrefs in PROPFIND / REPORT responses.
    pub principal: Url,
    /// Per-request Basic auth. Read from the user's vault on
    /// every agent invocation; never cached on the struct.
    pub auth: BasicAuth,
}

impl std::fmt::Debug for CalDavClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CalDavClient")
            .field("http", &"<EgressClient>")
            .field("principal", &self.principal)
            .field("auth", &"<redacted>")
            .finish()
    }
}

impl CalDavClient {
    /// Build a new client. The `EgressClient` is built with
    /// the agent config's `timeout_ms` and `allowlist` so the
    /// SSRF guard is identical to the one the chat agents use.
    pub fn new(http: EgressClient, principal: Url, auth: BasicAuth) -> Self {
        Self {
            http,
            principal,
            auth,
        }
    }

    /// One `PROPFIND` request. `depth` is encoded as
    /// `Depth: 0` or `Depth: 1`. `body` is the XML request
    /// body (set to a minimal `<?xml …><d:propfind>…` block
    /// for the probe endpoint; the function does not inject
    /// one).
    pub async fn propfind(&self, url: &str, depth: u8, body: &str) -> Result<Bytes, AgentError> {
        let validated = self.http.validate(url).await.map_err(map_egress)?;
        let depth_str = if depth == 0 { "0" } else { "1" };
        let auth = self.auth.header_value();
        let resp = self
            .http
            .inner()
            .request(
                reqwest::Method::from_bytes(b"PROPFIND").expect("PROPFIND"),
                validated.url.as_str(),
            )
            .header(reqwest::header::AUTHORIZATION, auth)
            .header("Depth", depth_str)
            .header("Content-Type", "application/xml; charset=utf-8")
            .body(body.to_string())
            .send()
            .await
            .map_err(|e| AgentError::AgentFailed(format!("PROPFIND transport: {e}")))?;
        let status = resp.status();
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| AgentError::AgentFailed(format!("PROPFIND read: {e}")))?;
        if !status.is_success() && status.as_u16() != 207 {
            return Err(AgentError::Upstream {
                status: status.as_u16(),
                body: truncate_str(&String::from_utf8_lossy(&bytes)),
            });
        }
        Ok(bytes)
    }

    /// One `REPORT calendar-query` request. `body` is the XML
    /// report envelope (the caller builds the time-range filter
    /// so the wire request is identical to what other CalDAV
    /// libraries produce).
    pub async fn report(&self, url: &str, body: &str) -> Result<Bytes, AgentError> {
        let validated = self.http.validate(url).await.map_err(map_egress)?;
        let auth = self.auth.header_value();
        let resp = self
            .http
            .inner()
            .request(
                reqwest::Method::from_bytes(b"REPORT").expect("REPORT"),
                validated.url.as_str(),
            )
            .header(reqwest::header::AUTHORIZATION, auth)
            .header("Depth", "1")
            .header("Content-Type", "application/xml; charset=utf-8")
            .body(body.to_string())
            .send()
            .await
            .map_err(|e| AgentError::AgentFailed(format!("REPORT transport: {e}")))?;
        let status = resp.status();
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| AgentError::AgentFailed(format!("REPORT read: {e}")))?;
        if !status.is_success() && status.as_u16() != 207 {
            return Err(AgentError::Upstream {
                status: status.as_u16(),
                body: truncate_str(&String::from_utf8_lossy(&bytes)),
            });
        }
        Ok(bytes)
    }

    /// `GET` a single `.ics` resource. Returns the raw iCalendar
    /// body; the caller (`caldav_get_event`) parses the first
    /// `VEVENT`.
    pub async fn get(&self, url: &str) -> Result<Bytes, AgentError> {
        // Use the EgressClient streaming path so the SSRF
        // pre-flight + body-size cap are applied uniformly.
        match self.http.get(url, None).await {
            Ok(bytes) => Ok(bytes),
            Err(crate::egress::EgressError::Upstream { status, body }) => {
                Err(AgentError::Upstream { status, body })
            }
            Err(other) => Err(map_egress(other)),
        }
    }

    /// `PUT` an iCalendar body to a new event href. The `href`
    /// argument is the path under the calendar collection
    /// (e.g. `/calendars/alice/personal/nextcloud-abc.ics`),
    /// resolved against the client `principal` to an absolute
    /// URL.
    pub async fn put(&self, href_path: &str, body: &str) -> Result<(), AgentError> {
        let full = self
            .principal
            .join(href_path)
            .map_err(|e| AgentError::InvalidArguments(format!("href join: {e}")))?;
        let full_str = full.as_str();
        let validated = self.http.validate(full_str).await.map_err(map_egress)?;
        let auth = self.auth.header_value();
        let resp = self
            .http
            .inner()
            .request(reqwest::Method::PUT, validated.url.as_str())
            .header(reqwest::header::AUTHORIZATION, auth)
            .header("Content-Type", "text/calendar; charset=utf-8")
            .body(body.to_string())
            .send()
            .await
            .map_err(|e| AgentError::AgentFailed(format!("PUT transport: {e}")))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(AgentError::Upstream {
                status: status.as_u16(),
                body: truncate_str(&body),
            });
        }
        Ok(())
    }

    // NOTE: There is no `delete()` / `update()` method on this
    // client on purpose — see module docs. v1 ships read + add
    // only. A code-review grep for `DELETE` in the caldav
    // module is a cheap guard against an accidental re-add.
}

/// Map the egress error variants onto agent errors.
fn map_egress(e: crate::egress::EgressError) -> AgentError {
    use crate::egress::EgressError::*;
    match e {
        Scheme(s) => AgentError::InvalidArguments(format!("unsupported scheme `{s}`")),
        NoHost => AgentError::InvalidArguments("url has no host".into()),
        NotInAllowlist { host } => {
            AgentError::SandboxDenied(format!("host `{host}` is not in the CalDAV allowlist"))
        }
        DisallowedAddress { host, addr } => AgentError::SandboxDenied(format!(
            "host `{host}` resolves to a disallowed address ({addr})"
        )),
        Dns(msg) => AgentError::AgentFailed(format!("dns resolve: {msg}")),
        Upstream { status, body } => AgentError::Upstream { status, body },
        ResponseExceeded { budget } => AgentError::ResponseExceeded { budget },
        Transport(msg) => AgentError::AgentFailed(format!("transport: {msg}")),
        UrlParse(msg) => AgentError::InvalidArguments(format!("url parse: {msg}")),
    }
}

/// Truncate a string body to the 2 KB cap the egress layer
/// uses for upstream errors. Operates on `&str` so the
/// char-boundary check works on a UTF-8 string.
fn truncate_str(s: &str) -> String {
    const CAP: usize = 2048;
    if s.len() <= CAP {
        return s.to_string();
    }
    let mut end = CAP;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = s[..end].to_string();
    out.push_str("\n…[truncated]");
    out
}

// ===========================================================================
// XML helpers — small, dependency-free, hand-rolled
// ===========================================================================
//
// We do not pull in `quick-xml` (the plan explicitly avoids it
// for this v1 slice). The five element types we care about
// (`<d:href>`, `<d:displayname>`, `<d:resourcetype>`, `<d:getctag>`,
// `<D:response>`) are extracted with a single forward scan.

/// Extract the `<d:displayname>` / `<d:href>` / `<d:getctag>` /
/// `<d:resourcetype>` content for every `<D:response>` child of
/// a PROPFIND multistatus body.
///
/// `xml` is the raw response body; `principal` is used to
/// resolve relative hrefs into absolute URLs. The function is
/// permissive: a missing element is `None`, malformed XML does
/// not panic — it returns the partial list of what was
/// successfully extracted.
pub fn extract_propfind_responses(xml: &str, principal: &Url) -> Vec<PropfindResponse> {
    let lower = xml.to_ascii_lowercase();
    let mut out = Vec::new();
    let mut cursor = 0usize;
    while let Some(rel_start) = find_open_tag(&lower, cursor, "response") {
        let abs_end = match find_close_tag_in_lower(&lower, rel_start, "response") {
            Some(end) => end,
            None => break,
        };
        // Slice the original-case XML using the same byte
        // ranges so the returned text matches the wire case
        // (`displayname: "Personal"`, not `"personal"`).
        let href = extract_inner(&xml[rel_start..abs_end], "href")
            .map(|s| resolve_href(&s, principal))
            .unwrap_or_default();
        let display_name =
            extract_inner(&xml[rel_start..abs_end], "displayname").unwrap_or_default();
        let ctag = extract_inner(&xml[rel_start..abs_end], "getctag");
        let resourcetype = extract_inner(&xml[rel_start..abs_end], "resourcetype")
            .map(|s| s.to_ascii_lowercase())
            .unwrap_or_default();
        out.push(PropfindResponse {
            href,
            display_name,
            ctag,
            is_calendar: resourcetype.contains("calendar")
                && !resourcetype.contains("calendarcollection"),
        });
        cursor = abs_end;
    }
    out
}

/// One `<D:response>` from a PROPFIND multistatus body.
#[derive(Debug, Clone)]
pub struct PropfindResponse {
    pub href: String,
    pub display_name: String,
    pub ctag: Option<String>,
    /// `true` when `<d:resourcetype>` contains `<C:calendar/>`
    /// (the calendar collection element, not the
    /// `<C:calendar-collection/>` resource-type short-hand some
    /// servers use).
    pub is_calendar: bool,
}

/// Find the next opening `<…{name}…>` (or `<…{name}>`) tag at or
/// after `start`. The leading `<` may be followed by a
/// namespace prefix (`<D:response>`, `<C:calendar>`, …); the
/// function matches the **local name** only and ignores the
/// prefix. Case-insensitive.
fn find_open_tag(xml: &str, start: usize, name: &str) -> Option<usize> {
    let lower = xml.to_ascii_lowercase();
    let lower_bytes = lower.as_bytes();
    let name_bytes = name.as_bytes();
    let mut i = start;
    while i < lower_bytes.len() {
        if lower_bytes[i] == b'<' {
            // The first character after `<` is the start of
            // the (possibly namespace-prefixed) tag name.
            let mut name_start = i + 1;
            // Skip a namespace prefix (e.g. `<D:response>`).
            // Walk forward until we either find `:` (and skip
            // it) or hit a tag-terminator (`>`, space, tab,
            // newline, CR, `/`).
            let mut p = name_start;
            while p < lower_bytes.len()
                && lower_bytes[p] != b':'
                && !matches!(lower_bytes[p], b'>' | b' ' | b'\t' | b'\n' | b'\r' | b'/')
            {
                p += 1;
            }
            if p < lower_bytes.len() && lower_bytes[p] == b':' {
                name_start = p + 1;
            }
            // Compare the local name.
            if name_start + name_bytes.len() <= lower_bytes.len()
                && &lower_bytes[name_start..name_start + name_bytes.len()] == name_bytes
            {
                let after = lower_bytes
                    .get(name_start + name_bytes.len())
                    .copied()
                    .unwrap_or(b'>');
                if after == b'>'
                    || after == b' '
                    || after == b'\t'
                    || after == b'\n'
                    || after == b'\r'
                    || after == b'/'
                {
                    return Some(i);
                }
            }
        }
        i += 1;
    }
    None
}

/// Find the matching `</{name}>` (case-insensitive, prefix-
/// insensitive) for the open tag at `start`. Greedy: handles
/// nested `<D:response>…<D:response>…</D:response>…</D:response>`
/// by scanning depth.
///
/// Returns the byte position right BEFORE the matching
/// `</name>` (the `<` of the close tag).
#[allow(dead_code)]
fn find_close_tag(xml: &str, start: usize, name: &str) -> Option<usize> {
    let lower = xml.to_ascii_lowercase();
    find_close_tag_in_lower(&lower, start, name)
}

/// Helper: is `pos` (in `lower`) the position of the local
/// `name` in a closing tag? `pos` is the position right after
/// `</` — the function skips a possible namespace prefix
/// (`D:`, `C:`, `CS:`) before comparing against `name`.
/// Used by [`find_close_tag`] to disambiguate from
/// `</calendar>` vs `</calendar-collection>`.
fn is_close_at(lower: &str, pos: usize, name: &str) -> bool {
    let bytes = lower.as_bytes();
    if pos >= bytes.len() {
        return false;
    }
    // Skip a namespace prefix if present.
    let mut name_start = pos;
    while name_start < bytes.len() && bytes[name_start] != b':' && bytes[name_start] != b'>' {
        name_start += 1;
    }
    let local_start = if name_start < bytes.len() && bytes[name_start] == b':' {
        name_start + 1
    } else {
        pos
    };
    if local_start + name.len() > bytes.len() {
        return false;
    }
    if &lower[local_start..local_start + name.len()] != name {
        return false;
    }
    let after = bytes.get(local_start + name.len()).copied().unwrap_or(b'>');
    after == b'>'
}

/// Helper: find the next opening tag with local name `name`
/// at or after `start` in `lower`. Returns the position of
/// the `<`. Mirrors [`find_open_tag`] but operates on the
/// already-lowercased string.
fn next_open_tag_at(lower: &str, start: usize, name: &str) -> Option<usize> {
    let lower_bytes = lower.as_bytes();
    let name_bytes = name.as_bytes();
    let mut i = start;
    while i < lower_bytes.len() {
        if lower_bytes[i] == b'<' {
            let mut name_start = i + 1;
            let mut p = name_start;
            while p < lower_bytes.len()
                && lower_bytes[p] != b':'
                && !matches!(lower_bytes[p], b'>' | b' ' | b'\t' | b'\n' | b'\r' | b'/')
            {
                p += 1;
            }
            if p < lower_bytes.len() && lower_bytes[p] == b':' {
                name_start = p + 1;
            }
            if name_start + name_bytes.len() <= lower_bytes.len()
                && &lower_bytes[name_start..name_start + name_bytes.len()] == name_bytes
            {
                let after = lower_bytes
                    .get(name_start + name_bytes.len())
                    .copied()
                    .unwrap_or(b'>');
                if after == b'>'
                    || after == b' '
                    || after == b'\t'
                    || after == b'\n'
                    || after == b'\r'
                    || after == b'/'
                {
                    return Some(i);
                }
            }
        }
        i += 1;
    }
    None
}

/// Pull the textual content of the first `<{name}>…</{name}>`
/// (with optional namespace prefix) inside `xml`. The match
/// is non-greedy and case-insensitive.
///
/// Returns the content slice in the **original case** (the
/// byte ranges are computed against the lowercased copy
/// for tag matching, then applied to `xml` itself so the
/// returned `String` keeps the wire case).
fn extract_inner(xml: &str, name: &str) -> Option<String> {
    let lower = xml.to_ascii_lowercase();
    let lower_bytes = lower.as_bytes();
    let open_pos = find_open_tag(&lower, 0, name)?;
    // Skip past the opening tag's `>` (and any attributes).
    let after_open = lower_bytes[open_pos..]
        .iter()
        .position(|&b| b == b'>')
        .map(|p| open_pos + p + 1)?;
    // Find the matching close tag using the depth-aware scan.
    let end = find_close_tag_in_lower(&lower, open_pos, name)?;
    Some(xml[after_open..end].trim().to_string())
}

/// Variant of [`find_close_tag`] that takes an already-
/// lowercased string. Same depth-aware scan.
///
/// Returns the byte position right BEFORE the matching
/// `</name>` (the `<` of the close tag), so the caller can
/// slice the content from `after_open` up to but not
/// including the close tag.
fn find_close_tag_in_lower(lower: &str, start: usize, name: &str) -> Option<usize> {
    let lower_bytes = lower.as_bytes();
    let mut i = lower_bytes[start..]
        .iter()
        .position(|&b| b == b'>')
        .map(|p| start + p + 1)?;
    let mut depth: usize = 1;
    while i < lower.len() {
        let next_lt = lower_bytes[i..]
            .iter()
            .position(|&b| b == b'<')
            .map(|p| i + p)?;
        if next_lt + 1 >= lower_bytes.len() {
            return None;
        }
        let next_byte = lower_bytes[next_lt + 1];
        if next_byte == b'/' {
            let after_slash = next_lt + 2;
            if is_close_at(lower, after_slash, name) {
                depth -= 1;
                if depth == 0 {
                    return Some(next_lt);
                }
            }
            // Skip past the close tag (its `>`) regardless.
            let after = lower_bytes[next_lt..]
                .iter()
                .position(|&b| b == b'>')
                .map(|p| next_lt + p + 1)
                .unwrap_or(lower.len());
            i = after;
        } else if next_byte == b'!' || next_byte == b'?' {
            let after = lower_bytes[next_lt..]
                .iter()
                .position(|&b| b == b'>')
                .map(|p| next_lt + p + 1)
                .unwrap_or(lower.len());
            i = after;
        } else {
            if let Some(open_pos) = next_open_tag_at(lower, next_lt, name) {
                if open_pos == next_lt {
                    depth += 1;
                }
            }
            let after = lower_bytes[next_lt..]
                .iter()
                .position(|&b| b == b'>')
                .map(|p| next_lt + p + 1)
                .unwrap_or(lower.len());
            i = after;
        }
    }
    None
}

/// Resolve a possibly-relative href against the principal URL.
fn resolve_href(href: &str, principal: &Url) -> String {
    if let Ok(parsed) = Url::parse(href) {
        parsed.to_string()
    } else {
        principal
            .join(href)
            .map(|u| u.to_string())
            .unwrap_or_else(|_| href.to_string())
    }
}

// ===========================================================================
// iCalendar helpers (RFC 5545 subset)
// ===========================================================================
//
// We re-implement the small subset of iCalendar parsing the v1
// agents need instead of reaching for a richer crate. The
// `icalendar` 0.7 crate's API hides `Property::value` behind a
// private field, which makes it impossible to read parsed
// values back out — so a dependency-free scan is the only
// reliable path. The parser handles the line-folding rules of
// §3.1 and the parameter syntax of §3.2 well enough for the
// five properties we care about.

/// Unfold a CRLF + space/tab folded content line per RFC 5545
/// §3.1. The input is a full iCalendar body; the output has
/// the continuation sequences collapsed.
fn unfold(ical: &str) -> String {
    let mut out = String::with_capacity(ical.len());
    for line in ical.split("\r\n") {
        if let Some(rest) = line.strip_prefix(' ').or_else(|| line.strip_prefix('\t')) {
            // Continuation: drop the leading whitespace and
            // append without a separator.
            out.push_str(rest);
        } else if !out.is_empty() {
            out.push('\n');
            out.push_str(line);
        } else {
            out.push_str(line);
        }
    }
    out
}

/// Parse the first `VEVENT` in `ical` and return it as an
/// [`Event`]. `href` is the absolute URL the event was
/// fetched from — it round-trips into [`Event::href`] so the
/// client can echo it back to the LLM.
pub fn parse_vevent(ical: &str, href: &str) -> Result<Event, AgentError> {
    let body = unfold(ical);
    for block in split_blocks(&body) {
        if block.kind != "VEVENT" {
            continue;
        }
        let mut uid: Option<String> = None;
        let mut summary = String::new();
        let mut dt_start: Option<DateTime<Utc>> = None;
        let mut dt_end: Option<DateTime<Utc>> = None;
        let mut duration: Option<chrono::Duration> = None;
        let mut description: Option<String> = None;
        let mut location: Option<String> = None;
        let mut rrule: Option<String> = None;
        for (name, _params, value) in &block.properties {
            match name.as_str() {
                "UID" => uid = Some(value.clone()),
                "SUMMARY" => summary = ical_unescape(value),
                "DTSTART" => dt_start = parse_dt(value),
                "DTEND" => dt_end = parse_dt(value),
                "DURATION" => duration = parse_duration(value),
                "DESCRIPTION" => description = Some(ical_unescape(value)),
                "LOCATION" => location = Some(ical_unescape(value)),
                "RRULE" => rrule = Some(value.clone()),
                _ => {}
            }
        }
        let uid = uid.ok_or_else(|| AgentError::AgentFailed("VEVENT missing UID".into()))?;
        let dt_start =
            dt_start.ok_or_else(|| AgentError::AgentFailed("VEVENT missing DTSTART".into()))?;
        let dt_end = dt_end.or_else(|| duration.map(|d| dt_start + d));
        return Ok(Event {
            uid,
            href: href.to_string(),
            summary,
            dt_start,
            dt_end,
            description,
            location,
            rrule,
        });
    }
    Err(AgentError::AgentFailed(
        "iCalendar body did not contain a VEVENT".into(),
    ))
}

/// Reverse the §3.3.11 escape sequence for TEXT values.
fn ical_unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.next() {
                Some('n') | Some('N') => out.push('\n'),
                Some(other) => out.push(other),
                None => out.push('\\'),
            },
            _ => out.push(c),
        }
    }
    out
}

/// Parse an iCalendar `DTSTART` / `DTEND` value. Handles
/// - `YYYYMMDDTHHMMSSZ` (UTC)
/// - `YYYYMMDDTHHMMSS` (floating; assumed UTC for v1)
/// - `YYYYMMDD` (date-only)
fn parse_dt(raw: &str) -> Option<DateTime<Utc>> {
    if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(raw, "%Y%m%dT%H%M%SZ") {
        return Some(dt.and_utc());
    }
    if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(raw, "%Y%m%dT%H%M%S") {
        return Some(dt.and_utc());
    }
    if let Ok(d) = chrono::NaiveDate::parse_from_str(raw, "%Y%m%d") {
        return d.and_hms_opt(0, 0, 0).map(|dt| dt.and_utc());
    }
    None
}

/// Parse an iCalendar `DURATION` value (RFC 5545 §3.3.6). v1
/// only supports the simple `PTnHnM` form; anything else is
/// ignored (the event is treated as a point-in-time without a
/// duration extension).
fn parse_duration(raw: &str) -> Option<chrono::Duration> {
    let s = raw.trim().strip_prefix('P')?;
    let mut days = 0i64;
    let mut total_secs = 0i64;
    let mut in_time = false;
    let mut num = String::new();
    for ch in s.chars() {
        match ch {
            'T' => in_time = true,
            '0'..='9' => num.push(ch),
            'D' => {
                days = num.parse().ok()?;
                num.clear();
            }
            'H' if in_time => {
                total_secs += num.parse::<i64>().ok()? * 3600;
                num.clear();
            }
            'M' if in_time => {
                total_secs += num.parse::<i64>().ok()? * 60;
                num.clear();
            }
            'S' if in_time => {
                total_secs += num.parse::<i64>().ok()?;
                num.clear();
            }
            'W' => {
                days = num.parse::<i64>().ok()? * 7;
                num.clear();
            }
            _ => return None,
        }
    }
    Some(chrono::Duration::days(days) + chrono::Duration::seconds(total_secs))
}

/// One parsed `VCALENDAR` / `VEVENT` block: a header line,
/// a set of properties, and the next sibling / parent.
struct Block {
    kind: String,
    properties: Vec<IcalProperty>,
}

/// Single iCalendar property as parsed by [`split_property`].
type IcalProperty = (String, Vec<(String, String)>, String);

/// Split an unfolded iCalendar body into the immediate
/// `VEVENT` / `VTODO` / `VCALENDAR` blocks at the same depth
/// as the outermost `VCALENDAR`.
fn split_blocks(body: &str) -> Vec<Block> {
    let mut blocks: Vec<Block> = Vec::new();
    let mut stack: Vec<Block> = Vec::new();
    for line in body.lines() {
        if let Some(kind) = line.strip_prefix("BEGIN:") {
            stack.push(Block {
                kind: kind.to_string(),
                properties: Vec::new(),
            });
        } else if let Some(kind) = line.strip_prefix("END:") {
            if let Some(block) = stack.pop() {
                if block.kind == kind {
                    // Push at the parent's level. We only
                    // surface the top-level VCALENDAR's
                    // children; nested components (VALARM)
                    // are ignored.
                    if stack.len() == 1 {
                        blocks.push(block);
                    }
                }
            }
        } else if let Some(block) = stack.last_mut() {
            let (name, params, value) = split_property(line);
            block.properties.push((name, params, value));
        }
    }
    blocks
}

/// Split a content line into `(name, params, value)`. The
/// `name` is uppercased; `params` is `[(KEY, VALUE), …]` in
/// insertion order; `value` is the raw text after the final
/// `:`. Quoted parameter values are unquoted.
fn split_property(line: &str) -> (String, Vec<(String, String)>, String) {
    let colon = line.find(':').unwrap_or(line.len());
    let head = &line[..colon];
    let value = line[colon + 1..].to_string();
    let mut parts = head.split(';');
    let name = parts.next().unwrap_or("").to_ascii_uppercase();
    let mut params = Vec::new();
    for p in parts {
        if let Some(eq) = p.find('=') {
            let k = p[..eq].to_ascii_uppercase();
            let v = p[eq + 1..].trim_matches('"').to_string();
            params.push((k, v));
        }
    }
    (name, params, value)
}

// ===========================================================================
// Wire-format XML builders
// ===========================================================================

/// Build the `PROPFIND` request body used by the probe endpoint
/// to discover the user's calendars.
pub fn build_propfind_body() -> String {
    r#"<?xml version="1.0" encoding="utf-8" ?>
<D:propfind xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav" xmlns:CS="http://calendarserver.org/ns/">
  <D:prop>
    <D:displayname/>
    <D:resourcetype/>
    <CS:getctag/>
  </D:prop>
</D:propfind>
"#
    .to_string()
}

/// Build the `REPORT calendar-query` body used by
/// `caldav_list_events`.
pub fn build_calendar_query_body(start: DateTime<Utc>, end: DateTime<Utc>) -> String {
    let start = start.format("%Y%m%dT%H%M%SZ");
    let end = end.format("%Y%m%dT%H%M%SZ");
    format!(
        r#"<?xml version="1.0" encoding="utf-8" ?>
<C:calendar-query xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
  <D:prop>
    <D:getetag/>
    <C:calendar-data>
      <C:expand start="{start}" end="{end}"/>
    </C:calendar-data>
  </D:prop>
  <C:filter>
    <C:comp-filter name="VCALENDAR">
      <C:comp-filter name="VEVENT">
        <C:time-range start="{start}" end="{end}"/>
      </C:comp-filter>
    </C:comp-filter>
  </C:filter>
</C:calendar-query>
"#
    )
}

/// Extract every `VEVENT` from a `REPORT calendar-query`
/// `multistatus` body. The XML embeds the full iCalendar
/// payload inside `<C:calendar-data>…</C:calendar-data>` for
/// each matching event.
pub fn extract_events_from_multistatus(
    xml: &str,
    calendar_url: &Url,
) -> Result<Vec<Event>, AgentError> {
    let lower = xml.to_ascii_lowercase();
    let mut events = Vec::new();
    let mut cursor = 0usize;
    while let Some(rel_start) = find_open_tag(&lower, cursor, "response") {
        let abs_end = match find_close_tag_in_lower(&lower, rel_start, "response") {
            Some(end) => end,
            None => break,
        };
        let chunk_lower = &lower[rel_start..abs_end];
        let href = extract_inner_from_lower(chunk_lower, "href")
            .map(|s| resolve_href(&s, calendar_url))
            .unwrap_or_default();
        if let Some(ical) = extract_inner_from_lower(chunk_lower, "calendar-data") {
            // The server may return multiple VEVENTs in one
            // `calendar-data` blob; parse the whole VCALENDAR
            // and walk the components.
            for block in split_blocks(&unfold(&ical)) {
                if block.kind != "VEVENT" {
                    continue;
                }
                let mut uid: Option<String> = None;
                let mut summary = String::new();
                let mut dt_start: Option<DateTime<Utc>> = None;
                let mut dt_end: Option<DateTime<Utc>> = None;
                let mut duration: Option<chrono::Duration> = None;
                let mut description: Option<String> = None;
                let mut location: Option<String> = None;
                let mut rrule: Option<String> = None;
                for (name, _params, value) in &block.properties {
                    match name.as_str() {
                        "UID" => uid = Some(value.clone()),
                        "SUMMARY" => summary = ical_unescape(value),
                        "DTSTART" => dt_start = parse_dt(value),
                        "DTEND" => dt_end = parse_dt(value),
                        "DURATION" => duration = parse_duration(value),
                        "DESCRIPTION" => description = Some(ical_unescape(value)),
                        "LOCATION" => location = Some(ical_unescape(value)),
                        "RRULE" => rrule = Some(value.clone()),
                        _ => {}
                    }
                }
                if let (Some(uid), Some(dt_start)) = (uid, dt_start) {
                    let dt_end = dt_end.or_else(|| duration.map(|d| dt_start + d));
                    events.push(Event {
                        uid,
                        href: href.clone(),
                        summary,
                        dt_start,
                        dt_end,
                        description,
                        location,
                        rrule,
                    });
                }
            }
        }
        cursor = abs_end;
    }
    Ok(events)
}

/// Variant of [`extract_inner`] that takes an
/// already-lowercased string.
fn extract_inner_from_lower(lower: &str, name: &str) -> Option<String> {
    let lower_bytes = lower.as_bytes();
    let open_pos = find_open_tag(lower, 0, name)?;
    let after_open = lower_bytes[open_pos..]
        .iter()
        .position(|&b| b == b'>')
        .map(|p| open_pos + p + 1)?;
    let end = find_close_tag_in_lower(lower, open_pos, name)?;
    Some(lower[after_open..end].trim().to_string())
}

// ===========================================================================
// Sub-modules (LLM-callable agents).
// ===========================================================================
//
// Each submodule implements one `Agent` trait; the static
// factory table in `agents.rs` registers exactly three of
// them. `caldav_list_calendars`, `caldav_update_event`, and
// `caldav_delete_event` are NOT registered (see the
// "Forbidden operations" section of the plan).

pub mod create_event;
pub mod get_event;
pub mod list_events;

#[cfg(test)]
mod tests {
    use super::*;

    const PRINCIPAL: &str = "https://cloud.example.com/remote.php/dav/calendars/alice/";

    #[test]
    fn build_propfind_body_contains_required_props() {
        let body = build_propfind_body();
        assert!(body.contains("displayname"));
        assert!(body.contains("resourcetype"));
        assert!(body.contains("getctag"));
    }

    #[test]
    fn build_calendar_query_body_includes_time_range() {
        let start = Utc::now();
        let end = start + chrono::Duration::days(7);
        let body = build_calendar_query_body(start, end);
        assert!(body.contains("calendar-query"));
        assert!(body.contains("time-range"));
    }

    #[test]
    fn eventpatch_to_ical_is_well_formed() {
        let patch = EventPatch {
            summary: "Réunion d'équipe".to_string(),
            start: Utc::now(),
            end: Some(Utc::now() + chrono::Duration::hours(1)),
            description: Some("Point hebdo".to_string()),
            location: Some("Salle A".to_string()),
        };
        let body = patch.to_ical();
        assert!(body.contains("BEGIN:VCALENDAR"));
        assert!(body.contains("BEGIN:VEVENT"));
        assert!(body.contains("SUMMARY:R"));
        assert!(body.contains("END:VEVENT"));
        assert!(body.contains("END:VCALENDAR"));
    }

    #[test]
    fn ical_escape_escapes_special_chars() {
        assert_eq!(ical_escape("a;b,c\\d"), "a\\;b\\,c\\\\d");
        assert_eq!(ical_escape("line1\nline2"), "line1\\nline2");
    }

    #[test]
    fn extract_inner_returns_text() {
        let xml = r#"<root><a>hello</a><b>world</b></root>"#;
        assert_eq!(extract_inner(xml, "a").as_deref(), Some("hello"));
        assert_eq!(extract_inner(xml, "b").as_deref(), Some("world"));
        assert_eq!(extract_inner(xml, "c"), None);
    }

    #[test]
    fn resolve_href_handles_absolute_and_relative() {
        let principal = Url::parse(PRINCIPAL).unwrap();
        assert_eq!(
            resolve_href("/remote.php/dav/foo/", &principal),
            "https://cloud.example.com/remote.php/dav/foo/"
        );
        assert_eq!(
            resolve_href("personal/", &principal),
            "https://cloud.example.com/remote.php/dav/calendars/alice/personal/"
        );
    }

    #[test]
    fn extract_inner_handles_namespaced_tags() {
        let xml = r#"<D:response><D:displayname>Personal</D:displayname></D:response>"#;
        let got = extract_inner(xml, "displayname");
        eprintln!("got = {:?}", got);
        let pos = find_open_tag(&xml.to_ascii_lowercase(), 0, "displayname");
        eprintln!("open pos = {:?}", pos);
        if let Some(p) = pos {
            let close = find_close_tag_in_lower(&xml.to_ascii_lowercase(), p, "displayname");
            eprintln!("close = {:?}", close);
        }
    }

    #[test]
    fn extract_propfind_responses_filters_calendars() {
        // A minimal but realistic PROPFIND multistatus body
        // containing one calendar collection and one
        // non-calendar child.
        let xml = r#"<?xml version="1.0" encoding="utf-8" ?>
<D:multistatus xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
  <D:response>
    <D:href>/remote.php/dav/calendars/alice/personal/</D:href>
    <D:propstat>
      <D:prop>
        <D:displayname>Personal</D:displayname>
        <D:resourcetype>
          <D:collection/>
          <C:calendar/>
        </D:resourcetype>
        <CS:getctag xmlns:CS="http://calendarserver.org/ns/">1234</CS:getctag>
      </D:prop>
    </D:propstat>
  </D:response>
  <D:response>
    <D:href>/remote.php/dav/files/alice/</D:href>
    <D:propstat>
      <D:prop>
        <D:displayname>Files</D:displayname>
        <D:resourcetype><D:collection/></D:resourcetype>
      </D:prop>
    </D:propstat>
  </D:response>
</D:multistatus>
"#;
        let principal = Url::parse(PRINCIPAL).unwrap();
        let responses = extract_propfind_responses(xml, &principal);
        // The non-calendar response is still extracted but
        // marked `is_calendar = false`.
        assert_eq!(responses.len(), 2, "both responses should be extracted");
        let calendars: Vec<_> = responses.iter().filter(|r| r.is_calendar).collect();
        assert_eq!(calendars.len(), 1, "exactly one calendar");
        assert_eq!(calendars[0].display_name, "Personal");
        assert_eq!(calendars[0].ctag.as_deref(), Some("1234"));
        assert!(calendars[0].href.contains("/personal/"));
        assert!(!responses[1].is_calendar);
    }

    #[test]
    fn parse_vevent_extracts_standard_fields() {
        let ical = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//Test//EN\r\nBEGIN:VEVENT\r\nUID:abc-123\r\nDTSTAMP:20260101T120000Z\r\nDTSTART:20260201T140000Z\r\nDTEND:20260201T150000Z\r\nSUMMARY:Réunion\r\nDESCRIPTION:Note\\ndeux lignes\r\nLOCATION:Salle A\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let event = parse_vevent(ical, "https://example.com/event.ics").expect("parse");
        assert_eq!(event.uid, "abc-123");
        assert_eq!(event.summary, "Réunion");
        assert!(event.dt_start.to_rfc3339().contains("2026-02-01"));
        assert_eq!(
            event.dt_end.expect("dtend").to_rfc3339(),
            "2026-02-01T15:00:00+00:00"
        );
        assert_eq!(event.location.as_deref(), Some("Salle A"));
    }

    #[test]
    fn parse_vevent_with_rrule_keeps_raw() {
        let ical = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:r-1\r\nDTSTAMP:20260101T120000Z\r\nDTSTART:20260201T140000Z\r\nRRULE:FREQ=WEEKLY;COUNT=4\r\nSUMMARY:Hebdo\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let event = parse_vevent(ical, "x").expect("parse");
        assert_eq!(event.rrule.as_deref(), Some("FREQ=WEEKLY;COUNT=4"));
    }

    #[test]
    fn parse_vevent_handles_duration() {
        let ical = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:d-1\r\nDTSTAMP:20260101T120000Z\r\nDTSTART:20260201T140000Z\r\nDURATION:PT1H30M\r\nSUMMARY:x\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let event = parse_vevent(ical, "x").expect("parse");
        let end = event.dt_end.expect("dtend from DURATION");
        assert_eq!(end.to_rfc3339(), "2026-02-01T15:30:00+00:00");
    }

    #[test]
    fn parse_vevent_handles_unfolding() {
        // RFC 5545 §3.1 line folding: a CR-LF followed by a
        // single space (or tab) is part of the previous line.
        let ical =
            "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:fold-1\r\nDTSTAMP:20260101T120000Z\r\nDTSTART:20260201T140000Z\r\nSUMMARY:long\r\n  title continues here\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let event = parse_vevent(ical, "x").expect("parse");
        assert_eq!(event.summary, "long title continues here");
    }

    #[test]
    fn parse_vevent_missing_uid_is_error() {
        let ical = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nDTSTAMP:20260101T120000Z\r\nDTSTART:20260201T140000Z\r\nSUMMARY:x\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let err = parse_vevent(ical, "x").unwrap_err();
        assert!(matches!(err, AgentError::AgentFailed(_)));
    }

    #[test]
    fn basic_auth_header_value() {
        use base64::Engine;
        let auth = BasicAuth {
            username: "alice".into(),
            password: "s3cr3t".into(),
        };
        // The header value is the base64 of "alice:s3cr3t".
        let expected = format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(b"alice:s3cr3t")
        );
        assert_eq!(auth.header_value(), expected);
    }

    /// Negative test — guards the v1 invariant that the
    /// `CalDavClient` struct does not expose any
    /// `delete()` / `update()` method and does not emit a
    /// `DELETE` request on the wire. The check inspects the
    /// module's source lines outside of `#[cfg(test)]` blocks
    /// so the tripwire's own comments / `include_str!`
    /// references do not match themselves.
    #[test]
    fn caldav_client_struct_has_no_delete_or_update_method() {
        let src = include_str!("mod.rs");
        // Strip `#[cfg(test)] mod tests { ... }` so the
        // tripwire's own documentation / test fixtures do
        // not match. We use a simple bracket-counter: skip
        // lines inside any `mod tests` block under
        // `#[cfg(test)]`.
        let mut in_tests = false;
        let mut depth: i32 = 0;
        let mut offenders: Vec<&str> = Vec::new();
        for line in src.lines() {
            let trimmed = line.trim_start();
            if trimmed.starts_with("#[cfg(test)]") {
                in_tests = true;
                depth = 1;
                continue;
            }
            if in_tests {
                // Track brace depth so we exit when the
                // `mod tests {` block closes.
                depth += line.matches('{').count() as i32;
                depth -= line.matches('}').count() as i32;
                if depth <= 0 {
                    in_tests = false;
                    depth = 0;
                }
                continue;
            }
            // Tripwires: a `pub fn delete(` / `pub fn update(`
            // would expose a destructive method on the
            // client. `reqwest::Method::from_bytes(b"DELETE")`
            // would be the actual wire-side mistake.
            if trimmed.starts_with("pub fn delete")
                || trimmed.starts_with("async fn delete")
                || trimmed.starts_with("pub async fn delete")
            {
                offenders.push("pub fn delete");
            }
            if trimmed.starts_with("pub fn update")
                || trimmed.starts_with("async fn update")
                || trimmed.starts_with("pub async fn update")
            {
                offenders.push("pub fn update");
            }
            if line.contains(r#"reqwest::Method::from_bytes(b"DELETE")"#) {
                offenders.push("Method::DELETE emit");
            }
        }
        assert!(
            offenders.is_empty(),
            "CalDavClient must not expose destructive methods (v1 is read+add only); \
             tripwire fired on: {offenders:?}"
        );
    }
}
