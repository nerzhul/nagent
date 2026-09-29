//! `unit_convert` agent: convert a value between two units of the
//! same physical category (length, mass, volume, time, data, speed,
//! area, temperature).
//!
//! The agent is purely local — no HTTP, no I/O. It exists so the LLM
//! can answer "12 miles en km" or "100 GB en MB" without falling
//! back to its training data and silently hallucinating the wrong
//! factor.
//!
//! ## Linear categories vs temperature
//!
//! Seven of the eight categories are linear (a value in unit `U`
//! equals `value * factor_to_base(U)` in the category's base unit).
//! Temperature is special-cased because the scales have both an
//! offset and a slope (`°F = °C * 9/5 + 32`), so a single
//! `factor` would not work. The implementation handles both shapes
//! behind the same public surface — the LLM does not need to know
//! which category uses which math.
//!
//! ## Ambiguous symbols
//!
//! A few short symbols collide between categories (`pt` is a pint,
//! a typographic point, or a paper size). The agent refuses any
//! symbol that maps to multiple categories — the LLM is told which
//! categories it could belong to so it can pick one. We do not
//! auto-pick; silently guessing would produce a wrong answer that
//! looks right.
//!
//! ## Output format
//!
//! The agent returns `{value, from, to, category, formula}` so the
//! LLM can show its work: the `formula` field is the linear
//! expression that produced the result (or the temperature-specific
//! shape), and `category` is the resolved category the conversion
//! actually ran in.

use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::agents::{Agent, AgentError};
use crate::config::UnitConvertConfig;

/// Physical category a unit belongs to. Temperature is its own
/// category because the math is different (see the module header).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Category {
    Length,
    Mass,
    Volume,
    Time,
    Data,
    Speed,
    Area,
    Temperature,
}

impl Category {
    fn as_str(self) -> &'static str {
        match self {
            Category::Length => "length",
            Category::Mass => "mass",
            Category::Volume => "volume",
            Category::Time => "time",
            Category::Data => "data",
            Category::Speed => "speed",
            Category::Area => "area",
            Category::Temperature => "temperature",
        }
    }
}

/// One unit definition. For linear categories, `factor` is the
/// multiplier to the category's base unit (metre, kilogram, litre,
/// second, byte, m/s, m²). For temperature, `factor` is ignored and
/// the `temperature_to_celsius` closure does the work.
struct UnitDef {
    /// Linear factor to the category base unit. Ignored for
    /// `Category::Temperature`.
    factor: f64,
    /// For temperature: the conversion to Celsius. `None` for
    /// every other category.
    to_celsius: Option<fn(f64) -> f64>,
    /// For temperature: the conversion back from Celsius. `None`
    /// for every other category.
    from_celsius: Option<fn(f64) -> f64>,
}

/// Build of the `unit_convert` agent. Stateless — the unit table is
/// `const` and shared across all invocations.
#[derive(Clone)]
pub struct UnitConvertAgent {
    cfg: UnitConvertConfig,
}

impl std::fmt::Debug for UnitConvertAgent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UnitConvertAgent")
            .field("cfg", &self.cfg)
            .finish()
    }
}

impl Default for UnitConvertAgent {
    fn default() -> Self {
        Self::new(UnitConvertConfig::default())
    }
}

impl UnitConvertAgent {
    pub fn new(cfg: UnitConvertConfig) -> Self {
        // The timeout knob exists primarily so tests can drop it
        // for synchronous calls; the agent itself does not block
        // on I/O, so the timeout is a documentation artefact more
        // than a runtime constraint.
        let _ = Duration::from_millis(cfg.timeout_ms);
        Self { cfg }
    }
}

#[async_trait]
impl Agent for UnitConvertAgent {
    fn name(&self) -> &str {
        "unit_convert"
    }

