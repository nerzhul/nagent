# Multi-user secure mode + pluggable external sources for nagent

## Context

PR1 (authentication & user identity) has shipped. The remaining gap between
"single-user trust boundary with bearer auth" and "daily assistant platform"
is two capabilities:

1. **Per-user agent context + RBAC + audit log (PR2)** — agents receive the
   calling user's `AuthUser`, declared scopes are enforced, every
   invocation lands in an append-only audit log.
2. **Pluggable external sources (PR3)** — agents reach external systems
   (Google Workspace, Nextcloud, GitHub, Home Assistant, OpenStreetMap, …)
   through a uniform `Source` abstraction, with the LLM calling them via
   the existing `Agent` trait.

Sections are written to be **cut independently**: each one is a self-contained
workstream with its own task list. The plan deliberately defers
implementation choices (OAuth library, storage engine, MCP strategy) to
the pre-requisite PR2/PR3 plans that must land first.

## Hard pre-requisites (must land before anything else)

> **Status:** PR1 (Authentication & user identity) **landed** in commits
> `aaa465a` and `7fde83d`. The full multi-user auth subsystem is in place:
> `AuthUser`, `require_auth_middleware`, `passkey`/`password`/`oidc`
> providers, SQLite/Postgres session store, `/api/me`, CSRF, per-session
> TTL (`auth.session_ttl_days`, default 7 days), global portal and login
> UI. PR1 is removed from this plan but referenced from PR2+ as a satisfied
> dependency. PR2 and PR3 below remain the work to do before F1–F7.

These two remaining workstreams are the foundation. Every section below assumes they exist. PR1 (authentication) is already implemented.

### PR2. Agent context + RBAC + audit log

- Extend the `Agent` trait with an optional context: `async fn invoke_with_ctx(&self, ctx: &AgentContext, args: Value) -> Result<String, AgentError>`. The existing `invoke` becomes a default that calls `invoke_with_ctx` with a sentinel "anonymous" context, so current agents (`web_fetch`, `get_datetime`, `get_weather`, `get_stock`, plus the P1 daily-assistant ones from `1790446782053-daily-assistant-agents.md`) keep compiling unchanged.
- `AgentContext { user_id, display_name, scopes: HashSet<Scope>, oauth_tokens: SecretBox, audit_sink }`.
- A `Scope` is a string key (`"agent:memory:write"`, `"source:google_calendar:read"`, …). Agents and sources declare required scopes at registration; the dispatcher refuses the call if the calling user lacks them.
- Append-only audit log: one row per `agent_invoke` and per `source_call`, written from `audit_sink`. Backed by SQLite, queryable via `GET /api/audit?since=…&until=…`.
- Rate limit becomes per-user (extension of `crates/stt-server/src/rate_limit.rs`).

### PR3. Pluggable source framework

- New trait `Source: Send + Sync` with `name`, `scopes`, `auth_type`, `fetch(ctx, query)`, `health()`. Mirrors the shape of `Agent` but separate so the wire format (OpenAI `tools`) stays unchanged.
- A `SourceRegistry` parallels `AgentRegistry`. Sources are not directly exposed as LLM tools; instead, each source feeds one or more agents (`google_calendar` source → `calendar_today` / `calendar_add` agents that wrap it).
- First-class protocol: **MCP** (Model Context Protocol) — register a remote MCP server via URL + bearer, and the server auto-discovers its tools, wrapping each as an `Agent`. Operators can also drop in hand-written Rust agents as today.
- OAuth token vault: tokens encrypted with AES-GCM, key from `NAGENT_VAULT_KEY` env or KMS. `SecretBox<String>` wrapper prevents accidental logging. Tokens are loaded into `AgentContext.oauth_tokens` lazily per source.

These workstreams are sequenced PR2 → PR3 (PR1 already shipped). Each is a separate plan/PR. Nothing below is safe to start before PR3 lands.

## Feature workstreams (each cut independently after PR1+PR2+PR3)

### F1. Memory & identity agents

**Goal:** persistent per-user memory the LLM can read/write, plus a few
zero-cost identity/audit helpers that ship with PR2.
**Agents:** `remember_fact`, `recall`, `forget_fact`, `whoami`, `audit_log_self`.

**Memory agents (`remember_fact`, `recall`, `forget_fact`):** the canonical
design — `memories` table schema, the `memory-agent` cargo feature, the
`AgentRegistry::from_config` wiring, the system-prompt injection, the
opt-in observer/classifier worker, and the test matrix — lives in
`.kilo/plans/1790623943495-llm-learning-memory.md`. This section only pins
the **RBAC contract** that plan must honour:
- `remember_fact` requires scope `"agent:memory:write"`.
- `recall` requires scope `"agent:memory:read"`.
- `forget_fact` requires scope `"agent:memory:write"`.
- Every memory query filters on `user_id = ?` — no cross-user reads.
- Embeddings remain out of scope (deferred to a follow-up; first cut uses
  literal `LIKE` retrieval).

