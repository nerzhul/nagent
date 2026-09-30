# Configuration examples

Drop-in TOML files for `nagent-server --config <path>`. Each file is
a complete, runnable example — copy it to `~/.config/nagent/config.toml`
(or any other path) and tweak the knobs you care about.

| File | Use case |
| ---- | -------- |
| [`config.toml.example`](config.toml.example) | Reference config with every knob documented inline. Defaults to `auth.enabled = false` (the pre-PR1 single-user trust boundary). |
| [`auth.sqlite.toml`](auth.sqlite.toml) | Fresh local install. Auto-bootstraps the first admin with a random password on first run. |
| [`auth.postgres.toml`](auth.postgres.toml) | Shared cluster (postgres-backed auth DB). Admins must be created via the `nagent-server auth create-admin` CLI — auto-bootstrap is intentionally disabled. |
| [`credentials.sqlite.toml`](credentials.sqlite.toml) | SQLite + per-user credentials vault (AES-256-GCM). Boot recipe included. |

**Reminder for the credentials example**: the AES-256-GCM
encryption key lives in plaintext inside `[auth.credentials].key`.
Treat the TOML file as a secret (encrypted volume, restricted
ACLs, no VCS).

See the README "Authentication" section for the bootstrap walkthrough
and the `auth.*` env-var / TOML-key reference table; see the
"Per-user credentials" section for the threat model and the
key-generation recipe.
