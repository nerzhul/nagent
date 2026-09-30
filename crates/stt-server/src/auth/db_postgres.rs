//! PostgreSQL-backed implementation of
//! [`crate::auth::store::AuthStore`]. Mirrors
//! [`crate::auth::db_sqlite`] line-for-line; the only meaningful
//! differences are the pool type and the `&[u8]` ↔ `Vec<u8>`
//! mapping for `BLOB` ↔ `BYTEA` (sqlx handles this transparently
//! when the `uuid` feature is on, which it is).

use chrono::{DateTime, Utc};
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};
use std::time::Duration;
use uuid::Uuid;

use crate::auth::error::AuthError;
use crate::auth::session::SessionRecord;
use crate::auth::store::{
    AuthUserRecord, MigrationRow, MigrationStatus, NewAuthEvent, NewPasskeyRecord, PasskeyRecord,
    UserCredentialRow,
};
use crate::config::AuthConfig;

static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

#[derive(Clone, Debug)]
pub struct PgStore {
    pool: PgPool,
}

impl PgStore {
    pub(crate) async fn connect(cfg: &AuthConfig) -> Result<Self, AuthError> {
        let pool = PgPoolOptions::new()
            .max_connections(cfg.db.max_connections.max(1))
            .connect(&cfg.db.url)
            .await?;
        Ok(Self { pool })
    }

    pub(crate) async fn migrate(&self) -> Result<(), AuthError> {
        MIGRATOR
            .run(&self.pool)
            .await
            .map_err(|e| AuthError::Internal(format!("postgres migrations failed: {e}")))
    }

    /// Read `_sqlx_migrations` and diff it against the embedded
    /// `MIGRATOR` static. On a fresh DB `_sqlx_migrations` does not
    /// exist; postgres surfaces that as
    /// `SqlState::UNDEFINED_TABLE` (42P01). Anything else is a real
    /// error.
    pub(crate) async fn migration_status(&self) -> Result<MigrationStatus, AuthError> {
        let rows = sqlx::query(
            "SELECT version, description FROM _sqlx_migrations \
             WHERE success = TRUE ORDER BY version",
        )
        .fetch_all(self.pool())
        .await;
        let applied: Vec<MigrationRow> = match rows {
            Ok(rows) => rows
                .into_iter()
                .map(|r| {
                    Ok::<_, sqlx::Error>(MigrationRow {
                        version: r.try_get::<i64, _>("version")?,
                        description: r.try_get::<String, _>("description")?,
                    })
                })
                .collect::<Result<Vec<_>, _>>()?,
            Err(sqlx::Error::Database(db_err)) if db_err.code().as_deref() == Some("42P01") => {
                Vec::new()
            }
            Err(e) => return Err(AuthError::Database(e)),
        };

        let applied_versions: std::collections::HashSet<i64> =
            applied.iter().map(|r| r.version).collect();
        // Deduplicate by version: `MIGRATOR.iter()` returns one
        // entry per file (`.up.sql` + `.down.sql` for each
        // reversible migration). See `db_sqlite.rs::migration_status`
        // for the rationale.
        let mut pending: Vec<MigrationRow> = Vec::new();
        let mut seen: std::collections::HashSet<i64> = std::collections::HashSet::new();
        for m in MIGRATOR.iter() {
            if applied_versions.contains(&m.version) {
                continue;
            }
            if !seen.insert(m.version) {
                continue;
            }
            pending.push(MigrationRow {
                version: m.version,
                description: m.description.to_string(),
            });
        }
        pending.sort_by_key(|r| r.version);
        let highest_applied = applied.iter().map(|r| r.version).max();
        Ok(MigrationStatus {
            applied,
            pending,
            highest_applied,
        })
    }

    /// Reverse every applied migration whose version is strictly
    /// greater than `target_version`. The target itself stays
    /// applied (matches sqlx 0.8.6 `Migrator::undo` semantics).
    pub(crate) async fn revert_to(&self, target_version: i64) -> Result<(), AuthError> {
        MIGRATOR
            .undo(&self.pool, target_version)
            .await
            .map_err(|e| AuthError::Internal(format!("postgres migrations undo failed: {e}")))
    }

