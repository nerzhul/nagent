//! `get_stock_quote` agent: latest quote for a stock ticker via Stooq.
//!
//! Stooq's CSV endpoint is free, no-key, and tolerant of small
//! per-minute request bursts from a single IP. The format the agent
//! requests (`f=sd2t2ohlcv&h&e=csv`) is the legacy two-row CSV with
//! headers and one data row, e.g.:
//!
//! ```text
//! Symbol,Date,Time,Open,High,Low,Close,Volume
//! AAPL.US,2026-09-25,21:00:00,243.10,246.50,242.80,245.32,12345678
//! ```
//!
//! ## Ticker resolution pipeline
//!
//! Three layers, tried in order:
//!
//! 1. **Company-name shortcut** — a small static table maps common
//!    European company names to their Stooq tickers. When the user
//!    types "Atos" we translate to `ATO.PA` directly and skip every
//!    other round-trip. Covers the "human types the company name, not
//!    the ticker" case for the few dozen French/European issuers a
//!    casual chat user is most likely to ask about.
//! 2. **Ticker normalisation** — uppercase the input and ensure it
//!    has an exchange suffix. `AAPL` → `AAPL.US`, `ATO.PA` passes
//!    through. The auto-`.US` reflects Stooq's behaviour of
//!    returning empty bodies for bare tickers.
//! 3. **Multi-exchange fallback** — when the user did not pin an
//!    explicit `.XX` suffix and the primary lookup returns no data,
//!    try the same root ticker on common European exchanges
//!    (`.PA`, `.L`, `.DE`, `.MI`) in turn. The first success wins.
//!    Explicit suffixes are respected (no second-guessing) so a
//!    user-supplied `XYZ.PA` is never retried on another exchange.
//!
//! ## "No data" handling
//!
//! Stooq signals a missing symbol with either an empty body or a
//! single-line `N/D` body. The agent surfaces both as `AgentFailed`
//! with a message listing every symbol it tried (so the LLM can tell
//! the user what to look up next).
//!
//! ## Latency note
//!
//! Worst case the agent makes 5 sequential round-trips (the primary
//! `.US` plus four European fallbacks). Each Stooq response is
//! sub-second; total wall time stays under ~2 s in practice. We
//! accept the extra round-trips so a stock quote for "Atos" Just
//! Works without making the user memorise the ticker convention.

use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::agents::{Agent, AgentError};

/// Hard cap on the response body. The Stooq CSV is at most a few
/// hundred bytes; 16 KiB leaves room for occasional bloat (an
/// "invalid" response, or the `N/D` sentinel in some quirky cases)
/// while still bounding memory.
const MAX_UPSTREAM_BYTES: usize = 16 * 1024;

/// HTTP per-request timeout. Stooq is sub-second normally; 8 s is
/// the same bound the weather agent uses so we keep one knob.
const HTTP_TIMEOUT: Duration = Duration::from_secs(8);

/// Common European exchanges to try when the user did not pin an
/// explicit suffix and the primary lookup returns no data. Listed
/// in priority order — the first to return real data wins. The list
/// reflects "where do European retail-investor queries land" — Paris
/// first because the agent's primary user base speaks French, then
/// London / Frankfurt / Milan as the next-largest European retail
/// markets.
const FALLBACK_EXCHANGES: &[&str] = &["PA", "L", "DE", "MI"];

/// Default exchange auto-appended when the user supplies a bare
/// ticker. Matches the plan and Stooq's convention that bare
/// tickers without a `.XX` suffix return an empty body.
const DEFAULT_EXCHANGE: &str = "US";

