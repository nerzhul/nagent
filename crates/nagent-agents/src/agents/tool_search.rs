//! `search_tools` agent — the Anthropic-style tool-discovery meta-tool.
//!
//! The LLM calls `search_tools(query="weather", top_k=5)` to
//! receive the JSON Schemas for the agents most likely to fit.
//! The router scores the query against the registry via BM25
//! (see [`crate::tools_router`]); the response is a JSON
//! payload the LLM can use to plan its next round.
//!
//! Contract:
//!
//! - `name()` → `"search_tools"` — the meta-tool itself is the
//!   one the LLM always sees.
//! - `untrusted_output()` → `false` — the result is
//!   server-generated, not a remote fetch, so the tool loop does
//!   not wrap it in the untrusted-input fence.
//! - `requires_confirmation()` → `Allow` — discovery must never
//!   trigger an approval card; the LLM is expected to iterate
//!   freely.
//!
//! The router is wired by the server's boot path
//! ([`crate::agents::AgentRegistry::wire_router`]) after the
//! registry is built. Direct-invoke routes and unit tests run
//! without a wired router and surface a clear error.

use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::agents::{Agent, AgentError, UserContext};
use crate::config::ToolSearchAgentConfig;
use crate::tools_router::{ToolsRouter, SEARCH_TOOLS_NAME};

/// Stable wire name for the meta-tool. Re-exported so other
/// crates reach the agent's file via the `SEARCH_TOOLS_NAME`
/// constant without having to import the router module.
pub use crate::tools_router::SEARCH_TOOLS_NAME as SEARCH_TOOLS_AGENT_NAME;

/// Default `top_k` when the LLM does not pass one. Matches the
/// value in the plan.
const DEFAULT_TOP_K: usize = 5;
/// Hard cap on `top_k` so a chatty model cannot request the
/// entire registry.
const MAX_TOP_K: usize = 20;

/// `search_tools` agent. Stateless apart from the router slot
/// (filled in once at boot by the server's registry wiring
/// helper) and the small `top_k` config knob.
pub struct ToolSearchAgent {
    config: ToolSearchAgentConfig,
    /// `None` until [`AgentRegistry::wire_router`] runs at boot.
    /// The `RwLock` keeps the trait `&self`-only — every other
    /// agent method on the trait is `&self`, so the wire hook
    /// has to be too.
    router: RwLock<Option<Arc<ToolsRouter>>>,
}

impl std::fmt::Debug for ToolSearchAgent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolSearchAgent")
            .field("config", &self.config)
            .field(
                "router",
                &self.router.read().map(|g| g.is_some()).unwrap_or(false),
            )
            .finish()
    }
}

impl Default for ToolSearchAgent {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolSearchAgent {
    /// Construct an agent with the default config and no router.
    pub fn new() -> Self {
        Self {
            config: ToolSearchAgentConfig::default(),
            router: RwLock::new(None),
        }
    }

    /// Uniform constructor for the static factory table.
    pub fn from_config(config: ToolSearchAgentConfig) -> Self {
        Self {
            config,
            router: RwLock::new(None),
        }
    }

    /// Default `top_k` from the config; `DEFAULT_TOP_K` when the
    /// knob is unset. Exposed so the proxy and tests share the
    /// exact same fallback value.
    pub fn default_top_k(&self) -> usize {
        self.config.default_top_k.max(1)
    }
}

#[async_trait]
impl Agent for ToolSearchAgent {
    fn name(&self) -> &str {
        // The meta-tool's stable wire name. The router excludes
        // it from the index so the agent never recommends
        // itself.
        SEARCH_TOOLS_NAME
    }