**Other F1 agents (not memory):**
- `whoami` is zero-cost (reads from `AgentContext`), no DB hit.
- `audit_log_self` queries the audit log filtered by `user_id`, paginated.

### F2. Personal productivity agents

**Goal:** todos, notes, reminders, journal, habits, flashcards — all per-user.
**Agents:** `todos_add/list/complete`, `notes_create/search/list`, `reminders_set/list/cancel`, `journal_append/search`, `habit_log/stats`, `flashcards_add/review`.
**Storage:** SQLite per-user via row-level isolation in a shared DB. Tables: `todos`, `notes`, `reminders`, `journal_entries`, `habits`, `flashcards`.
**Tasks:**
1. `todo-agent` / `notes-agent` / `reminders-agent` / `journal-agent` / `habit-agent` / `flashcard-agent` cargo features, each with its own module.
2. `reminders_set` requires a small in-process scheduler that survives the request — backed by `tokio::time::Instant` + a `DashMap<(user_id, reminder_id), oneshot::Sender>`. For persistence across restarts, write a `scheduled_at` row and replay on boot.
3. `flashcards` picks an SRS algorithm; recommended **FSRS** (open spec, smaller review load than SM-2). Review state per card: `due_at`, `stability`, `difficulty`, `reps`.
4. Tests: cross-user isolation (Alice can't read Bob's todos), idempotent `todos_complete`, reminder fires within 1 s of `due_at`.

### F3. Calendar, email, contacts (external sources)

**Goal:** the LLM can summarise the day's agenda, search mail, find a contact.
**Sources:** Google Workspace (OAuth2), Nextcloud (CalDAV/CardDAV/WebDAV, 🇪🇺 self-hostable), Microsoft 365.
**Agents:** `calendar_today/week/add`, `email_search/summary/send`, `contacts_search`.
**Tasks:**
1. Implement the `Source` trait for Google (`googleapis.com/calendar/v3`, `gmail/v1`, `people/v1`). OAuth2 with PKCE + refresh token stored in vault.
2. Same for Nextcloud (CalDAV/CardDAV via `caldav` crate; WebDAV for file ops). 🇪🇺 friendly alternative to Google.
3. Wrap each source method as a narrow `Agent` so the LLM sees clean tool signatures.
4. `email_send` requires an extra confirmation scope and writes to the audit log with the message hash.
5. Tests: mocked OAuth server via `wiremock`, end-to-end happy path with a recorded fixture.

### F4. Knowledge & docs (external sources)

**Goal:** search/read user-owned documents.
**Sources:** Notion, GitHub, GitLab, Obsidian vault via WebDAV, Bookmarks (Pocket / Linkwarden).
**Agents:** per-source `*_search`, `*_read`, plus a generic `drive_search/read` for any WebDAV-backed source.
**Tasks:**
1. `notion-source` and `notion-agents` (`notion_search`, `notion_read`).
2. `github-source` and `github-agents` (`github_list_issues`, `github_search_code`, …) using OAuth App or PAT in vault.
3. `obsidian-agent` talks WebDAV directly to the user's vault (no third-party).
4. `drive_search` is generic over any WebDAV source — reuse for Nextcloud too.

### F5. Domestic & local 🇪🇺

**Goal:** practical EU-first daily utilities.
**Sources:** Home Assistant (WebSocket + long-lived token), OpenStreetMap Nominatim, Navitia.io, Open-Meteo, Open Food Facts, OpenAQ.
**Agents:** `ha_call_service`, `osm_geocode`, `navitia_journey`, `open_meteo_forecast`, `open_food_facts_lookup`, `openaq_air_quality`.
**Tasks:**
1. Add `domestic-agent` cargo feature; sub-feature per source if each needs its own cred.
2. `ha_call_service` requires a confirmation scope `"agent:ha:write"` and confirms the action in the chat reply ("j'ai allumé la lumière du salon, OK ?").
3. Cache Nominatim responses (rate-limited, ~1 req/s); cache key = normalised query string.
4. Tests: live integration tests against public Nominatim / Open-Meteo, marked `#[ignore]` by default (env-gated) so CI doesn't hit the network.

### F6. Finance

**Goal:** expense logging and PSD2 bank sync (EU).
**Sources:** Salt Edge / Bridge API (PSD2 aggregator, 🇪🇺), exchange-rate.host (keyless, 🇪🇺), Bitcoin/Ethereum via public RPC.
**Agents:** `expense_log/summary`, `bank_sync_account/transactions`, `crypto_portfolio`.
**Tasks:** split into `finance-local-agent` (expense_log) and `finance-external-agent` (PSD2, exchange rates). PSD2 requires user consent flow — implement the consent redirect handling against Salt Edge or Bridge.

### F7. Developer / power-user