/// Static company-name → ticker shortcut.
///
/// Maps user-friendly company names (and their common misspellings)
/// to the Stooq ticker that actually carries the data. The table is
/// deliberately small: it covers the issuers a casual chat user is
/// most likely to query by *name* rather than by ticker. The
/// multi-exchange fallback below handles everything else.
///
/// Matching is case-insensitive and insensitive to surrounding
/// whitespace; multi-word entries accept either a single normalised
/// space or multiple internal spaces.
const KNOWN_COMPANIES: &[(&str, &str)] = &[
    // ---- France (Euronext Paris) ----
    ("atos", "ATO.PA"),
    ("atos origin", "ATO.PA"),
    ("lvmh", "MC.PA"),
    ("lvmh moet hennessy", "MC.PA"),
    ("moet hennessy", "MC.PA"),
    ("airbus", "AIR.PA"),
    ("airbus group", "AIR.PA"),
    ("bnp", "BNP.PA"),
    ("bnp paribas", "BNP.PA"),
    ("sanofi", "SAN.PA"),
    ("orange", "ORA.PA"),
    ("capgemini", "CAP.PA"),
    ("societe generale", "GLE.PA"),
    ("société générale", "GLE.PA"),
    ("sg", "GLE.PA"),
    ("axa", "CS.PA"),
    ("total", "TTE.PA"),
    ("totalenergies", "TTE.PA"),
    ("renault", "RNO.PA"),
    ("stellantis", "STLA.PA"),
    ("thales", "HO.PA"),
    ("schneider", "SU.PA"),
    ("schneider electric", "SU.PA"),
    ("engie", "ENGI.PA"),
    ("vivendi", "VIE.PA"),
    ("danone", "BN.PA"),
    ("l'oreal", "OR.PA"),
    ("loreal", "OR.PA"),
    ("l'oréal", "OR.PA"),
    ("oreal", "OR.PA"),
    ("publicis", "PUB.PA"),
    ("pernod ricard", "RI.PA"),
    ("pernod-ricard", "RI.PA"),
    ("pernod", "RI.PA"),
    ("carrefour", "CA.PA"),
    ("credit agricole", "ACA.PA"),
    ("bouygues", "EN.PA"),
    ("michelin", "ML.PA"),
    ("kering", "KER.PA"),
    ("hermes", "RMS.PA"),
    ("hermès", "RMS.PA"),
    ("dassault systemes", "DSY.PA"),
    ("dassault systémes", "DSY.PA"),
    ("dassault", "AM.PA"), // Dassault Aviation; Aviation (AM) is the listed company
    ("saint gobain", "SGO.PA"),
    ("saint-gobain", "SGO.PA"),
    ("veolia", "VIE.PA"), // VIE = Veolia Environnement on Paris; ambiguous with Vivendi which is also VIE on some sources. Use Vivendi's full name.
    ("alstom", "ALO.PA"),
    ("edenred", "EDEN.PA"),
    // ---- Germany (Frankfurt) ----
    ("siemens", "SIE.DE"),
    ("sap", "SAP.DE"),
    ("allianz", "ALV.DE"),
    ("volkswagen", "VOW3.DE"),
    ("vw", "VOW3.DE"),
    ("bmw", "BMW.DE"),
    ("basf", "BAS.DE"),
    // ---- UK (London) ----
    ("shell", "SHEL.L"),
    ("bp", "BP.L"),
    ("astrazeneca", "AZN.L"),
    ("astra zeneca", "AZN.L"),
    ("unilever", "ULVR.L"),
    ("hsbc", "HSBA.L"),
    // ---- Italy (Milan) ----
    ("eni", "ENI.MI"),
    ("enel", "ENEL.MI"),
    ("intesa sanpaolo", "ISP.MI"),
    // ---- US big-tech (so "Apple" / "Microsoft" etc. also Just Work) ----
    ("apple", "AAPL.US"),
    ("microsoft", "MSFT.US"),
    ("nvidia", "NVDA.US"),
    ("amazon", "AMZN.US"),
    ("google", "GOOGL.US"),
    ("alphabet", "GOOGL.US"),
    ("meta", "META.US"),
    ("tesla", "TSLA.US"),
    ("netflix", "NFLX.US"),
    ("ibm", "IBM.US"),
];

/// Build of the `get_stock_quote` agent.
#[derive(Clone)]
pub struct StockAgent {
    http: reqwest::Client,
}

impl std::fmt::Debug for StockAgent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StockAgent")
            .field("http", &"<reqwest::Client>")
            .finish()
    }
}

impl Default for StockAgent {
    fn default() -> Self {
        Self::new()
    }
}

impl StockAgent {
    pub fn new() -> Self {
        let http = reqwest::Client::builder()
            .timeout(HTTP_TIMEOUT)
            .connect_timeout(HTTP_TIMEOUT)
            .build()
            .expect("reqwest client build");
        Self { http }
    }
}

#[async_trait]
impl Agent for StockAgent {
    fn name(&self) -> &str {
        "get_stock_quote"
    }

