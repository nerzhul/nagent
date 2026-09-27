-- 0001_init.sql — single-file auth schema for PR1.
--
-- All tables live in one migration because the schema is small and
-- the tables are tightly coupled (sessions + passkeys reference
-- users; pending OIDC states reference nothing but are part of the
-- auth subsystem as a whole). Future PRs that need to evolve the
-- schema land in their own timestamped files (`0002_*.sql`,
-- `0003_*.sql`, …).
--
-- Portability notes:
--
-- * `TEXT PRIMARY KEY` for UUID columns works on both sqlite
--   (stored as TEXT) and postgres (stored as TEXT, not the native
--   `uuid` type). The Rust side converts via the `uuid` crate.
-- * `BLOB` for binary columns (password_hash, credential_id,
--   public_key, aaguid) becomes `bytea` on postgres and stays
--   `BLOB` on sqlite — sqlx maps the two transparently.
-- * `BIGINT PRIMARY KEY` (without `AUTOINCREMENT` / `BIGSERIAL`)
--   for `auth_events.id` so the DDL is portable; the application
--   generates the id with an atomic counter (see
--   `next_event_id()` in `db_sqlite.rs` / `db_postgres.rs`).
-- * `CURRENT_TIMESTAMP` is the default for every timestamp column.
--   Both engines return a string in the same RFC 3339-like form
--   so the Rust parser (`parse_rfc3339`) does not have to branch
--   on the engine.
-- * `BIGINT` for the passkey counter (rather than `INTEGER` /
--   `u32`) so the schema accepts the full range webauthn-rs uses
--   when wrapping a real hardware counter.

-- ---------------------------------------------------------------------------
-- users: identity + per-user metadata. The `provider` string drives the
-- login surface (local / oidc:<issuer> / passkey) and PR2 RBAC scoping.
-- `password_hash` is the argon2id encoded hash for the local backend
-- and NULL for OIDC-only / passkey-only users.
-- ---------------------------------------------------------------------------
CREATE TABLE users (
    id              TEXT PRIMARY KEY,
    email           TEXT NOT NULL UNIQUE,
    display_name    TEXT NOT NULL,
    provider        TEXT NOT NULL,
    password_hash   BLOB,
    created_at      TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    disabled_at     TEXT
);

CREATE INDEX users_provider_idx ON users(provider);

-- ---------------------------------------------------------------------------
-- passkeys: WebAuthn credentials. One row per registered authenticator.
-- `credential_id` is the raw 16+ bytes the authenticator emits;
-- `public_key` is the serialised webauthn-rs `Passkey` JSON (which
-- carries the COSE key + attestation metadata + extensions so we
-- can reconstruct the `Passkey` on login without poking at the
-- crate's private fields). `counter` is the monotonic
-- authenticator counter — a cloned authenticator would replay an
-- old counter and fail the `webauthn-rs` signature check.
-- ---------------------------------------------------------------------------
CREATE TABLE passkeys (
    id              TEXT PRIMARY KEY,
    user_id         TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    credential_id   BLOB NOT NULL UNIQUE,
    public_key      BLOB NOT NULL,
    counter         BIGINT NOT NULL DEFAULT 0,
    transports      TEXT NOT NULL DEFAULT '',
    aaguid          BLOB,
    created_at      TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    last_used_at    TEXT
);

CREATE INDEX passkeys_user_id_idx ON passkeys(user_id);

-- ---------------------------------------------------------------------------
-- sessions: durable session table. `expires_at` is computed at login as
-- `now() + auth.session_ttl_days` and is NEVER extended on activity
-- (see plan D6a — NIST SP 800-63B absolute session timeout).
-- `last_seen_at` is debug-only. `csrf_token` is a 32-byte random hex
-- string minted once at session creation; the middleware compares it
-- against the `x-csrf-token` header on state-changing requests.
-- ---------------------------------------------------------------------------
CREATE TABLE sessions (
    id              TEXT PRIMARY KEY,
    user_id         TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    csrf_token      TEXT NOT NULL,
    created_at      TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    expires_at      TEXT NOT NULL,
    last_seen_at    TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    ip              TEXT,
    user_agent      TEXT
);

CREATE INDEX sessions_user_id_idx ON sessions(user_id);
CREATE INDEX sessions_expires_at_idx ON sessions(expires_at);

-- ---------------------------------------------------------------------------
-- auth_events: append-only audit trail. `user_id` is NULL on failed
-- login attempts where we never matched a user row. `id` is generated
-- by the application (see `next_event_id()`) so the DDL stays
-- portable across sqlite (no AUTOINCREMENT) and postgres (no
-- BIGSERIAL).
-- ---------------------------------------------------------------------------
CREATE TABLE auth_events (
    id              BIGINT PRIMARY KEY,
    user_id         TEXT,
    kind            TEXT NOT NULL,
    provider        TEXT NOT NULL,
    ip              TEXT,
    user_agent      TEXT,
    occurred_at     TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE INDEX auth_events_user_id_idx ON auth_events(user_id);
CREATE INDEX auth_events_occurred_at_idx ON auth_events(occurred_at);

-- ---------------------------------------------------------------------------
-- pending_oidc_states: OIDC `state` parameter persistence. Each row
-- represents an in-flight OIDC login: the `state` parameter
-- returned to the IdP, the PKCE verifier we generated at the
-- start, and the nonce we attached to the id_token request. The
-- callback reads the row, deletes it (one-shot), and exchanges
-- the authorisation code. Rows auto-expire after 5 minutes
-- (PENDING_TTL_SECS in oidc.rs); the application prunes them
-- lazily on lookup.
-- ---------------------------------------------------------------------------
CREATE TABLE pending_oidc_states (
    state           TEXT PRIMARY KEY,
    pkce_verifier   TEXT NOT NULL,
    nonce           TEXT NOT NULL,
    expires_at      TEXT NOT NULL
);

CREATE INDEX pending_oidc_states_expires_at_idx ON pending_oidc_states(expires_at);