**Goal:** scripted local actions, opt-in only.
**Agents:** `shell_run`, `read_file`, `grep`, `git_status`, `git_diff`.
**Gating:** requires role `developer`, an explicit env flag `AGENTS_SYSTEM_ENABLED=true`, and a per-user `WorkspaceConfig { allowed_paths, allowed_commands }`.
**Tasks:** separate `system-agent` feature flag. Every invocation must show the command in the chat reply and require user confirmation before executing (the LLM cannot auto-confirm).

## EU-first source catalog (quick reference)

| Source | Host / vendor | Country | Auth | Free tier |
|---|---|---|---|---|
| Nextcloud | self-host / Nextcloud GmbH | 🇩🇪 | CalDAV/CardDAV/WebDAV | self-host free |
| OpenStreetMap Nominatim | OSM Foundation | 🇬🇧 | none | yes, rate-limited ~1 req/s |
| Open-Meteo | open-meteo.com | 🇩🇪 | none | yes |
| Open Food Facts | openfoodfacts.org | 🇫🇷 | none | yes |
| Navitia.io | Kisio Digital | 🇫🇷 | API key (free tier 2000 req/day) | yes |
| OpenAQ | openaq.org | 🇳🇱 | API key (free tier 5000 req/mo) | yes |
| OpenRouteService | HeiGIT / Uni Heidelberg | 🇩🇪 | API key (free tier 2000 req/day) | yes |
| Salt Edge / Bridge | Salt Edge / Bridge Financial | 🇪🇺 | PSD2 OAuth | pay-as-you-go |
| exchange-rate.host | (open-source, EU mirrors) | 🇪🇺 | none | yes |

## Out of scope (deferred)

- Voice biometrics / speaker identification (multi-user voice is a much harder problem than text).
- Federated instances / cross-tenant search.
- On-device LLM (currently the LLM is upstream; this plan does not change that).
- Local embedding model for semantic memory search (planned in F1 but deferred — first cut uses literal `LIKE` matching).

## Open questions for the user to arbitrate

Marked **[ARB]** where the plan cannot make the call without user input.

- **[ARB-A]** *Resolved by PR1.* The landed auth subsystem ships with three
  configurable backends (`passkey`, `password`, `oidc`) gated by the
  `[auth].providers` config; the operator chooses per-deployment. No
  further decision needed here.
- **[ARB-B]** Storage: per-user SQLite files (`data/users/<id>.sqlite`) vs single Postgres with row-level security. Per-user SQLite is simpler and matches the current embedded ethos; Postgres scales better and supports FTS / pgvector out of the box.
- **[ARB-C]** MCP strategy: host our own MCP server inline (clients connect to `nagent` as if it were an MCP server) vs consume remote MCP servers (each integration brings its own). The plan assumes both are possible via PR3 but recommends starting with the latter.
- **[ARB-D]** Ranking of workstreams F1–F7. Recommended order: F1 → F3 (Google + Nextcloud) → F5 → F2 → F4 → F6 → F7, but the user may prefer finance or dev first.
- **[ARB-E]** Audit log retention: 30 / 90 / 365 days? EU GDPR recommends "no longer than necessary".
- **[ARB-F]** Should `whoami` data include PII (email) or stay minimal (display name + ID)? Affects `GET /api/me` shape.

## Validation

- **Unit tests** per agent and per source, mirroring the existing pattern in `crates/stt-server/src/agents/datetime_agent.rs::tests`.
- **Integration tests** with `wiremock` for OAuth handshake + upstream API.
- **Cross-user isolation tests** in every workstream that touches per-user storage: a test that creates two users, asserts Alice cannot read or write Bob's rows.
- **Security pass** before each PR: review token vault code, audit log writes, scope enforcement.
- **`cargo clippy --workspace --all-features --all-targets -- -D warnings`** and `cargo test --all-features` must pass before each commit, per AGENTS.md §3 and §5.

## Affected files (top-level sketch)

- `crates/stt-server/src/auth.rs` — **already in place** as `auth/mod.rs`
  + `auth/{boot,cli,db_postgres,db_sqlite,error,middleware,mod,oidc,passkey,password,rate_limit,router,routes,session,store}.rs`.
- `crates/stt-server/src/middleware.rs` — `RequireAuth` layer **already in
  place** via `auth::middleware::require_auth_middleware`.
- `crates/stt-server/src/agents/mod.rs` — `Agent` trait gets `invoke_with_ctx`.
- `crates/stt-server/src/sources/mod.rs` (new) — `Source` trait + registry.
- `crates/stt-server/src/vault.rs` (new) — AES-GCM token storage.
- `crates/stt-server/src/audit.rs` (new) — append-only audit sink.
- `crates/stt-server/src/db.rs` (new) — SQLite pool + migrations (sqlx or rusqlite).
- `crates/stt-server/Cargo.toml` — new cargo features per workstream.
- `examples/multi-user.toml` (new) — sample config with auth + DB + vault + scopes.
- `README.md` — multi-user section, EU-first source catalog.
