//! `llm::sse` — SSE frame parsing + tool-loop event builders.
//!
//! Everything here deals with the wire format of an SSE stream:
//!
//! - [`SseStream`] wraps the upstream byte stream with an idle timeout
//! and yields parsed [`SseEvent`]s (one event per blank-line split).
//! - [`drain_upstream_round`] drives the loop, forwarding each event
//! to the client byte-for-byte while accumulating `tool_calls`
//! deltas internally.
//! - [`ToolCallAccumulator`] merges SSE deltas into [`PendingToolCall`]
//! records keyed by the upstream `index` field.
//! - [`sse_tool_call_event`] / [`sse_tool_result_event`] /
//! [`sse_error_event`] are the synthesised frames the tool loop
//! emits in addition to the verbatim upstream bytes.

use std::time::Duration;

use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use serde_json::{json, Value};

/// Per-round inner state for the upstream→client loop.
///
/// Each round we open a fresh upstream request and drain its SSE byte
/// stream into the client channel, buffering any `tool_calls` deltas
/// for the round we just finished.
pub(crate) struct ToolCallAccumulator {
    /// Buffered tool calls keyed by their `index` field in the SSE
    /// delta. Multiple `index` values are possible when the LLM emits
    /// parallel tool calls in one round (rare with the supported
    /// models but legal in the spec).
    by_index: std::collections::BTreeMap<u32, PendingToolCall>,
}

#[derive(Debug, Default, Clone)]
pub(crate) struct PendingToolCall {
    pub(crate) id: String,
    pub(crate) name: Option<String>,
    pub(crate) arguments: String,
}

impl ToolCallAccumulator {
    pub(crate) fn new() -> Self {
        Self {
            by_index: std::collections::BTreeMap::new(),
        }
    }
    pub(crate) fn apply_delta(&mut self, raw: &Value) {
        let Some(arr) = raw.as_array() else { return };
        for tc in arr {
            let idx = tc.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
            let entry = self.by_index.entry(idx).or_default();
            if let Some(id) = tc.get("id").and_then(|v| v.as_str()) {
                entry.id = id.to_string();
            }
            if let Some(func) = tc.get("function") {
                if let Some(name) = func.get("name").and_then(|v| v.as_str()) {
                    entry.name = Some(name.to_string());
                }
                if let Some(args) = func.get("arguments").and_then(|v| v.as_str()) {
                    entry.arguments.push_str(args);
                }
            }
        }
    }
    pub(crate) fn into_sorted(self) -> Vec<PendingToolCall> {
        self.by_index.into_values().collect()
    }
}

