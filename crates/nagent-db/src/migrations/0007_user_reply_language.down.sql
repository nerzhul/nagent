-- 0007_user_reply_language.down.sql — reverts the reply-language
-- column added by 0007_user_reply_language.up.sql. Used by the
-- `migrate revert` CLI for symmetry only; running it in production
-- silently drops every user's saved reply-language choice (the
-- assistant will revert to the default "match the user's input
-- language" behaviour and TTS will fall back to reading the STT
-- input picker).
ALTER TABLE user_preferences
  DROP COLUMN reply_language;