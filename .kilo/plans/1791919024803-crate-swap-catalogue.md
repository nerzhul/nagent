# Crate-swap catalogue for `nagent` workspace

## Goal

Audit the workspace for hand-rolled code that overlaps with a popular,
well-maintained public crate already (or easily available) on crates.io.
Rank candidates by LOC reduction and migration risk so future plans can
pick an item, design the swap, and submit it for implementation.

This plan is **catalogue-only**: no source changes, no migrations,
no commits. Implementation plans are produced separately per item.

Repo license is `LicenseRef-OpenCore-Source-Available-1.0`. Copyleft
crates (GPL / AGPL) are NOT considered regardless of LOC savings.

## Out of scope (verified already-deliberate)

These were considered but the existing code is justified in source
comments or is a thin layer over a maintained crate. Re-confirming
in any later swap will need a code-comment update, but the deletion
itself is the wrong move:

| File | Reason kept |
|---|---|
| `crates/nagent-server/src/auth/password.rs` | Delegates to `argon2 0.5` (`PasswordHasher` / `PasswordVerifier`). |
| `crates/nagent-server/src/auth/passkey.rs` | Uses `webauthn-rs 0.6.1-dev` ceremony API correctly; bulk is axum route plumbing. |
| `crates/nagent-server/src/credentials/{crypto,key}.rs` | Thin wrapper around `aes-gcm 0.10` + `secrecy 0.10` + `zeroize 1`; `Debug` redaction is deliberate. |
| `crates/nagent-support/src/cpu.rs` | `BoundedConfig` + `run_bounded` is a thin layer over `tokio::sync::Semaphore` + `spawn_blocking`; no crate wraps that exact shape. |
| `crates/nagent-agents/src/agents/email/parse.rs` | IMAP SEARCH criteria validator, not RFC 5322 parsing (`mailparse` handles that). |
| `crates/nagent-agents/src/egress.rs` (EgressClient) | Per-agent policy layer on top of `reqwest`, not a reimplementation. |
| `crates/nagent-server/src/main.rs` tracing init | Properly delegates to `tracing_subscriber::EnvFilter` from `RUST_LOG`. |
| `crates/nagent-server/src/llm/sse.rs` builders (lines ~300-393) | 1-line SSE event builders — trivially small. |

## Candidates (ranked)

Format per item: **Where / What / Crate / Why / LOC saved / Risk /
Migration sketch**.

### 1. CLI argv parsing — `clap` derive ≈ 600 LOC
- **Where:** `crates/nagent-server/src/cli/auth.rs:73-91, 175-186, 255-285, 377-397, 426-447`,
  `cli/migrate.rs:77-96, 114-148, 186-199, 251-284, 409-423`,
  `cli/documents.rs` (`split_global_flags`, `parse_common`,
  `parse_create_admin`, `parse_list_users`, `parse_delete_user`,
  `parse_up`, `parse_down`, `parse_status`, `pid_file_path`,
  `check_running_server`).
- **What:** Hand-rolled `--flag value` / `--flag=value` walking;
  `split_global_flags` duplicated across two namespaces; the full
  `stt-server auth …` / `migrate …` / `documents …` argv grammar.
  Existing in-source comment: *"Hand-rolled flag parsing … no `clap` dep"*.
- **Crate:** `clap = "4"` (derive `Args` + `Parser` + `Subcommand`).
- **Why:** Each subcommand repeats the same shape
  (`--email`, `--password`, `--from-stdin`, `--force`, `--help`).
  Clap derive reproduces each subcommand's surface in one struct plus
  gives `--help`, `--version`, and shell completion for free.
- **LOC saved:** ~500–650.
- **Risk:** None on the wire. The PID-file guard + `kill -0 <pid>`
  carve-out is independent of argv parsing and stays where it is.
- **Migration sketch:** Replace `parse_*` functions with `#[derive(Parser)]`
  structs. Keep the global `--config` / `-v` / `--force` parsing as
  the top-level `Cli` struct's global args. The
  `--from-stdin` toggle becomes a `value_parser` that reads from
  stdin when the flag is present and `arg_required_else_help` is off.
  Snapshot tests for `--help` should be added (per AGENTS §5).

### 2. OAuth2 X / Twitter wiring — `oauth2` (already in lockfile) ≈ 500 LOC
- **Where:** `crates/nagent-server/src/oauth/x.rs:1-722`,
  `crates/nagent-agents/src/agents/x_timeline/mod.rs:1-861`,
  plus `crates/nagent-server/src/oauth/{pkce,refresh,state}.rs`.
- **What:** Authorization URL building, PKCE verifier wiring,
  token exchange, refresh round-trip — all hand-implemented over
  `reqwest + serde_json`.
- **Crate:** `oauth2 = "5.0"` (already in `Cargo.lock`, version
  `5.0.0`).