/// Comma-separated list of tool names for log lines.
///
/// Names may be missing when the model emits the `arguments` delta
/// before the `name` (the SSE spec permits either order); we surface
/// `<unknown>` in that case so the count stays honest and the log
/// line is unambiguous. Arguments / payloads are deliberately omitted
/// — operators only need to know *which* agents ran, not *what they
/// were called with*.
pub(crate) fn tool_call_names(tool_calls: &[PendingToolCall]) -> String {
    tool_calls
        .iter()
        .map(|tc| tc.name.as_deref().unwrap_or("<unknown>"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Drain a single upstream SSE round and return what we learned.
///
/// `tx` receives the verbatim upstream SSE bytes — anything that
/// arrives from the upstream (including its final `[DONE]` marker) is
/// pushed straight through to the client. We additionally accumulate
/// `tool_calls` deltas internally and report them in the returned
/// [`RoundOutcome`] so the caller can decide whether to start a new
/// round.
pub(crate) async fn drain_upstream_round(
    upstream_bytes: impl Stream<Item = Result<Bytes, reqwest::Error>> + Unpin,
    idle_timeout: Duration,
    tx: &tokio::sync::mpsc::Sender<Result<Bytes, std::io::Error>>,
) -> Result<RoundOutcome, std::io::Error> {
    let mut acc = ToolCallAccumulator::new();
    let mut assistant_text = String::new();

    let mut sse = SseStream::new(idle_timeout, upstream_bytes);
    while let Some(event) = sse.next_event().await? {
        // The `[DONE]` sentinel is special: the client uses it to
        // close the response stream (`reader.cancel()` on the
        // browser). If we forward it here, the client cuts the
        // connection and any subsequent `event: tool_call` /
        // `event: tool_result` frames we emit on the same response
        // arrive after the cancel and are dropped — the tool bubble
        // never resolves. So we *swallow* `[DONE]` here; the
        // `run_tool_loop` outer function emits a single final
        // `[DONE]` after the last round has completed.
        if event.data.trim() == "[DONE]" {
            break;
        }
        // Forward the event verbatim, then parse it for tool_calls.
        // We *only* add an `event:` line when the upstream named the
        // event; this preserves the original SSE framing (no
        // synthetic `event: message`) so existing clients that expect
        // raw `data:` lines keep working byte-for-byte.
        let frame = match &event.name {
            Some(name) => format!("event: {name}\ndata: {d}\n\n", d = event.data),
            None => format!("data: {d}\n\n", d = event.data),
        };
        if tx.send(Ok(Bytes::from(frame))).await.is_err() {
            // Client disconnected mid-stream. Stop processing: the
            // outer loop will pick up on the dropped channel on the
            // next iteration.
            return Ok(RoundOutcome {
                assistant_text,
                tool_calls: acc.into_sorted(),
            });
        }
        let parsed: Value = match serde_json::from_str(&event.data) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if let Some(choices) = parsed.get("choices").and_then(|c| c.as_array()) {
            for choice in choices {
                if let Some(delta) = choice.get("delta") {
                    if let Some(text) = delta.get("content").and_then(|v| v.as_str()) {
                        assistant_text.push_str(text);
                    }
                    if let Some(tcs) = delta.get("tool_calls") {
                        acc.apply_delta(tcs);
                    }
                }
            }
        }
    }

    Ok(RoundOutcome {
        assistant_text,
        tool_calls: acc.into_sorted(),
    })
}

pub(crate) struct RoundOutcome {
    pub(crate) assistant_text: String,
    pub(crate) tool_calls: Vec<PendingToolCall>,
}

/// SSE event we have parsed out of the upstream byte stream.
pub(crate) struct SseEvent {
    pub(crate) name: Option<String>,
    pub(crate) data: String,
}

/// Line-oriented SSE parser wrapped around a byte stream with an
/// idle-timeout. The upstream sends events as
/// `event: name\ndata: …\n\n` (or just `data: …\n\n`); we accumulate
/// until we see a blank line, then yield the event. We tolerate CRLF
/// and tolerate comments / unknown fields.
pub(crate) struct SseStream<S> {
    src: S,
    timeout: Duration,
    buf: String,
}

impl<S> SseStream<S>
where
    S: Stream<Item = Result<Bytes, reqwest::Error>> + Unpin,
{
    pub(crate) fn new(timeout: Duration, src: S) -> Self {
        Self {
            src,
            timeout,
            buf: String::new(),
        }
    }

    pub(crate) async fn next_event(&mut self) -> Result<Option<SseEvent>, std::io::Error> {
        loop {
            // Split off the first complete event if we already have one
            // buffered.
            if let Some(idx) = self.buf.find("\n\n") {
                let raw = self.buf[..idx].to_string();
                self.buf.drain(..idx + 2);
                return Ok(Some(parse_sse_frame(&raw)));
            }
            // Otherwise wait for more bytes.
            let chunk = match tokio::time::timeout(self.timeout, self.src.next()).await {
                Ok(Some(Ok(b))) => b,
                Ok(Some(Err(e))) => {
                    return Err(std::io::Error::other(e.to_string()));
                }
                Ok(None) => {
                    if self.buf.trim().is_empty() {
                        return Ok(None);
                    }
                    // Flush whatever is left.
                    let raw = std::mem::take(&mut self.buf);
                    return Ok(Some(parse_sse_frame(&raw)));
                }
                Err(_elapsed) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "upstream idle timeout",
                    ));
                }
            };
            self.buf.push_str(&String::from_utf8_lossy(&chunk));
        }
    }
}

