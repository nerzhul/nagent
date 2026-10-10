# nagent — Email agent (IMAP/SMTP)

Concrete implementation plan for **B4 — Email (IMAP/SMTP)**, the
"first concrete follow-up after CalDAV" item from
`.kilo/plans/1791234616248-ux-feature-agent-catalog.md` §B4.

The plan reuses every existing wiring point: per-user credentials
vault (§2.6 of `docs/architecture.md`), the inline approval card
(§3.4.1), the `ServiceDef` form + audit row pattern, and the new
`EgressPool` / `reqwest` posture that keeps CalDAV + X stable.

## 0. Scope and methodology

- **v1 surface (what this plan ships)**:
  - Two new `ServiceDef`s: `email_imap` + `email_smtp`. The plan
    *opts out* of Gmail OAuth in v1 — see §1 below for the
    rationale and the path back.
  - Five new chat agents (LLM tools):
    `email_list_inbox`, `email_get_message`, `email_send`,
    `email_search`, `email_mark_read`.
  - One setup-only HTTP endpoint per service: a "probe"
    endpoint that validates the IMAP credentials and lists
    mailboxes / from-addresses, mirroring
    `POST /api/integrations/caldav/probe-calendars`.
  - Hardening: per-service hostname allow-list, explicit TLS
    posture (STARTTLS required on submission, IMAPS-only by
    default for read), audit rows identical to CalDAV.
- **Out of scope (deferred to v2)**:
  - Gmail OAuth / provider-specific flows.
  - Attachments (`email_get_message` returns metadata only;
    `email_send` does not yet accept `attachments`).
  - Folder management (`email_create_folder`,
    `email_move_message`).
  - IDLE / push notifications (real-time "new mail" pings).
  - S/MIME / PGP signing.
  - Sync state cache across sessions (every list / search
    round-trips the IMAP server).
- **No backend changes outside the email subtree**: no DB
  migration, no LLM-provider plumbing, no auth subsystem
  rewrite. Like CalDAV, this ships behind one new cargo
  feature (`email-agent`) and the existing `[agents]`
  master switch.

### Why two `ServiceDef`s, not one

The catalog B4 lists "single IMAP/SMTP agent pair, or two
separate `ServiceDef`s with provider-specific UIs" as an open
question. The plan **resolves this in favour of two
`ServiceDef`s** (`email_imap`, `email_smtp`):

- The IMAP and SMTP endpoints for the same mailbox very often
  differ (`imap.fastmail.com:993` + `smtp.fastmail.com:465`,
  but `imap.gmail.com` + `smtp.gmail.com`, etc.) and the
  credentials are not always symmetric (submission sometimes
  uses an app-specific password while IMAP uses a real one).
- The existing `FieldDef::echo_on_edit` flag + the form
  rendering code already supports one form per `ServiceDef`
  cleanly; folding both protocols into a single `ServiceDef`
  would force the UI to render two logical sections inside one
  card, which no existing per-feature integration does.
- The Gmail OAuth follow-up (v2) **does** want one
  `ServiceDef` per provider because the OAuth fields
  (`client_id`, `client_secret`, `refresh_token`) dominate
  the config surface — adding a third `gmail` `ServiceDef`
  later does not conflict with the IMAP/SMTP pair.

The chat agents stay protocol-driven (`email_list_inbox`
talks IMAP regardless of whether IMAP was reached via
`email_imap` or a future `gmail` OAuth shim that reuses
the same `EmailImap` capability trait).

## 1. Wire surface

### 1.1 Tools (LLM-callable)

| Tool | Kind | Confirmation | Args |
| --- | --- | --- | --- |
| `email_list_inbox` | read | confirm (inline card) | `folder?` (`"INBOX"` default), `since?` (RFC 3339), `limit?` (default 25, cap 100) |
| `email_get_message` | read | confirm (inline card) | `uid` (IMAP UID), `folder?` |
| `email_search` | read | confirm (inline card) | `query` (IMAP `SEARCH` criteria string), `folder?`, `limit?` |
| `email_send` | write | **always confirm, no session override** | `to` (string), `subject`, `body` (plaintext), `cc?`, `bcc?`, `in_reply_to?`, `references?` |
| `email_mark_read` | write | confirm (inline card) | `uid`, `folder?`, `read` (bool, default `true`) |