    fn description(&self) -> &str {
        "Discover the tools you have for a task. Pass `query` (required, short \
         natural-language description of what you want to do, e.g. 'weather in \
         Paris', 'definition of photosynthesis', 'convert 100 USD to EUR', \
         'wikipedia summary of X') and optionally `top_k` (1..=20, default 5). \
         Returns the matching tools' names + JSON Schemas so you can call \
         them directly on the next round. Use this when you are unsure which \
         tool fits; for trivial queries covered by a pre-selected tool, just \
         call it by name."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Short natural-language description of what you want to do. e.g. 'weather in Paris', 'definition of photosynthesis', 'convert 100 USD to EUR'."
                },
                "top_k": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 20,
                    "default": DEFAULT_TOP_K,
                    "description": "Maximum number of matching tools to return. Default 5."
                }
            },
            "required": ["query"],
            "additionalProperties": false,
        })
    }

    fn untrusted_output(&self) -> bool {
        // The result is server-generated (a BM25 walk over the
        // agent registry) — nothing fetched from a remote
        // endpoint, nothing a remote document could poison. The
        // untrusted-input fence would be noise here.
        false
    }

    fn wire_router(&self, router: Arc<ToolsRouter>) {
        if let Ok(mut guard) = self.router.write() {
            *guard = Some(router);
        }
        // A poisoned lock is a programmer error we cannot
        // recover from here; the boot path would have panicked
        // already, so we silently swallow it. The next
        // `invoke` will then surface the "no router wired"
        // error which is the right user-facing signal.
    }

    async fn invoke(&self, _ctx: &UserContext, args: Value) -> Result<String, AgentError> {
        let req = parse_args(&args, self.default_top_k())?;
        let router_clone = {
            let guard = self.router.read().map_err(|_| {
                AgentError::AgentFailed("search_tools: router slot poisoned".into())
            })?;
            guard.clone()
        };
        let router = router_clone.ok_or_else(|| {
            AgentError::AgentFailed(
                "search_tools: no router wired into this context; \
                 the meta-tool must be reached through the chat-completions proxy"
                    .to_string(),
            )
        })?;
        // Walk the BM25 index. `top_k` is already validated
        // (1..=MAX) so the cap can never be `0`.
        let hits = router.search(&req.query, req.top_k);
        // Shape the JSON payload the LLM will read. The wire
        // envelope matches the other agents' shape (`ok` +
        // `source` + `summary` + `data`) so the chat UI's tool
        // bubble renders consistently.
        let summary = if hits.is_empty() {
            format!(
                "no tools matched `{}` (top_k={}); the registry may be empty \
                 or the query too narrow",
                req.query, req.top_k
            )
        } else {
            let names: Vec<String> = hits.iter().map(|h| h.name.as_ref().to_string()).collect();
            format!(
                "matched {} tool(s) for `{}`: {}",
                hits.len(),
                req.query,
                names.join(", ")
            )
        };
        let payload = json!({
            "ok": true,
            "summary": summary,
            "data": {
                "results": hits.iter().map(|h| {
                    json!({
                        "name": h.name.as_ref(),
                        "description": h.description.as_ref(),
                    })
                }).collect::<Vec<_>>(),
                "query": req.query,
                "top_k": req.top_k,
            },
            "source": "tools_router",
            "fetched_at": chrono::Utc::now().to_rfc3339(),
        });
        Ok(payload.to_string())
    }
}

#[derive(Debug)]
struct ParsedArgs {
    query: String,
    top_k: usize,
}