fn parse_sse_frame(raw: &str) -> SseEvent {
    let mut name: Option<String> = None;
    let mut data_lines: Vec<String> = Vec::new();
    for line in raw.split('\n') {
        let line = line.trim_end_matches('\r');
        if line.is_empty() || line.starts_with(':') {
            continue;
        }
        if let Some(rest) = line.strip_prefix("event:") {
            name = Some(rest.trim().to_string());
        } else if let Some(rest) = line.strip_prefix("data:") {
            data_lines.push(rest.trim_start().to_string());
        }
        // All other fields (id:, retry:, …) are intentionally
        // ignored — we forward what we know to the browser verbatim
        // and the LLM proxy doesn't depend on them.
    }
    SseEvent {
        name,
        data: data_lines.join("\n"),
    }
}

pub(crate) fn sse_tool_call_event(id: &str, name: &str, arguments: &str, index: u32) -> String {
    // `arguments` may be partial JSON if the model split the call
    // across SSE chunks — we forward what we have at the moment we
    // decide to dispatch. The browser is the only consumer of this
    // event for the live bubble; the LLM gets the full arguments
    // through the next round's request body.
    let args_value: Value = serde_json::from_str(arguments).unwrap_or(Value::Null);
    let payload = json!({
        "id": id,
        "name": name,
        "args": args_value,
        "arguments_raw": arguments,
        "index": index,
    });
    format!("event: tool_call\ndata: {payload}\n\n")
}

pub(crate) fn sse_tool_result_event(id: &str, name: &str, ok: bool, payload: &str) -> String {
    let summary = if ok {
        // Pull a short "summary" out of the JSON payload when
        // possible so the bubble can show something more useful than
        // the full text. Best-effort — falls back to the raw string
        // for non-JSON payloads.
        serde_json::from_str::<Value>(payload)
            .ok()
            .and_then(|v| v.get("summary").and_then(|s| s.as_str().map(String::from)))
            .unwrap_or_else(|| payload.chars().take(80).collect::<String>())
    } else {
        payload.chars().take(160).collect::<String>()
    };
    let payload_json = json!({
        "id": id,
        "name": name,
        "ok": ok,
        "summary": summary,
        "content": payload,
    });
    format!("event: tool_result\ndata: {payload_json}\n\n")
}

pub(crate) fn sse_error_event(reason: &str, detail: &str) -> String {
    let payload = json!({
        "error": reason,
        "detail": detail,
    });
    format!("event: error\ndata: {payload}\n\n")
}

/// Public alias so [`crate::llm::proxy`] and
/// [`crate::llm::tool_loop`] can spell the upstream byte stream
/// uniformly.
pub(crate) type UpstreamByteStream =
    std::pin::Pin<Box<dyn futures_util::Stream<Item = Result<Bytes, reqwest::Error>> + Send>>;

/// Comma-separated tool-name list, public alias used by
/// [`crate::llm::tool_loop`] from inside the `info!` log line.
pub(crate) fn tool_call_names_pub(tool_calls: &[PendingToolCall]) -> String {
    tool_call_names(tool_calls)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tc(name: Option<&str>) -> PendingToolCall {
        PendingToolCall {
            id: "call_x".into(),
            name: name.map(str::to_owned),
            arguments: "{}".into(),
        }
    }

    #[test]
    fn tool_call_names_joins_known_names() {
        let calls = vec![tc(Some("web_fetch")), tc(Some("web_search"))];
        assert_eq!(tool_call_names(&calls), "web_fetch, web_search");
    }

    #[test]
    fn tool_call_names_marks_missing_name_as_unknown() {
        // Parallel calls where the SSE delta order dropped the `name`
        // before `arguments`. The log must still be unambiguous.
        let calls = vec![tc(Some("web_fetch")), tc(None), tc(Some("weather"))];
        assert_eq!(tool_call_names(&calls), "web_fetch, <unknown>, weather");
    }

    #[test]
    fn tool_call_names_handles_empty_input() {
        assert_eq!(tool_call_names(&[]), "");
    }
}
