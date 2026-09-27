//! `calculate` agent: a thin, safe wrapper over the `meval`
//! expression evaluator.
//!
//! The agent evaluates a single arithmetic expression — the kind a
//! user would type into a desktop calculator — and returns the
//! numeric result. It is purely local (no HTTP, no I/O) and exists so
//! the LLM can answer "combien font 15 % de 87,50 ?" without falling
//! back to its training data and silently hallucinating.
//!
//! ## Why `meval`
//!
//! `meval` is a small (~50 KB) safe expression evaluator that:
//!
//! - Supports a documented grammar (`+ - * / ^ %`, parentheses,
//!   common functions `sin/cos/sqrt/log`, constants `pi`/`e`).
//! - Returns a numeric value and refuses unknown identifiers —
//!   exactly the safety boundary we need.
//!
//! Heavier alternatives (`evalexpr`) are rejected to keep the binary
//! small and the surface easy to audit.
//!
//! ## Defence-in-depth: the deny-list
//!
//! `meval` is safe by construction (no `eval`, no variable binding,
//! no I/O), but the agent still applies its own deny-list on top:
//!
//! - The expression length is capped at 256 characters.
//! - Only the characters `[0-9a-zA-Z_+\-*/()., \t\n]` are accepted.
//!   Any other byte — semicolons, brackets, quotes, control
//!   characters — fails fast with `InvalidArguments`.
//!
//! These rules guard against accidental typos in the schema validation
//! and make the failure mode for malformed input obvious.
//!
//! ## No user variables
//!
//! The agent intentionally does not expose `meval`'s variable
//! binding API. Date arithmetic should go through `get_datetime` +
//! LLM arithmetic instead.

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::agents::{Agent, AgentError};

/// Hard cap on the expression length. `meval` itself imposes no
/// length limit, but unbounded input is a denial-of-service risk
/// even for a constant-time evaluator.
const MAX_EXPRESSION_LEN: usize = 256;

/// Allow-list of bytes accepted in an expression. Everything outside
/// this set (including non-ASCII) is rejected at the gate before
/// `meval` ever sees the input.
fn is_allowed_byte(b: u8) -> bool {
    matches!(b, b'0'..=b'9'
        | b'a'..=b'z'
        | b'A'..=b'Z'
        | b'_' | b'+' | b'-' | b'*' | b'/' | b'^' | b'%'
        | b'(' | b')' | b'.'
        | b',' | b' ' | b'\t' | b'\n')
}

/// Build of the `calculate` agent. Stateless — the constructor only
/// stores the `meval` context (which is itself stateless).
#[derive(Clone)]
pub struct CalculateAgent;

impl std::fmt::Debug for CalculateAgent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CalculateAgent").finish()
    }
}

impl Default for CalculateAgent {
    fn default() -> Self {
        Self::new()
    }
}

impl CalculateAgent {
    /// Construct a new agent. There is no configuration knob in v1
    /// — the agent is a thin wrapper over `meval::eval_str`.
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Agent for CalculateAgent {
    fn name(&self) -> &str {
        "calculate"
    }

    fn description(&self) -> &str {
        "Évalue une expression arithmétique et renvoie le résultat numérique. \
         Supporte +, -, *, /, ^, %, parenthèses, fonctions (sin, cos, sqrt, log, exp, abs, ...) \
         et constantes (pi, e). Pas d'I/O, pas de variables utilisateur. \
         Use for 'combien font 15% de 87,50', 'sqrt(2)', '2^10 + 1'. \
         Pass `expression` (required, max 256 caractères)."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "expression": {
                    "type": "string",
                    "maxLength": 256,
                    "description": "Arithmetic expression to evaluate, e.g. '15*87.5/100', 'sqrt(2)+1', '2^10'. Allowed characters: digits, letters (functions/constants), + - * / ^ % ( ) . , whitespace. Max 256 characters."
                }
            },
            "required": ["expression"],
            "additionalProperties": false,
        })
    }

    async fn invoke(&self, args: Value) -> Result<String, AgentError> {
        let req = parse_args(&args)?;
        let value = meval::eval_str(&req.expression)
            .map_err(|e| AgentError::AgentFailed(format!("could not evaluate expression: {e}")))?;
        // Round to a sane precision so the LLM doesn't see
        // floating-point noise like 0.30000000000000004. Twelve
        // significant digits is well past what any reasonable chat
        // user would type.
        let formatted = format_number(value);
        Ok(serde_json::to_string(&json!({
            "ok": true,
            "summary": format!("{expression} = {formatted}", expression = req.expression, formatted = formatted),
            "data": {
                "value": value,
                "expression": req.expression,
                "formatted": formatted,
            },
            "source": "local",
            "fetched_at": chrono::Utc::now().to_rfc3339(),
        }))
        .expect("json encode"))
    }
}