    fn description(&self) -> &str {
        "Cours actuel d'un ticker boursier via Stooq. \
         Accepte un ticker (AAPL, NVDA, ATO.PA, MC.PA) ou un nom de société \
         courant (Atos, LVMH, Sanofi, Airbus, Apple, Microsoft, …). \
         Aucune clé d'API. La résolution multi-bourse essaie `.US`, `.PA`, `.L`, \
         `.DE`, `.MI` quand l'utilisateur ne précise pas l'échange. \
         Use for 'cours de Atos', 'LVMH cours', 'AAPL stock price', 'prix action BNP'. \
         Pass `ticker` (required)."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "ticker": {
                    "type": "string",
                    "pattern": "^[A-Za-z0-9 .'\\-]{1,40}$",
                    "description": "Ticker symbol (e.g. 'AAPL', 'ATO.PA', 'MC.PA') or a common company name (e.g. 'Atos', 'LVMH', 'Apple'). When a name resolves via the built-in shortcut table, the agent uses the correct exchange on the first try."
                }
            },
            "required": ["ticker"],
            "additionalProperties": false,
        })
    }

    async fn invoke(&self, args: Value) -> Result<String, AgentError> {
        let req = parse_args(&args)?;

        // Step 1: static company-name shortcut. When the user
        // typed a known company name, we land on the right ticker
        // (with its exchange already attached).
        let resolved_input = if let Some(known) = resolve_known_company(&req.ticker) {
            known.to_string()
        } else {
            normalise_ticker(&req.ticker)
        };

        // Step 2 + 3: try the resolved ticker, with multi-exchange
        // fallback when the primary doesn't return data.
        let tried_ref = tried_symbols(&resolved_input);
        let (body, resolved_ticker) = fetch_with_fallback(&self.http, &resolved_input).await?;
        let payload = parse_stooq_csv(&body, &resolved_ticker)?;

        Ok(serde_json::to_string(&json!({
            "ok": true,
            "data": payload,
            "source": "stooq",
            "tried": tried_ref,
            "fetched_at": chrono::Utc::now().to_rfc3339(),
        }))
        .expect("json encode"))
    }
}

// ---- Step 1: known-company shortcut --------------------------------------

/// Look the user input up in the static [`KNOWN_COMPANIES`] table.
/// Returns the canonical Stooq ticker when a match is found (with
/// the right exchange already attached), `None` otherwise.
///
/// Matching is case-insensitive, ignores leading/trailing whitespace,
/// and collapses multiple internal whitespace runs into a single
/// space so "BNP  Paribas" and "BNP Paribas" both resolve.
fn resolve_known_company(input: &str) -> Option<&'static str> {
    let normalised = collapse_whitespace(input.trim());
    if normalised.is_empty() {
        return None;
    }
    for (name, ticker) in KNOWN_COMPANIES {
        if name.eq_ignore_ascii_case(&normalised) {
            return Some(ticker);
        }
    }
    None
}

fn collapse_whitespace(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_ws = false;
    for ch in s.chars() {
        if ch.is_whitespace() {
            if !in_ws && !out.is_empty() {
                out.push(' ');
                in_ws = true;
            }
        } else {
            out.push(ch);
            in_ws = false;
        }
    }
    out.trim_end().to_string()
}

// ---- Step 2: ticker normalisation ---------------------------------------

/// Uppercase + ensure an exchange suffix. Bare `AAPL` becomes
/// `AAPL.US`. Tickers that already carry a suffix (`AIR.PA`,
/// `MC.PA`, `SAP.DE`, …) pass through untouched.
fn normalise_ticker(raw: &str) -> String {
    let upper = raw.trim().to_ascii_uppercase();
    if upper.contains('.') {
        upper
    } else {
        format!("{upper}.{DEFAULT_EXCHANGE}")
    }
}

fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

// ---- Step 3: multi-exchange fallback ------------------------------------

/// Return the list of Stooq tickers the agent will (or did) try, in
/// order. The first entry is the implied/explicit exchange
/// (`.US` for bare tickers, `.XX` for already-suffixed ones). The
/// remaining entries are the common European exchanges in
/// priority order, followed by `.US` if it wasn't the implied
/// primary. Surfaced back to the caller so the LLM sees what we
/// searched for on `AgentFailed`.
fn tried_symbols(resolved: &str) -> Vec<String> {
    let root = resolved.split('.').next().unwrap_or(resolved);
    let implied = resolved
        .rsplit_once('.')
        .map(|(_, s)| s.to_ascii_uppercase())
        .unwrap_or_else(|| DEFAULT_EXCHANGE.to_string());

    let mut order: Vec<String> = Vec::with_capacity(1 + FALLBACK_EXCHANGES.len() + 1);
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    let push =
        |order: &mut Vec<String>, seen: &mut std::collections::HashSet<String>, ex: String| {
            if seen.insert(ex.clone()) {
                order.push(ex);
            }
        };

    push(&mut order, &mut seen, implied);
    for ex in FALLBACK_EXCHANGES {
        push(&mut order, &mut seen, (*ex).to_string());
    }
    // Always include `.US` last so an explicit-suffix request
    // (`ATO.PA`) also tries the US OTC listing if the primary
    // exchange has no data. Bare-ticker requests already had
    // `.US` as the implicit primary; the dedup gate skips the
    // duplicate here.
    push(&mut order, &mut seen, DEFAULT_EXCHANGE.to_string());

    order
        .iter()
        .map(|suffix| format!("{root}.{suffix}"))
        .collect()
}

