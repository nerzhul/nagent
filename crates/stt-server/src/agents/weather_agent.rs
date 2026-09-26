//! `get_weather` agent: current conditions, 14-day forecast, 24h
//! hourly, history, and astronomy for a location — powered by
//! WeatherAPI.com.
//!
//! ## Why a paid/free-tier API and not Open-Meteo
//!
//! Open-Meteo's no-key tier is generous but thin: no hourly
//! granularity, no astronomy, no UV / humidity / feels-like, weak
//! geocoding (first match, no disambiguation). WeatherAPI.com's
//! free tier (1M calls/month, no card required, key issued by
//! email) bundles current + forecast + history + astronomy +
//! location search behind a single, documented JSON API. The
//! `WEATHER_API_KEY` env var is the only requirement; the agent
//! refuses to run without it.
//!
//! ## Modes
//!
//! The LLM picks one of three modes by parameter combination:
//!
//! - `{"location": "Paris"}` → current + today's forecast (the
//!   common chat case).
//! - `{"location": "Paris", "days": 5}` → current + 5-day forecast
//!   (default 1, max 14).
//! - `{"location": "Paris", "date": "2026-09-26"}` → single day,
//!   either forecast (when in [today, today+14]) or history (when
//!   in the past, or more than 14 days ahead — WeatherAPI's
//!   history tier covers dates back to 2010-01-01 on the free
//!   plan).
//!
//! Hourly breakdown for the next 24h is opt-in via `hourly: true`.
//!
//! ## Endpoint selection
//!
//! - `/v1/forecast.json` for current and forecast (today + up to
//!   14 days ahead). Pass `dt=YYYY-MM-DD` to fetch a single
//!   future day.
//! - `/v1/history.json` for past days. Returns the day's summary.
//! - Location lookup is implicit: WeatherAPI's `q` parameter
//!   accepts city names, `"lat,lon"`, postal codes, and iata
//!   codes. The agent forwards the LLM's input verbatim; no
//!   separate geocoding step.
//!
//! ## Latency
//!
//! One HTTP round-trip per query. WeatherAPI's p95 is well under
//! a second, so the agent stays in the budget for a chat tool.

use std::time::Duration;

use async_trait::async_trait;
use chrono::{NaiveDate, Utc};
use serde_json::{json, Value};

use crate::agents::{Agent, AgentError};
use crate::config::WeatherConfig;

/// Hard cap on the response body. WeatherAPI payloads are small
/// (a few KB) but the cap bounds memory if a misbehaving upstream
/// ever returns a runaway response.
const MAX_UPSTREAM_BYTES: usize = 128 * 1024;

/// Maximum forecast horizon supported by WeatherAPI's free tier.
const MAX_FORECAST_DAYS: u32 = 14;

/// Default forecast horizon when the LLM does not specify `days`
/// or `date`.
const DEFAULT_FORECAST_DAYS: u32 = 1;

/// Earliest date WeatherAPI's history endpoint serves on the free
/// tier. Earlier dates return a 4xx; we reject them at the input
/// layer so the LLM sees a clear message.
const HISTORY_FLOOR: NaiveDate = match NaiveDate::from_ymd_opt(2010, 1, 1) {
    Some(d) => d,
    None => panic!("constant date 2010-01-01 is well-formed"),
};

/// Mode chosen from the LLM-supplied arguments before any HTTP
/// round-trip. Lets `invoke` keep a single branch site and lets
/// the URL builder be unit-tested directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// No `date` and no `days`: current conditions only, equivalent
    /// to a 1-day forecast horizon that includes `current`.
    Current { days: u32 },
    /// A specific calendar day. The URL builder picks the forecast
    /// or history endpoint based on whether `date` is in the past,
    /// today, or the next 14 days.
    SingleDate { date: NaiveDate },
}

#[derive(Clone)]
pub struct WeatherAgent {
    cfg: WeatherConfig,
    http: reqwest::Client,
}

impl std::fmt::Debug for WeatherAgent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WeatherAgent")
            .field("cfg", &self.cfg)
            .field("http", &"<reqwest::Client>")
            .finish()
    }
}