Every tool routes through the existing inline approval card
machinery (§3.4.1 of `docs/architecture.md`). Reads hit a
third-party service with the user's credentials, so even
`email_list_inbox` is gated — same posture as
`caldav_list_events`.

`email_send` is the strictest tool in the registry: the
inline approval card fires **on every invocation** and the
"`ApproveAlways` for this session" path is intentionally
**not** wired for it. Sending mail leaves the operator's
perimeter, so the user must click "Autoriser" for each
message — even after they have just confirmed another
message in the same chat session. The other four tools
keep the session-override behaviour (the user clicking
"Always allow `email_list_inbox`" skips the card for the
remainder of the session) because they are read / flag-set
operations and the existing `PermissionStore` already covers
that path. This split matches the CalDAV posture
(`caldav_create_event` is confirm-on-write too, but the
read tools take the session-override path because they only
fetch third-party data — sending is one notch stricter
because it produces a third-party effect).

`email_send` is also double-gated by a server-side check
on the `to` / `cc` / `bcc` recipients (the operator-set
`[email].recipient_allowlist`) — see §4. Without that
allow-list the chat could be tricked into handing a
misconfigured vault row to an arbitrary address.

### 1.2 Setup-only HTTP endpoints

- `POST /api/integrations/email_imap/probe` — body
  `{host, port, username, password, tls: "ssl"|"starttls"|"none"}`,
  response `{mailboxes: [{name, uidnext?, total?}, ...]}` and
  `{capabilities: ["IDLE", "CONDSTORE", ...]}`. Authenticated
  via the existing `RequireAuth` layer; same audit-row shape
  as CalDAV (`email_imap_probe_<outcome>`, host + outcome,
  password never stored).
- `POST /api/integrations/email_smtp/probe` — body
  `{host, port, username, password, tls, from_address}`,
  response `{server_capabilities: [...], smtp_ok: true}`. The
  probe SMTP-`EHLO`s the server with the configured
  `[email].probe_helo` identity and rejects if the `From:`
  address is rejected by the server (mailboxes that require
  address whitelisting often reject on `MAIL FROM`).
- The probes share a generic `crate::probe::ProbeState<…>`
  pattern, already factored by CalDAV (see
  `docs/architecture.md` §2.7).

### 1.3 IMAP configuration surface (per-user form)

The `email_imap` `ServiceDef` exposes only the **minimum the
user types**, matching the CalDAV form (3 fields). The full
configuration surface lives in the operator's `[email]`
TOML section so the operator can dial the per-deployment
security posture without code changes.

User-facing `email_imap` `ServiceDef` fields:

