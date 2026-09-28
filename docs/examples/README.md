# Configuration examples

Drop-in TOML files for `stt-server --config <path>`. Each file is
a complete, runnable example — copy it to `~/.config/nagent/config.toml`
(or any other path) and tweak the knobs you care about.

| File | Use case |
| ---- | -------- |
| [`config.toml.example`](config.toml.example) | Reference config with every knob documented inline. Defaults to `auth.enabled = false` (the pre-PR1 single-user trust boundary). |
| [`auth.sqlite.toml`](auth.sqlite.toml) | Fresh local install. Auto-bootstraps the first admin with a random password on first run. |
| [`auth.postgres.toml`](auth.postgres.toml) | Shared cluster (postgres-backed auth DB). Admins must be created via the `stt-server auth create-admin` CLI — auto-bootstrap is intentionally disabled. |

See the README "Authentication" section for the bootstrap walkthrough
and the `auth.*` env-var / TOML-key reference table.