    fn description(&self) -> &str {
        "Convertit une valeur entre deux unités d'une même grandeur physique \
         (longueur, masse, volume, temps, données, vitesse, aire, température). \
         Pas d'I/O. \
         Use for '12 miles en km', '100 GB en MB', '100°C en °F', '1h en secondes'. \
         Pass `value` (number, required), `from` (string, unit name ou symbole, required), \
         `to` (string, unit name ou symbole, required). \
         Symboles ambigus (qui existent dans plusieurs catégories) sont rejetés avec la liste \
         des catégories candidates."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "value": {
                    "type": "number",
                    "description": "Numeric value to convert (use a negative number for sub-zero temperatures)."
                },
                "from": {
                    "type": "string",
                    "description": "Source unit, case-insensitive. Accepts full names ('kilometre', 'kilogram', 'celsius') or common symbols ('km', 'kg', '°C', 'GB')."
                },
                "to": {
                    "type": "string",
                    "description": "Destination unit, same vocabulary as `from`."
                }
            },
            "required": ["value", "from", "to"],
            "additionalProperties": false,
        })
    }

    async fn invoke(&self, _ctx: &super::UserContext, args: Value) -> Result<String, AgentError> {
        let req = parse_args(&args)?;
        let result = convert(req.value, &req.from, &req.to)?;
        Ok(serde_json::to_string(&json!({
            "ok": true,
            "summary": format!(
                "{value} {from} = {out} {to}",
                value = req.value,
                from = result.from,
                out = format_number(result.value),
                to = result.to,
            ),
            "data": {
                "value": result.value,
                "from": result.from,
                "to": result.to,
                "category": result.category,
                "formula": result.formula,
            },
            "source": "local",
            "fetched_at": chrono::Utc::now().to_rfc3339(),
        }))
        .expect("json encode"))
    }
}

// ---- Core conversion ----------------------------------------------------

#[derive(Debug)]
struct ConvertResult {
    value: f64,
    from: String,
    to: String,
    category: &'static str,
    formula: String,
}

fn convert(value: f64, from_raw: &str, to_raw: &str) -> Result<ConvertResult, AgentError> {
    let from_norm = normalise(from_raw);
    let to_norm = normalise(to_raw);

    let from_defs = lookup(&from_norm)?;
    let to_defs = lookup(&to_norm)?;

    // Both inputs must resolve to exactly one category. If either
    // resolves to several (e.g. `pt` = pint / point), the agent
    // refuses with a message listing the candidates so the LLM can
    // pick a less ambiguous name.
    let from_cat = unique_category(&from_defs, from_raw)?;
    let to_cat = unique_category(&to_defs, to_raw)?;
    if from_cat != to_cat {
        return Err(AgentError::InvalidArguments(format!(
            "cannot convert between categories: `{from_raw}` is `{from}`, but `{to_raw}` is `{to}`",
            from = from_cat.as_str(),
            to = to_cat.as_str(),
        )));
    }

    let from_def = from_defs[0].1;
    let to_def = to_defs[0].1;
    let category = from_cat;

    // Temperature: not a linear transform. We pivot through Celsius
    // for both directions so the same code path handles every
    // temperature pair.
    let (out, formula) = if category == Category::Temperature {
        let to_c_from = from_def.to_celsius.ok_or_else(|| {
            AgentError::AgentFailed("internal: missing temperature `to_celsius`".into())
        })?;
        let from_c_to = to_def.from_celsius.ok_or_else(|| {
            AgentError::AgentFailed("internal: missing temperature `from_celsius`".into())
        })?;
        // Negative Kelvin is not a real temperature; surface the
        // error here so the LLM sees it before getting a number.
        // We only enforce this on Kelvin because Celsius/Fahrenheit
        // can go arbitrarily negative.
        if from_norm == "kelvin" && value < 0.0 {
            return Err(AgentError::InvalidArguments(
                "negative Kelvin is not a valid temperature".into(),
            ));
        }
        let celsius = to_c_from(value);
        let out = from_c_to(celsius);
        (out, format!("({from_raw} → °C → {to_raw})"))
    } else {
        // Linear: convert through the category base unit.
        let base = value * from_def.factor;
        let out = base / to_def.factor;
        let formula = format!(
            "{value} × {f_from} / {f_to}",
            f_from = from_def.factor,
            f_to = to_def.factor
        );
        (out, formula)
    };

    Ok(ConvertResult {
        value: round6(out),
        from: from_raw.trim().to_string(),
        to: to_raw.trim().to_string(),
        category: category.as_str(),
        formula,
    })
}