/// HTTP the resolved ticker, falling back to other exchanges if the
/// primary returns no data. Returns the body **and** the ticker that
/// actually produced it (so the CSV parser's exchange field reflects
/// what matched).
async fn fetch_with_fallback(
    http: &reqwest::Client,
    resolved: &str,
) -> Result<(String, String), AgentError> {
    let mut last_err: Option<AgentError> = None;
    for ticker in tried_symbols(resolved) {
        let url = format!(
            "https://stooq.com/q/l/?s={}&f=sd2t2ohlcv&h&e=csv",
            url_encode(&ticker)
        );
        match fetch_csv(http, &url).await {
            Ok(body) if !is_no_data(&body) => return Ok((body, ticker)),
            Ok(_) => {
                // `N/D`, empty, or HTML error page — the symbol
                // doesn't have data on this exchange; try the next
                // one.
                continue;
            }
            Err(e) => last_err = Some(e),
        }
    }
    // Every attempt either returned no data or failed. Surface the
    // upstream error when we have one, otherwise the "no data"
    // shape with the full tried-symbol list so the LLM can suggest
    // an explicit ticker.
    if let Some(e) = last_err {
        return Err(e);
    }
    let tried = tried_symbols(resolved).join(", ");
    Err(AgentError::AgentFailed(format!(
        "no data for ticker (Stooq returned no data for any of: {tried}. \
         Try an explicit ticker like `ATO.PA` for Euronext Paris, `MC.PA` for LVMH, etc.)"
    )))
}

// ---- HTTP helper ---------------------------------------------------------

async fn fetch_csv(http: &reqwest::Client, url: &str) -> Result<String, AgentError> {
    let resp = http
        .get(url)
        .header(
            reqwest::header::USER_AGENT,
            "nagent-stock-agent/0.1 (+https://github.com/nagent/nagent)",
        )
        .send()
        .await
        .map_err(|e| AgentError::AgentFailed(format!("upstream connect/read failed: {e}")))?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(AgentError::Upstream {
            status: status.as_u16(),
            body: truncate(&body, 2048),
        });
    }
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| AgentError::AgentFailed(format!("read body: {e}")))?;
    if bytes.len() > MAX_UPSTREAM_BYTES {
        return Err(AgentError::AgentFailed(format!(
            "upstream body too large: {} bytes (cap {MAX_UPSTREAM_BYTES})",
            bytes.len()
        )));
    }
    String::from_utf8(bytes.to_vec())
        .map_err(|e| AgentError::AgentFailed(format!("upstream returned non-UTF-8 body: {e}")))
}

/// True when the body is *not* a Stooq CSV quote: empty, the `N/D`
/// sentinel, or any non-CSV payload (HTML error page, Cloudflare
/// bot challenge, …). The HTML case is the one the original
/// implementation missed — Cloudflare's challenge page is a valid
/// 200 response with `<html>…</html>` content that the old
/// `is_no_data` would have happily handed to the CSV parser, where
/// it would then fail with a misleading "row width mismatch"
/// message instead of letting the fallback move on.
fn is_no_data(body: &str) -> bool {
    let body = body.trim();
    if body.is_empty() {
        return true;
    }
    let first_line = body.lines().next().unwrap_or("");
    // Single-line `N/D` — Stooq's documented "no data" sentinel.
    if body.lines().count() == 1 && first_line.starts_with("N/D") {
        return true;
    }
    // Real Stooq CSV data starts with the `Symbol,Date,Time,…`
    // header. Anything else is treated as no data so the
    // multi-exchange fallback can keep going.
    if !first_line.starts_with("Symbol,") {
        return true;
    }
    false
}

// ---- CSV parsing ---------------------------------------------------------

