-- 0012_chat_messages.up.sql — per-session chat message store.
--
-- Reversed by `0012_chat_messages.down.sql`; keep the two siblings in
-- sync (sqlx 0.8.6 pairs them via the `<version>_<name>.up.sql` /
-- `<version>_<name>.down.sql` filename convention).
--
-- The `chat_sessions(id)` row is the per-user ownership carrier
-- (SEV 2 fix, plan 4.A): every message row is keyed on
-- `(user_id, session_id)` so a route can verify the binding once
-- through the existing `ChatSessions::touch_and_verify` and then
-- only ever touch rows scoped to that pair.
--
-- `id` is a server-minted UUID v4 (the browser never gets to pick).
-- `ordinal` is a per-session monotonically increasing counter so the
-- `list` order is stable even when two messages share the same `ts`
-- (browsers throttle `Date.now()` and the LLM stream can finish two
-- turns in the same millisecond). The PATCH route uses
-- `version` for optimistic concurrency: a stale edit from a
-- second tab is rejected with `409 VersionMismatch` instead of
-- silently overwriting the newer row.
--
-- `model` is `NULL` for `role = 'user'` (the user did not pick a
-- model) and required-on-write for `role = 'assistant'` /
-- `'system'`. The CHECK constraint enforces a closed set of roles
-- so a buggy client cannot smuggle a `tool` row past the gate
-- (tool traces stay in `localStorage` and the LLM context for
-- now — see plan §0 "out of scope").
--
-- No `attachments` column: the plan explicitly defers attachment
-- persistence server-side (it lives in the localStorage copy and
-- re-hydrates via the chips). The `documents` API keeps the
-- authoritative upload store.

CREATE TABLE IF NOT EXISTS chat_messages (
    id            TEXT        NOT NULL PRIMARY KEY,
    session_id    TEXT        NOT NULL REFERENCES chat_sessions(id) ON DELETE CASCADE,
    user_id       TEXT        NOT NULL REFERENCES users(id)       ON DELETE CASCADE,
    role          TEXT        NOT NULL CHECK (role IN ('user', 'assistant', 'system')),
    content       TEXT        NOT NULL,
    model         TEXT        NULL,
    ts            TEXT        NOT NULL DEFAULT CURRENT_TIMESTAMP,
    ordinal       INTEGER     NOT NULL,
    version       INTEGER     NOT NULL DEFAULT 1
);

CREATE INDEX IF NOT EXISTS chat_messages_session_ordinal
    ON chat_messages (session_id, ordinal);
CREATE INDEX IF NOT EXISTS chat_messages_user_ts
    ON chat_messages (user_id, ts DESC);
