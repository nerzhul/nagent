# LLM configuration

The chat proxy is upstream-agnostic: it speaks the OpenAI
`/v1/chat/completions` wire format, so any LLM that exposes it
(Ollama, llama-server, vLLM, LM Studio, OpenRouter, …) works out
of the box. This page documents the two knobs the operator
controls server-side (`llm_max_tool_rounds`, `llm_max_auto_continues`)
and the two Ollama-native knobs the operator controls
upstream-side (`ollama_num_predict`, `ollama_num_ctx`) that have
the biggest impact on long reasoning chains.

## Providers

`[llm].provider` selects the upstream family and its defaults:

| `provider` | `base_url` default | `default_model` default | outbound `api_key` | env vars for the key |
| --- | --- | --- | --- | --- |
| `ollama` (default) | `http://localhost:11434` | `llama3.1` | optional | `OLLAMA_API_KEY`, then `LLM_UPSTREAM_API_KEY` |
| `mistral` | `https://api.mistral.ai/v1` | `mistral-small-latest` | **required** (startup fails closed without it) | `MISTRAL_API_KEY`, then `LLM_UPSTREAM_API_KEY` |
| `openai-compatible` | none — **startup error** if unset and `enabled = true` | none — **startup error** if unset | optional | `LLM_UPSTREAM_API_KEY` |

- TOML key: `[llm].provider` (`"ollama"` | `"openai-compatible"` | `"mistral"`)
- Env var: `LLM_PROVIDER` (strict — a typo is a boot error)

Resolution order for every field stays **env > TOML > provider
default > built-in default**. The generic env names `LLM_BASE_URL`,
`LLM_MODEL` and `LLM_UPSTREAM_API_KEY` apply to every provider; the
legacy `OLLAMA_BASE_URL` / `OLLAMA_MODEL` / `OLLAMA_API_KEY` names
are honoured only when `provider = "ollama"` (the default), so
existing deployments keep working untouched. `MISTRAL_API_KEY` is
only read when `provider = "mistral"`.

