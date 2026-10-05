# LLM configuration

The chat proxy is upstream-agnostic: it speaks the OpenAI
`/v1/chat/completions` wire format, so any LLM that exposes it
(Ollama, llama-server, vLLM, LM Studio, OpenRouter, …) works out
of the box. This page documents the two knobs the operator
controls server-side (`llm_max_tool_rounds`, `llm_max_auto_continues`)
and the one knob the operator controls upstream-side
(`num_ctx` / `n_ctx`) that has the biggest impact on long
reasoning chains.

## Server-side knobs

Both knobs live on the `[llm]` section of `config.toml` (or the
matching env vars). They are designed to be set-and-forget;
the defaults are tuned for the long-thinking reasoning models
this proxy is meant to drive (DeepSeek-R1, Qwen3.5 with
thinking on, o1/o3).

### `llm_max_tool_rounds`

Hard cap on the number of tool-call rounds a single user turn
may trigger before the proxy surfaces an error bubble. Defends
against models that loop on a tool call. Default `64` covers
deep agent compositions where the model chains many tool calls
in a single turn; lower the cap for upstreams that show runaway
behaviour.

- TOML key: `[llm].llm_max_tool_rounds`
- Env var: `LLM_MAX_TOOL_ROUNDS`
- Clamp: `1..=256`

### `llm_max_auto_continues`

Maximum number of **auto-continue rounds** the loop appends
when the upstream emits `finish_reason: "length"` while
producing only reasoning (no visible `delta.content` yet). Each
auto-continue is a fresh upstream request asking the model to
"continue from where you left off and produce the visible
answer now". Default `32` is generous — a long reasoning chain
that needs 30+ truncations is rare; the cap exists to bound
upstream spend, not to constrain the model. Set to `0` to
disable the heuristic (the loop then closes the stream on
truncation, surfacing the reasoning text to the user and
leaving continuation to an explicit "continue" prompt).

- TOML key: `[llm].llm_max_auto_continues`
- Env var: `LLM_MAX_AUTO_CONTINUES`
- Clamp: `0..=64`

## Upstream-side knob: `n_ctx` (the actual lever for long thinking)

`llm_max_auto_continues` is a fallback. The real fix for
"the model ran out of context mid-reasoning" is to give the
upstream more context. Each `num_ctx` bump roughly doubles
the amount of reasoning the model can produce in a single
round, and removes the auto-continue round-trips that cost
network latency and token budget.

The log line that means the upstream truncated:

```
slot release: id 0 | task 0 | stop processing: n_tokens = 4095, truncated = 1
```

`n_tokens = 4095` is the per-request token cap (here 4096 —
llama.cpp's default `n_ctx`). `truncated = 1` means the model
wanted to emit more but was cut off. Bumping `n_ctx` is the
fix.

### Ollama

`num_ctx` is the context-window size. Default 2048; bump to
**32768 or 65536** for reasoning models.

Three ways to set it, in increasing order of permanence:

1. **Per-request** via the OpenAI-compatible `/v1/chat/completions` API:
   ```json
   {
     "model": "deepseek-r1:32b",
     "messages": [...],
     "options": { "num_ctx": 32768 }
   }
   ```
2. **Per-model** in the Modelfile (recommended for a dedicated reasoning model):
   ```dockerfile
   # Modelfile
   FROM deepseek-r1:32b
   PARAMETER num_ctx 32768
   ```
   Then `ollama create reasoning-model -f Modelfile`.
3. **Server-wide** via the `OLLAMA_CONTEXT_LENGTH` env var on the Ollama process:
   ```bash
   OLLAMA_CONTEXT_LENGTH=32768 ollama serve
   ```

Verify with:

```bash
curl -s http://localhost:11434/api/show -d '{"name": "deepseek-r1:32b"}' | jq .model_info
# Look for "deepseek_r1.context_length": 32768
```

### llama-server (`llama.cpp`)

`llama-server` accepts the `-c` / `--ctx-size` flag at startup:

```bash
llama-server \
  -m /path/to/model.gguf \
  -c 32768 \
  --host 0.0.0.0 \
  --port 8080
```

For batch / one-shot modes, the equivalent flag is
`--ctx-size`. The OpenAI-compatible endpoint on llama-server
honours whatever `-c` is set to.

### Other OpenAI-compatible servers

- **vLLM**: `max_model_len` in the engine config or
  `--max-model-len 32768` on the CLI.
- **LM Studio**: set in the server UI under "Context Length".
- **OpenRouter / cloud providers**: the per-model context
  window is fixed by the provider; cannot be bumped from the
  client. This is exactly the case where `llm_max_auto_continues`
  matters — it lets the model keep producing across the
  provider's hard cap.

## How auto-continue interacts with the upstream cap

When the proxy sees `finish_reason: "length"` and the round
emitted no visible content, it appends a "please continue"
user message to the conversation and re-requests from the
upstream. The new round starts with a **fresh** context
window on the upstream — the model does not get the prior
truncation as input (that would double-count tokens). What
the user does see, in real time on the chat UI, is the
reasoning text from each prior round (streamed verbatim via
SSE), so the `<details>` block accumulates the model's
thinking as the loop progresses.

The loop closes when:

- The upstream emits `finish_reason: "stop"` (clean completion)
  or `finish_reason: "tool_calls"` (the model wants to call
  an agent — the tool loop dispatches it).
- The upstream emits `finish_reason: "length"` with visible
  content (e.g. the model produced half a sentence and ran out
  of context — the user gets what they got).
- `llm_max_auto_continues` is hit — the proxy surfaces a
  clear SSE `error` event naming the cap so the chat UI can
  render a friendly bubble ("the model kept reasoning after
  32 auto-continues without producing an answer; try a shorter
  question or a different model").
