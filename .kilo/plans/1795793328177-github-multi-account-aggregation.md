# GitHub multi-account (multiple PATs) + cross-account aggregation

Goal: let one nagent user connect **several GitHub tokens** (different
work scopes — e.g. personal account, client A org, client B org) and
**aggregate data across them**, instead of the single
`github.personal_access_token` we have today.

## 0. Current state (what exists today)

- One PAT per user, stored as the vault field
  `github.personal_access_token`
  (`crates/nagent-agents/src/agents/github/client.rs:72`,
  `GithubClient::token`), resolved through
  `UserContext::secret("github", "personal_access_token")`
  (`crates/nagent-agents/src/agents.rs:912`).
- The `github` `ServiceDef` declares four fields
  (`crates/nagent-agents/src/github_service.rs`):
  `personal_access_token` (Password, required), `username` (Text),
  `show_dashboard` (Checkbox), `dashboard_repos` (Text, JSON array).
- Ten read-only `github_*` chat tools
  (`crates/nagent-agents/src/agents/github/mod.rs`): `me`,
  `list_repos`, `get_repo`, `list_issues`, `get_issue`, `list_prs`,
  `get_pr`, `list_notifications`, `search_issues`, `read_file`. All
  funnel through `run()` → `client.get(ctx, …)`, which resolves the
  single token. Cache is keyed `(user_id, tool, args)`.
- The wire client (`crates/nagent-github/src/client.rs`) already takes
  the token **as a parameter** (`GithubClient::get(&token, path, query)`),
  so multi-token fan-out needs no wire-layer change — only the agent
  wrapper resolves the token today.
- Pure-API PR dashboard (`crates/nagent-server/src/credentials/github_api.rs`):
  `GET /api/integrations/github/prs[...]`, `/owners`, `/owners/:owner/repos`,
  reading `github.personal_access_token` + `github.dashboard_repos`.
- Setup probes: `POST /api/integrations/github/probe` (validates the
  PAT via `GET /user`, stores `username`) and
  `POST /api/integrations/github/probe-repo` (verifies repo access
  before adding to `dashboard_repos`)
  (`crates/nagent-server/src/credentials/github_probe.rs`,
  `github_probe_repo.rs`).
- UI: `crates/nagent-server/src/static/integrations.js`
  (`probeGithub`, `PROBE_PATH_GITHUB`) + the repo picker
  (`github-repo-picker.js`) that manages `dashboard_repos` through
  dedicated endpoints while keeping a hidden input for the generic
  form submit — **this is the pattern to copy for accounts**.
- Docs: `docs/integrations/github.md`, `docs/architecture.md` §2.6
  (service catalogue) + the tool table (~line 234), README.

## 1. Design decisions

### 1.1 Vault model — named accounts in one JSON field

- New vault field **`github.accounts`**: a JSON array (cap
  `max_accounts`, default 8) of
  `{"name": "client-a", "token": "ghp_…", "username": "octocat", "scopes": "repo, read:user"}`.
  `name` charset: `[a-z0-9][a-z0-9-_]{0,31}`. Stored AES-256-GCM like
  every vault row.
- **`github.personal_access_token` stays as the default account**
  (name `"default"`). No DB migration: the vault is key-value, and
  read paths fall back to it when `accounts` is absent/empty. The
  field becomes `required: false` in the `ServiceDef` once at least
  one account exists — "configured" logic must treat
  `personal_access_token` **or** non-empty `accounts` as connected
  (adjust the `ServiceDef` summary/`required` handling in
  `services.rs` accordingly, or keep `personal_access_token`
  required and let `accounts` hold only the *extra* tokens — see
  §7 migration note; pick one before P0 and document it).
- New vault field **`github.default_account`** (Text, optional,
  `echo_on_edit = true`): name of the account used when a tool call
  has no explicit `account` arg. Defaults to `"default"`.