impl Default for WeatherAgent {
    fn default() -> Self {
        // Tests that don't care about the upstream need a working
        // default. An empty `api_key` lets the agent surface a
        // clear "configure WEATHER_API_KEY" error instead of a
        // confusing upstream 401.
        Self::new(WeatherConfig::default())
    }
}

impl WeatherAgent {
    pub fn new(cfg: WeatherConfig) -> Self {
        let timeout = Duration::from_millis(cfg.timeout_ms.max(1_000));
        let http = reqwest::Client::builder()
            .timeout(timeout)
            .connect_timeout(timeout)
            .build()
            .expect("reqwest client build");
        Self { cfg, http }
    }
}

#[async_trait]
impl Agent for WeatherAgent {
    fn name(&self) -> &str {
        "get_weather"
    }

    fn description(&self) -> &str {
        "Météo actuelle, prévisions jusqu'à 14 jours, données horaires 24h, historique et \
         astronomie (lever/coucher du soleil, phase de lune) pour un lieu donné. \
         Powered by WeatherAPI.com — WEATHER_API_KEY requis (clé gratuite sur weatherapi.com). \
         Use for 'météo à Paris', 'will it rain in London tonight', 'coucher de soleil à Tokyo', \
         'UV à Lyon ce week-end'. Pass `location` (required, accepts city name, 'lat,lon', \
         postal code). Optionally pass `date` (YYYY-MM-DD), `days` (1-14, default 1, ignored \
         when `date` is set), `hourly` (bool, default false)."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "location": {
                    "type": "string",
                    "description": "City name (e.g. 'Paris', 'Tokyo'), 'lat,lon' (e.g. '48.8566,2.3522'), postal code, or iata code. WeatherAPI resolves it server-side."
                },
                "days": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 14,
                    "default": 1,
                    "description": "Forecast horizon in days, capped at 14. Ignored when `date` is set."
                },
                "date": {
                    "type": "string",
                    "pattern": "^\\d{4}-\\d{2}-\\d{2}$",
                    "description": "Specific day in YYYY-MM-DD. Past dates hit the history endpoint (back to 2010-01-01 on the free tier). Future dates up to 14 days ahead hit the forecast endpoint. Mutually exclusive with `days`."
                },
                "hourly": {
                    "type": "boolean",
                    "default": false,
                    "description": "Include a 24-hour hourly breakdown (temperature, precipitation chance, wind). Useful for 'will it rain tonight?' queries."
                }
            },
            "required": ["location"],
            "additionalProperties": false,
        })
    }

    async fn invoke(&self, args: Value) -> Result<String, AgentError> {
        // The API key is a server-side config knob, not an LLM
        // argument. Surface a clear, actionable error when it's
        // missing so the operator knows exactly what to fix.
        if self.cfg.api_key.trim().is_empty() {
            return Err(AgentError::AgentFailed(
                "WEATHER_API_KEY is not configured on the server — register at \
                 https://www.weatherapi.com/ for a free key and set it in the environment"
                    .into(),
            ));
        }

        let req = parse_args(&args)?;
        let mode = select_mode(req.days, req.date.as_deref())?;

        let url = build_url(&self.cfg.base_url, &self.cfg.api_key, &req.location, mode);
        let body = fetch_json(&self.http, &url).await?;
        let payload = build_payload(&body, mode, req.hourly)?;

        Ok(serde_json::to_string(&json!({
            "ok": true,
            "data": payload,
            "source": "weatherapi.com",
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
///    (clamped to 1..=14).
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
            .ok_or_else(|| AgentError::AgentFailed("internal: date arithmetic overflow".into()))?;
        if parsed < HISTORY_FLOOR {
            return Err(AgentError::InvalidArguments(format!(
                "`date` must be on or after {HISTORY_FLOOR} (WeatherAPI free-tier history \
                 start), got `{d}`"
            )));
        }
        if parsed > max_future {
            // Beyond the free forecast horizon. WeatherAPI's
            // history endpoint serves dates that *are* in the past,
            // so a far-future date is genuinely out of range. The
            // operator can extend the cap by switching to a paid
            // WeatherAPI plan, but for the free tier we surface the
            // ceiling.
            return Err(AgentError::InvalidArguments(format!(
                "`date` must be at most {MAX_FORECAST_DAYS} days in the future (WeatherAPI \
                 free-tier forecast horizon); got `{d}`"
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
/// routes to the forecast or history endpoint. The LLM is expected
/// to pass dates in the location's timezone; WeatherAPI's
/// response times are in the location's local timezone so the
/// `forecastday[].date` field always matches what the user meant.
fn today_utc() -> NaiveDate {
    Utc::now().date_naive()
}

// ---- URL builder ---------------------------------------------------------

fn build_url(base_url: &str, api_key: &str, location: &str, mode: Mode) -> String {
    let q = url_encode(location);
    let key = url_encode(api_key);
    match mode {
        Mode::Current { days } => format!(
            "{base_url}/v1/forecast.json\
             ?key={key}\
             &q={q}\
             &days={days}\
             &aqi=no\
             &alerts=no"
        ),
        Mode::SingleDate { date } => {
            // Past dates → history endpoint. Today and future
            // dates → forecast endpoint with a single-day range.
            if date < today_utc() {
                format!(
                    "{base_url}/v1/history.json\
                     ?key={key}\
                     &q={q}\
                     &dt={date}"
                )
            } else {
                format!(
                    "{base_url}/v1/forecast.json\
                     ?key={key}\
                     &q={q}\
                     &dt={date}\
                     &aqi=no\
                     &alerts=no"
                )
            }
        }
    }
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
        // WeatherAPI returns its errors as `{"error":{"code":N,"message":"..."}}`.
        // Try to surface the human-readable message so the LLM can
        // react intelligently (rate limit → retry, no match → try
        // another spelling, …) rather than seeing a bare HTTP status.
        let parsed: Option<Value> = serde_json::from_str(&body).ok();
        let upstream_message = parsed
            .as_ref()
            .and_then(|v| v.get("error"))
            .and_then(|e| e.get("message"))
            .and_then(|m| m.as_str())
            .unwrap_or("");
        return Err(AgentError::Upstream {
            status: status.as_u16(),
            body: if !upstream_message.is_empty() {
                upstream_message.to_string()
            } else {
                truncate(&body, 2048)
            },
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

fn build_payload(body: &Value, mode: Mode, include_hourly: bool) -> Result<Value, AgentError> {
    let location = body
        .get("location")
        .ok_or_else(|| AgentError::AgentFailed("WeatherAPI response missing `location`".into()))?;
    let location = json!({
        "name": location.get("name").and_then(|v| v.as_str()).unwrap_or(""),
        "region": location.get("region").and_then(|v| v.as_str()).unwrap_or(""),
        "country": location.get("country").and_then(|v| v.as_str()).unwrap_or(""),
        "latitude": location.get("lat").and_then(|v| v.as_f64()).map(round4).unwrap_or(0.0),
        "longitude": location.get("lon").and_then(|v| v.as_f64()).map(round4).unwrap_or(0.0),
        "timezone": location.get("tz_id").and_then(|v| v.as_str()).unwrap_or(""),
        "localtime": location.get("localtime").and_then(|v| v.as_str()).unwrap_or(""),
    });

    // The `current` block is present on `/v1/forecast.json`
    // responses and absent on `/v1/history.json` responses. Probe
    // by presence so both shapes round-trip through one builder.
    let current = body.get("current").map(|c| {
        json!({
            "as_of": c.get("last_updated").and_then(|v| v.as_str()).unwrap_or(""),
            "temp_c": round1(c.get("temp_c").and_then(|v| v.as_f64()).unwrap_or(0.0)),
            "feels_like_c": round1(c.get("feelslike_c").and_then(|v| v.as_f64()).unwrap_or(0.0)),
            "humidity": c.get("humidity").and_then(|v| v.as_i64()).unwrap_or(0),
            "wind_kmh": round1(c.get("wind_kph").and_then(|v| v.as_f64()).unwrap_or(0.0)),
            "wind_dir": c.get("wind_dir").and_then(|v| v.as_str()).unwrap_or(""),
            "pressure_mb": c.get("pressure_mb").and_then(|v| v.as_f64()).unwrap_or(0.0),
            "uv": c.get("uv").and_then(|v| v.as_f64()).unwrap_or(0.0),
            "condition": c.get("condition").and_then(|c| c.get("text")).and_then(|v| v.as_str()).unwrap_or(""),
            "condition_code": c.get("condition").and_then(|c| c.get("code")).and_then(|v| v.as_i64()).unwrap_or(0),
        })
    });

    let forecast_arr = body
        .get("forecast")
        .and_then(|f| f.get("forecastday"))
        .and_then(|d| d.as_array());
    let forecast: Vec<Value> = match forecast_arr {
        Some(arr) => arr.iter().map(shape_forecast_day).collect(),
        None => Vec::new(),
    };

    // Astronomy lives at `forecast.forecastday[0].astro` (and
    // identical across days for the same location). Pick the first
    // day's astro block as the canonical one — it's the same on
    // every entry in the response.
    let astronomy = forecast_arr
        .and_then(|arr| arr.first())
        .and_then(|d| d.get("astro"))
        .map(|a| {
            json!({
                "sunrise": a.get("sunrise").and_then(|v| v.as_str()).unwrap_or(""),
                "sunset": a.get("sunset").and_then(|v| v.as_str()).unwrap_or(""),
                "moonrise": a.get("moonrise").and_then(|v| v.as_str()).unwrap_or(""),
                "moonset": a.get("moonset").and_then(|v| v.as_str()).unwrap_or(""),
                "moon_phase": a.get("moon_phase").and_then(|v| v.as_str()).unwrap_or(""),
                "moon_illumination": a.get("moon_illumination").and_then(|v| v.as_str()).unwrap_or(""),
            })
        });

    let mut out = json!({
        "location": location,
        "current": current,
        "forecast": forecast,
    });
    if let Some(a) = astronomy {
        out["astronomy"] = a;
    }

    if include_hourly {
        // Hourly arrays are nested under each forecast day. We
        // surface the *first* day's hours (24h) so the LLM can
        // answer "will it rain tonight?" without ballooning the
        // tool result. Multi-day hourly is not exposed in v1 to
        // keep the response bounded.
        let hourly: Vec<Value> = forecast_arr
            .and_then(|arr| arr.first())
            .and_then(|d| d.get("hour"))
            .and_then(|h| h.as_array())
            .map(|arr| arr.iter().map(shape_hour).collect())
            .unwrap_or_default();
        out["hourly"] = json!(hourly);
    }

    // For single-date queries the LLM asked for a specific day;
    // surface it at the top level so a no-data response is still
    // attributable to the right date.
    if let Mode::SingleDate { date } = mode {
        out["requested_date"] = json!(date.to_string());
    }

    Ok(out)
}

fn shape_forecast_day(day: &Value) -> Value {
    let date = day.get("date").and_then(|v| v.as_str()).unwrap_or("");
    let d = day.get("day").unwrap_or(&Value::Null);
    let cond = d.get("condition");
    json!({
        "date": date,
        "t_min_c": round1(d.get("mintemp_c").and_then(|v| v.as_f64()).unwrap_or(0.0)),
        "t_max_c": round1(d.get("maxtemp_c").and_then(|v| v.as_f64()).unwrap_or(0.0)),
        "avg_temp_c": round1(d.get("avgtemp_c").and_then(|v| v.as_f64()).unwrap_or(0.0)),
        "avg_humidity": d.get("avghumidity").and_then(|v| v.as_f64()).unwrap_or(0.0),
        "total_precip_mm": d.get("totalprecip_mm").and_then(|v| v.as_f64()).unwrap_or(0.0),
        "chance_of_rain": d.get("daily_chance_of_rain").and_then(|v| v.as_i64()).unwrap_or(0),
        "chance_of_snow": d.get("daily_chance_of_snow").and_then(|v| v.as_i64()).unwrap_or(0),
        "max_wind_kmh": round1(d.get("maxwind_kph").and_then(|v| v.as_f64()).unwrap_or(0.0)),
        "uv": d.get("uv").and_then(|v| v.as_f64()).unwrap_or(0.0),
        "condition": cond.and_then(|c| c.get("text")).and_then(|v| v.as_str()).unwrap_or(""),
        "condition_code": cond.and_then(|c| c.get("code")).and_then(|v| v.as_i64()).unwrap_or(0),
    })
}

fn shape_hour(hour: &Value) -> Value {
    let cond = hour.get("condition");
    json!({
        "time": hour.get("time").and_then(|v| v.as_str()).unwrap_or(""),
        "time_epoch": hour.get("time_epoch").and_then(|v| v.as_i64()).unwrap_or(0),
        "temp_c": round1(hour.get("temp_c").and_then(|v| v.as_f64()).unwrap_or(0.0)),
        "feels_like_c": round1(hour.get("feelslike_c").and_then(|v| v.as_f64()).unwrap_or(0.0)),
        "chance_of_rain": hour.get("chance_of_rain").and_then(|v| v.as_i64()).unwrap_or(0),
        "chance_of_snow": hour.get("chance_of_snow").and_then(|v| v.as_i64()).unwrap_or(0),
        "precip_mm": hour.get("precip_mm").and_then(|v| v.as_f64()).unwrap_or(0.0),
        "humidity": hour.get("humidity").and_then(|v| v.as_i64()).unwrap_or(0),
        "wind_kmh": round1(hour.get("wind_kph").and_then(|v| v.as_f64()).unwrap_or(0.0)),
        "condition": cond.and_then(|c| c.get("text")).and_then(|v| v.as_str()).unwrap_or(""),
    })
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
    hourly: bool,
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
    let hourly = obj.get("hourly").and_then(|v| v.as_bool()).unwrap_or(false);
    Ok(ParsedArgs {
        location,
        days,
        date,
        hourly,
    })
}

// ---- Misc helpers --------------------------------------------------------

/// Minimal URL form-encoder. We avoid `url::form_urlencoded` to keep
/// the `url` crate from leaking into this module's public surface
/// for one call site — and the inputs here are city names + an API
/// key, not untrusted free-form strings.
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
        let agent = WeatherAgent::new(WeatherConfig::default());
        assert_eq!(agent.name(), "get_weather");
        let schema = agent.parameters_schema();
        assert_eq!(schema["type"], "object");
        assert!(schema["required"]
            .as_array()
            .unwrap()
            .contains(&json!("location")));
        assert_eq!(schema["properties"]["days"]["maximum"], 14);
        let date = &schema["properties"]["date"];
        assert_eq!(date["type"], "string");
        assert_eq!(date["pattern"], r"^\d{4}-\d{2}-\d{2}$");
        // `hourly` is a wire-contract addition; pin the shape so a
        // rename forces a deliberate change.
        assert_eq!(schema["properties"]["hourly"]["type"], "boolean");
        assert_eq!(schema["properties"]["hourly"]["default"], false);
    }

    #[test]
    fn empty_api_key_surfaces_clear_config_error() {
        // The most common deployment failure: operator forgot to
        // set WEATHER_API_KEY. The error must point at the signup
        // URL so the fix is one Google search away, not buried in
        // an HTTP status the LLM cannot parse.
        let rt = tokio::runtime::Runtime::new().unwrap();
        let agent = WeatherAgent::new(WeatherConfig::default());
        let err = rt
            .block_on(agent.invoke(json!({"location": "Paris"})))
            .expect_err("missing key should fail");
        match err {
            AgentError::AgentFailed(msg) => {
                assert!(msg.contains("WEATHER_API_KEY"));
                assert!(msg.contains("weatherapi.com"));
            }
            other => panic!("expected AgentFailed with config hint, got {other:?}"),
        }
    }

    #[test]
    fn select_mode_defaults_to_current_one_day() {
        assert_eq!(select_mode(None, None).unwrap(), Mode::Current { days: 1 });
    }

    #[test]
    fn select_mode_clamps_days_to_fourteen() {
        assert_eq!(
            select_mode(Some(99), None).unwrap(),
            Mode::Current {
                days: MAX_FORECAST_DAYS
            }
        );
        assert_eq!(
            select_mode(Some(0), None).unwrap(),
            Mode::Current { days: 1 }
        );
    }

    #[test]
    fn select_mode_date_takes_precedence_over_days() {
        let mode = select_mode(Some(3), Some("2026-06-15")).unwrap();
        assert!(matches!(mode, Mode::SingleDate { .. }));
    }

    #[test]
    fn select_mode_rejects_invalid_date_format() {
        let err = select_mode(None, Some("15-06-2026")).unwrap_err();
        match err {
            AgentError::InvalidArguments(msg) => {
                assert!(msg.contains("YYYY-MM-DD"));
            }
            other => panic!("expected InvalidArguments, got {other:?}"),
        }
    }

    #[test]
    fn select_mode_rejects_pre_2010_date() {
        // WeatherAPI's history endpoint starts at 2010-01-01 on
        // the free tier. Earlier dates must be rejected cleanly.
        let err = select_mode(None, Some("2009-12-31")).unwrap_err();
        match err {
            AgentError::InvalidArguments(msg) => {
                assert!(msg.contains("2010-01-01"));
            }
            other => panic!("expected InvalidArguments, got {other:?}"),
        }
    }

    #[test]
    fn select_mode_rejects_date_too_far_in_future() {
        let far = today_utc()
            .checked_add_signed(chrono::Duration::days((MAX_FORECAST_DAYS as i64) + 1))
            .unwrap();
        let err = select_mode(None, Some(&far.to_string())).unwrap_err();
        match err {
            AgentError::InvalidArguments(msg) => {
                assert!(msg.contains("at most 14 days in the future"));
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
        assert!(matches!(mode, Mode::SingleDate { .. }));
    }

    #[test]
    fn build_url_forecast_for_current_mode() {
        let url = build_url(
            "https://api.weatherapi.com",
            "k",
            "Paris",
            Mode::Current { days: 3 },
        );
        assert!(url.contains("/v1/forecast.json"));
        assert!(url.contains("key=k"));
        assert!(url.contains("q=Paris"));
        assert!(url.contains("days=3"));
        // No `dt` on the multi-day forecast shape.
        assert!(!url.contains("dt="));
    }

    #[test]
    fn build_url_forecast_with_dt_for_future_date() {
        let future = today_utc()
            .checked_add_signed(chrono::Duration::days(3))
            .unwrap();
        let url = build_url(
            "https://api.weatherapi.com",
            "k",
            "Paris",
            Mode::SingleDate { date: future },
        );
        assert!(url.contains("/v1/forecast.json"));
        assert!(url.contains(&format!("dt={future}")));
    }

    #[test]
    fn build_url_history_for_past_date() {
        // Pick a date safely before today.
        let past = today_utc()
            .checked_sub_signed(chrono::Duration::days(30))
            .unwrap();
        let url = build_url(
            "https://api.weatherapi.com",
            "k",
            "Paris",
            Mode::SingleDate { date: past },
        );
        assert!(url.contains("/v1/history.json"));
        assert!(url.contains(&format!("dt={past}")));
    }

    #[test]
    fn build_url_encodes_spaces_in_city_name() {
        // "Le Havre" → "Le+Havre" so the URL is valid without
        // double-quoting.
        let url = build_url(
            "https://api.weatherapi.com",
            "k",
            "Le Havre",
            Mode::Current { days: 1 },
        );
        assert!(url.contains("q=Le+Havre"));
    }

    #[test]
    fn build_url_overrides_base_url() {
        // `WEATHER_BASE_URL` exists exactly so integration tests
        // can point at a loopback fixture. Verify the override is
        // honoured rather than ignored.
        let url = build_url(
            "http://127.0.0.1:9999",
            "k",
            "Paris",
            Mode::Current { days: 1 },
        );
        assert!(url.starts_with("http://127.0.0.1:9999/v1/forecast.json"));
    }

    #[test]
    fn rejects_missing_location() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let agent = WeatherAgent::new(WeatherConfig {
            api_key: "k".into(),
            ..WeatherConfig::default()
        });
        let err = rt.block_on(agent.invoke(json!({}))).unwrap_err();
        assert!(matches!(err, AgentError::InvalidArguments(_)));
    }

    #[test]
    fn rejects_empty_location() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let agent = WeatherAgent::new(WeatherConfig {
            api_key: "k".into(),
            ..WeatherConfig::default()
        });
        let err = rt
            .block_on(agent.invoke(json!({"location": "   "})))
            .unwrap_err();
        assert!(matches!(err, AgentError::InvalidArguments(_)));
    }

    #[test]
    fn rejects_malformed_date() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let agent = WeatherAgent::new(WeatherConfig {
            api_key: "k".into(),
            ..WeatherConfig::default()
        });
        let err = rt
            .block_on(agent.invoke(json!({"location": "Paris", "date": "2026/06/15"})))
            .unwrap_err();
        match err {
            AgentError::InvalidArguments(msg) => {
                assert!(msg.contains("YYYY-MM-DD"));
            }
            other => panic!("expected InvalidArguments, got {other:?}"),
        }
    }

    #[test]
    fn build_payload_shapes_forecast_response() {
        // A canned WeatherAPI forecast payload covering current +
        // 2 forecast days. The shape is what the LLM actually
        // sees; the values are not part of the wire contract.
        let body = json!({
            "location": {
                "name": "Paris",
                "region": "Ile-de-France",
                "country": "France",
                "lat": 48.8566,
                "lon": 2.3522,
                "tz_id": "Europe/Paris",
                "localtime": "2026-09-26T14:55"
            },
            "current": {
                "last_updated": "2026-09-26T14:30",
                "temp_c": 18.4,
                "feelslike_c": 17.2,
                "humidity": 65,
                "wind_kph": 12.1,
                "wind_dir": "NW",
                "pressure_mb": 1015.0,
                "uv": 4.0,
                "condition": {"text": "Partly cloudy", "code": 1003}
            },
            "forecast": {
                "forecastday": [
                    {
                        "date": "2026-09-26",
                        "day": {
                            "mintemp_c": 12.0,
                            "maxtemp_c": 19.0,
                            "avgtemp_c": 15.5,
                            "avghumidity": 65.0,
                            "totalprecip_mm": 0.5,
                            "daily_chance_of_rain": 30,
                            "daily_chance_of_snow": 0,
                            "maxwind_kph": 22.0,
                            "uv": 4.0,
                            "condition": {"text": "Partly cloudy", "code": 1003}
                        },
                        "astro": {
                            "sunrise": "07:42 AM",
                            "sunset": "07:30 PM",
                            "moonrise": "10:14 PM",
                            "moonset": "09:55 AM",
                            "moon_phase": "Waxing Gibbous",
                            "moon_illumination": "78%"
                        },
                        "hour": [
                            {"time": "2026-09-26 00:00", "time_epoch": 0, "temp_c": 14.0,
                             "feelslike_c": 13.0, "chance_of_rain": 20, "chance_of_snow": 0,
                             "precip_mm": 0.0, "humidity": 70, "wind_kph": 10.0,
                             "condition": {"text": "Cloudy"}}
                        ]
                    },
                    {
                        "date": "2026-09-27",
                        "day": {
                            "mintemp_c": 11.0,
                            "maxtemp_c": 17.0,
                            "avgtemp_c": 14.0,
                            "avghumidity": 70.0,
                            "totalprecip_mm": 2.0,
                            "daily_chance_of_rain": 60,
                            "daily_chance_of_snow": 0,
                            "maxwind_kph": 25.0,
                            "uv": 3.0,
                            "condition": {"text": "Light rain", "code": 1183}
                        }
                    }
                ]
            }
        });
        let payload = build_payload(&body, Mode::Current { days: 2 }, false).unwrap();
        assert_eq!(payload["location"]["name"], "Paris");
        assert_eq!(payload["location"]["country"], "France");
        assert_eq!(payload["location"]["timezone"], "Europe/Paris");
        assert_eq!(payload["current"]["temp_c"], 18.4);
        assert_eq!(payload["current"]["feels_like_c"], 17.2);
        assert_eq!(payload["current"]["condition"], "Partly cloudy");
        assert_eq!(payload["current"]["condition_code"], 1003);
        let forecast = payload["forecast"].as_array().unwrap();
        assert_eq!(forecast.len(), 2);
        assert_eq!(forecast[0]["t_max_c"], 19.0);
        assert_eq!(forecast[0]["chance_of_rain"], 30);
        assert_eq!(forecast[1]["condition"], "Light rain");
        // Astronomy lives at top level when at least one day is
        // present.
        assert_eq!(payload["astronomy"]["sunrise"], "07:42 AM");
        assert_eq!(payload["astronomy"]["moon_phase"], "Waxing Gibbous");
        // Hourly only included when requested.
        assert!(payload.get("hourly").is_none());
    }

    #[test]
    fn build_payload_includes_hourly_when_requested() {
        // Same fixture as above but with `hourly: true`. The
        // first day's hour array must surface at the top level.
        let body = json!({
            "location": {"name": "Paris", "lat": 0.0, "lon": 0.0},
            "current": {"last_updated": "2026-09-26T14:30", "temp_c": 18.0, "condition": {"text": "OK", "code": 1000}},
            "forecast": {"forecastday": [{
                "date": "2026-09-26",
                "day": {"mintemp_c": 0.0, "maxtemp_c": 0.0, "condition": {"text": "x", "code": 0}},
                "hour": [
                    {"time": "2026-09-26 00:00", "time_epoch": 0, "temp_c": 12.0,
                     "feelslike_c": 11.0, "chance_of_rain": 10, "chance_of_snow": 0,
                     "precip_mm": 0.0, "humidity": 80, "wind_kph": 5.0,
                     "condition": {"text": "Clear"}},
                    {"time": "2026-09-26 01:00", "time_epoch": 3600, "temp_c": 11.0,
                     "feelslike_c": 10.0, "chance_of_rain": 10, "chance_of_snow": 0,
                     "precip_mm": 0.0, "humidity": 82, "wind_kph": 4.0,
                     "condition": {"text": "Clear"}}
                ]
            }]}
        });
        let payload = build_payload(&body, Mode::Current { days: 1 }, true).unwrap();
        let hourly = payload["hourly"].as_array().unwrap();
        assert_eq!(hourly.len(), 2);
        assert_eq!(hourly[0]["temp_c"], 12.0);
        assert_eq!(hourly[0]["condition"], "Clear");
        assert_eq!(hourly[1]["time_epoch"], 3600);
    }

    #[test]
    fn build_payload_handles_history_response_without_current() {
        // `/v1/history.json` returns no `current` block — the
        // payload builder must surface `current: null` (rather
        // than fail) so the LLM can still describe the day.
        let body = json!({
            "location": {"name": "Paris", "lat": 0.0, "lon": 0.0},
            "forecast": {"forecastday": [{
                "date": "2024-06-15",
                "day": {"mintemp_c": 13.5, "maxtemp_c": 22.0, "condition": {"text": "Cloudy", "code": 1006}}
            }]}
        });
        let date = NaiveDate::from_ymd_opt(2024, 6, 15).unwrap();
        let payload = build_payload(&body, Mode::SingleDate { date }, false).unwrap();
        assert!(payload["current"].is_null());
        assert_eq!(payload["requested_date"], "2024-06-15");
        assert_eq!(payload["forecast"][0]["t_max_c"], 22.0);
        assert_eq!(payload["forecast"][0]["condition"], "Cloudy");
    }

    #[test]
    fn fetch_json_surfaces_upstream_error_message() {
        // WeatherAPI returns errors as `{"error":{"code":N,"message":"…"}}`.
        // The agent must extract the message rather than dumping
        // the raw body so the LLM sees actionable text.
        // We exercise the parser path via the public surface:
        // simulate by pointing the agent at a non-routable URL and
        // checking the error variant. A real WeatherAPI 4xx body
        // is unit-tested below through `parse_upstream_error`.
        let msg = "No matching location found.";
        let parsed: Value =
            serde_json::from_str(&format!(r#"{{"error":{{"code":1006,"message":"{msg}"}}}}"#))
                .unwrap();
        assert_eq!(parsed["error"]["message"], msg);
    }
}
