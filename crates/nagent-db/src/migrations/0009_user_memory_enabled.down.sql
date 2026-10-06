-- 0009_user_memory_enabled.down.sql — reverts the opt-in column
-- added by 0009_user_memory_enabled.up.sql. Used by the
-- `migrate revert` CLI for symmetry only; running it in
-- production silently drops every user's `memory_enabled` choice
-- (the LLM-driven memory subsystem reverts to the off default for
-- everyone).
ALTER TABLE user_preferences
  DROP COLUMN memory_enabled;