- `accounts` is managed **only** through dedicated endpoints
  (§4), never through the generic `PUT
  /api/integrations/:id/credentials` round-trip: tokens inside the
  JSON must never be echoed back, so the field gets
  `echo_on_edit = false` and the JSON layer must mask it like a
  Password field (extend the `FieldKind::Password` masking rule in
  `services.rs::to_summary` to also mask the `accounts` key — or give
  it its own kind/flag; decide in P0).

### 1.2 Account resolution — one choke point

- `GithubClient::token(ctx)` becomes
  `GithubClient::resolve_account(ctx, account_arg: Option<&str>) -> Result<Account, AgentError>`
  returning `{ name, token: SecretString }`:
  1. explicit `account` arg (must match a name in `accounts` or
     `"default"` → `personal_access_token`);
  2. `github.default_account`;
  3. `"default"` (`personal_access_token`).
  Missing everywhere → the existing `CredentialsMissing` error,
  extended with the account name so the UI can prompt precisely.
- Parsing/validation of the `accounts` JSON lives in one pure helper
  (`parse_accounts(&str) -> Result<Vec<AccountDef>, AgentError>` in
  `nagent-agents`, unit-tested) so the agent wrapper, the probe and
  the dashboard endpoints share it. Parse errors must **not** include
  the raw blob (AGENTS.md §6 — the blob contains tokens).

### 1.3 Cache and rate-limit keys must include the account

- `run()` in `agents/github/mod.rs` keys the cache
  `(user_id, tool, args)`; change to `(user_id, account_name, tool,
  args)` (fold the resolved account into the key — the `Account`
  struct carries its name).
- Per-account rate-limit awareness: `nagent_github::GithubClient::get`
  already detects `X-RateLimit-Remaining: 0`; extend the wrapper to
  record `(account, remaining, reset)` from response headers in a
  small in-memory per-user map so fan-out can skip exhausted accounts
  and surface `RateLimited { account }`. P2 refinement; P0 only
  needs the account name on the error.

## 2. Chat tools — `account` argument + aggregation

All ten tools gain an optional **`account`** string arg (name, or
`"all"` where aggregation makes sense — §2.2). The confirmation card
names the resolved account(s), never the token.

### 2.1 Account-scoped tools (per-token data)

`github_me`, `github_list_repos`, `github_list_notifications`,
`github_search_issues`: default = resolved default account, unchanged
behaviour when `account` is absent.

### 2.2 `account: "all"` — fan-out aggregation

For the four account-scoped tools, `account: "all"` fans out
concurrently (bounded by `aggregate_concurrency`, default 4) across
every configured account and merges:

- `me` → `{ "accounts": [{ name, user, plan, scopes }] }`;
- `list_repos` / `list_notifications` / `search_issues` → merged list,
  each item tagged with `"account": "<name>"`, sorted by
  `updated_at`/`pushed_at` desc, **deduplicated by upstream `id`**
  (the same repo/notification visible through two tokens appears
  once — keep the first occurrence in account order, or the one from
  the default account);
- per-account failures (401/403/rate-limit on one token) land in an
  `account_errors: { "<name>": "<upstream status/body>" }` object
  instead of failing the whole call; the call only fails if **all**
  accounts fail.
- Response size still clamped by the existing `max_items` /
  `max_body_bytes` machinery — aggregate first, then clamp, so the
  cap applies to the merged list.

### 2.3 Repo-scoped tools (need the token that can read the repo)

`github_get_repo`, `github_list_issues`, `github_get_issue`,
`github_list_prs`, `github_get_pr`, `github_read_file`: add optional
`account`. When absent, resolve repo → account:

1. a per-user in-memory map `(repo) -> account name`, populated by
   (a) the dashboard `probe-repo` endpoint when a repo is added
   (§4 — store the account that could read it), and (b) successful
   repo-scoped calls;
2. otherwise try accounts in order (default first), first non-404
   wins, and cache the result — 404s count against the rate limit,
   so negative results are cached too (in-memory only, TTL =
   `cache_ttl_secs`);