- **Why:** `oauth2::AuthorizationCode::new(code).set_pkce_challenge(pkce)`
  + `oauth2::basic::BasicClient::exchange_code()` collapse the wire
  layer.
- **LOC saved:** ~400–550.
- **Risk:** Keep `oauth2::RedirectUrl` / `ClientSecret` mapping into
  the existing encrypted-secret types; nothing user-facing changes.
- **Migration sketch:** Replace direct `reqwest::Client::post` calls
  in `x.rs` with `oauth2::Client::exchange_code` /
  `exchange_refresh_token`. Keep `TokenSet` /
  `RefreshTokenClient` abstraction in `refresh.rs` as-is.

### 3. Rate limiting — `governor` / `tower-governor` ≈ 1 400 LOC
- **Where:**
  `crates/nagent-support/src/ratelimit.rs:170-355`
  (`TokenBucket`, `TokenBucketMap`),
  `crates/nagent-support/src/ratelimit.rs:396-624`
  (`WindowedBucket`, `WindowedBucketMap`),
  `crates/nagent-server/src/rate_limit.rs:93-213`
  (`RateLimiter`),
  `crates/nagent-server/src/auth/login_rate_limit.rs:148-336`
  (`LoginRateLimiter`).
- **What:** Two primitives (`TokenBucketMap`, `WindowedBucketMap`) +
  a per-IP `RateLimiter` + a three-family login limiter layered on
  top — all built on `DashMap` + `AtomicU64`-gated sweeps.
- **Crate:** `governor = "0.6"` for the bucket primitives;
  `tower-governor = "0.4"` for the axum middleware.
- **Why:** `governor::Quota::per_minute(n).allow_burst(n)` matches
  the "capacity == rate" knob exactly; idle eviction is a built-in
  side-effect.
- **LOC saved:** ~1 400 (the three files combined).
- **Risk:** Current code deliberately has a "loopback bypass" carve-out
  and `LoginRateLimitDecision` carries a tagged `BucketKind` for
  diagnostics — both survive by wrapping `governor` rather than
  replacing it. Tests that seed bucket state by hand need rewriting.
- **Migration sketch:** Build thin wrappers around `governor::RateLimiter`
  that translate `BucketKind` decisions → `LoginRateLimitDecision`.
  Use `tower-governor::GovernorLayer` for the per-IP layer in
  `rate_limit.rs`. Disable `governor`'s background task in favour of
  the existing `sweep_clock` tick so the eviction cadence stays
  deterministic.

### 4. TTL map / in-memory cache — `moka` ≈ 350 LOC
- **Where:** `crates/nagent-support/src/ttl_map.rs:30-258`
  (`TtlMap`, `InsertOutcome`, opportunistic-on-insert sweep).
- **What:** Hand-rolled `DashMap<K, Entry<V>>` with `expires_at`,
  opportunistic sweep on `insert`, periodic `sweep_expired` hook,
  LRU eviction on cap.
- **Crate:** `moka = "0.12"` (or `mini-moka` for `no_std`).
- **Why:** `moka::future::Cache::builder().time_to_live(ttl).max_capacity(cap).build()`
  gives the exact primitive.
- **LOC saved:** ~350.
- **Risk:** `InsertOutcome { Inserted, EvictedExpired, EvictedOldest }`
  drives per-subsystem observability; keep that as a wrapper around
  `Cache::entry()` if needed.
- **Migration sketch:** Replace the struct with a thin newtype over
  `moka::future::Cache<K, V>`. Translate `insert` outcomes through
  the wrapper. Keep the `sweep_expired` hook but make it a no-op
  (`moka` sweeps in the background).

### 5. iCalendar parser (RFC 5545) — `icalendar` *if accessor available* ≈ 400 LOC
- **Where:** `crates/nagent-agents/src/agents/caldav/mod.rs:854-1230`
  (`unfold`, `parse_vevent`, `split_blocks`, `split_property`,
  `parse_dt`, `parse_duration`, `ical_escape`, `ical_unescape`) +
  duplicate extract path at `extract_events_from_multistatus:1249-1310`.
- **What:** VCALENDAR/VEVENT unfold + parse + escape. The author's
  doc on lines 36–41 justifies rolling this: *"the `icalendar` 0.7
  crate hides the property values behind a private field, so the
  only reliable path is to parse the text directly"*. Pinned at
  `0.7.1` in `Cargo.lock`.
- **Crate:** `icalendar = "0.7.x"` (already a dep).
  *Re-evaluate before adding*: the rejection cites 0.7's
  encapsulation; check whether 0.7.2+ or 0.8 lifts the private
  field. If not, this candidate is blocked.
- **Why:** Once the property accessor is reachable, the entire
  unfold/path/parse layer shrinks to
  `for event in Calendar::new_from(&body).events` walks.
