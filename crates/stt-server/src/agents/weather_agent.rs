//! `get_weather` agent: current weather, short-term forecast, and
//! recent historical weather for a location.
//!
//! The agent is wired to three free, no-API-key Open-Meteo endpoints:
//!
//! 1. Geocoding (`https://geocoding-api.open-meteo.com/v1/search`) —
//!    resolves a city name to `(lat, lon, country)`.
//! 2. Forecast (`https://api.open-meteo.com/v1/forecast`) — returns
//!    current conditions plus up to 7 days of daily summaries.
//! 3. Archive (`https://archive-api.open-meteo.com/v1/archive`) —
//!    returns historical daily summaries back to 1940-01-01.
//!
//! ## API shape
//!
//! The LLM picks one of three modes by parameter combination:
//!
//! - `{"location": "Paris"}` → current conditions + today's forecast
//!   (default when no `date` or `days` is supplied).
//! - `{"location": "Paris", "days": 3}` → current + 3-day forecast.
//! - `{"location": "Paris", "date": "2026-09-26"}` → single day,
//!   either forecast (when in [today, today+7]) or archive (when in
//!   the past or before today's UTC date). The location's local
//!   timezone is honoured by Open-Meteo's `timezone=auto` so the
//!   LLM-supplied date is interpreted in the location's calendar.
//!
//! ## No-cache decision (v1)
//!
//! All three endpoints tolerate ~10 req/s per IP for unauthenticated
//! use, which is comfortably above what a single chat user will
//! produce. If a deployment starts hitting the limit, a TTL cache
//! layer can be added later — the agent's wire shape does not need
//! to change.

use std::time::Duration;

use async_trait::async_trait;
use chrono::{NaiveDate, Utc};
use serde_json::{json, Value};

use crate::agents::{Agent, AgentError};

/// Hard cap on the response body the agent will read from each
/// upstream call. Open-Meteo's payloads are small (a few KB), but
/// the cap exists so a misbehaving upstream cannot exhaust memory.
const MAX_UPSTREAM_BYTES: usize = 64 * 1024;

/// Default forecast horizon when the LLM does not specify `days`
/// or `date`.
const DEFAULT_FORECAST_DAYS: u32 = 1;

/// Maximum forecast horizon. The Open-Meteo free tier caps at 16 but
/// we trim to 7 to keep the tool-result payload bounded — a one-week
/// forecast is the largest answer that still fits comfortably in a
/// chat bubble. Single-day `date` queries past this window go to
/// the archive endpoint instead.
const MAX_FORECAST_DAYS: u32 = 7;

/// HTTP per-request timeout. Open-Meteo responses are sub-second;
/// 8 s leaves plenty of headroom for the geocoding + forecast
/// pair while still bounding the worst case.
const HTTP_TIMEOUT: Duration = Duration::from_secs(8);

/// Mode chosen from the LLM-supplied arguments before any HTTP
/// round-trip. Captured here so `invoke` keeps a single branch site
/// and the URL builder can be unit-tested with a `Mode` directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// No `date` and no `days`: current conditions only. Equivalent
    /// to a 1-day forecast horizon that includes `current`.
    Current { days: u32 },
    /// A specific calendar day. The URL builder picks the forecast
    /// or archive endpoint based on whether `date` is in the past,
    /// today, or the next 7 days.
    SingleDate { date: NaiveDate },
}

#[derive(Clone)]
pub struct WeatherAgent {
    http: reqwest::Client,
}

impl std::fmt::Debug for WeatherAgent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WeatherAgent")
            .field("http", &"<reqwest::Client>")
            .finish()
    }
}

impl Default for WeatherAgent {
    fn default() -> Self {
        Self::new()
    }
}

impl WeatherAgent {
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
impl Agent for WeatherAgent {
    fn name(&self) -> &str {
        "get_weather"
    }

