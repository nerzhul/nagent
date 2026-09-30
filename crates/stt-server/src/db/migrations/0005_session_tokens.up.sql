-- 0005_session_tokens.up.sql — security plan #7.
--
-- Session ids are currently a UUIDv4 stored in plaintext in
-- `sessions.id`, echoed in login JSON bodies, written to logs, and
-- passed to the browser in cookies. Any log reader can hijack any
-- active session. This migration replaces the plaintext id with
-- the SHA-256 hash of a 256-bit random token, the same way the
-- well-known "bearer hash" pattern works for API tokens.
--
-- Wire format:
--   - cookie + JSON body: 43-char base64url plaintext token
--     (32 bytes of OS-RNG entropy, never persisted)
--   - DB PK (`token_hash`): SHA-256 of the plaintext token, 32 bytes
--
-- The migration DROPs and recreates the `sessions` table — there is
-- no way to recover plaintext UUIDs once we hash them, so every
-- existing session is invalidated by the deploy. Operators must
-- log in again. This matches the user-approved plan (security plan
-- #7 — "invalidate every existing session on the deploy that ships
-- this migration").
--
-- Auth events still reference sessions via `user_id` (not
-- `session_id`), so dropping the table does not break the audit
-- trail. Pending OIDC states, passkeys, and credentials are
-- untouched.
DROP TABLE IF EXISTS sessions;

CREATE TABLE sessions (
    token_hash      BLOB PRIMARY KEY,
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