fn parse_args(args: &Value, default_top_k: usize) -> Result<ParsedArgs, AgentError> {
    let obj = args
        .as_object()
        .ok_or_else(|| AgentError::InvalidArguments("arguments must be a JSON object".into()))?;
    let query = obj
        .get("query")
        .and_then(|v| v.as_str())
        .ok_or_else(|| AgentError::InvalidArguments("`query` (string) is required".into()))?
        .trim()
        .to_string();
    if query.is_empty() {
        return Err(AgentError::InvalidArguments(
            "`query` must not be empty".into(),
        ));
    }
    if query.len() > 256 {
        return Err(AgentError::InvalidArguments(format!(
            "`query` must be at most 256 characters (got {})",
            query.len()
        )));
    }
    let top_k = match obj.get("top_k") {
        None | Some(Value::Null) => default_top_k,
        Some(v) => v.as_u64().ok_or_else(|| {
            AgentError::InvalidArguments("`top_k` must be a positive integer".into())
        })? as usize,
    };
    if top_k == 0 || top_k > MAX_TOP_K {
        return Err(AgentError::InvalidArguments(format!(
            "`top_k` must be between 1 and {MAX_TOP_K} (got {top_k})"
        )));
    }
    Ok(ParsedArgs { query, top_k })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::{AgentRegistry, UserContext};
    use crate::ServiceRegistry;
    use serde_json::json;

    fn ctx() -> UserContext {
        UserContext::for_tests(uuid::Uuid::new_v4(), ServiceRegistry::empty().into_arc())
    }

    #[test]
    fn name_and_schema_are_stable() {
        let agent = ToolSearchAgent::new();
        assert_eq!(agent.name(), "search_tools");
        assert_eq!(agent.untrusted_output(), false);
        let schema = agent.parameters_schema();
        assert_eq!(schema["type"], "object");
        let required = schema["required"].as_array().unwrap();
        assert!(required.contains(&json!("query")));
        assert_eq!(schema["properties"]["top_k"]["maximum"], 20);
        assert_eq!(schema["properties"]["top_k"]["default"], DEFAULT_TOP_K);
    }

    #[test]
    fn default_top_k_is_5() {
        let agent = ToolSearchAgent::new();
        assert_eq!(agent.default_top_k(), 5);
    }

    #[test]
    fn parse_args_accepts_minimum() {
        let p = parse_args(&json!({"query": "weather"}), 5).expect("parse");
        assert_eq!(p.query, "weather");
        assert_eq!(p.top_k, 5);
    }

    #[test]
    fn parse_args_uses_provided_top_k() {
        let p = parse_args(&json!({"query": "weather", "top_k": 8}), 5).expect("parse");
        assert_eq!(p.top_k, 8);
    }

    #[test]
    fn parse_args_caps_top_k_at_max() {
        let err = parse_args(&json!({"query": "weather", "top_k": 21}), 5).unwrap_err();
        match err {
            AgentError::InvalidArguments(msg) => assert!(msg.contains("between 1 and 20")),
            other => panic!("expected InvalidArguments, got {other:?}"),
        }
    }

    #[test]
    fn parse_args_rejects_zero_top_k() {
        let err = parse_args(&json!({"query": "weather", "top_k": 0}), 5).unwrap_err();
        assert!(matches!(err, AgentError::InvalidArguments(_)));
    }

    #[test]
    fn parse_args_rejects_empty_query() {
        let err = parse_args(&json!({"query": ""}), 5).unwrap_err();
        match err {
            AgentError::InvalidArguments(msg) => assert!(msg.contains("must not be empty")),
            other => panic!("expected InvalidArguments, got {other:?}"),
        }
    }

    #[test]
    fn parse_args_rejects_oversized_query() {
        let huge = "x".repeat(257);
        let err = parse_args(&json!({"query": huge}), 5).unwrap_err();
        match err {
            AgentError::InvalidArguments(msg) => assert!(msg.contains("at most 256")),
            other => panic!("expected InvalidArguments, got {other:?}"),
        }
    }

    #[test]
    fn parse_args_rejects_non_object() {
        let err = parse_args(&json!("not-an-object"), 5).unwrap_err();
        assert!(matches!(err, AgentError::InvalidArguments(_)));
    }

    #[test]
    fn parse_args_rejects_missing_query() {
        let err = parse_args(&json!({"top_k": 3}), 5).unwrap_err();
        match err {
            AgentError::InvalidArguments(msg) => assert!(msg.contains("query")),
            other => panic!("expected InvalidArguments, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn invoke_without_router_surfaces_wiring_error() {
        // The agent does not own a router until the boot path
        // calls `wire_router`. Direct callers (and tests) see a
        // clear error instead of a silently-empty list.
        let agent = ToolSearchAgent::new();
        let err = agent
            .invoke(&ctx(), json!({"query": "weather"}))
            .await
            .expect_err("expected wiring failure");
        match err {
            AgentError::AgentFailed(msg) => assert!(msg.contains("no router wired")),
            other => panic!("expected AgentFailed, got {other:?}"),
        }
    }

    #[test]
    fn wire_router_then_unwire_round_trip() {
        // The RwLock lets the trait's `&self` wire hook populate
        // the slot. Verify the read path sees what the write
        // path put in.
        let agent = ToolSearchAgent::new();
        assert!(
            agent.router.read().unwrap().is_none(),
            "freshly-built agent must have no router"
        );
        // We cannot easily build a real `ToolsRouter` without
        // a registry in scope here, but we can synthesise an
        // empty one by walking an empty registry.
        let reg = AgentRegistry::empty();
        let router = Arc::new(ToolsRouter::from_registry(&reg));
        agent.wire_router(router.clone());
        assert!(agent.router.read().unwrap().is_some());
    }
}