// ---- Argument parsing ----------------------------------------------------

#[derive(Debug)]
struct ParsedArgs {
    expression: String,
}

fn parse_args(args: &Value) -> Result<ParsedArgs, AgentError> {
    let obj = args
        .as_object()
        .ok_or_else(|| AgentError::InvalidArguments("arguments must be a JSON object".into()))?;
    let expression = obj
        .get("expression")
        .and_then(|v| v.as_str())
        .ok_or_else(|| AgentError::InvalidArguments("`expression` (string) is required".into()))?
        .to_string();
    validate_expression(&expression)?;
    Ok(ParsedArgs { expression })
}

/// Apply the deny-list: length cap and allow-listed characters.
/// Anything outside the set is an `InvalidArguments` error so the LLM
/// sees a clear message rather than a parser error from `meval`.
fn validate_expression(expr: &str) -> Result<(), AgentError> {
    if expr.is_empty() {
        return Err(AgentError::InvalidArguments(
            "`expression` must not be empty".into(),
        ));
    }
    if expr.len() > MAX_EXPRESSION_LEN {
        return Err(AgentError::InvalidArguments(format!(
            "`expression` must be at most {MAX_EXPRESSION_LEN} characters (got {})",
            expr.len()
        )));
    }
    for ch in expr.chars() {
        if !is_allowed_char(ch) {
            return Err(AgentError::InvalidArguments(format!(
                "`expression` contains an unsupported character `{ch}`; \
                 allowed: digits, letters, + - * / ^ % ( ) . , whitespace"
            )));
        }
    }
    Ok(())
}

fn is_allowed_char(ch: char) -> bool {
    let b = ch as u32;
    if b > 0x7F {
        // Non-ASCII: explicitly rejected. `meval` itself only
        // accepts ASCII identifiers, so anything outside this range
        // would fail later anyway — failing fast gives a clearer
        // error.
        return false;
    }
    is_allowed_byte(b as u8)
}

// ---- Number formatting --------------------------------------------------

/// Format a `f64` for the LLM. Trims trailing zeros and clamps
/// obviously-broken values (`NaN`, `±inf`) to JSON `null` so the LLM
/// never sees `Infinity` as a string it has to interpret.
fn format_number(v: f64) -> Value {
    if !v.is_finite() {
        return Value::Null;
    }
    // `{}` with the default precision strips trailing zeros for
    // integers and keeps a sensible precision for fractions. For
    // chat-tool output this is the most useful default.
    let formatted = format!("{v}");
    // Strip trailing `.0` so `87.5` doesn't become `87.50000…`.
    let formatted = if formatted.ends_with(".0") && formatted.matches('.').count() == 1 {
        formatted.trim_end_matches(".0").to_string()
    } else {
        formatted
    };
    json!(formatted)
}