    fn description(&self) -> &str {
        "Météo actuelle, prévisions jusqu'à 7 jours, ou météo historique pour un lieu donné. \
         Accepte un nom de ville (Paris, Tokyo) ou des coordonnées (48.85,2.35). \
         Aucune clé d'API requise (Open-Meteo). \
         Use for 'météo à Paris', 'temps qu'il fera demain à Tokyo', \
         'météo à Lyon la semaine dernière', 'will it rain in London'. \
         Pass `location` (required). Optionally pass `days` (1-7, future forecast), \
         or `date` (YYYY-MM-DD, past or future single day)."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "location": {
                    "type": "string",
                    "description": "City name (e.g. 'Paris', 'Tokyo') or 'lat,lon' (e.g. '48.85,2.35')."
                },
                "days": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 7,
                    "default": 1,
                    "description": "Forecast horizon in days, capped at 7. Ignored when `date` is set."
                },
                "date": {
                    "type": "string",
                    "pattern": "^\\d{4}-\\d{2}-\\d{2}$",
                    "description": "Specific day in YYYY-MM-DD. Past dates use the archive endpoint; future dates (up to 7 days ahead) use the forecast endpoint. Interpreted in the location's local timezone. Mutually exclusive with `days`."
                }
            },
            "required": ["location"],
            "additionalProperties": false,
        })
    }

    async fn invoke(&self, args: Value) -> Result<String, AgentError> {
        let req = parse_args(&args)?;
        let (lat, lon, display_name) = resolve_location(&self.http, &req.location).await?;

        let mode = select_mode(req.days, req.date.as_deref())?;
        let url = build_url(mode, lat, lon);

        let body = fetch_json(&self.http, &url).await?;
        let payload = build_payload(&body, lat, lon, &display_name, mode)?;

        let source = match mode {
            Mode::SingleDate { date } if date < today_utc() => "open-meteo-archive",
            _ => "open-meteo",
        };

        Ok(serde_json::to_string(&json!({
            "ok": true,
            "data": payload,
            "source": source,
            "fetched_at": chrono::Utc::now().to_rfc3339(),
        }))
        .expect("json encode"))
    }
}

// ---- Mode selection ------------------------------------------------------

/// Pick the request mode from the LLM-supplied `days` + `date`. The
/// precedence is:
///
/// 1. `date` set → [`Mode::SingleDate`]. `days` is ignored (the LLM
///    asked for a specific day).
/// 2. `days` set → [`Mode::Current`] with the supplied horizon
///    (clamped to 1..=7).
/// 3. Neither → [`Mode::Current`] with the default 1-day horizon.
fn select_mode(days: Option<u32>, date: Option<&str>) -> Result<Mode, AgentError> {
    if let Some(d) = date {
        let parsed = NaiveDate::parse_from_str(d, "%Y-%m-%d").map_err(|e| {
            AgentError::InvalidArguments(format!(
                "`date` must be a valid YYYY-MM-DD calendar date (got `{d}`: {e})"
            ))
        })?;
        let today = today_utc();
        let max_future = today
            .checked_add_signed(chrono::Duration::days(MAX_FORECAST_DAYS as i64))
            .ok_or_else(|| {
                AgentError::AgentFailed("internal: date arithmetic overflow".into())
            })?;
        // NaiveDate is good through year 9999 so the early bound
        // (1940-01-01, Open-Meteo archive start) is the only one
        // worth checking — and even that is enforced by Open-Meteo
        // returning a 4xx, but we reject it cleanly so the LLM gets
        // a friendly message.
        let archive_start = NaiveDate::from_ymd_opt(1940, 1, 1)
            .expect("constant date is well-formed");
        if parsed < archive_start {
            return Err(AgentError::InvalidArguments(format!(
                "`date` must be on or after {archive_start} (Open-Meteo archive start), got `{d}`"
            )));
        }
        if parsed > max_future {
            return Err(AgentError::InvalidArguments(format!(
                "`date` must be at most {} days in the future (forecast horizon); got `{d}`",
                MAX_FORECAST_DAYS
            )));
        }
        return Ok(Mode::SingleDate { date: parsed });
    }
    let horizon = days
        .unwrap_or(DEFAULT_FORECAST_DAYS)
        .clamp(1, MAX_FORECAST_DAYS);
    Ok(Mode::Current { days: horizon })
}

/// Today as a UTC date. Used to decide whether a `date` argument
/// belongs to the forecast or archive endpoint. The location's
/// local calendar may differ by a few hours; the LLM is expected
/// to pass dates in the location's timezone, and Open-Meteo's
/// `timezone=auto` honours that on its side.
fn today_utc() -> NaiveDate {
    Utc::now().date_naive()
}

