-- 0004_chat_sessions.up.sql — server-bound chat session ids.
--
-- The `X-Chat-Session-Id` header used to be client-controlled
-- (the browser minted a UUID v4 in localStorage on first visit).
-- The PR audit (SEV 2) called that out: a logged-in user could
-- send ANY UUID and read docs in someone else's session if the
-- UUID was leaked.
--
-- This table binds each chat session id to the authenticated
-- user that minted it. The browser now calls
-- `POST /v1/chat/session` on boot to get a server-minted id
-- (with no client control over the value) and reuses it on every
-- subsequent request. Every documents route / agent invocation
-- validates `(current_user, X-Chat-Session-Id)` against this table
-- and rejects mismatches with `403`.
--
-- `last_seen_at` is bumped on every documents request so a
-- future plan can implement "session is stale after N days" via
-- a periodic sweep — same pattern as `auth.sessions`.

CREATE TABLE IF NOT EXISTS chat_sessions (
    id          TEXT PRIMARY KEY,
    user_id     TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    created_at  TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    last_seen_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE INDEX IF NOT EXISTS chat_sessions_user_idx
    ON chat_sessions(user_id, last_seen_at DESC);