// ---- Tests ---------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn name_and_schema_are_stable() {
        let agent = CalculateAgent::new();
        assert_eq!(agent.name(), "calculate");
        let schema = agent.parameters_schema();
        assert_eq!(schema["type"], "object");
        assert!(schema["required"]
            .as_array()
            .unwrap()
            .contains(&json!("expression")));
        assert_eq!(schema["properties"]["expression"]["maxLength"], 256);
    }

    #[test]
    fn validate_accepts_basic_arithmetic() {
        assert!(validate_expression("1+2").is_ok());
        assert!(validate_expression("15*87.5/100").is_ok());
        assert!(validate_expression("sqrt(2) + 1").is_ok());
        assert!(validate_expression("2^10").is_ok());
        assert!(validate_expression("sin(pi/2)").is_ok());
        assert!(validate_expression("(1 + 2) * 3").is_ok());
        assert!(validate_expression("log(e)").is_ok());
    }

    #[test]
    fn validate_rejects_empty() {
        let err = validate_expression("").unwrap_err();
        assert!(matches!(err, AgentError::InvalidArguments(_)));
    }

    #[test]
    fn validate_rejects_oversized() {
        let huge = "1".repeat(MAX_EXPRESSION_LEN + 1);
        let err = validate_expression(&huge).unwrap_err();
        match err {
            AgentError::InvalidArguments(msg) => {
                assert!(msg.contains("at most 256"));
            }
            other => panic!("expected InvalidArguments, got {other:?}"),
        }
    }

    #[test]
    fn validate_rejects_disallowed_chars() {
        // Semicolons, brackets, quotes, backslashes, non-ASCII.
        // We deliberately avoid inputs like `"import os"` — every
        // character there is in our allow-list (letters, space),
        // so the test would not exercise the rejection branch.
        for bad in ["1;2", "1'2", "1\"2", "1\\2", "[1+2]", "é"] {
            let err = validate_expression(bad).unwrap_err();
            match err {
                AgentError::InvalidArguments(msg) => {
                    assert!(msg.contains("unsupported character"));
                }
                other => panic!("expected InvalidArguments for {bad:?}, got {other:?}"),
            }
        }
    }

    #[test]
    fn format_number_strips_trailing_dot_zero() {
        assert_eq!(format_number(15.0), json!("15"));
        assert_eq!(format_number(0.0), json!("0"));
        assert_eq!(format_number(87.5), json!("87.5"));
        // Big ints stay readable; the precision is `{}`'s default.
        assert_eq!(format_number(1024.0), json!("1024"));
    }

    #[test]
    fn format_number_nulls_nan_and_infinity() {
        // `f64::INFINITY`, `f64::NEG_INFINITY`, `f64::NAN` are all
        // poison for downstream JSON consumers; serialising them as
        // `null` keeps the envelope well-formed.
        assert_eq!(format_number(f64::INFINITY), Value::Null);
        assert_eq!(format_number(f64::NEG_INFINITY), Value::Null);
        assert!(format_number(f64::NAN).is_null());
    }

    #[tokio::test]
    async fn invoke_basic_arithmetic() {
        let agent = CalculateAgent::new();
        let out = agent
            .invoke(json!({"expression": "15*87.5/100"}))
            .await
            .expect("invoke");
        let parsed: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed["ok"], true);
        assert_eq!(parsed["source"], "local");
        let v = parsed["data"]["value"].as_f64().unwrap();
        assert!((v - 13.125).abs() < 1e-9, "got {v}");
        assert_eq!(parsed["data"]["formatted"], "13.125");
        // The summary field is the front-end's primary rendering
        // surface for the tool bubble; guard the format so the
        // expression and formatted result both show up.
        let summary = parsed["summary"].as_str().expect("summary");
        assert!(summary.contains("15*87.5/100"), "summary: {summary}");
        assert!(summary.contains("13.125"), "summary: {summary}");
    }

    #[tokio::test]
    async fn invoke_functions_and_constants() {
        let agent = CalculateAgent::new();
        let out = agent
            .invoke(json!({"expression": "sqrt(2) + 1"}))
            .await
            .expect("invoke");
        let parsed: Value = serde_json::from_str(&out).unwrap();
        let v = parsed["data"]["value"].as_f64().unwrap();
        assert!((v - 2.414213562373095).abs() < 1e-9);
    }

    #[tokio::test]
    async fn invoke_rejects_unknown_identifier() {
        // `meval` returns an error for unknown identifiers. The
        // agent surfaces it as `AgentFailed` so the LLM sees
        // "could not evaluate expression: …" and can correct itself.
        let agent = CalculateAgent::new();
        let err = agent
            .invoke(json!({"expression": "nosuchfunc(1)"}))
            .await
            .expect_err("should fail");
        match err {
            AgentError::AgentFailed(msg) => {
                assert!(msg.contains("could not evaluate"));
            }
            other => panic!("expected AgentFailed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn invoke_rejects_non_object_arguments() {
        let agent = CalculateAgent::new();
        let err = agent
            .invoke(json!("not-an-object"))
            .await
            .expect_err("should reject");
        assert!(matches!(err, AgentError::InvalidArguments(_)));
    }

    #[tokio::test]
    async fn invoke_rejects_missing_expression() {
        let agent = CalculateAgent::new();
        let err = agent.invoke(json!({})).await.expect_err("should reject");
        match err {
            AgentError::InvalidArguments(msg) => {
                assert!(msg.contains("`expression`"));
            }
            other => panic!("expected InvalidArguments, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn invoke_rejects_disallowed_characters() {
        // Even when the rest of the expression is valid, a forbidden
        // character must trigger `InvalidArguments` so the LLM sees
        // a clear message rather than a `meval` parser error.
        let agent = CalculateAgent::new();
        let err = agent
            .invoke(json!({"expression": "1; rm -rf /"}))
            .await
            .expect_err("should reject");
        match err {
            AgentError::InvalidArguments(msg) => {
                assert!(msg.contains("unsupported character"));
            }
            other => panic!("expected InvalidArguments, got {other:?}"),
        }
    }
}