| Field | Type | Required | Default | Notes |
| --- | --- | --- | --- | --- |
| `host` | Text | yes | — | IMAP server hostname. Validated against `[email].allowlist` at save time. |
| `port` | Text (number) | yes | `993` | TCP port. 993 for implicit TLS, 143 for STARTTLS, custom allowed. |
| `tls_mode` | Text | yes | `implicit` | One of `implicit` (TLS on connect), `starttls` (plain → STARTTLS), `none` (refused at save time unless `[email].allow_cleartext_imap = true`). |
| `username` | Text | yes | — | Login name. `echo_on_edit = false` (credential-adjacent, mirrors CalDAV's `username`). |
| `password` | Password | yes | — | App-specific password. Stored AES-256-GCM. |

User-facing `email_smtp` `ServiceDef` fields:

| Field | Type | Required | Default | Notes |
| --- | --- | --- | --- | --- |
| `host` | Text | yes | — | SMTP submission / relay hostname. |
| `port` | Text (number) | yes | `465` | TCP port. 465 for implicit TLS, 587 for STARTTLS, 25 refused (relay-only). |
| `tls_mode` | Text | yes | `implicit` | `implicit` / `starttls` / `none` (only when `[email].allow_cleartext_smtp = true`). |
| `username` | Text | yes | — | Same `echo_on_edit = false` rule as IMAP. |
| `password` | Password | yes | — | Same as IMAP. |
| `from_address` | Text (email) | yes | — | Single allowed sender. Validated by `MAIL FROM` probe; cached in `email_allowed_from`. |

### 1.4 Operator configuration surface (per-deployment)

The `[email]` TOML section (and its `EMAIL_*` env-var
counterparts) covers the full protocol + security surface.
Operators set the policy; users only see the form.

| Knob | Type | Default | Description |
| --- | --- | --- | --- |
| `allowlist` | CSV | `[]` | Hostname allow-list. Fails closed when empty. |
| `timeout_ms` | u64 | 15000 | Per-request connect+read timeout. |
| `max_messages_per_call` | usize | 100 | Cap on `email_list_inbox` / `email_search`. |
| `max_body_bytes` | usize | 2 MiB | Cap on RFC 5322 body the LLM ever sees. |
| `max_attachment_bytes` | usize | 0 (deny in v1) | Cap on attachment bytes (future `email_get_attachment`). |
| `allow_cleartext_imap` | bool | false | Permits `tls_mode = "none"` for IMAP. Off by default (RFC 3501 §3.4). |
| `allow_cleartext_smtp` | bool | false | Permits `tls_mode = "none"` for SMTP. |
| `strict_tls` | bool | true | When true, refuses self-signed certs regardless of the user form. |
| `recipient_allowlist` | CSV | `[]` | Operator-set outbound `to` / `cc` / `bcc` allow-list for `email_send`. Empty = the user's own `from_address` only. |
| `provider_presets` | map | empty | Pre-fill `host` / `port` / `tls_mode` by `key` (e.g. `fastmail`, `gmail_basic`, `icloud`). The form exposes a dropdown when at least one preset exists. |
| `default_probe_helo` | string | `nagent.local` | `EHLO` identity used by the SMTP probe. |
| `capability_blacklist` | CSV | `[]` | Optional hard-deny of IMAP capabilities (e.g. `["IDLE"]` to keep v1 round-trip). |

### 1.5 IMAP protocol features (v1 vs v2)

The wire client supports IMAP4rev1 + the common extensions.
v1 ships the subset the five chat tools need; the
remainder lives in the client layer so v2 features can
land without redesigning it. The `[email].capability_blacklist`
knob lets an operator hard-deny a capability even if the
server advertises it (defence-in-depth for paranoid setups).

| Feature | RFC | v1 | v2 | Used by |
| --- | --- | --- | --- | --- |
| IMAP4rev1 base (LIST, SELECT, FETCH, SEARCH, STORE, EXPUNGE) | RFC 3501 | yes | — | every tool |
| `CAPABILITY` negotiation | RFC 3501 | yes | — | per-call cache |
| `STARTTLS` | RFC 3501 §6.2.1 | yes | — | `tls_mode=starttls` |
| `LOGINDISABLED` enforcement | RFC 3501 §6.2.3 | yes | — | forces AUTHENTICATE |
| `AUTH=PLAIN` / `AUTH=LOGIN` | RFC 3501 §6.2.2 | yes | — | `auth_mechanism` |
| `AUTH=OAUTHBEARER` | RFC 7628 | no | yes | Gmail v2 |
| `COMPRESS=DEFLATE` | RFC 4978 | yes | — | on by default when advertised |
| `ID` (client identification) | RFC 2971 | yes | — | abuse-tracing payload |
| `NAMESPACE` | RFC 2342 | yes | — | multi-tenant / shared folders |
| `SPECIAL-USE` | RFC 6154 | yes | — | auto-resolve Sent / Drafts / Trash |
| `UIDPLUS` | RFC 4315 | yes | — | accurate UID echo |
| `CONDSTORE` | RFC 7162 | yes | — | `email_mark_read` MODSEQ |
| `QRESYNC` | RFC 7162 | no | yes | incremental sync (out of v1) |
| `SORT` | RFC 5256 | yes | — | server-side ordering in `email_search` |
| `THREAD=REFERENCES` | RFC 5256 | yes | — | grouping in `email_list_inbox` (optional) |
| `SEARCH` full criteria | RFC 3501 §6.4.4 | yes | — | `email_search` (allow-listed) |
| `ESEARCH` | RFC 4731 | yes | — | `email_search` return ALL |
| `IDLE` | RFC 2177 | no | yes | real-time new-mail pings (deferred) |
| `MULTIAPPEND` | RFC 3502 | no | yes | bulk append |
| `OBJECTID` | RFC 8474 | no | yes | object store |
| `BINARY` / `CATENATE` | RFC 3516 | no | yes | binary body fetches |
| `SASL-IR` | RFC 4959 | yes | — | one-round AUTHENTICATE when advertised |
| `LITERAL+` / `LITERAL-` | RFC 2088 | yes | — | non-sync literals |
| `ENABLE` | RFC 5161 | yes | — | capability enablement |

### 1.6 Direct HTTP routes

The existing `GET /v1/agents` and `POST /v1/agents/:name/invoke`
(enumerate, direct-invoke) work for the email agents without
changes — they go through `AgentRegistry` like every other
agent.

### 1.6 Direct HTTP routes

The existing `GET /v1/agents` and `POST /v1/agents/:name/invoke`
(enumerate, direct-invoke) work for the email agents without
changes — they go through `AgentRegistry` like every other
agent.

## 2. File map

All new files are feature-gated on the new `email-agent`
cargo feature in `crates/nagent-agents/Cargo.toml`; the
server feature of the same name gates the corresponding
mount_* helpers in `nagent-server`.

### 2.1 `crates/nagent-agents`

New module:

```
crates/nagent-agents/src/agents/email/
    mod.rs             # EmailImap + EmailSmtp capability traits + shared error type
    client_imap.rs     # thin async IMAP4 client over async-imap + rustls
    client_smtp.rs     # thin async SMTP client over async-smtp + rustls
    list_inbox.rs      # ListInboxAgent
    get_message.rs     # GetMessageAgent
    search.rs          # SearchAgent
    send.rs            # SendAgent
    mark_read.rs       # MarkReadAgent
    parse.rs           # RFC 5322 / RFC 3501 SUBSTRING / SEARCH criteria helpers
```

New `ServiceDef`s (mirroring `crates/nagent-agents/src/caldav_service.rs`):

```
crates/nagent-agents/src/email_imap_service.rs
crates/nagent-agents/src/email_smtp_service.rs
```

Per-feature config wired through the existing `AgentConfigs`
chain (`crates/nagent-server/src/config/agents.rs`):

- new `EmailAgentConfig` struct (timeout, allow-list, max
  messages per call, max body bytes);
- new `[agents.email]` section in `config_file.rs`;
- new `email_agent` sub-config under `config/agents.rs`.

Two `AgentDescriptor` entries appended to `AGENT_DESCRIPTORS`
in `crates/nagent-agents/src/agents.rs` (one descriptor per
agent, gated on the `email-agent` feature — five new
entries).

### 2.2 `crates/nagent-server`

New module under `src/credentials/`:

```
crates/nagent-server/src/credentials/email_probe.rs
    # POST /api/integrations/email_imap/probe
    # POST /api/integrations/email_smtp/probe
```

New config files:

```
crates/nagent-server/src/config/email.rs
    # EmailAgentConfig shell + from_env_with_toml() parser
```

Wiring changes (small):

- `crates/nagent-server/src/state.rs` — expose the SMTP
  allow-list + `from_address` per-user slot into `AppState`
  (one new `Arc<RwLock<HashSet<String>>>`).
- `crates/nagent-server/src/http/mod.rs` — mount the two
  probe endpoints via a single `mount_email_probe_routes`
  helper, gated on the `email-agent` cargo feature.
- `crates/nagent-server/src/app.rs` — pass the SMTP allow-list
  into `tools_schema()` so the configured integrations
  prompt block carries the per-user from-address hint.

### 2.3 Documentation

New operator-facing doc (mirrors `docs/integrations/caldav.md`
and `docs/integrations/x.md`):

```
docs/integrations/email.md
```

Updates to existing surfaces (per AGENTS.md §5 — Documentation
Sync):

- `docs/architecture.md`:
  - §2.4 (built-in agents table) — five new rows;
  - §2.6 (services) — two new paragraphs describing the
    `email_imap` + `email_smtp` `ServiceDef`s and the SMTP
    allow-list role.
- `docs/integrations/caldav.md` and `docs/integrations/x.md`
  gain a sibling link in their "see also" footer.
- `README.md` — add `[agents.email]` and the new env vars to
  the configuration matrix; the existing
  `docs/architecture.md` cross-link from `README.md` stays.
- `AGENTS.md` — no change (it does not enumerate integrations).

## 3. Dependencies

Add to `crates/nagent-agents/Cargo.toml` (all optional,
feature-gated on `email-agent`):

| Crate | Version | Why |
| --- | --- | --- |
| `async-imap` | 0.11 | Small, async-first IMAP4 client. Pure tokio + rustls-friendly via `async-native-tls` or the `async-imap` companion `tokio-rustls` feature. |
| `async-smtp` | 0.7 | Async SMTP client, same maintenance lineage. |
| `mailparse` | 0.15 | Dependency-free RFC 5322 parser used to surface `from`, `subject`, `date`, `body`. |
| `base64` | already in tree | IMAP literals / AUTHENTICATE PLAIN. |
| `tokio-rustls` | 0.26 | TLS for IMAP / SMTP (STARTTLS + implicit TLS). Avoids pulling `native-tls` which the project otherwise does not use. |

The crate already carries `reqwest` + `url` + `bytes` +
`futures-util` + `chrono`, so no new transitive deps beyond
the four above.

The `nagent-server` crate gets nothing new — the SMTP
allow-list and the `EmailAgentConfig` are plain types; no
async runtime plumbing moves out of `nagent-agents`.

## 4. Hardening

The CalDAV + X patterns define the security posture; the email
agents follow them line by line:

- **Per-service hostname allow-list**: `[email].allowlist` in
  TOML (or `EMAIL_ALLOWLIST` env). Default = empty. Agents
  refuse every call when empty (`email_send` and the setup
  probes) with `AgentError::SandboxDenied` /
  HTTP 403. Reuses the existing
  `nagent_agents::egress::host_matches_allowlist` helper.
- **TLS posture — IMAP**:
  - `tls: "ssl"` (default) → connect on port 993 with implicit
    TLS; agent refuses a `starttls`-only server with a clear
    error.
  - `tls: "starttls"` → connect on port 143, mandatory
    STARTTLS (the client never proceeds without it).
  - `tls: "none"` → refused by the agent at config time
    (clear-text IMAP violates §3.4 of RFC 3501; we don't want
    to ship a credential-capturing toggle).
- **TLS posture — SMTP**:
  - `tls: "ssl"` (default) → port 465 (implicit TLS / "SMTPS").
  - `tls: "starttls"` (default for port 587) → mandatory
    STARTTLS before AUTH.
  - `tls: "none"` → refused for port 25, allowed only when
    the operator sets `[email].allow_cleartext_smtp = true`
    (intended for relay scenarios the operator controls).
- **SMTP sender allow-list** (per-user, server-set):
  `email_send` reads the user's configured `from_address`
  from the vault; the agent never lets the LLM pick one.
  The server validates the value with a one-shot SMTP probe
  at save time (`MAIL FROM:<x>` round-trip per address),
  caching the verdict in an `address_allowlist` column on a
  new `email_allowed_from` table — see §6 migration below.
  No LLM-driven outbound mail ever bypasses the row.
- **Capabilities cache**: the IMAP client on first connect
  captures `CAPABILITY` and caches `(idle, condstore,
  search_keys, …)`. `email_search` only sends `SEARCH
  <keys>` when the server advertises `SEARCH KEYWORD …`
  (rare but real — iCloud caps to `ALL`), falling back to
  full-text `SEARCH BODY …` otherwise.
- **Audit row per call**: every agent writes one
  `auth_events` row of kind `email_<tool>_<outcome>` with
  `target_service = "email_imap" | "email_smtp"`; the
  per-call wire bytes are **never** captured (RFC 5322 bodies
  leak PII), only the host + outcome + at most a 256-byte
  message-id hash. The `secret resolver` already audits
  credential access on the read path; the agent duplicates
  one write so the row carries the tool name explicitly.

## 5. The fetch path

The web/SMTP traffic is *not* HTTP, so the existing
`EgressClient` does not apply. The plan introduces a small
`EmailImap` / `EmailSmtp` capability trait next to
`SecretSource` / `SecretSink` / `DocumentSource`, and
threading the actual TCP+TLS wiring stays inside the
`nagent-agents` crate (it does not reach the DB at all).

**Connection model: per-call (decided).** Each agent
invocation opens its own TCP+TLS connection, runs `LOGIN` /
`AUTHENTICATE`, executes the IMAP/SMTP commands, and closes
the connection. No persistent IMAP state on the server, no
per-user connection pool, no `IDLE` listener. This costs
~150 ms of connect+TLS per call on a typical residential
link and is acceptable for the v1 use case. The wire client
is built to be replaceable: a v2 follow-up can layer a
per-user pool (with a `tokio::sync::Mutex<HashMap<Uuid,
AsyncImapClient>>` keyed on `user_id`) without changing
the capability trait or any agent code.

```rust
#[async_trait]
pub trait EmailImap: Send + Sync {
    async fn list_inbox(
        &self,
        user_id: Uuid,
        folder: &str,
        since: Option<DateTime<Utc>>,
        limit: usize,
    ) -> Result<Vec<EmailSummary>, AgentError>;

    async fn get_message(
        &self,
        user_id: Uuid,
        folder: &str,
        uid: u32,
    ) -> Result<EmailMessage, AgentError>;

    async fn search(&self, user_id: Uuid, folder: &str,
        criteria: &str, limit: usize,
    ) -> Result<Vec<EmailSummary>, AgentError>;

    async fn set_flags(&self, user_id: Uuid, folder: &str,
        uid: u32, read: bool,
    ) -> Result<(), AgentError>;
}

#[async_trait]
pub trait EmailSmtp: Send + Sync {
    async fn send(&self, user_id: Uuid, message: &OutgoingMessage)
        -> Result<(), AgentError>;
}
```

The `nagent-agents` crate owns the trait object; the
server-side `EmailImapImpl` / `EmailSmtpImpl` builds the
per-user client (IMAP connect over TLS, log in with the
credentials the vault returns), holds it for the lifetime of
the agent invocation, and tears it down. The trait stays
single-tenant by construction — the per-user `user_id`
parameter is the only identity the agents see.

## 6. Database migration

New migration `0011_email_allowed_from.sql`:

```sql
CREATE TABLE email_allowed_from (
    user_id     UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    from_addr   TEXT NOT NULL,
    probed_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (user_id, from_addr)
);

CREATE INDEX email_allowed_from_user_idx
    ON email_allowed_from (user_id);
```

Lives next to the existing memory migration per the
`nagent_db::migrations` convention. The `probed_at` column
catches the operator-set `TTL` for the row (we cap re-probe
interval at 7 days so a server policy change propagates).

No new credentials columns: IMAP/SMTP credentials ride the
existing `user_credentials (user_id, service, field, …)`
table with `service = "email_imap"` / `"email_smtp"`.

## 7. Agent registry wiring

Append to `crates/nagent-agents/src/agents.rs` `AGENT_DESCRIPTORS`:

```rust
#[cfg(feature = "email-agent")]
AgentDescriptor {
    id: "email_list_inbox", feature: "email-agent",
    build: |cfgs, _pool| Ok(Box::new(email::ListInboxAgent::new(
        cfgs.email.clone(), /* shared EmailImap impl sourced at boot */))),
},
#[cfg(feature = "email-agent")]
AgentDescriptor { id: "email_get_message", /* … */ },
#[cfg(feature = "email-agent")]
AgentDescriptor { id: "email_search",      /* … */ },
#[cfg(feature = "email-agent")]
AgentDescriptor { id: "email_send",        /* … */ },
#[cfg(feature = "email-agent")]
AgentDescriptor { id: "email_mark_read",   /* … */ },
```

The five agents share the same `EmailAgentConfig` + the
`Arc<dyn EmailImap>` / `Arc<dyn EmailSmtp>` set the
server-side `AgentRegistryFactory` attaches, the same way the
server attaches the `DocumentSource` for `read_document`
(see `docs/architecture.md` §2.1).

The `all-agents` meta-feature in the agents `Cargo.toml`
gets the new `email-agent` string in its feature list.

## 8. Phased delivery

Each phase is self-contained: every commit compiles, every
phase has unit tests, and the LLM tool surface stays
backwards-compatible (no name collisions with existing
agents).

### Phase 1 — IMAP probe + read skeleton

- New `email-agent` cargo feature.
- New `EmailImap` trait (interface only).
- `crates/nagent-agents/src/email_imap_service.rs` with the
  `email_imap` `ServiceDef`.
- `crates/nagent-server/src/credentials/email_probe.rs` with
  the IMAP probe endpoint.
- `crates/nagent-server/src/probe.rs` `ProbeConfig` impl for
  the IMAP shell + audit row kind `email_imap_probe_<outcome>`.
- Unit tests: `email_probe.rs` against a loopback
  `async-imap` test fixture (the existing `crates/nagent-server`
  CI already inlines one for the CalDAV probe).
- **No LLM tools yet** — the probe endpoint only.

### Phase 2 — SMTP probe + `email_smtp` `ServiceDef`

- `crates/nagent-agents/src/email_smtp_service.rs` +
  `crates/nagent-server/src/credentials/email_probe.rs::probe_smtp`.
- Wires the SMTP probe (`EHLO`, `MAIL FROM:<address>` round
  trip; `email_smtp_probe_<outcome>` audit row).
- The migration `0011_email_allowed_from.sql` lands in this
  phase because the probe needs to write to the table.

### Phase 3 — `email_list_inbox` + `email_get_message`

- The `EmailImapImpl` server adapter; `AsyncImapClient`
  built per-call from the vault (Basic + LOGIN or
  AUTHENTICATE PLAIN, depending on `CAPABILITY`).
- `mailparse`-driven response decoding (one summary row per
  UID + `Subject`, `From`, `Date`, `Snippet`, body stored as
  a base64-fenced `text/plain` excerpt capped at 4 KiB).
- `email_list_inbox` and `email_get_message` agents added
  to `AGENT_DESCRIPTORS`.
- Integration tests against the loopback IMAP fixture.

### Phase 4 — `email_search` + `email_mark_read`

- `email_search` issues an IMAP `SEARCH <criteria>` and walks
  the matches; capability-aware (`SEARCH KEYWORD` if present,
  else `SEARCH BODY`). The query string is constrained to a
  small allow-list of IMAP criteria tokens (see §10 Open
  Question 4).
- `email_mark_read` issues `STORE <uid> +FLAGS (\Seen)` (or
  `-FLAGS`).
- Both agents are confirm-on-write or confirm-on-read
  matching Phase 3.

### Phase 5 — `email_send`

- `EmailSmtpImpl` server adapter; `AsyncSmtpClient` built
  per-call from the vault; STARTTLS or implicit TLS chosen
  per the saved config.
- The `email_send` agent checks the LLM-supplied `to` /
  `cc` / `bcc` against the operator allow-list
  (`[email].recipient_allowlist`, default = the user's
  own address only) **before** it reaches the `MAIL FROM`
  envelope — a misconfigured ask is rejected with
  `AgentError::SandboxDenied("recipient … not in operator
  allow-list")` and never touches the network.
- Server writes one `email_send_outcome` audit row per call
  (recipient is hashed with SHA-256 to keep PII out of the
  audit log).
- Integration tests inject a loopback SMTP fixture that
  inspects the wire bytes the agent emitted.

### Phase 6 — Documentation + integration tests

- `docs/integrations/email.md` operator walk-through.
- `docs/architecture.md` §2.4 table updated with the five
  rows; §2.6 updated for `email_imap` + `email_smtp`.
- `crates/nagent-server/tests/email_e2e.rs` mirroring
  `tests/caldav_e2e.rs` shape — boot the test app,
  register the per-user credentials through the
  `email_imap` `ServiceDef` PUT flow, invoke
  `POST /v1/agents/email_list_inbox/invoke` against the
  loopback fixture, assert on the JSON shape.
- `Makefile` — add a new `make run-email-agent` target
  (or extend `make run-llm`) that compiles
  `--features nagent-server/email-agent` per the
  `make_run_targets_feature_matrix` pattern locked in
  project memory.

## 9. Open questions

### Q1 — Gmail OAuth vs generic IMAP

The catalog flags this as an open question. The plan goes
**with generic IMAP/SMTP first (this PR)**, and reserves a
follow-up plan for `email_gmail` (OAuth + Gmail-specific
capabilities like `X-GM-EXT-1`). The two `ServiceDef`s stay
distinct: `gmail_*` tools (or the same five agent names
dispatching through the `ServiceDef` the user actually
configured) are added in v2. The capabilities the user picks
let one chat agent name serve both paths without the LLM
seeing the underlying protocol choice.

### Q2 — SMTP allow-list operator-set vs per-user

The plan opts for **both**: the operator sets the
`recipient_allowlist` (default = user's address only), and
the per-user `email_allowed_from` row carries the verified
`From:` addresses. Two separate gates, two different
threats (outbound spam vs outbound impersonation).

### Q3 — Sync state / IDLE push (decided: v2)

Defer to v2 (per the §1.5 matrix and the §5 per-call
connection model). A future plan can layer `IDLE`
notifications through the existing WebSocket + a new
`crate::notifications` channel; the existing B1
(`reminders`) and B9 (`timers`) plans ride the same
plumbing when they land. The v1 wire client already speaks
`IDLE` (the capability is in §1.5 as "no" only because v1
does not subscribe to it).

### Q4 — `email_search` query safety

The plan constrains `email_search`'s `query` to a
hand-rolled token allow-list (the IMAP criteria set: `ALL`,
`SUBJECT`, `FROM`, `TO`, `BODY`, `SINCE`, `BEFORE`, `UID`,
`UNSEEN`, `SEEN`). The agent rejects anything else with
`AgentError::InvalidArguments`. Free-form Gmail-style
queries (e.g. `from:alice subject:report`) are deliberately
*v2* — implementing a proper query parser would mean
re-importing the iCloud/Gmail search grammar, which is out
of scope for this plan.

### Q5 — Attachments

Out of v1 (see §0). The agent returns the
`Content-Type: text/plain` body only and surfaces an
`attachments: [{filename, mime, size_bytes, content_id?}]`
envelope. A separate `email_get_attachment(uid, cid)`
follow-up plan can land later without changing the existing
agent shape.

## 10. Risks and mitigations

- **Per-call connect cost** (the v1 design choice — see §5):
  every IMAP call re-opens the connection. Acceptable for
  v1; a per-user pool can be layered in v2 without changing
  the capability trait.
- **Per-mailbox UIDVALIDITY bumps**: an IMAP server may
  reset UIDs out from under the LLM. The `email_get_message`
  agent checks `UIDValidity` against the cached value and
  returns a clear `AgentError::AgentFailed("UIDs were
  re-issued; please list again")` if it shifts. The chat UI
  already exposes agent errors verbatim, so this is a visible
  signal not a silent one.
- **Plain-text IMAP / SMTP**: the agent refuses the
  configuration at save time (not at call time), so a user
  who manually edits a vault row gets the same refusal —
  the agent refuses to log in regardless of how the
  credentials were inserted.
- **iCloud app-specific password workflow**: iCloud is the
  only mainstream provider that still ships plain-text IMAP
  with App-Specific Passwords. The `email_imap` `ServiceDef`
  ships a *configurable* "Apple iCloud Quick Setup" hidden
  behind a `[email].provider_presets` array so the operator
  can pre-fill `host=imap.mail.me.com, port=993,
  tls=ssl`. Documented in `docs/integrations/email.md`.

## 11. What this plan is **not**

- It does not propose storing inbox contents server-side
  (everything stays at the IMAP provider).
- It does not propose Gmail OAuth as a v1 capability.
- It does not touch the LLM tool loop, the SSE framing, or
  the existing `/v1/agents` discovery logic — the new
  descriptors ride the same plumbing.
- It does not add a service-worker / browser-side cache; the
  LLM is the only client and every call re-fetches.
- It does not modify `nagent-db` row shapes beyond
  `0011_email_allowed_from.sql`.

## 12. Validation and rollout

Each phase has:

- A unit-test suite next to the new module
  (`agents/email/client_imap.rs::tests`, etc.).
- An integration test under `crates/nagent-server/tests/`
  using a loopback IMAP / SMTP fixture, following the
  pattern of the existing `tests/caldav_e2e.rs`.
- One audit row per wire call so the operator can trace
  per-call host + outcome + tool name without reading the
  LLM trace.
- A documentation sync (per AGENTS.md §5) that updates
  `docs/architecture.md`, `docs/integrations/email.md`,
  and `README.md`'s configuration matrix.

Feature flag:

- New cargo feature `email-agent` in both
  `crates/nagent-agents/Cargo.toml` and
  `crates/nagent-server/Cargo.toml`. Listed in
  `all-agents` meta-feature.
- `Makefile` target `make run-email-agent` (and a matching
  entry in the existing `make run-llm` feature list) per
  the `make_run_targets_feature_matrix` pattern locked in
  project memory.

Operator migration:

- One new `nagent_db` migration: `0011_email_allowed_from.sql`.
- The migration is **backwards-compatible** (new table, no
  edits to existing rows).
- A first install requires the operator to (a) set
  `[email].allowlist` and (b) the user to type their IMAP /
  SMTP creds in the Integrations page; no other operator
  step.