// ---- URL builder ---------------------------------------------------------

fn build_url(mode: Mode, lat: f64, lon: f64) -> String {
    match mode {
        Mode::Current { days } => format!(
            "https://api.open-meteo.com/v1/forecast\
             ?latitude={lat:.4}\
             &longitude={lon:.4}\
             &current=temperature_2m,wind_speed_10m,weather_code\
             &daily=temperature_2m_max,temperature_2m_min,weather_code\
             &forecast_days={days}\
             &timezone=auto"
        ),
        Mode::SingleDate { date } => {
            let today = today_utc();
            if date <= today {
                // Past date (or today): the archive endpoint serves
                // historical daily summaries. `current` is not in the
                // archive response, so we skip the parameter; the
                // payload builder sees a body without `current`.
                format!(
                    "https://archive-api.open-meteo.com/v1/archive\
                     ?latitude={lat:.4}\
                     &longitude={lon:.4}\
                     &daily=temperature_2m_max,temperature_2m_min,weather_code\
                     &start_date={date}\
                     &end_date={date}\
                     &timezone=auto"
                )
            } else {
                // Future date in the forecast window: same forecast
                // endpoint, but with an explicit single-day range
                // instead of `forecast_days`. The response carries
                // `daily` for that day but no `current`.
                format!(
                    "https://api.open-meteo.com/v1/forecast\
                     ?latitude={lat:.4}\
                     &longitude={lon:.4}\
                     &current=temperature_2m,wind_speed_10m,weather_code\
                     &daily=temperature_2m_max,temperature_2m_min,weather_code\
                     &start_date={date}\
                     &end_date={date}\
                     &timezone=auto"
                )
            }
        }
    }
}

// ---- Location resolution -------------------------------------------------

/// Manual `lat,lon` parser. We avoid pulling in `regex` for a
/// five-byte grammar; a manual parse is faster and keeps the dep
/// tree slim.
fn try_parse_lat_lon(s: &str) -> Option<(f64, f64)> {
    let s = s.trim();
    let (lat_s, lon_s) = s.split_once(',')?;
    let lat: f64 = lat_s.trim().parse().ok()?;
    let lon: f64 = lon_s.trim().parse().ok()?;
    if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lon) {
        return None;
    }
    Some((lat, lon))
}

async fn resolve_location(
    http: &reqwest::Client,
    raw: &str,
) -> Result<(f64, f64, String), AgentError> {
    if let Some((lat, lon)) = try_parse_lat_lon(raw) {
        return Ok((lat, lon, raw.trim().to_string()));
    }
    let name = raw.trim();
    if name.is_empty() {
        return Err(AgentError::InvalidArguments(
            "`location` must be a city name or 'lat,lon'".into(),
        ));
    }
    // Open-Meteo sorts results by population score; `count=1` plus the
    // population ranking gives the most relevant Paris/Tokyo/… for the
    // supplied language. `language=fr` keeps French queries matching
    // French city names.
    let encoded = url_encode(name);
    let url = format!(
        "https://geocoding-api.open-meteo.com/v1/search?name={encoded}&language=fr&count=1"
    );
    let body = fetch_json(http, &url).await?;
    let results = body.get("results").and_then(|v| v.as_array());
    let first = results.and_then(|arr| arr.first()).ok_or_else(|| {
        AgentError::InvalidArguments(format!(
            "no geocoding match for `{name}` (try a different spelling or pass `lat,lon` directly)"
        ))
    })?;
    let lat = first
        .get("latitude")
        .and_then(|v| v.as_f64())
        .ok_or_else(|| AgentError::AgentFailed("geocoding result missing latitude".into()))?;
    let lon = first
        .get("longitude")
        .and_then(|v| v.as_f64())
        .ok_or_else(|| AgentError::AgentFailed("geocoding result missing longitude".into()))?;
    let resolved_name = first
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or(name)
        .to_string();
    let country = first
        .get("country")
        .and_then(|v| v.as_str())
        .map(|c| format!(" ({c})"))
        .unwrap_or_default();
    Ok((lat, lon, format!("{resolved_name}{country}")))
}

// ---- HTTP helper ---------------------------------------------------------

