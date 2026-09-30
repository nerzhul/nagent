-- 0005_session_tokens.down.sql — security plan #7 rollback.
--
-- Reverts the `sessions` table to the plaintext-UUID id schema
-- that 0001_init.up.sql ships. This migration is here for the
-- sqlx `down` symmetry only — running it in production would
-- invalidate every active session a second time (all the
-- opaque-token cookies in flight would no longer resolve).
DROP TABLE IF EXISTS sessions;

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
