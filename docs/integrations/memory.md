# Long-term memory (`memory-agent`)

> **Status**: shipped behind the `memory-agent` cargo feature and the
> `LLM_ALLOW_USER_MEMORY` env knob. Implementation-ready plan:
> `.kilo/plans/1791267136806-memory-agent-plan.md`.

## What it does

The LLM forgets across `POST /v1/chat/session` calls. The
`memory-agent` feature adds a per-user encrypted triple store
(`subject`, `predicate`, `value`) so a fact the user shared in one
chat ("my doctor is Dr Martin") is auto-injected at the start of
the next chat.

Three things happen:

1. **Per-user encryption** — every row's `value` and `notes` are
   AES-256-GCM ciphertext, encrypted with the same key the
   per-user credentials vault uses (`[auth.credentials].key`,
   loaded once at boot). The taxonomy columns (`subject`,
   `predicate`, `tags`) are plaintext to keep the recall query a
   `WHERE subject LIKE …` scan without per-row decrypt (plan §1.2
   documents this taxonomy-leak tradeoff — a blind-index PR is
   reserved for v2).
2. **Server-side auto-injection** — when the user has opted in
   (`memory_enabled` on the `user_preferences` row) AND has at
   least one stored row, the LLM proxy injects a system message
   beginning with `The user's long-term memories include:` at
   index 1 (right after the admin prompt) so the LLM can answer
   "what's my doctor's name?" without a tool call. Top-K is
   configurable via `[memory].recalled_top_k` (default `10`).
4. **LLM-driven store / recall / list / forget** — four agents
   (`memory_store`, `memory_recall`, `memory_list`,
   `memory_forget`) are wired into the registry when the cargo
   feature is on. `memory_store` is `Allow` (idempotent on
   `(subject, predicate)`); `memory_forget` is
   `NeedsConfirmation` (the chat UI surfaces an "are you sure?"
   bubble).

## When the subsystem is killed

The subsystem fails closed at four distinct layers, so an
operator can disable it without surprises:

- **`memory-agent` cargo feature off** — the four agents are not
  compiled in. `AGENT_DESCRIPTORS` does not include them.
- **`[auth.credentials].key` missing** — the `MemorySource`
  adapter is not built. `LlmState.memory_source` stays `None`,
  the proxy threads `None` into every chat-session
  `UserContext`, and the agents (if compiled) surface a clean
  `AgentError::AgentFailed("memory: source not wired in this
  context")` error.
- **`LLM_ALLOW_USER_MEMORY=false`** (or `[llm].allow_user_memory =
  false`) — the auto-prompt inject is skipped, and the defensive
  kill-switch in `llm::privacy::strip_user_memories_if_disabled`
  drops the marker block from the upstream payload before the
  upstream is called (so a half-built cached `__PG_state` cannot
  leak through).
- **`memory_enabled = false`** (per-user preference, default) —
  the user has not opted in; the proxy does not call
  `MemorySource::recall` for the auto-prompt and the LLM is told
  in the system prompt to refuse `memory_store` calls.

The four kill-switches are independent — an operator may forbid
one without touching the others.

## Configuration

| Knob | Type | Default | Notes |
| --- | --- | --- | --- |
| `[llm].allow_user_memory` | bool | `true` | Env mirror `LLM_ALLOW_USER_MEMORY`. Defensive kill-switch. |
| `[memory].recalled_top_k` | integer | `10` | Env mirror `MEMORY_TOP_K`. Clamped `[1, 64]`. |
| `memory_enabled` (per-user preference) | bool | `false` | Set via `PUT /api/me/preferences`. Drives the auto-prompt and the LLM's proactive `memory_store` policy. |

TOML snippet:

```toml
[llm]
allow_user_memory = true  # default

[memory]
recalled_top_k = 10  # default
```

## What gets stored where

| Column | Plaintext? | Why |
| --- | --- | --- |
| `value` | NO (BLOB) | Holds the PII ("Dr Martin", "penicillin"). AES-256-GCM. |
| `notes` | NO (BLOB) | Free-form user-supplied context. AES-256-GCM. |
| `subject` | YES (TEXT) | Categorical key (e.g. "doctor"). Drives the LIKE filter. |
| `predicate` | YES (TEXT) | Categorical key (e.g. "name"). |
| `tags` | TEXT NOT NULL DEFAULT '' | Comma-separated, used for LIKE filter. |
| `confidence` | REAL | Used in `ORDER BY confidence DESC`. |
| `source_session_id` | TEXT | Nullable provenance. |
| `source_kind` | TEXT NOT NULL DEFAULT 'user_stated' | `'user_stated' \| 'llm_inferred'`. |
| `created_at` / `last_used_at` / `expires_at` | TEXT | RFC 3339 timestamps. |

A `subject` / `predicate` leak still leaks taxonomy ("user is
allergic to X") but not the value ("penicillin"). The plan
documents the tradeoff and reserves a blind-index PR for v2.

## HTTP routes

| Route | Verb | Notes |
| --- | --- | --- |
| `GET /api/memories` | GET | List metadata rows, newest first. **No plaintext** — the SPA only ever sees the taxonomy. |
| `DELETE /api/memories/:id` | DELETE | Forget one memory. Cross-user attempts return 404 (no row exists from the caller's perspective). CSRF-protected. |

## Audit rows

The `memories` adapter writes one row per call:

| `kind` | `target_service` | When |
| --- | --- | --- |
| `memory_store` | `memory` | One per accepted `memory_store` call. Body never logged. |
| `memory_recall` | `memory` | One per `memory_recall` / `memory_list` call. |
| `memory_forget` | `memory` | One per `memory_forget` call. |
| `memory_decrypt_failed` | `memory` | One per row whose AES-GCM auth failed. |

The audit row never carries plaintext — only the row id and the
taxonomy columns. See `crates/nagent-db/src/events.rs` for the full
audit kind table.

## Operator notes

- The `memory-agent` cargo feature is part of the `all-agents`
  meta-feature. A targeted slim build can leave it off via
  `--no-default-features --features <other agents>`.
- The `MemorySource` adapter is `Clone`-cheap (everything is
  `Arc`-backed). The encryption key is `SecretBox`-wrapped so
  accidental `Debug` output does not leak the bytes.
- A future per-user quota (count + bytes) is deferred — see plan
  §6.

## See also

- `docs/architecture.md` §2.4 — agents table.
- `docs/architecture.md` §1.1 — composition tree.
- `.kilo/plans/1791267136806-memory-agent-plan.md` — full plan.