- **LOC saved:** ~370–450.
- **Risk:** The 0.7 → 0.8 hop is the blocker; if `icalendar 0.8` is
  unavailable, the existing comment is a legitimate "deliberately
  rolled" justification and this item drops off the list.
- **Migration sketch:** Verify `Event::properties().get(&…)` is
  reachable in 0.7.x; if yes, swap the parser loop for `icalendar`
  iterators. If no, close out as "deliberate".

### 6. PROPFIND / multistatus XML parser — `quick-xml` ≈ 400 LOC
- **Where:** `crates/nagent-agents/src/agents/caldav/mod.rs:445-841`
  (`extract_propfind_responses`, `extract_calendar_home_hrefs`,
  `is_user_calendar`, `find_open_tag`, `find_close_tag`,
  `extract_inner`, `next_open_tag_at`, `resolve_href`).
- **What:** Substring-scanning PROPFIND XML parser — no real
  namespace handling, dozens of `to_ascii_lowercase()` + manual
  offset walks.
- **Crate:** `quick-xml = "0.36"` (with `namespaces` feature).
- **Why:** CalDAV responses carry standard element names
  (`multistatus`, `response`, `propstat`, `prop`, `calendar-data`,
  `resourcetype`); a streaming reader collapses the offset arithmetic.
- **LOC saved:** ~350–450.
- **Risk:** CalDAV XML uses 1.0 default namespaces (`DAV:`,
  `urn:ietf:params:xml:ns:caldav`); `quick-xml`'s namespace resolver
  handles them natively. No wire changes.

### 7. BM25 router — `bm25` ≈ 500 LOC
- **Where:** `crates/nagent-agents/src/tools_router.rs:1-725`
  (`ToolsRouter`, `IndexedTool`, `ToolSearchHit`, `tokenise_*`,
  `count_term`, BM25 scoring math).
- **What:** From-scratch BM25 scorer (TF-IDF with K1/B parameters,
  document frequency map, average document length) for routing LLM
  prompts to ~12 candidate tools. The module's docstring (line 1–15)
  literally calls out *"purely local and dependency-free: a tiny
  hand-rolled BM25"*.
- **Crate:** `bm25 = "0.1"`.
- **Why:** Same scoring in ~100 LOC instead of ~700.
- **LOC saved:** ~500.
- **Risk:** Custom routing surface (`search_tools` exclusion,
  `description + keywords_lower` tokenisation, top-K tie-break) must
  be preserved; the author explicitly says this is a deliberate
  trade-off.
- **Migration sketch:** Likely **not worth swapping** until the
  registry grows past the current ~12 tools. Defer.

### 8. SSRF IP classifier — `ipnet` allowlist (dep already) ≈ 90 LOC
- **Where:** `crates/nagent-agents/src/egress.rs:506-606`
  (`is_addr_allowed`, `is_v4_allowed`, `is_v6_allowed`,
  `host_matches_allowlist`, plus the `Ipv4AddrLinkbackExt` dead shim
  at 553-559).
- **What:** Manual IPv4/IPv6 policy with octet-level checks for
  RFC1918, 100.64.0.0/10 CGNAT, ULA `fc00::/7`, IPv4-mapped IPv6, etc.
- **Crate:** `ipnet = "2"` (already a dep, used for
  `TrustedProxiesConfig`).
- **Why:** Build a
  `const BLOCKLIST: &[IpNet] = &[ ... ]` and reduce
  `is_addr_allowed` to `!BLOCKLIST.iter().any(|n| n.contains(&addr))`.
  `ipnet` handles IPv4-mapped IPv6 internally.
- **LOC saved:** ~80–100.
- **Risk:** None — current logic is correct; swap is one-to-one plus
  a never-variant for IPv6 unspecified / multicast.

### 9. Config env > TOML > default precedence — `figment` ≈ 400 LOC
- **Where:**
  `crates/nagent-server/src/config/mod.rs:66-152`
  (`env_opt`, `resolve_primitive`, `resolve_opt_string`,
  `resolve_opt_primitive`, `resolve_csv`),
  `crates/nagent-server/src/config/server.rs`,
  plus the same precedence chain repeated in ~10 section files
  (`config/llm.rs`, `config/tts.rs`, `config/auth.rs`, …).
- **What:** Hand-rolled *"env > TOML > default"* helpers + duplicate
  logic in every config section's `with_env_and_toml` constructor.
- **Crate:** `figment = "0.10"` (or `config = "0.14"`).
- **Why:**
  `Figment::new().merge(Toml::file(path)).merge(Env::prefixed("NAGENT_"))`
  reproduces the precedence, layer order, and XDG search path in a few
  lines; the empty-env-as-unset rule and CSV splitters are built-ins.