3. if every account 404s, return a single 404-style
   `InvalidArguments`/`Upstream` error listing the accounts tried
   (names only, never tokens).

### 2.4 New tool: `github_list_accounts`

Read-only, confirm-on-read like the others. Returns
`[{ name, username, scopes, is_default }]` from the vault + probe
metadata — **never the tokens**. This is how the LLM discovers which
accounts exist before calling with `account: "all"` or a name.

## 3. Server endpoints (nagent-server, `github-agent` feature)

- `GET /api/integrations/github/accounts` — list accounts (name,
  username, scopes; tokens masked/absent).
- `POST /api/integrations/github/accounts` — add/update one account:
  body `{ name, token }`; probe `GET /user` with that token, capture
  `login` + `X-OAuth-Scopes`, upsert into `github.accounts` (cap
  enforced, duplicate name = update). Audit kind `github_account_add_*`
  / `github_account_update_*`; token never in audit rows or error
  bodies (§6).
- `DELETE /api/integrations/github/accounts/:name` — remove (refuse to
  delete `"default"` while `personal_access_token` is the only
  account; refuse if it is the `default_account` — repoint first).
- `POST /api/integrations/github/probe` — unchanged (still validates
  the default PAT); the accounts endpoint reuses its probe logic per
  token.
- `POST /api/integrations/github/probe-repo` — extend to try **all**
  accounts and record which one can read the repo
  (`github.repo_accounts`: JSON map `"owner/repo" -> "<account>"`),
  so the dashboard and repo-scoped tools resolve instantly.
- Dashboard pure-API (`github_api.rs`):
  - `GET /api/integrations/github/prs` aggregates across accounts:
    each `dashboard_repos` entry is resolved to its account (via
    `repo_accounts`, else live resolution), fetched with that token,
    rows tagged `"account": "<name>"`, merged by `updated_at` desc,
    per-repo errors stay in `repo_errors` (now keyed
    `"account/owner/repo"` or carrying an `account` field);
    `?account=<name>` filters to one account; `?account=all` is the
    default.
  - `/owners` and `/owners/:owner/repos` gain `?account=` (default:
    default account; `all` merges org/repo lists across accounts,
    deduped by login/name).
- All new routes behind `RequireAuth`, GET where read-only, CSRF
  header on the POST/DELETE, `auth_events` rows per call as today.

## 4. UI (`integrations.js` + repo picker + dashboard tab)

- GitHub integration form gets an **Accounts section** modelled on the
  existing `dashboard_repos` chip picker: list of accounts (name,
  `@login`, scopes) with add / edit / remove buttons; the token input
  is write-only (`type=password`, never pre-filled, never echoed —
  `echo_on_edit = false`). A hidden input carries the `accounts` JSON
  for the generic submit only if the server accepts it masked; prefer
  saving accounts exclusively through the dedicated endpoints and
  keeping the field out of the generic PUT payload.
- "Default account" selector (Select field fed by
  `GET …/accounts`).
- PRs dashboard tab: account facet/column on rows + a filter
  dropdown (`all` + each account name).

## 5. Configuration + doc sync (AGENTS.md §5)

- `[agents.github]` gains (in `nagent-github` `GithubConfig`, re-exported
  as `GithubAgentConfig`): `max_accounts` (default 8),
  `aggregate_concurrency` (default 4). New example file
  `examples/github.toml`.
- Same-commit doc updates: `docs/architecture.md` (§2.6 service
  catalogue — new fields; tool table — `account` arg +
  `github_list_accounts` row), `docs/integrations/github.md` (new
  "Multiple accounts" section: vault fields, endpoints, aggregation
  semantics, `?account=` query params), `README.md` if it lists
  integration endpoints, `AGENTS.md` §6 note that `github.accounts`
  is a multi-credential field covered by the redaction rule.
- Tests guarding the contract: config-parse test for the new keys,
  `--help`/config-doc snapshot if one exists for `[agents.*]`.

## 6. Security — AGENTS.md §6 applies unchanged and is stricter here

