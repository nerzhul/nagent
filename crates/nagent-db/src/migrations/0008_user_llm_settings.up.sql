-- 0008_user_llm_settings.up.sql — per-user LLM settings.
--
-- Migration 0007 hoisted three explicit user preferences (location
-- sharing, timezone sharing, reply language) into the
-- `user_preferences` row. This migration adds the last two LLM
-- controls that were previously held only in the SPA's DOM state
-- and lost on every reload: the user-supplied *additional
-- instructions* (appended after the server's default system
-- prompt) and the per-turn *temperature*.
--
-- Storage shape:
--   additional_instructions  TEXT NULL DEFAULT NULL — free-form
--                            additional system prompt. Empty / unset
--                            means the user did not customise the
--                            LLM; the server-side default system
--                            prompt (env `LLM_SYSTEM_PROMPT` / TOML
--                            `[llm].system_prompt`) still applies.
--                            No length cap is enforced at the DB
--                            layer — `chat.js` validates the
--                            textarea length on input.
--   temperature            REAL NULL DEFAULT NULL — sampling
--                            temperature in `[0.0, 2.0]`. NULL
--                            means "use the proxy default" so a
--                            user who never touched the slider
--                            keeps the upstream model's
--                            recommended sampling. Out-of-range
--                            values are clamped client-side by the
--                            `<input type="number" min="0" max="2">`
--                            contract — the server does not
--                            re-validate (out of scope for this
--                            plan, see `docs/ui_features.md`
--                            §4.4a).
--
-- Column position: appended after `reply_language` and before
-- `updated_at`, matching the order of fields on the Rust
-- `UserPreferences` struct.
--
-- Nullable so no existing user row needs a value: a user who
-- never picked a preference keeps the default "use upstream
-- defaults" behaviour unchanged.

ALTER TABLE user_preferences
  ADD COLUMN additional_instructions TEXT NULL DEFAULT NULL;

ALTER TABLE user_preferences
  ADD COLUMN temperature             REAL NULL DEFAULT NULL;