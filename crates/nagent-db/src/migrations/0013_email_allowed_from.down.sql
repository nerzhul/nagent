-- 0013_email_allowed_from.down.sql — drops the
-- `email_allowed_from` table added by 0013_email_allowed_from.up.sql.
-- Used by the `migrate revert` CLI for symmetry only; running it in
-- production silently drops every user's verified `from_address`
-- rows. After the rollback the SMTP probe will need to re-validate
-- every user's address on the next save.
DROP TABLE email_allowed_from;