`base_url` may already end in `/v1` (Mistral's default does): the
proxy appends the endpoint path (`chat/completions`, `models`)
without doubling the version segment, so
`https://api.mistral.ai/v1` + `chat/completions` becomes
`https://api.mistral.ai/v1/chat/completions`, not
`…/v1/v1/chat/completions` (which the hosted API rejects with
404 "no Route matched with those values"). A base without the
suffix (Ollama's `http://localhost:11434`) gets `/v1` added as
before.

### Request-body adaptation per provider

The proxy speaks the same OpenAI-compatible wire protocol to every
provider, but the generation-cap knob differs:

- `provider = "ollama"` → `ollama_num_predict` / `ollama_num_ctx`
  are merged into the Ollama-native `options` object of the
  forwarded body (see the two dedicated sections below).
- any other provider → the `options` object is **stripped** from
  the forwarded body (strict hosted APIs may 422 on it), and the
  generic `[llm].max_tokens` (env `LLM_MAX_TOKENS`) is injected as
  the top-level `max_tokens` field. `ollama_num_predict` is honoured
  *as* `max_tokens` on non-Ollama providers when `max_tokens` itself
  is unset, so one knob covers both families — the generic
  `max_tokens` wins when both are set.

Every upstream request (chat completions, `/v1/models`, tool-loop
rounds) also carries a `User-Agent: nagent/<version>` header, which
some hosted APIs require.

### Startup validation (fail closed)

- `provider = "mistral"` without an API key → startup error naming
  `MISTRAL_API_KEY` / `[llm].api_key`.
- `provider = "mistral"` with a `base_url` host other than
  `api.mistral.ai` → allowed (Mistral-compatible gateways, Azure
  AI), logged at `WARN` with the resolved host.
- any provider with a non-loopback `base_url` over plain `http://`
  and an `api_key` set → `WARN` (the bearer token would cross the
  wire in cleartext).
- `provider = "openai-compatible"` without `base_url` /
  `default_model` → startup error.

### `max_tokens`

Generic per-response generation cap (in tokens) for non-Ollama
providers, forwarded as the top-level `max_tokens` field of
`/v1/chat/completions`. `None` (the default) leaves the upstream's
own cap untouched. On `provider = "ollama"` this knob is ignored —
use `ollama_num_predict` there.

- TOML key: `[llm].max_tokens`
- Env var: `LLM_MAX_TOKENS`

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

## Upstream-side knob: `ollama_num_ctx` (the actual lever for long thinking)

`ollama_num_ctx` (forwarded to the upstream as `options.num_ctx`)
controls how much conversation history + system prompt the model
can see at once. `ollama_num_predict` (next section) controls how
many tokens it can emit per response. Both matter; bumping only
one often hides the other.

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

### Setting `num_ctx` through the proxy

The proxy exposes two equivalent paths for operators that prefer
not to touch the Modelfile:

- Env var: `LLM_OLLAMA_NUM_CTX=32768`
- TOML key: `[llm] ollama_num_ctx = 32768`

When set, the proxy forwards `options.num_ctx` on every
`/v1/chat/completions` request, including every tool-loop round
and every auto-continue round. The proxy wins on conflict: any
`options.num_ctx` the browser sets on a request body is
overridden by the server knob, so a misbehaving client cannot
shrink the context window. A `None` / unset value leaves the
upstream's `num_ctx` untouched — deployments that already pin
it via the Modelfile or `OLLAMA_CONTEXT_LENGTH` keep working
unchanged. The proxy only injects the field when the operator
opts in.

Recommended values mirror §5: `32768` for 9B–14B reasoners,
`65536` for 32B+ reasoners.

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

## Upstream-side knob: `ollama_num_predict` (the actual lever for per-response length)

`ollama_num_predict` (forwarded to the upstream as
`options.num_predict`) is **how many tokens the model may emit
in a single response**, distinct from `ollama_num_ctx` (how much
history the model can see). It is the most common cause of "the
model keeps reasoning but never produces an answer" symptoms on
reasoning models.

### The Ollama default that bites

Ollama's default `num_predict` is **128 tokens**. Reasoning
models stream a long thinking phase before any tool call or
visible text:

```
1203 chars of reasoning  → ~250 tokens wanted
128-token cap           -> cut off mid-thought
finish_reason: "length" -> triggers llm_max_auto_continues
"please continue" prompt -> model re-derives from scratch
                            (~50 chars of reasoning before
                            the next truncation)
```

…which produces a 30+ round auto-continue loop on a question
that should take one or two rounds. The model is not stupid;
it's being denied the budget to finish a thought.

### Fix

Set `ollama_num_predict` to `2048` (safe for 9B-class reasoners
like `qwen3.5:9b`) or `4096` (for 32B+ reasoners like
`deepseek-r1:32b`). One line:

```bash
LLM_OLLAMA_NUM_PREDICT=2048
```

…or in `config.toml`:

```toml
[llm]
ollama_num_predict = 2048
```

The proxy forwards this to the upstream as
`options.num_predict` on every `/v1/chat/completions` request,
including the tool-loop rounds. Setting it is purely additive —
operators who do not opt in keep the upstream's choice (e.g.
Ollama's 128-token default for non-reasoning models is fine).
The proxy wins on conflict: any `options.num_predict` the
browser sets on the request body is overridden by the server
knob, so a misbehaving client cannot force a tiny
`num_predict` on a reasoning model.

- Env var: `LLM_OLLAMA_NUM_PREDICT`
- TOML key: `[llm].ollama_num_predict`
- Recommended: `2048` for 9B reasoners, `4096` for 32B+ reasoners
- A `None` / unset value leaves the upstream's default alone —
  the proxy only injects the field when the operator opts in.

This is the single most effective knob for stopping the
auto-continue loop. Bumping `llm_max_auto_continues` is a
safety net for cloud-hosted providers with fixed small caps;
bumping `ollama_num_predict` is the actual fix for local Ollama.

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