async fn fetch_json(http: &reqwest::Client, url: &str) -> Result<Value, AgentError> {
    let resp = http
        .get(url)
        .header(
            reqwest::header::USER_AGENT,
            "nagent-weather-agent/0.1 (+https://github.com/nagent/nagent)",
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
    serde_json::from_slice(&bytes)
        .map_err(|e| AgentError::AgentFailed(format!("upstream JSON parse: {e}")))
}

// ---- Payload shaping ------------------------------------------------------

fn build_payload(
    body: &Value,
    lat: f64,
    lon: f64,
    display_name: &str,
    mode: Mode,
) -> Result<Value, AgentError> {
    let daily = build_daily(body)?;

    // The forecast endpoint with `current=…` always returns a
    // `current` block; the archive endpoint and the future-date
    // forecast variant (single-day range, no `current` requested)
    // do not. Probe by presence so the two response shapes round-
    // trip through one builder.
    let current = if let Some(c) = body.get("current") {
        Some(json!({
            "temp_c": round1(
                c.get("temperature_2m")
                    .and_then(|v| v.as_f64())
                    .ok_or_else(|| AgentError::AgentFailed(
                        "missing current.temperature_2m".into(),
                    ))?
            ),
            "wind_kmh": round1(
                c.get("wind_speed_10m")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0)
            ),
            "condition": wmo_to_condition(
                c.get("weather_code")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(-1) as i32
            ),
            "as_of": c
                .get("time")
                .and_then(|v| v.as_str())
                .unwrap_or(""),
        }))
    } else {
        None
    };

    // For single-date requests, surface the date we asked for at
    // the top level so the LLM does not have to scan the `daily`
    // array to figure out what was requested — and so a no-data
    // response (upstream returned no `daily` block) is still
    // attributable to the right date.
    let requested_date = match mode {
        Mode::SingleDate { date } => Some(date.to_string()),
        Mode::Current { .. } => None,
    };

    Ok(json!({
        "location": {
            "name": display_name,
            "latitude": round4(lat),
            "longitude": round4(lon),
        },
        "current": current,
        "requested_date": requested_date,
        "daily": daily,
    }))
}

fn build_daily(body: &Value) -> Result<Value, AgentError> {
    let d = match body.get("daily").and_then(|v| v.as_object()) {
        Some(d) => d,
        None => return Ok(json!([])),
    };
    let dates = d.get("date").and_then(|v| v.as_array());
    let maxes = d.get("temperature_2m_max").and_then(|v| v.as_array());
    let mins = d.get("temperature_2m_min").and_then(|v| v.as_array());
    let codes = d.get("weather_code").and_then(|v| v.as_array());
    let (Some(dates), Some(maxes), Some(mins), Some(codes)) = (dates, maxes, mins, codes) else {
        return Ok(json!([]));
    };
    let n = dates
        .len()
        .min(maxes.len())
        .min(mins.len())
        .min(codes.len());

    let mut entries: Vec<Value> = Vec::with_capacity(n);
    for i in 0..n {
        let date = dates[i].as_str().unwrap_or("").to_string();
        let t_max = maxes[i].as_f64().unwrap_or(0.0);
        let t_min = mins[i].as_f64().unwrap_or(0.0);
        let c = codes[i].as_i64().unwrap_or(-1) as i32;
        entries.push(json!({
            "date": date,
            "t_min_c": round1(t_min),
            "t_max_c": round1(t_max),
            "condition": wmo_to_condition(c),
        }));
    }
    Ok(json!(entries))
}

// ---- WMO weather code mapping -------------------------------------------

/// Map WMO weather codes (Open-Meteo's `weather_code` field) to short,
/// stable condition strings. The codes are defined by the WMO; we only
/// cover the ones Open-Meteo actually emits across its endpoints,
/// plus an `unknown` fallback for any future addition.
///
/// See <https://open-meteo.com/en/docs> (WMO Weather interpretation
/// codes).
fn wmo_to_condition(code: i32) -> &'static str {
    match code {
        0 => "clear",
        1 => "mainly_clear",
        2 => "partly_cloudy",
        3 => "cloudy",
        45 | 48 => "fog",
        51 | 53 | 55 => "drizzle",
        56 | 57 => "freezing_drizzle",
        61 => "light_rain",
        63 => "rain",
        65 => "heavy_rain",
        66 | 67 => "freezing_rain",
        71 => "light_snow",
        73 => "snow",
        75 => "heavy_snow",
        77 => "snow_grains",
        80 => "rain_showers",
        81 => "heavy_rain_showers",
        82 => "violent_rain_showers",
        85 => "snow_showers",
        86 => "heavy_snow_showers",
        95 => "thunderstorm",
        96 | 99 => "thunderstorm_with_hail",
        _ => "unknown",
    }
}

