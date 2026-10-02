-- 0008_user_llm_settings.down.sql — reverts the LLM settings
-- columns added by 0008_user_llm_settings.up.sql. Used by the
-- `migrate revert` CLI for symmetry only; running it in
-- production silently drops every user's saved additional
-- instructions and temperature choice (the LLM will revert to
-- the upstream model defaults).
--
-- Two separate statements because SQLite's `ALTER TABLE ... DROP
-- COLUMN` only accepts a single column per statement (same
-- limitation as `ADD COLUMN` — see `0008_user_llm_settings.up.sql`
-- for the matching rationale).
ALTER TABLE user_preferences
  DROP COLUMN temperature;

ALTER TABLE user_preferences
  DROP COLUMN additional_instructions;