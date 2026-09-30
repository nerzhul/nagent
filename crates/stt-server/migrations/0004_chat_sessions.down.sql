-- 0004_chat_sessions.down.sql — undo `0004_chat_sessions.up.sql`.
--
-- Drops the table + its indexes. Files under `cache_dir` are
-- NOT removed by `down` — the operator is expected to run
-- `stt-server documents purge --older-than 0s` afterwards.
DROP INDEX IF EXISTS chat_sessions_user_idx;
DROP TABLE IF EXISTS chat_sessions;