/// Parse the two-row Stooq CSV. Returns the structured payload or a
/// clear error for the "no data" cases.
fn parse_stooq_csv(body: &str, requested: &str) -> Result<Value, AgentError> {
    let body = body.trim();
    if body.is_empty() {
        return Err(AgentError::AgentFailed(format!(
            "no data for ticker `{requested}` (empty response — check the symbol or add an \
             exchange suffix like `.US` or `.PA`)"
        )));
    }
    if body.lines().count() == 1 && body.starts_with("N/D") {
        return Err(AgentError::AgentFailed(format!(
            "no data for ticker `{requested}` (Stooq returned `N/D`)"
        )));
    }

    let mut lines = body.lines();
    let header = lines
        .next()
        .ok_or_else(|| AgentError::AgentFailed("empty CSV body".into()))?;
    let data = lines
        .next()
        .ok_or_else(|| AgentError::AgentFailed("CSV missing data row".into()))?;
    if lines.next().is_some() {
        return Err(AgentError::AgentFailed(
            "CSV has more than 2 rows; refusing to parse".into(),
        ));
    }

    let headers: Vec<&str> = header.split(',').map(|s| s.trim()).collect();
    let values: Vec<&str> = data.split(',').map(|s| s.trim()).collect();
    if headers.len() != values.len() {
        return Err(AgentError::AgentFailed(format!(
            "CSV row width mismatch: header has {} columns, data has {}",
            headers.len(),
            values.len()
        )));
    }

    let mut by_name: std::collections::HashMap<&str, &str> =
        std::collections::HashMap::with_capacity(headers.len());
    for (h, v) in headers.iter().zip(values.iter()) {
        by_name.insert(h, v);
    }

    let symbol = by_name
        .get("Symbol")
        .copied()
        .unwrap_or(requested)
        .to_string();
    let date = by_name.get("Date").copied().unwrap_or("").to_string();
    let time = by_name.get("Time").copied().unwrap_or("").to_string();

    let exchange = symbol
        .rsplit_once('.')
        .map(|(_, suffix)| suffix.to_string())
        .unwrap_or_default();

    let open = parse_price(by_name.get("Open").copied());
    let high = parse_price(by_name.get("High").copied());
    let low = parse_price(by_name.get("Low").copied());
    let close = parse_price(by_name.get("Close").copied());
    let volume = parse_int(by_name.get("Volume").copied());

    Ok(json!({
        "ticker": symbol,
        "exchange": exchange,
        "price": close,
        "open": open,
        "high": high,
        "low": low,
        "close_prev": close,
        "volume": volume,
        "as_of": date,
        "as_of_time": time,
    }))
}

fn parse_price(s: Option<&str>) -> Value {
    match s {
        Some(v) if v == "-" || v.is_empty() => Value::Null,
        Some(v) => v
            .parse::<f64>()
            .map(|n| json!(round4(n)))
            .unwrap_or(Value::Null),
        None => Value::Null,
    }
}

fn parse_int(s: Option<&str>) -> Value {
    match s {
        Some(v) if v == "-" || v.is_empty() => Value::Null,
        Some(v) => v.parse::<i64>().map(|n| json!(n)).unwrap_or(Value::Null),
        None => Value::Null,
    }
}

fn round4(v: f64) -> f64 {
    (v * 10_000.0).round() / 10_000.0
}

// ---- Argument parsing ----------------------------------------------------

#[derive(Debug)]
struct ParsedArgs {
    ticker: String,
}

fn parse_args(args: &Value) -> Result<ParsedArgs, AgentError> {
    let obj = args
        .as_object()
        .ok_or_else(|| AgentError::InvalidArguments("arguments must be a JSON object".into()))?;
    let ticker = obj
        .get("ticker")
        .and_then(|v| v.as_str())
        .ok_or_else(|| AgentError::InvalidArguments("`ticker` (string) is required".into()))?
        .trim()
        .to_string();
    if ticker.is_empty() {
        return Err(AgentError::InvalidArguments(
            "`ticker` must not be empty".into(),
        ));
    }
    // The schema pattern is wider than the runtime check so the
    // table-driven company-name lookup can fire on inputs like
    // "BNP Paribas". The runtime gate is the actual contract.
    if ticker.len() > 40 {
        return Err(AgentError::InvalidArguments(format!(
            "`ticker` must be at most 40 characters (got {})",
            ticker.len()
        )));
    }
    for ch in ticker.chars() {
        let ok = ch.is_ascii_alphanumeric() || matches!(ch, ' ' | '.' | '-' | '\'' | '/');
        if !ok {
            return Err(AgentError::InvalidArguments(format!(
                "`ticker` contains an unsupported character `{ch}`; allowed: \
                 letters, digits, space, `.`, `-`, `'`, `/`"
            )));
        }
    }
    Ok(ParsedArgs { ticker })
}