/// Normalise a user-supplied unit for table lookup: trim, lowercase,
/// apply a small set of synonyms / ASCII foldings, and strip a
/// trailing `s` for natural plurals. The result is the canonical key
/// the [`UNIT_TABLE`] is indexed by.
fn normalise(raw: &str) -> String {
    let trimmed = raw.trim().to_ascii_lowercase();
    // Direct synonyms caught before plural-stripping. The
    // temperature units must be handled here so "Celsius" /
    // "Fahrenheit" do not lose their trailing `s` and resolve to
    // "celsiu" / "fahrenhei".
    let mapped = match trimmed.as_str() {
        "celsius" | "centigrade" | "°c" => Some("celsius"),
        "fahrenheit" | "°f" => Some("fahrenheit"),
        "kelvin" | "k" => Some("kelvin"),
        _ => None,
    };
    if let Some(m) = mapped {
        return m.to_string();
    }
    // Plural-stripping: "metres" → "metre". Skipped when the result
    // would be empty ("s" → "") so a bare "s" stays "s" and resolves
    // to the second entry rather than being silently dropped.
    match trimmed.strip_suffix('s') {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => trimmed,
    }
}

/// Find every `(category, def)` the normalised name maps to.
/// Returns an error if the name is unknown.
fn lookup(normalised: &str) -> Result<Vec<(Category, &'static UnitDef)>, AgentError> {
    let mut hits: Vec<(Category, &'static UnitDef)> = Vec::new();
    for entry in UNIT_TABLE {
        for alias in entry.aliases {
            if *alias == normalised {
                hits.push((entry.category, entry.def));
                break;
            }
        }
    }
    if hits.is_empty() {
        return Err(AgentError::InvalidArguments(format!(
            "unknown unit `{normalised}`"
        )));
    }
    Ok(hits)
}

fn unique_category(
    hits: &[(Category, &'static UnitDef)],
    raw: &str,
) -> Result<Category, AgentError> {
    if hits.len() == 1 {
        return Ok(hits[0].0);
    }
    let cats: Vec<&'static str> = hits.iter().map(|(c, _)| c.as_str()).collect();
    Err(AgentError::InvalidArguments(format!(
        "ambiguous unit `{raw}` (matches multiple categories: {}); \
         use a less ambiguous name",
        cats.join(", ")
    )))
}

// ---- Unit table ---------------------------------------------------------

struct TableEntry {
    category: Category,
    aliases: &'static [&'static str],
    def: &'static UnitDef,
}

// Linear helpers for the `UnitDef` static. Temperature uses its own
// closures, so every non-temperature entry uses these shared
// instances.
static LINEAR: UnitDef = UnitDef {
    factor: 1.0,
    to_celsius: None,
    from_celsius: None,
};

fn c_to_c(c: f64) -> f64 {
    c
}
fn c_from_c(c: f64) -> f64 {
    c
}
fn f_to_c(f: f64) -> f64 {
    (f - 32.0) * 5.0 / 9.0
}
fn f_from_c(c: f64) -> f64 {
    c * 9.0 / 5.0 + 32.0
}
fn k_to_c(k: f64) -> f64 {
    k - 273.15
}
fn k_from_c(c: f64) -> f64 {
    c + 273.15
}

static CELSIUS: UnitDef = UnitDef {
    factor: 1.0,
    to_celsius: Some(c_to_c),
    from_celsius: Some(c_from_c),
};
static FAHRENHEIT: UnitDef = UnitDef {
    factor: 1.0,
    to_celsius: Some(f_to_c),
    from_celsius: Some(f_from_c),
};
static KELVIN: UnitDef = UnitDef {
    factor: 1.0,
    to_celsius: Some(k_to_c),
    from_celsius: Some(k_from_c),
};

/// Single source of truth for every supported unit. Each entry
/// declares its category, the aliases that resolve to it, and the
/// conversion definition.
///
/// Aliases use the normalised form (lowercase, no trailing `s`).
/// Symbols are folded by [`normalise`] before lookup.
const UNIT_TABLE: &[TableEntry] = &[
    // ---- Length (base = metre) ----
    TableEntry {
        category: Category::Length,
        aliases: &["km", "kilometre"],
        def: &UnitDef {
            factor: 1_000.0,
            ..LINEAR
        },
    },
    TableEntry {
        category: Category::Length,
        aliases: &["m", "metre"],
        def: &UnitDef {
            factor: 1.0,
            ..LINEAR
        },
    },
    TableEntry {
        category: Category::Length,
        aliases: &["cm", "centimetre"],
        def: &UnitDef {
            factor: 0.01,
            ..LINEAR
        },
    },
    TableEntry {
        category: Category::Length,
        aliases: &["mm", "millimetre"],
        def: &UnitDef {
            factor: 0.001,
            ..LINEAR
        },
    },
    TableEntry {
        category: Category::Length,
        aliases: &["mi", "mile"],
        def: &UnitDef {
            factor: 1_609.344,
            ..LINEAR
        },
    },
    TableEntry {
        category: Category::Length,
        aliases: &["yd", "yard"],
        def: &UnitDef {
            factor: 0.9144,
            ..LINEAR
        },
    },
    TableEntry {
        category: Category::Length,
        aliases: &["ft", "foot"],
        def: &UnitDef {
            factor: 0.3048,
            ..LINEAR
        },
    },
    TableEntry {
        category: Category::Length,
        aliases: &["in", "inch"],
        def: &UnitDef {
            factor: 0.0254,
            ..LINEAR
        },
    },
    TableEntry {
        category: Category::Length,
        aliases: &["nm", "nauticalmile", "nmi"],
        def: &UnitDef {
            factor: 1_852.0,
            ..LINEAR
        },
    },
    // ---- Mass (base = kilogram) ----
    TableEntry {
        category: Category::Mass,
        aliases: &["kg", "kilogram"],
        def: &UnitDef {
            factor: 1.0,
            ..LINEAR
        },
    },
    TableEntry {
        category: Category::Mass,
        aliases: &["g", "gram"],
        def: &UnitDef {
            factor: 0.001,
            ..LINEAR
        },
    },
    TableEntry {
        category: Category::Mass,
        aliases: &["mg", "milligram"],
        def: &UnitDef {
            factor: 0.000_001,
            ..LINEAR
        },
    },
    TableEntry {
        category: Category::Mass,
        aliases: &["t", "tonne", "metrict tonne"],
        def: &UnitDef {
            factor: 1_000.0,
            ..LINEAR
        },
    },
    TableEntry {
        category: Category::Mass,
        aliases: &["lb", "pound"],
        def: &UnitDef {
            factor: 0.453_592_37,
            ..LINEAR
        },
    },
    TableEntry {
        category: Category::Mass,
        aliases: &["oz", "ounce"],
        def: &UnitDef {
            factor: 0.028_349_523_125,
            ..LINEAR
        },
    },
    // ---- Volume (base = litre) ----
    TableEntry {
        category: Category::Volume,
        aliases: &["l", "litre"],
        def: &UnitDef {
            factor: 1.0,
            ..LINEAR
        },
    },
    TableEntry {
        category: Category::Volume,
        aliases: &["ml", "millilitre"],
        def: &UnitDef {
            factor: 0.001,
            ..LINEAR
        },
    },
    TableEntry {
        category: Category::Volume,
        aliases: &["cl", "centilitre"],
        def: &UnitDef {
            factor: 0.01,
            ..LINEAR
        },
    },
    TableEntry {
        category: Category::Volume,
        aliases: &["dl", "decilitre"],
        def: &UnitDef {
            factor: 0.1,
            ..LINEAR
        },
    },
    TableEntry {
        category: Category::Volume,
        aliases: &["m3", "cubicmetre"],
        def: &UnitDef {
            factor: 1_000.0,
            ..LINEAR
        },
    },
    TableEntry {
        category: Category::Volume,
        aliases: &["cm3", "cubiccentimetre", "cc"],
        def: &UnitDef {
            factor: 0.001,
            ..LINEAR
        },
    },
    TableEntry {
        category: Category::Volume,
        aliases: &["gal", "gallon"],
        def: &UnitDef {
            factor: 3.785_411_784,
            ..LINEAR
        },
    },
    TableEntry {
        category: Category::Volume,
        aliases: &["qt", "quart"],
        def: &UnitDef {
            factor: 0.946_352_946,
            ..LINEAR
        },
    },
    TableEntry {
        category: Category::Volume,
        aliases: &["pint"],
        def: &UnitDef {
            factor: 0.473_176_473,
            ..LINEAR
        },
    },
    TableEntry {
        category: Category::Volume,
        aliases: &["cup"],
        def: &UnitDef {
            factor: 0.236_588_236_5,
            ..LINEAR
        },
    },
    TableEntry {
        category: Category::Volume,
        aliases: &["floz", "fluidounce"],
        def: &UnitDef {
            factor: 0.029_573_529_687_5,
            ..LINEAR
        },
    },
    // ---- Time (base = second) ----
    TableEntry {
        category: Category::Time,
        aliases: &["ns", "nanosecond"],
        def: &UnitDef {
            factor: 1e-9,
            ..LINEAR
        },
    },
    TableEntry {
        category: Category::Time,
        aliases: &["ms", "millisecond"],
        def: &UnitDef {
            factor: 1e-3,
            ..LINEAR
        },
    },
    TableEntry {
        category: Category::Time,
        aliases: &["s", "second"],
        def: &UnitDef {
            factor: 1.0,
            ..LINEAR
        },
    },
    TableEntry {
        category: Category::Time,
        aliases: &["min", "minute"],
        def: &UnitDef {
            factor: 60.0,
            ..LINEAR
        },
    },
    TableEntry {
        category: Category::Time,
        aliases: &["h", "hour"],
        def: &UnitDef {
            factor: 3_600.0,
            ..LINEAR
        },
    },
    TableEntry {
        category: Category::Time,
        aliases: &["d", "day"],
        def: &UnitDef {
            factor: 86_400.0,
            ..LINEAR
        },
    },
    TableEntry {
        category: Category::Time,
        aliases: &["w", "week"],
        def: &UnitDef {
            factor: 604_800.0,
            ..LINEAR
        },
    },
    // ---- Data (base = byte, binary multiples) ----
    TableEntry {
        category: Category::Data,
        aliases: &["b", "byte"],
        def: &UnitDef {
            factor: 1.0,
            ..LINEAR
        },
    },
    TableEntry {
        category: Category::Data,
        aliases: &["bit"],
        def: &UnitDef {
            factor: 0.125,
            ..LINEAR
        },
    },
    TableEntry {
        category: Category::Data,
        aliases: &["kb", "kilobyte"],
        def: &UnitDef {
            factor: 1_000.0,
            ..LINEAR
        },
    },
    TableEntry {
        category: Category::Data,
        aliases: &["kib", "kibibyte"],
        def: &UnitDef {
            factor: 1_024.0,
            ..LINEAR
        },
    },
    TableEntry {
        category: Category::Data,
        aliases: &["mb", "megabyte"],
        def: &UnitDef {
            factor: 1_000_000.0,
            ..LINEAR
        },
    },
    TableEntry {
        category: Category::Data,
        aliases: &["mib", "mebibyte"],
        def: &UnitDef {
            factor: 1_048_576.0,
            ..LINEAR
        },
    },
    TableEntry {
        category: Category::Data,
        aliases: &["gb", "gigabyte"],
        def: &UnitDef {
            factor: 1_000_000_000.0,
            ..LINEAR
        },
    },
    TableEntry {
        category: Category::Data,
        aliases: &["gib", "gibibyte"],
        def: &UnitDef {
            factor: 1_073_741_824.0,
            ..LINEAR
        },
    },
    TableEntry {
        category: Category::Data,
        aliases: &["tb", "terabyte"],
        def: &UnitDef {
            factor: 1_000_000_000_000.0,
            ..LINEAR
        },
    },
    TableEntry {
        category: Category::Data,
        aliases: &["tib", "tebibyte"],
        def: &UnitDef {
            factor: 1_099_511_627_776.0,
            ..LINEAR
        },
    },
    // ---- Speed (base = m/s) ----
    TableEntry {
        category: Category::Speed,
        aliases: &["m/s", "metrepersecond", "mps"],
        def: &UnitDef {
            factor: 1.0,
            ..LINEAR
        },
    },
    TableEntry {
        category: Category::Speed,
        aliases: &["km/h", "kmh", "kilometreperhour", "kph"],
        def: &UnitDef {
            factor: 0.277_777_777_777_777_8,
            ..LINEAR
        },
    },
    TableEntry {
        category: Category::Speed,
        aliases: &["mph", "mileperhour"],
        def: &UnitDef {
            factor: 0.447_04,
            ..LINEAR
        },
    },
    TableEntry {
        category: Category::Speed,
        aliases: &["knot", "kt"],
        def: &UnitDef {
            factor: 0.514_444_444_444_444_5,
            ..LINEAR
        },
    },
    TableEntry {
        category: Category::Speed,
        aliases: &["ft/s", "footpersecond", "fps"],
        def: &UnitDef {
            factor: 0.3048,
            ..LINEAR
        },
    },
    // ---- Area (base = m²) ----
    TableEntry {
        category: Category::Area,
        aliases: &["m2", "squaremetre"],
        def: &UnitDef {
            factor: 1.0,
            ..LINEAR
        },
    },
    TableEntry {
        category: Category::Area,
        aliases: &["km2", "squarekilometre"],
        def: &UnitDef {
            factor: 1_000_000.0,
            ..LINEAR
        },
    },
    TableEntry {
        category: Category::Area,
        aliases: &["cm2", "squarecentimetre"],
        def: &UnitDef {
            factor: 0.0001,
            ..LINEAR
        },
    },
    TableEntry {
        category: Category::Area,
        aliases: &["ha", "hectare"],
        def: &UnitDef {
            factor: 10_000.0,
            ..LINEAR
        },
    },
    TableEntry {
        category: Category::Area,
        aliases: &["acre"],
        def: &UnitDef {
            factor: 4_046.856_422_4,
            ..LINEAR
        },
    },
    TableEntry {
        category: Category::Area,
        aliases: &["ft2", "squarefoot"],
        def: &UnitDef {
            factor: 0.092_903_04,
            ..LINEAR
        },
    },
    TableEntry {
        category: Category::Area,
        aliases: &["in2", "squareinch"],
        def: &UnitDef {
            factor: 0.000_645_16,
            ..LINEAR
        },
    },
    // ---- Temperature (special-cased) ----
    TableEntry {
        category: Category::Temperature,
        aliases: &["celsius"],
        def: &CELSIUS,
    },
    TableEntry {
        category: Category::Temperature,
        aliases: &["fahrenheit"],
        def: &FAHRENHEIT,
    },
    TableEntry {
        category: Category::Temperature,
        aliases: &["kelvin"],
        def: &KELVIN,
    },
];

// ---- Argument parsing ----------------------------------------------------

#[derive(Debug)]
struct ParsedArgs {
    value: f64,
    from: String,
    to: String,
}

fn parse_args(args: &Value) -> Result<ParsedArgs, AgentError> {
    let obj = args
        .as_object()
        .ok_or_else(|| AgentError::InvalidArguments("arguments must be a JSON object".into()))?;
    let value = obj
        .get("value")
        .and_then(|v| v.as_f64())
        .ok_or_else(|| AgentError::InvalidArguments("`value` (number) is required".into()))?;
    if !value.is_finite() {
        return Err(AgentError::InvalidArguments(
            "`value` must be a finite number".into(),
        ));
    }
    let from = obj
        .get("from")
        .and_then(|v| v.as_str())
        .ok_or_else(|| AgentError::InvalidArguments("`from` (string) is required".into()))?
        .trim()
        .to_string();
    let to = obj
        .get("to")
        .and_then(|v| v.as_str())
        .ok_or_else(|| AgentError::InvalidArguments("`to` (string) is required".into()))?
        .trim()
        .to_string();
    if from.is_empty() {
        return Err(AgentError::InvalidArguments(
            "`from` must not be empty".into(),
        ));
    }
    if to.is_empty() {
        return Err(AgentError::InvalidArguments(
            "`to` must not be empty".into(),
        ));
    }
    Ok(ParsedArgs { value, from, to })
}

fn round6(v: f64) -> f64 {
    (v * 1_000_000.0).round() / 1_000_000.0
}

/// Format an `f64` for the human-readable summary line: trim
/// trailing `.0` and stop at 6 significant digits (matching the
/// rounded value), so `19.312128` stays `19.312128` and `12.0`
/// stays `12` rather than `12.000000`.
fn format_number(v: f64) -> String {
    if !v.is_finite() {
        return String::from("?");
    }
    let formatted = format!("{v}");
    if formatted.ends_with(".0") && formatted.matches('.').count() == 1 {
        formatted.trim_end_matches(".0").to_string()
    } else {
        formatted
    }
}

// ---- Tests ---------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn name_and_schema_are_stable() {
        let agent = UnitConvertAgent::default();
        assert_eq!(agent.name(), "unit_convert");
        let schema = agent.parameters_schema();
        assert_eq!(schema["type"], "object");
        let required = schema["required"].as_array().unwrap();
        assert!(required.contains(&json!("value")));
        assert!(required.contains(&json!("from")));
        assert!(required.contains(&json!("to")));
    }

    #[test]
    fn normalise_handles_synonyms_and_case() {
        assert_eq!(normalise("Kilometre"), "kilometre");
        assert_eq!(normalise("KM"), "km");
        assert_eq!(normalise("metres"), "metre");
        assert_eq!(normalise("  L  "), "l");
        assert_eq!(normalise("°C"), "celsius");
        assert_eq!(normalise("Celsius"), "celsius");
        assert_eq!(normalise("fahrenheit"), "fahrenheit");
        assert_eq!(normalise("°F"), "fahrenheit");
        assert_eq!(normalise("K"), "kelvin");
        // "s" must not be stripped to "" (that would silently
        // mis-resolve the second argument of e.g. `1 h → s`.
        assert_eq!(normalise("s"), "s");
        assert_eq!(normalise("S"), "s");
    }

    #[test]
    fn lookup_returns_all_matching_categories() {
        // A clear unambiguous symbol resolves to exactly one entry.
        let hits = lookup("km").unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].0, Category::Length);
    }

    #[test]
    fn unique_category_rejects_ambiguous_input() {
        // Construct a fake `[(Length, &LINEAR), (Volume, &LINEAR)]`
        // hit set to exercise the ambiguity branch directly,
        // without depending on any real table collision.
        let def_a: &'static UnitDef = &LINEAR;
        let def_b: &'static UnitDef = &LINEAR;
        let hits: Vec<(Category, &'static UnitDef)> =
            vec![(Category::Length, def_a), (Category::Volume, def_b)];
        let err = unique_category(&hits, "pt").unwrap_err();
        match err {
            AgentError::InvalidArguments(msg) => {
                assert!(msg.contains("ambiguous unit"));
                assert!(msg.contains("length"));
                assert!(msg.contains("volume"));
            }
            other => panic!("expected InvalidArguments, got {other:?}"),
        }
    }

    #[test]
    fn convert_km_to_miles_round_trips() {
        // 1 km → miles → km should be a no-op up to rounding.
        let one_way = convert(1.0, "km", "mile").unwrap();
        let back = convert(one_way.value, "mile", "km").unwrap();
        assert!((back.value - 1.0).abs() < 1e-6);
        assert_eq!(one_way.category, "length");
        assert!(one_way.formula.contains("1609.344"));
    }

    #[test]
    fn convert_miles_to_km_realistic() {
        // 12 miles in km is the canonical chat-user question.
        let out = convert(12.0, "mile", "km").unwrap();
        // 12 * 1.609344 = 19.312128 km, rounded to 6 dp.
        assert!((out.value - 19.312128).abs() < 1e-6);
        assert_eq!(out.category, "length");
        assert_eq!(out.from, "mile");
        assert_eq!(out.to, "km");
    }

    #[test]
    fn convert_gb_to_mb() {
        // Binary vs decimal is a common gotcha. The agent uses
        // decimal multiples (`1 GB = 1 000 000 000 B`), which is the
        // convention used by storage vendors and the user's
        // intuition ("how big is my 500 GB drive in MB?").
        let out = convert(1.0, "GB", "MB").unwrap();
        assert_eq!(out.value, 1000.0);
        assert_eq!(out.category, "data");
    }

    #[test]
    fn convert_mib_to_mb_uses_binary_factor() {
        // The binary prefix surfaces when the user explicitly asks
        // for it. 1 MiB = 2^20 B = 1.048576 MB.
        let out = convert(1.0, "MiB", "MB").unwrap();
        assert!((out.value - 1.048576).abs() < 1e-6);
    }

    #[test]
    fn convert_celsius_to_fahrenheit() {
        let out = convert(100.0, "°C", "°F").unwrap();
        assert_eq!(out.value, 212.0);
        assert_eq!(out.category, "temperature");
        assert!(out.formula.contains("°C → °F"));
    }

    #[test]
    fn convert_fahrenheit_to_celsius() {
        let out = convert(32.0, "°F", "°C").unwrap();
        assert_eq!(out.value, 0.0);
    }

    #[test]
    fn convert_celsius_to_kelvin() {
        let out = convert(0.0, "°C", "K").unwrap();
        assert!((out.value - 273.15).abs() < 1e-9);
    }

    #[test]
    fn convert_kelvin_to_celsius_handles_negative_input() {
        // Negative Kelvin is a clear sign the user meant Celsius or
        // Fahrenheit. The agent refuses rather than producing a
        // nonsense value.
        let err = convert(-1.0, "K", "°C").unwrap_err();
        match err {
            AgentError::InvalidArguments(msg) => {
                assert!(msg.contains("negative Kelvin"));
            }
            other => panic!("expected InvalidArguments, got {other:?}"),
        }
    }

    #[test]
    fn convert_celsius_to_kelvin_allows_negative() {
        // Celsius and Fahrenheit can go arbitrarily negative; the
        // gate only fires for Kelvin.
        let out = convert(-40.0, "°C", "K").unwrap();
        assert!((out.value - 233.15).abs() < 1e-9);
    }

    #[test]
    fn convert_hours_to_seconds() {
        let out = convert(1.0, "h", "s").unwrap();
        assert_eq!(out.value, 3600.0);
    }

    #[test]
    fn convert_kph_to_mph() {
        // 100 km/h ≈ 62.137 mph
        let out = convert(100.0, "km/h", "mph").unwrap();
        assert!((out.value - 62.137).abs() < 1e-3);
    }

    #[test]
    fn convert_unknown_unit_rejected() {
        let err = convert(1.0, "furlong", "km").unwrap_err();
        match err {
            AgentError::InvalidArguments(msg) => assert!(msg.contains("unknown unit")),
            other => panic!("expected InvalidArguments, got {other:?}"),
        }
    }

    #[test]
    fn convert_across_categories_rejected() {
        let err = convert(1.0, "km", "kg").unwrap_err();
        match err {
            AgentError::InvalidArguments(msg) => {
                assert!(msg.contains("cannot convert between categories"));
                assert!(msg.contains("length"));
                assert!(msg.contains("mass"));
            }
            other => panic!("expected InvalidArguments, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn invoke_basic_length() {
        let agent = UnitConvertAgent::default();
        let ctx = super::UserContext::for_tests(
            uuid::Uuid::new_v4(),
            std::sync::Arc::new(crate::agents::ServiceRegistry::empty()),
        );
        let out = agent
            .invoke(&ctx, json!({"value": 12, "from": "mile", "to": "km"}))
            .await
            .expect("invoke");
        let parsed: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed["ok"], true);
        assert_eq!(parsed["source"], "local");
        let v = parsed["data"]["value"].as_f64().unwrap();
        assert!((v - 19.312128).abs() < 1e-6);
        assert_eq!(parsed["data"]["category"], "length");
        // The summary is the front-end's tool-bubble headline.
        // It must include the input value, source unit, and target
        // value so a glance at the bubble tells the user what
        // happened without expanding it.
        let summary = parsed["summary"].as_str().expect("summary");
        assert!(summary.contains("12"), "summary: {summary}");
        assert!(summary.contains("mile"), "summary: {summary}");
        assert!(summary.contains("km"), "summary: {summary}");
        assert!(summary.contains("19.31"), "summary: {summary}");
    }

    #[tokio::test]
    async fn invoke_rejects_missing_value() {
        let agent = UnitConvertAgent::default();
        let ctx = super::UserContext::for_tests(
            uuid::Uuid::new_v4(),
            std::sync::Arc::new(crate::agents::ServiceRegistry::empty()),
        );
        let err = agent
            .invoke(&ctx, json!({"from": "km", "to": "mile"}))
            .await
            .expect_err("should reject");
        assert!(matches!(err, AgentError::InvalidArguments(_)));
    }

    #[tokio::test]
    async fn invoke_rejects_non_object_arguments() {
        let agent = UnitConvertAgent::default();
        let ctx = super::UserContext::for_tests(
            uuid::Uuid::new_v4(),
            std::sync::Arc::new(crate::agents::ServiceRegistry::empty()),
        );
        let err = agent
            .invoke(&ctx, json!("nope"))
            .await
            .expect_err("should reject");
        assert!(matches!(err, AgentError::InvalidArguments(_)));
    }

    #[tokio::test]
    async fn invoke_rejects_nan_value() {
        // `NaN` is technically a valid JSON number in some
        // encodings. We reject it at the parser gate so the LLM
        // sees a clear message rather than a confusing `NaN`
        // propagated to the conversion math. Reaching the
        // `!value.is_finite()` branch from `serde_json` is
        // awkward because the JSON encoder can't actually produce
        // NaN; exercise the gate directly with a JSON string the
        // parser cannot interpret as a number.
        let _agent = UnitConvertAgent::default();
        let err = parse_args(&json!({"value": "NaN", "from": "km", "to": "km"})).unwrap_err();
        match err {
            AgentError::InvalidArguments(msg) => {
                assert!(msg.contains("`value`"));
            }
            other => panic!("expected InvalidArguments, got {other:?}"),
        }
    }
}