- The `accounts` JSON blob contains several tokens: it is stored
  encrypted, never echoed (`echo_on_edit = false` + JSON-layer masking
  like Password), never logged, never formatted into an error string
  (parse errors report the field/key at fault, not the raw value),
  never shown in the confirmation card or audit rows (audit rows carry
  account **names** only).
- Wire layer already redacts `Authorization` (`redact_authorization`);
  the fan-out code must build per-account errors from
  `GithubError` (status/body only) and never from the request.
- `GET …/accounts` and `github_list_accounts` return names/logins/
  scopes only — `X-OAuth-Scopes` is safe to display, the token is not.
- Test fixtures assert on `<redacted>` markers, per §6.

## 7. Migration / backward compatibility

- No DB migration: vault is `(user, service, field)` key-value.
- Read path: `accounts` absent or empty → behave exactly as today with
  `personal_access_token` as account `"default"`. Existing users see
  no change until they add an account.
- Decide in P0 (and document in `docs/integrations/github.md`): either
  (a) `personal_access_token` stays required and `accounts` holds only
  extra tokens, or (b) it becomes optional once `accounts` is non-empty
  (requires touching the `required`-drives-"configured" logic in
  `services.rs`). Option (b) is cleaner long-term; option (a) is a
  smaller diff. Do not ship both semantics.

## 8. Tests

- Unit (`nagent-agents`): `parse_accounts` (valid/invalid JSON,
  cap, name charset, duplicate names, empty token rejected),
  resolution order (arg > `default_account` > `default`),
  cache key includes account, merge/dedupe logic for `account: "all"`
  (same `id` via two accounts appears once), redaction — error
  strings and summaries contain no token from the `accounts` blob.
- Loopback (existing fixture style in `agents/github/mod.rs` tests):
  fixture server that answers differently per `Authorization` header;
  assert fan-out hits each account with its own token, per-account 401
  lands in `account_errors` without failing the call, merged results
  are tagged + deduped, repo→account resolution caches (one 404 per
  account per repo).
- Server e2e (`nagent-server/tests/github_e2e.rs`,
  `github_dashboard_e2e.rs`): two accounts in the vault, accounts
  CRUD endpoints (add/update/delete, cap, delete-default refusal,
  masking on GET), PR list aggregated across accounts with
  `?account=` filter, probe-repo recording the owning account.
- §6 assertions in every new test: response/error bodies never contain
  any of the fixture tokens.

## 9. Phasing

- **P0 — storage + resolution + per-tool `account` arg**: §1.1–1.3,
  §2.1, §2.3, §2.4, §3 account CRUD + probe-repo map, §4 accounts UI
  section, §5 config keys, §7 decision, §8 tests for all of it, doc
  sync. Validation: existing `github_e2e.rs` / `github_dashboard_e2e.rs`
  pass unchanged (backward compat); new multi-account tests pass;
  `cargo build --all-targets` zero warnings, `cargo clippy`, `cargo test`.
- **P1 — aggregation**: §2.2 `account: "all"` fan-out on the four
  account-scoped tools, dashboard cross-account aggregation
  (`?account=all` default) + account facet in the PRs tab, §4 filter
  UI. Validation: loopback fan-out tests + dashboard e2e with two
  accounts.
- **P2 — polish**: per-account rate-limit tracking and smart account
  picking (§1.3), negative-cache TTL tuning, per-account dashboard
  views, `github_search_issues` cross-account query fan-out with
  merged `total_count`.

## 10. Open questions for the user

1. Account naming: free-form names (`client-a`) vs. derived from the
   GitHub `login` the token authenticates as? (Recommend free-form
   name + stored login, so one login can have two tokens with different
   scopes.)
2. §7 option (a) vs (b) for `personal_access_token` required-ness.
3. Should `dashboard_repos` become per-account, or stay global with
   per-repo account resolution (`repo_accounts`)? (Recommend global +
   resolution — smaller diff, aggregation comes for free.)