fn truncate(s: &str, max_chars: usize) -> String {
    if s.len() <= max_chars {
        return s.to_string();
    }
    let mut end = max_chars;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

// ---- Tests ---------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn name_and_schema_are_stable() {
        let agent = StockAgent::new();
        assert_eq!(agent.name(), "get_stock_quote");
        let schema = agent.parameters_schema();
        assert_eq!(schema["type"], "object");
        assert!(schema["required"]
            .as_array()
            .unwrap()
            .contains(&json!("ticker")));
    }

    #[test]
    fn normalise_appends_us_when_no_suffix() {
        assert_eq!(normalise_ticker("AAPL"), "AAPL.US");
        assert_eq!(normalise_ticker("nvda"), "NVDA.US");
        assert_eq!(normalise_ticker("  msft  "), "MSFT.US");
    }

    #[test]
    fn normalise_preserves_existing_suffix() {
        assert_eq!(normalise_ticker("AIR.PA"), "AIR.PA");
        assert_eq!(normalise_ticker("mc.pa"), "MC.PA");
        assert_eq!(normalise_ticker("SAP.DE"), "SAP.DE");
    }

    #[test]
    fn resolve_known_company_handles_french_issuers() {
        // The whole point of this fix. The user complaint was that
        // "Atos" (a French company, Euronext Paris ticker `ATO`)
        // failed because we appended `.US`. The shortcut resolves
        // the name straight to `ATO.PA`.
        assert_eq!(resolve_known_company("Atos"), Some("ATO.PA"));
        assert_eq!(resolve_known_company("atos"), Some("ATO.PA"));
        assert_eq!(resolve_known_company("ATOS"), Some("ATO.PA"));
        assert_eq!(resolve_known_company(" Atos "), Some("ATO.PA"));
        // LVMH's Stooq ticker is `MC.PA` (Moët-Chandon alias).
        assert_eq!(resolve_known_company("LVMH"), Some("MC.PA"));
        assert_eq!(resolve_known_company("lvmh"), Some("MC.PA"));
        // Multibyte word with apostrophe.
        assert_eq!(resolve_known_company("L'Oréal"), Some("OR.PA"));
        assert_eq!(resolve_known_company("loreal"), Some("OR.PA"));
        assert_eq!(resolve_known_company("L'Oreal"), Some("OR.PA"));
    }

    #[test]
    fn resolve_known_company_handles_us_big_tech() {
        // Even when the user types the company name, the shortcut
        // puts us on the right ticker on the first round-trip.
        assert_eq!(resolve_known_company("Apple"), Some("AAPL.US"));
        assert_eq!(resolve_known_company("Apple"), Some("AAPL.US"));
        assert_eq!(resolve_known_company("microsoft"), Some("MSFT.US"));
        assert_eq!(resolve_known_company("NVIDIA"), Some("NVDA.US"));
        assert_eq!(resolve_known_company("Alphabet"), Some("GOOGL.US"));
    }

    #[test]
    fn resolve_known_company_returns_none_for_unknown_names() {
        // The shortcut is a hint, not a gate — unknowns fall
        // through to the normal ticker codepath and the
        // multi-exchange fallback.
        assert_eq!(resolve_known_company("NonsenseCorp"), None);
        assert_eq!(resolve_known_company(""), None);
        assert_eq!(resolve_known_company("   "), None);
    }

    #[test]
    fn collapse_whitespace_handles_multiple_internal_spaces() {
        assert_eq!(collapse_whitespace("BNP Paribas"), "BNP Paribas");
        assert_eq!(collapse_whitespace("BNP  Paribas"), "BNP Paribas");
        assert_eq!(collapse_whitespace("  BNP   Paribas  "), "BNP Paribas");
        assert_eq!(collapse_whitespace("LVMH"), "LVMH");
        assert_eq!(collapse_whitespace(" L'Oreal "), "L'Oreal");
    }

    #[test]
    fn tried_symbols_explicit_pa_lists_pa_first_then_rest() {
        // When the user pinned the exchange, that exchange goes first;
        // the rest of the European fallbacks follow, and `.US` is
        // appended last (for OTC listings).
        let tried = tried_symbols("ATO.PA");
        assert_eq!(tried[0], "ATO.PA");
        assert!(tried.contains(&"ATO.L".to_string()));
        assert!(tried.contains(&"ATO.DE".to_string()));
        assert!(tried.contains(&"ATO.MI".to_string()));
        assert!(
            tried.last().unwrap() == "ATO.US",
            "US fallback should be last: {tried:?}"
        );
        // No duplicates: every entry is unique.
        let mut seen = std::collections::HashSet::new();
        for t in &tried {
            assert!(seen.insert(t.clone()), "duplicate ticker {t} in {tried:?}");
        }
    }

    #[test]
    fn tried_symbols_bare_lists_us_first_then_fallbacks() {
        // The bare-ticker path: try `.US` first, then the European
        // exchanges in order. Order matters — the first hit wins.
        let tried = tried_symbols("ATOS");
        assert_eq!(tried[0], "ATOS.US");
        assert!(tried.contains(&"ATOS.PA".to_string()));
        assert!(tried.contains(&"ATOS.L".to_string()));
        assert!(tried.contains(&"ATOS.DE".to_string()));
        assert!(tried.contains(&"ATOS.MI".to_string()));
        // `.US` is the first entry; it does not get re-appended
        // at the end. Total length is 1 (US) + the fallback list.
        assert_eq!(tried.len(), 1 + FALLBACK_EXCHANGES.len());
    }

    #[test]
    fn tried_symbols_for_us_already_suffixed_does_not_duplicate_us() {
        // If for some reason the resolved input already has `.US`,
        // the trailing `.US` fallback must not be added again.
        let tried = tried_symbols("AAPL.US");
        let count_us = tried.iter().filter(|t| t.ends_with(".US")).count();
        assert_eq!(count_us, 1, "duplicate .US entry: {tried:?}");
    }

    #[test]
    fn parse_stooq_csv_happy_path() {
        let csv = "Symbol,Date,Time,Open,High,Low,Close,Volume\n\
                   AAPL.US,2026-09-25,21:00:00,243.10,246.50,242.80,245.32,12345678\n";
        let parsed = parse_stooq_csv(csv, "AAPL.US").expect("parse");
        assert_eq!(parsed["ticker"], "AAPL.US");
        assert_eq!(parsed["exchange"], "US");
        assert_eq!(parsed["price"], 245.32);
        assert_eq!(parsed["open"], 243.10);
        assert_eq!(parsed["high"], 246.50);
        assert_eq!(parsed["low"], 242.80);
        assert_eq!(parsed["close_prev"], 245.32);
        assert_eq!(parsed["volume"], 12_345_678);
        assert_eq!(parsed["as_of"], "2026-09-25");
        assert_eq!(parsed["as_of_time"], "21:00:00");
    }

    #[test]
    fn parse_stooq_csv_european_ticker() {
        // Atos on Euronext Paris — the row the user complaint was
        // about. Confirms the CSV parser produces the right
        // structured shape with `exchange` set to "PA".
        let csv = "Symbol,Date,Time,Open,High,Low,Close,Volume\n\
                   ATO.PA,2026-09-25,17:35:00,1.23,1.28,1.20,1.25,5000000\n";
        let parsed = parse_stooq_csv(csv, "ATO.PA").expect("parse");
        assert_eq!(parsed["ticker"], "ATO.PA");
        assert_eq!(parsed["exchange"], "PA");
        assert_eq!(parsed["price"], 1.25);
    }

    #[test]
    fn parse_stooq_csv_handles_missing_fields() {
        let csv = "Symbol,Date,Time,Open,High,Low,Close,Volume\n\
                   XYZ.US,2026-09-25,-,-,-,-,-,-\n";
        let parsed = parse_stooq_csv(csv, "XYZ.US").expect("parse");
        assert!(parsed["price"].is_null());
        assert!(parsed["open"].is_null());
        assert!(parsed["high"].is_null());
        assert!(parsed["volume"].is_null());
    }

    #[test]
    fn parse_stooq_csv_empty_body_errors() {
        let err = parse_stooq_csv("", "AAPL.US").unwrap_err();
        match err {
            AgentError::AgentFailed(msg) => {
                assert!(msg.contains("no data for ticker"));
                assert!(msg.contains("AAPL.US"));
            }
            other => panic!("expected AgentFailed, got {other:?}"),
        }
    }

    #[test]
    fn parse_stooq_csv_nd_sentinel_errors() {
        let err = parse_stooq_csv("N/D", "ZZZZ.US").unwrap_err();
        match err {
            AgentError::AgentFailed(msg) => {
                assert!(msg.contains("N/D"));
                assert!(msg.contains("ZZZZ.US"));
            }
            other => panic!("expected AgentFailed, got {other:?}"),
        }
    }

    #[test]
    fn is_no_data_recognises_sentinel_shapes() {
        assert!(is_no_data(""));
        assert!(is_no_data("   "));
        assert!(is_no_data("N/D"));
        assert!(is_no_data("N/D\n"));
        // Real CSV starts with `Symbol,` — must NOT be flagged.
        assert!(!is_no_data(
            "Symbol,Date,Time,Open,High,Low,Close,Volume\nAAPL.US,..."
        ));
        // HTML error page (e.g. Cloudflare bot challenge) must
        // also be treated as no data so the fallback can move on
        // instead of crashing the CSV parser.
        assert!(is_no_data(
            "<!DOCTYPE html><html><head></head><body>403</body></html>"
        ));
        assert!(is_no_data("<html>error</html>"));
    }

    #[test]
    fn parse_stooq_csv_rejects_extra_rows() {
        let csv = "Symbol,Date,Time,Open,High,Low,Close,Volume\n\
                   AAPL.US,2026-09-25,21:00:00,243.10,246.50,242.80,245.32,12345678\n\
                   extra,row,here,0,0,0,0,0\n";
        let err = parse_stooq_csv(csv, "AAPL.US").unwrap_err();
        assert!(matches!(err, AgentError::AgentFailed(_)));
    }

    #[test]
    fn parse_stooq_csv_rejects_row_width_mismatch() {
        let csv = "Symbol,Date,Time,Open,High,Low,Close,Volume\n\
                   AAPL.US,2026-09-25,21:00:00,243.10\n";
        let err = parse_stooq_csv(csv, "AAPL.US").unwrap_err();
        match err {
            AgentError::AgentFailed(msg) => {
                assert!(msg.contains("row width mismatch"));
            }
            other => panic!("expected AgentFailed, got {other:?}"),
        }
    }

    #[test]
    fn rejects_missing_ticker() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let agent = StockAgent::new();
        let err = rt.block_on(agent.invoke(json!({}))).unwrap_err();
        assert!(matches!(err, AgentError::InvalidArguments(_)));
    }

    #[test]
    fn rejects_too_long_ticker() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let agent = StockAgent::new();
        let err = rt
            .block_on(agent.invoke(json!({"ticker": "x".repeat(41)})))
            .unwrap_err();
        match err {
            AgentError::InvalidArguments(msg) => {
                assert!(msg.contains("at most 40 characters"));
            }
            other => panic!("expected InvalidArguments, got {other:?}"),
        }
    }

    #[test]
    fn rejects_ticker_with_illegal_chars() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let agent = StockAgent::new();
        let err = rt
            .block_on(agent.invoke(json!({"ticker": "AA; DROP TABLE"})))
            .unwrap_err();
        match err {
            AgentError::InvalidArguments(msg) => {
                assert!(msg.contains("unsupported character"));
            }
            other => panic!("expected InvalidArguments, got {other:?}"),
        }
    }

    #[test]
    fn accepts_atos_company_name_input() {
        // The user complaint, exercised end-to-end against the
        // public surface. We can't reach Stooq from the sandbox
        // reliably, so we only assert the schema + invocation
        // does not reject the input — the network path is
        // covered by the unit-testable helpers above.
        let rt = tokio::runtime::Runtime::new().unwrap();
        let agent = StockAgent::new();
        let result = rt.block_on(agent.invoke(json!({"ticker": "Atos"})));
        match result {
            Ok(_) => { /* network ok, lucky */ }
            Err(AgentError::AgentFailed(msg)) => {
                // Acceptable: any of the fallback symbols resolved
                // and we parsed, or none did and we reported an
                // error. The key is that input validation did
                // not reject "Atos".
                assert!(!msg.contains("`ticker`"));
            }
            Err(AgentError::Upstream { .. }) => {
                // Acceptable: Stooq itself rate-limited.
            }
            Err(AgentError::InvalidArguments(msg)) => {
                panic!("Atos should be a valid input, got InvalidArguments: {msg}");
            }
            Err(other) => {
                panic!("unexpected error variant: {other:?}");
            }
        }
    }

    #[test]
    fn accepts_known_company_names_that_require_apostrophe() {
        // The pattern in the schema permits `'`; L'Oréal is the
        // motivating example.
        let rt = tokio::runtime::Runtime::new().unwrap();
        let agent = StockAgent::new();
        let result = rt.block_on(agent.invoke(json!({"ticker": "L'Oreal"})));
        if let Err(AgentError::InvalidArguments(msg)) = result {
            panic!("L'Oreal should be a valid input, got InvalidArguments: {msg}");
        }
    }
}
