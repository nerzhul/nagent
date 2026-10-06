-- 0010_memories.down.sql — drops the memories table added by
-- 0010_memories.up.sql. Used by the `migrate revert` CLI for
-- symmetry only; running it in production silently drops every
-- user's stored memories (the auto-injection block in
-- `llm::prompt::proxy::chat_completions` will read zero rows after
-- the rollback and the system prompt is silently omitted, which is
-- the safe degraded mode).
DROP TABLE memories;