- **LOC saved:** ~300–500 across section files.
- **Risk:** Custom env-var names that don't fit the
  `NAGENT_<SECTION>_<KEY>` shape need a small mapping table.
  `.env` semantics (handled by `dotenvy`) are unaffected.

### 10. SSE byte-stream parser — `eventsource-stream` / `sse-stream` ≈ 70 LOC
- **Where:** `crates/nagent-server/src/llm/sse.rs:214-298`
  (`SseStream`, `parse_sse_frame`).
- **What:** Hand-written SSE frame parser that accumulates `event:`
  / `data:` lines on a byte stream with an idle timeout.
- **Crate:** `sse-stream` or `eventsource-stream`.
- **Why:** Both crates yield typed events
  (`Sse::Event { event, data }`) directly from an
  `impl Stream<Item = Result<Bytes, _>>`, the same input shape
  `SseStream` accepts.
- **LOC saved:** ~60–80.
- **Risk:** Custom `[DONE]` sentinel handling + comment-line
  skipping + the deliberate "eat `[DONE]` so the tool loop can append
  its own" rule need to be layered on top.
- **Migration sketch:** Borderline; only worth doing if other SSE
  surfaces (real upstream, not just OpenAI) appear.

### 11. Cookie Set-Cookie builder / parser — `cookie` (dep already) ≈ 40 LOC
- **Where:** `crates/nagent-server/src/auth/session.rs:73-114, 123-152`
  (`extract_session_token`, `build_set_cookie`, `build_clear_cookie`).
- **What:** Manual `Cookie:` header split on `'; '` and a hand-built
  `Set-Cookie: name=val; Path=/; HttpOnly; SameSite=Lax; Max-Age=…; Secure`
  string.
- **Crate:** `cookie = "0.18.2"` (already in the dep tree).
- **Why:**
  `cookie::Cookie::build(name, value).path("/").http_only(true).same_site(SameSite::Lax).secure(true).max_age(Duration::seconds(n)).finish()`
  replaces both halves.
- **LOC saved:** ~30–50.
- **Risk:** The author explicitly rejected this in source on
  lines 124–127: *"the crate's `Cookie` type does not have a stable
  `Display` impl that matches what we want here, and (b) the surface
  area is tiny"*. Justification is up-to-date for 0.18; the read
  path (`split(';')`) can still be replaced in isolation if
  behaviour parity is verified.

## Total swing

≈ **4 700 LOC** of hand-rolled code deletable on top of crates that
already exist (and several are already in the dep tree). Largest
deltas: rate limiting (~1 400), CLI parsing (~600), BM25 router
(~500), OAuth2 X (~500).

## Recommended implementation order

Based on LOC savings × migration risk:

1. **`clap` derive** — biggest safe win, no behaviour change.
   Plan to follow.
2. **`moka`** — drops the `sweep_clock` dependency from `ttl_map`;
   low risk; existing tests use mostly `insert` / `get`.
3. **`ipnet` allowlist** — already a dep; pure simplification.
4. **`oauth2` X dance** — `oauth2` is already in `Cargo.lock`; the
   swap is mechanical.
5. **`governor`** — biggest win but requires keeping the loopback
   carve-out and BucketKind diagnostics; plan must include an
   adapter shim.
6. **`quick-xml` + maybe `icalendar`** — both CalDAV-shaped; bundle
   into one plan to avoid two migrations of the same module.
7. **`figment` config layer** — touches every `with_env_and_toml`
   constructor; needs a single PR with N agent config files
   updated together.
8. **`sse-stream`** — only if another SSE surface appears; defer.
9. **`bm25`** — defer until tool registry grows.
10. **`cookie`** — defer; existing rejection comment is up-to-date.

## Open questions

- **#1 `icalendar`** — does 0.7.2+ or 0.8 expose the property
  accessor? Needs a 30-min check before deciding on/off the list.
- **#9 `figment` vs `config`** — workspace preference between
  the two? Both fit; `figment` is lighter, `config` has wider
  recognition. No decision required for the catalogue, but the
  implementation plan will pick one.
- **#3 `governor`** — is the loopback bypass a per-bucket attribute
  or per-IP? The implementation plan needs to confirm where
  `RateLimiter` reads the bypass decision from.

## Validation (catalogue)

This plan introduces no code. No `cargo build`, `cargo test`,
`cargo clippy`, or `cargo fmt` run is required.

When items are picked up by follow-up plans, each plan must
include, per AGENTS §5:

- A `--help` snapshot test for any CLI-surface change.
- A `figment`-render-equivalent test for any config change.
- A round-trip test for any cache/rate-limit replacement
  (existing tests seed via inserts; the replacement must
  preserve observable behaviour with the same test inputs).
- All workspace docs (READMEs, AGENTS, config TOML files in
  `examples/`, doc-comments) updated in the same commit.