// ---- Number formatting ---------------------------------------------------

fn round1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

fn round4(v: f64) -> f64 {
    (v * 10_000.0).round() / 10_000.0
}

// ---- Argument parsing ----------------------------------------------------

#[derive(Debug)]
struct ParsedArgs {
    location: String,
    days: Option<u32>,
    date: Option<String>,
}

fn parse_args(args: &Value) -> Result<ParsedArgs, AgentError> {
    let obj = args
        .as_object()
        .ok_or_else(|| AgentError::InvalidArguments("arguments must be a JSON object".into()))?;
    let location = obj
        .get("location")
        .and_then(|v| v.as_str())
        .ok_or_else(|| AgentError::InvalidArguments("`location` (string) is required".into()))?
        .trim()
        .to_string();
    if location.is_empty() {
        return Err(AgentError::InvalidArguments(
            "`location` must not be empty".into(),
        ));
    }
    let days = obj.get("days").and_then(|v| v.as_u64()).map(|v| v as u32);
    let date = obj
        .get("date")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    Ok(ParsedArgs {
        location,
        days,
        date,
    })
}

// ---- Misc helpers --------------------------------------------------------

/// Minimal URL form-encoder. We avoid `url::form_urlencoded` to keep
/// the `url` crate from leaking into this module's public surface
/// for a one-call usage — and the inputs here are city names, not
/// untrusted free-form strings.
fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            // Spaces are common in city names; encode as `+` like
            // `application/x-www-form-urlencoded`.
            b' ' => out.push('+'),
            // Anything else becomes a percent-encoded triplet.
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
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

    #[test]
    fn name_and_schema_are_stable() {
        let agent = WeatherAgent::new();
        assert_eq!(agent.name(), "get_weather");
        let schema = agent.parameters_schema();
        assert_eq!(schema["type"], "object");
        assert!(schema["required"]
            .as_array()
            .unwrap()
            .contains(&json!("location")));
        assert_eq!(schema["properties"]["days"]["maximum"], 7);
        // The `date` parameter is a wire-contract addition: regression
        // guard its shape so a rename forces a deliberate change.
        let date = &schema["properties"]["date"];
        assert_eq!(date["type"], "string");
        assert_eq!(date["pattern"], r"^\d{4}-\d{2}-\d{2}$");
    }

    #[test]
    fn lat_lon_short_form_parses() {
        assert_eq!(try_parse_lat_lon("48.85,2.35"), Some((48.85, 2.35)));
        assert_eq!(
            try_parse_lat_lon("  -33.86,151.21  "),
            Some((-33.86, 151.21))
        );
        assert_eq!(try_parse_lat_lon("48.85"), None);
        assert_eq!(try_parse_lat_lon("foo,bar"), None);
        assert_eq!(try_parse_lat_lon("91,0"), None);
        assert_eq!(try_parse_lat_lon("0,181"), None);
    }

    #[test]
    fn days_clamped_to_seven() {
        assert_eq!(MAX_FORECAST_DAYS, 7);
        assert_eq!(DEFAULT_FORECAST_DAYS, 1);
    }

    #[test]
    fn select_mode_defaults_to_current_one_day() {
        // No `days`, no `date` → 1-day forecast horizon.
        assert_eq!(select_mode(None, None).unwrap(), Mode::Current { days: 1 });
    }

    #[test]
    fn select_mode_clamps_days_to_seven() {
        assert_eq!(
            select_mode(Some(99), None).unwrap(),
            Mode::Current { days: MAX_FORECAST_DAYS }
        );
        assert_eq!(select_mode(Some(0), None).unwrap(), Mode::Current { days: 1 });
    }

    #[test]
    fn select_mode_date_takes_precedence_over_days() {
        // When both are supplied, `date` wins. The LLM should not
        // pass both at once; if they did we honour the specific-day
        // intent because it is the more specific request.
        let mode = select_mode(Some(3), Some("2024-06-15")).unwrap();
        assert!(matches!(mode, Mode::SingleDate { .. }));
    }

    #[test]
    fn select_mode_rejects_invalid_date_format() {
        let err = select_mode(None, Some("15-06-2024")).unwrap_err();
        match err {
            AgentError::InvalidArguments(msg) => {
                assert!(msg.contains("YYYY-MM-DD"));
            }
            other => panic!("expected InvalidArguments, got {other:?}"),
        }
        let err = select_mode(None, Some("not-a-date")).unwrap_err();
        assert!(matches!(err, AgentError::InvalidArguments(_)));
    }

    #[test]
    fn select_mode_rejects_pre_archive_date() {
        let err = select_mode(None, Some("1939-12-31")).unwrap_err();
        match err {
            AgentError::InvalidArguments(msg) => {
                assert!(msg.contains("1940-01-01"));
            }
            other => panic!("expected InvalidArguments, got {other:?}"),
        }
    }

    #[test]
    fn select_mode_rejects_date_more_than_seven_days_in_future() {
        // Pick a date that's safely > MAX_FORECAST_DAYS ahead by
        // computing the bound from a frozen "today". We use the
        // agent's own `today_utc` + MAX_FORECAST_DAYS, so the test
        // doesn't drift over time.
        let far = today_utc()
            .checked_add_signed(chrono::Duration::days((MAX_FORECAST_DAYS as i64) + 1))
            .unwrap();
        let err = select_mode(None, Some(&far.to_string())).unwrap_err();
        match err {
            AgentError::InvalidArguments(msg) => {
                assert!(msg.contains("at most") && msg.contains("days in the future"));
            }
            other => panic!("expected InvalidArguments, got {other:?}"),
        }
    }

    #[test]
    fn select_mode_accepts_yesterday() {
        let yesterday = today_utc()
            .checked_sub_signed(chrono::Duration::days(1))
            .unwrap();
        let mode = select_mode(None, Some(&yesterday.to_string())).unwrap();
        // Past dates land in `SingleDate`; the URL builder then
        // picks the archive endpoint.
        assert!(matches!(mode, Mode::SingleDate { .. }));
    }

    #[test]
    fn build_url_archive_for_past_date() {
        // Pick any fixed past date so the test is deterministic
        // regardless of the wall clock.
        let past = NaiveDate::from_ymd_opt(2024, 6, 15).unwrap();
        let url = build_url(Mode::SingleDate { date: past }, 48.85, 2.35);
        assert!(
            url.contains("archive-api.open-meteo.com"),
            "past date should hit the archive endpoint, got: {url}"
        );
        assert!(url.contains("start_date=2024-06-15"));
        assert!(url.contains("end_date=2024-06-15"));
    }

    #[test]
    fn build_url_forecast_range_for_future_date() {
        let future = today_utc()
            .checked_add_signed(chrono::Duration::days(3))
            .unwrap();
        let url = build_url(Mode::SingleDate { date: future }, 48.85, 2.35);
        assert!(
            url.contains("api.open-meteo.com"),
            "future date should hit the forecast endpoint, got: {url}"
        );
        assert!(url.contains(&format!("start_date={future}")));
        assert!(url.contains(&format!("end_date={future}")));
    }

    #[test]
    fn build_url_current_uses_forecast_days() {
        let url = build_url(Mode::Current { days: 3 }, 48.85, 2.35);
        assert!(url.contains("forecast_days=3"));
        assert!(!url.contains("start_date"));
    }

    #[test]
    fn wmo_codes_cover_common_conditions() {
        assert_eq!(wmo_to_condition(0), "clear");
        assert_eq!(wmo_to_condition(3), "cloudy");
        assert_eq!(wmo_to_condition(61), "light_rain");
        assert_eq!(wmo_to_condition(95), "thunderstorm");
        assert_eq!(wmo_to_condition(-1), "unknown");
        assert_eq!(wmo_to_condition(999), "unknown");
    }

    #[test]
    fn url_encoder_handles_spaces_and_unicode() {
        assert_eq!(url_encode("Paris"), "Paris");
        assert_eq!(url_encode("Le Havre"), "Le+Havre");
        let encoded = url_encode("Sao Paulo");
        assert_eq!(encoded, "Sao+Paulo");
    }

    #[test]
    fn rejects_missing_location() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let agent = WeatherAgent::new();
        let err = rt.block_on(agent.invoke(json!({}))).unwrap_err();
        assert!(matches!(err, AgentError::InvalidArguments(_)));
    }

    #[test]
    fn rejects_empty_location() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let agent = WeatherAgent::new();
        let err = rt
            .block_on(agent.invoke(json!({"location": "   "})))
            .unwrap_err();
        assert!(matches!(err, AgentError::InvalidArguments(_)));
    }

    #[test]
    fn rejects_malformed_date() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let agent = WeatherAgent::new();
        let err = rt
            .block_on(agent.invoke(json!({"location": "Paris", "date": "2024/06/15"})))
            .unwrap_err();
        match err {
            AgentError::InvalidArguments(msg) => {
                assert!(msg.contains("YYYY-MM-DD"));
            }
            other => panic!("expected InvalidArguments, got {other:?}"),
        }
    }

    #[test]
    fn build_payload_shapes_current_and_daily() {
        // A canned Open-Meteo forecast payload. We only assert the
        // shape — the actual numeric values come from upstream and
        // are not part of the wire contract we are testing here.
        let body = json!({
            "current": {
                "temperature_2m": 18.4,
                "wind_speed_10m": 12.1,
                "weather_code": 2,
                "time": "2026-09-25T21:00"
            },
            "daily": {
                "date": ["2026-09-26", "2026-09-27"],
                "temperature_2m_max": [19.0, 17.0],
                "temperature_2m_min": [12.0, 11.5],
                "weather_code": [3, 61]
            }
        });
        let payload = build_payload(&body, 48.85, 2.35, "Paris (FR)", Mode::Current { days: 2 }).unwrap();
        assert_eq!(payload["location"]["name"], "Paris (FR)");
        assert_eq!(payload["current"]["temp_c"], 18.4);
        assert_eq!(payload["current"]["condition"], "partly_cloudy");
        assert_eq!(payload["daily"].as_array().unwrap().len(), 2);
        assert_eq!(payload["daily"][0]["condition"], "cloudy");
        assert_eq!(payload["daily"][1]["condition"], "light_rain");
    }

    #[test]
    fn build_payload_handles_archive_response_without_current() {
        // Archive responses have no `current` block — the payload
        // builder must surface `current: null` (rather than fail or
        // panic) so the LLM can still describe the daily summary.
        // We also assert `requested_date` is surfaced at the top
        // level for `SingleDate` mode, so a no-data response is
        // still attributable to the date the user asked for.
        let body = json!({
            "daily": {
                "date": ["2024-06-15"],
                "temperature_2m_max": [22.0],
                "temperature_2m_min": [13.5],
                "weather_code": [3]
            }
        });
        let date = NaiveDate::from_ymd_opt(2024, 6, 15).unwrap();
        let payload = build_payload(&body, 48.85, 2.35, "Paris (FR)", Mode::SingleDate { date }).unwrap();
        assert!(payload["current"].is_null());
        assert_eq!(payload["requested_date"], "2024-06-15");
        assert_eq!(payload["daily"][0]["date"], "2024-06-15");
        assert_eq!(payload["daily"][0]["condition"], "cloudy");
    }

    #[test]
    fn build_payload_current_mode_omits_requested_date() {
        // `requested_date` is `null` for forecast-horizon calls so
        // the schema stays uniform across modes (the LLM can branch
        // on `requested_date == null` instead of checking key
        // presence).
        let body = json!({
            "current": {"temperature_2m": 10.0, "weather_code": 0},
            "daily": {"date": ["2026-09-25"], "temperature_2m_max": [12.0], "temperature_2m_min": [4.0], "weather_code": [0]}
        });
        let payload = build_payload(&body, 48.85, 2.35, "Paris", Mode::Current { days: 1 }).unwrap();
        assert!(payload["requested_date"].is_null());
    }
}