    pub(crate) fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Public pool handle for `AuthStore::pool()`. Returns the raw
    /// `PgPool` for one-off queries (OIDC state persistence).
    pub fn pool_handle(&self) -> PgPool {
        self.pool.clone()
    }

    pub(crate) async fn get_user_by_id(
        &self,
        id: Uuid,
    ) -> Result<Option<AuthUserRecord>, AuthError> {
        let row = sqlx::query(
            "SELECT id::text AS id, email, display_name, provider, password_hash, \
                    created_at::text AS created_at \
             FROM users WHERE id = $1 AND disabled_at IS NULL",
        )
        .bind(id)
        .fetch_optional(self.pool())
        .await?;
        Ok(row.map(row_to_user))
    }

    pub(crate) async fn get_user_by_email(
        &self,
        email: &str,
    ) -> Result<Option<AuthUserRecord>, AuthError> {
        let row = sqlx::query(
            "SELECT id::text AS id, email, display_name, provider, password_hash, \
                    created_at::text AS created_at \
             FROM users WHERE LOWER(email) = LOWER($1) AND disabled_at IS NULL",
        )
        .bind(email)
        .fetch_optional(self.pool())
        .await?;
        Ok(row.map(row_to_user))
    }

    pub(crate) async fn create_user(
        &self,
        email: &str,
        display_name: &str,
        provider: &str,
        password_hash: Option<&[u8]>,
    ) -> Result<Uuid, AuthError> {
        let id = Uuid::new_v4();
        let normalised = email.trim().to_ascii_lowercase();
        let res = sqlx::query(
            "INSERT INTO users (id, email, display_name, provider, password_hash) \
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(id)
        .bind(&normalised)
        .bind(display_name)
        .bind(provider)
        .bind(password_hash)
        .execute(self.pool())
        .await;
        match res {
            Ok(_) => Ok(id),
            Err(sqlx::Error::Database(db_err)) if db_err.is_unique_violation() => Err(
                AuthError::Conflict(format!("user with email {normalised:?} already exists")),
            ),
            Err(e) => Err(AuthError::Database(e)),
        }
    }

    pub(crate) async fn set_password(
        &self,
        user_id: Uuid,
        password_hash: &[u8],
    ) -> Result<u64, AuthError> {
        let res = sqlx::query("UPDATE users SET password_hash = $1 WHERE id = $2")
            .bind(password_hash)
            .bind(user_id)
            .execute(self.pool())
            .await?;
        Ok(res.rows_affected())
    }

    pub(crate) async fn delete_user(&self, user_id: Uuid) -> Result<u64, AuthError> {
        let res = sqlx::query("DELETE FROM users WHERE id = $1")
            .bind(user_id)
            .execute(self.pool())
            .await?;
        Ok(res.rows_affected())
    }

    pub(crate) async fn delete_user_by_email(
        &self,
        email: &str,
    ) -> Result<Option<Uuid>, AuthError> {
        let row = sqlx::query("SELECT id::text AS id FROM users WHERE LOWER(email) = LOWER($1)")
            .bind(email)
            .fetch_optional(self.pool())
            .await?;
        let Some(r) = row else { return Ok(None) };
        let id_str: String = r.try_get("id")?;
        let id = Uuid::parse_str(&id_str).expect("DB UUID must parse");
        self.delete_user(id).await?;
        Ok(Some(id))
    }

    pub(crate) async fn count_users_by_provider(&self, provider: &str) -> Result<i64, AuthError> {
        let row = sqlx::query("SELECT COUNT(*) AS c FROM users WHERE provider = $1")
            .bind(provider)
            .fetch_one(self.pool())
            .await?;
        let c: i64 = row.try_get("c")?;
        Ok(c)
    }

    pub(crate) async fn list_users(
        &self,
        provider_prefix: Option<&str>,
    ) -> Result<Vec<AuthUserRecord>, AuthError> {
        let rows = if let Some(prefix) = provider_prefix {
            sqlx::query(
                "SELECT id::text AS id, email, display_name, provider, password_hash, \
                        created_at::text AS created_at \
                 FROM users WHERE provider LIKE $1 ORDER BY created_at DESC",
            )
            .bind(format!("{prefix}%"))
            .fetch_all(self.pool())
            .await?
        } else {
            sqlx::query(
                "SELECT id::text AS id, email, display_name, provider, password_hash, \
                        created_at::text AS created_at \
                 FROM users ORDER BY created_at DESC",
            )
            .fetch_all(self.pool())
            .await?
        };
        Ok(rows.into_iter().map(row_to_user).collect())
    }

    pub(crate) async fn create_session(
        &self,
        user_id: Uuid,
        ttl: Duration,
        ip: Option<&str>,
        user_agent: Option<&str>,
    ) -> Result<SessionRecord, AuthError> {
        // Security plan #7 — see `db_sqlite.rs::create_session`
        // for the full rationale.
        let (token, token_hash) = crate::auth::session::new_session_token();
        let csrf = crate::auth::session::new_csrf_token();
        let now = Utc::now();
        let expires_at = now + chrono::Duration::from_std(ttl).unwrap_or(chrono::Duration::days(7));
        sqlx::query(
            "INSERT INTO sessions (token_hash, user_id, csrf_token, created_at, expires_at, last_seen_at, ip, user_agent) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
        )
        .bind(&token_hash[..])
        .bind(user_id)
        .bind(&csrf)
        .bind(now.to_rfc3339())
        .bind(expires_at.to_rfc3339())
        .bind(now.to_rfc3339())
        .bind(ip)
        .bind(user_agent)
        .execute(self.pool())
        .await?;
        Ok(SessionRecord {
            token_hash,
            user_id,
            csrf_token: csrf,
            expires_at,
            ip: ip.map(|s| s.to_string()),
            user_agent: user_agent.map(|s| s.to_string()),
            plaintext_token: Some(token),
        })
    }

    pub(crate) async fn lookup_session_by_token_hash(
        &self,
        token_hash: &crate::auth::session::SessionTokenHash,
    ) -> Result<Option<(SessionRecord, AuthUserRecord)>, AuthError> {
        let row = sqlx::query(
            "SELECT s.token_hash AS s_token_hash, s.user_id::text AS s_user_id, s.csrf_token, \
                    s.expires_at::text AS s_expires_at, s.ip AS s_ip, s.user_agent AS s_user_agent, \
                    u.id::text AS u_id, u.email AS u_email, u.display_name AS u_display_name, \
                    u.provider AS u_provider, u.created_at::text AS u_created_at, \
                    u.password_hash AS u_password_hash \
             FROM sessions s JOIN users u ON u.id = s.user_id \
             WHERE s.token_hash = $1 AND u.disabled_at IS NULL",
        )
        .bind(&token_hash[..])
        .fetch_optional(self.pool())
        .await?;
        let Some(r) = row else { return Ok(None) };
        let expires_at = parse_rfc3339(&r.try_get::<String, _>("s_expires_at")?);
        if expires_at <= Utc::now() {
            return Ok(None);
        }
        let mut token_hash_row = [0u8; crate::auth::session::SESSION_HASH_BYTES];
        {
            let bytes: Vec<u8> = r.try_get("s_token_hash")?;
            if bytes.len() != token_hash_row.len() {
                return Ok(None);
            }
            token_hash_row.copy_from_slice(&bytes);
        }
        let user_id =
            Uuid::parse_str(&r.try_get::<String, _>("s_user_id")?).expect("DB UUID must parse");
        let session = SessionRecord {
            token_hash: token_hash_row,
            user_id,
            csrf_token: r.try_get("csrf_token")?,
            expires_at,
            ip: r.try_get("s_ip")?,
            user_agent: r.try_get("s_user_agent")?,
            plaintext_token: None,
        };
        let user = AuthUserRecord {
            id: user_id,
            email: r.try_get("u_email")?,
            display_name: r.try_get("u_display_name")?,
            provider: r.try_get("u_provider")?,
            created_at: parse_rfc3339(&r.try_get::<String, _>("u_created_at")?),
            password_hash: r.try_get("u_password_hash")?,
        };
        Ok(Some((session, user)))
    }

    pub(crate) async fn touch_session(
        &self,
        token_hash: &crate::auth::session::SessionTokenHash,
    ) -> Result<(), AuthError> {
        sqlx::query("UPDATE sessions SET last_seen_at = $1 WHERE token_hash = $2")
            .bind(Utc::now().to_rfc3339())
            .bind(&token_hash[..])
            .execute(self.pool())
            .await?;
        Ok(())
    }

    pub(crate) async fn delete_session(
        &self,
        token_hash: &crate::auth::session::SessionTokenHash,
    ) -> Result<u64, AuthError> {
        let res = sqlx::query("DELETE FROM sessions WHERE token_hash = $1")
            .bind(&token_hash[..])
            .execute(self.pool())
            .await?;
        Ok(res.rows_affected())
    }

    pub(crate) async fn delete_sessions_for_user(&self, user_id: Uuid) -> Result<u64, AuthError> {
        let res = sqlx::query("DELETE FROM sessions WHERE user_id = $1")
            .bind(user_id)
            .execute(self.pool())
            .await?;
        Ok(res.rows_affected())
    }

    pub(crate) async fn insert_passkey(&self, record: NewPasskeyRecord) -> Result<Uuid, AuthError> {
        let res = sqlx::query(
            "INSERT INTO passkeys (id, user_id, credential_id, public_key, counter, transports, aaguid, created_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
        )
        .bind(record.id)
        .bind(record.user_id)
        .bind(&record.credential_id)
        .bind(&record.public_key)
        .bind(record.counter as i64)
        .bind(&record.transports)
        .bind(record.aaguid.as_deref())
        .bind(Utc::now().to_rfc3339())
        .execute(self.pool())
        .await;
        match res {
            Ok(_) => Ok(record.id),
            Err(sqlx::Error::Database(db_err)) if db_err.is_unique_violation() => Err(
                AuthError::Conflict("credential_id already registered".into()),
            ),
            Err(e) => Err(AuthError::Database(e)),
        }
    }

    pub(crate) async fn get_passkey_by_credential_id(
        &self,
        credential_id: &[u8],
    ) -> Result<Option<PasskeyRecord>, AuthError> {
        let row = sqlx::query(
            "SELECT id::text AS id, user_id::text AS user_id, credential_id, public_key, \
                    counter, transports \
             FROM passkeys WHERE credential_id = $1",
        )
        .bind(credential_id)
        .fetch_optional(self.pool())
        .await?;
        let Some(r) = row else { return Ok(None) };
        Ok(Some(PasskeyRecord {
            id: Uuid::parse_str(&r.try_get::<String, _>("id")?).expect("DB UUID must parse"),
            user_id: Uuid::parse_str(&r.try_get::<String, _>("user_id")?)
                .expect("DB UUID must parse"),
            credential_id: r.try_get("credential_id")?,
            public_key: r.try_get("public_key")?,
            counter: r.try_get::<i64, _>("counter")? as u32,
            transports: r.try_get("transports")?,
        }))
    }

    pub(crate) async fn bump_passkey_counter(
        &self,
        passkey_id: Uuid,
        new_counter: u32,
    ) -> Result<(), AuthError> {
        sqlx::query("UPDATE passkeys SET counter = $1, last_used_at = $2 WHERE id = $3")
            .bind(new_counter as i64)
            .bind(Utc::now().to_rfc3339())
            .bind(passkey_id)
            .execute(self.pool())
            .await?;
        Ok(())
    }

    pub(crate) async fn record_event(&self, event: NewAuthEvent) {
        // UUIDv4 — matches the `id TEXT PRIMARY KEY` pattern every
        // other table in the schema uses. The DB enforces uniqueness
        // via the PRIMARY KEY constraint; the value is generated in
        // Rust so it is portable across sqlite (no built-in UUID
        // function) and postgres. Pre-`0002_auth_events_uuid.sql`
        // this column was a BIGINT fed by a process-local counter
        // that reset to 1 on every server restart and collided with
        // rows from the previous run.
        let id = Uuid::new_v4();
        let res = sqlx::query(
            "INSERT INTO auth_events (id, user_id, kind, provider, ip, user_agent, target_service, occurred_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
        )
        .bind(id.to_string())
        .bind(event.user_id)
        .bind(&event.kind)
        .bind(&event.provider)
        .bind(event.ip.as_deref())
        .bind(event.user_agent.as_deref())
        .bind(event.target_service.as_deref())
        .bind(Utc::now().to_rfc3339())
        .execute(self.pool())
        .await;
        if let Err(e) = res {
            tracing::warn!(error = %e, "failed to write auth_events row");
        }
    }

    pub(crate) async fn upsert_user_credentials(
        &self,
        user_id: Uuid,
        service_id: &str,
        fields: &[(String, Vec<u8>, Vec<u8>)],
    ) -> Result<(), AuthError> {
        let mut tx = self.pool.begin().await?;
        // Wipe first so a partial PUT cannot leave a service half
        // configured (the `UNIQUE (user_id, service_id, field_key)`
        // would otherwise reject a duplicate insert with a 409 we
        // would have to map manually).
        sqlx::query("DELETE FROM user_credentials WHERE user_id = $1 AND service_id = $2")
            .bind(user_id)
            .bind(service_id)
            .execute(&mut *tx)
            .await?;
        for (field_key, nonce, ciphertext) in fields {
            if nonce.len() != 12 {
                return Err(AuthError::BadRequest(format!(
                    "nonce for field {field_key:?} must be 12 bytes, got {}",
                    nonce.len()
                )));
            }
            let id = Uuid::new_v4().to_string();
            sqlx::query(
                "INSERT INTO user_credentials \
                 (id, user_id, service_id, field_key, nonce, ciphertext, created_at, updated_at) \
                 VALUES ($1, $2, $3, $4, $5, $6, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP)",
            )
            .bind(&id)
            .bind(user_id)
            .bind(service_id)
            .bind(field_key)
            .bind(nonce)
            .bind(ciphertext)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub(crate) async fn delete_service_credentials(
        &self,
        user_id: Uuid,
        service_id: &str,
    ) -> Result<u64, AuthError> {
        let res =
            sqlx::query("DELETE FROM user_credentials WHERE user_id = $1 AND service_id = $2")
                .bind(user_id)
                .bind(service_id)
                .execute(self.pool())
                .await?;
        Ok(res.rows_affected())
    }

    pub(crate) async fn list_configured_field_keys(
        &self,
        user_id: Uuid,
        service_id: &str,
    ) -> Result<Vec<String>, AuthError> {
        let rows = sqlx::query(
            "SELECT field_key FROM user_credentials \
             WHERE user_id = $1 AND service_id = $2 ORDER BY field_key",
        )
        .bind(user_id)
        .bind(service_id)
        .fetch_all(self.pool())
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| r.try_get::<String, _>("field_key"))
            .collect::<Result<Vec<_>, _>>()?)
    }

    pub(crate) async fn fetch_user_credential(
        &self,
        user_id: Uuid,
        service_id: &str,
        field_key: &str,
    ) -> Result<Option<UserCredentialRow>, AuthError> {
        let row = sqlx::query(
            "SELECT nonce, ciphertext FROM user_credentials \
             WHERE user_id = $1 AND service_id = $2 AND field_key = $3",
        )
        .bind(user_id)
        .bind(service_id)
        .bind(field_key)
        .fetch_optional(self.pool())
        .await?;
        let Some(r) = row else { return Ok(None) };
        Ok(Some(UserCredentialRow {
            nonce: r.try_get("nonce")?,
            ciphertext: r.try_get("ciphertext")?,
        }))
    }
}

fn row_to_user(row: sqlx::postgres::PgRow) -> AuthUserRecord {
    AuthUserRecord {
        id: Uuid::parse_str(&row.get::<String, _>("id")).expect("DB UUID must parse"),
        email: row.get("email"),
        display_name: row.get("display_name"),
        provider: row.get("provider"),
        created_at: parse_rfc3339(&row.get::<String, _>("created_at")),
        password_hash: row.get("password_hash"),
    }
}

fn parse_rfc3339(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now())
}
