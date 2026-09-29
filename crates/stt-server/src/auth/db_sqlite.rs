//! SQLite-backed implementation of [`crate::auth::store::AuthStore`].
//!
//! Uses `sqlx::SqlitePool`. Foreign-key enforcement is enabled via
//! `PRAGMA foreign_keys = ON` on every new connection (sqlite
//! ships with FK support off by default — without this the
//! `ON DELETE CASCADE` clauses on `passkeys` / `sessions` would be
//! silently ignored and the auth DB would leak rows).
//!
//! The migrations live in `crates/stt-server/migrations/` and are
//! applied at boot via `sqlx::migrate!`. The same files are used
//! for postgres — see [`crate::auth::db_postgres`] for the
//! postgres-specific code path.
//!
//! All SQL is written as raw strings rather than the
//! `sqlx::query!` compile-time-checked macros so the binary does
//! not need `DATABASE_URL` set at build time (plan D5: "we use
//! runtime"). The trade-off is that typos in column names surface
//! at first call rather than at `cargo build`; integration tests
//! cover every query path.

use chrono::{DateTime, Utc};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{Row, SqlitePool};
use std::str::FromStr;
use std::time::Duration;
use uuid::Uuid;

use crate::auth::error::AuthError;
use crate::auth::session::SessionRecord;
use crate::auth::store::{
    AuthUserRecord, NewAuthEvent, NewPasskeyRecord, PasskeyRecord, UserCredentialRow,
};
use crate::config::AuthConfig;

static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

/// Cheap to clone — the underlying pool is `Arc`-backed.
#[derive(Clone, Debug)]
pub struct SqliteStore {
    pool: SqlitePool,
}

impl SqliteStore {
    pub(crate) async fn connect(cfg: &AuthConfig) -> Result<Self, AuthError> {
        let opts = SqliteConnectOptions::from_str(&cfg.db.url)
            .map_err(|e| AuthError::Internal(format!("invalid sqlite URL: {e}")))?
            .create_if_missing(true)
            // The DB lives on disk by default; explicitly disable
            // the in-memory mode so an operator who forgets the
            // `memory:` prefix does not silently lose all users on
            // every restart.
            .foreign_keys(true)
            // WAL gives us concurrent readers + a single writer
            // (the migration step + the occasional login) without
            // the "database is locked" errors that the default
            // journal mode triggers under load.
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
            .synchronous(sqlx::sqlite::SqliteSynchronous::Normal)
            .busy_timeout(Duration::from_secs(5));
        let pool = SqlitePoolOptions::new()
            .max_connections(cfg.db.max_connections.max(1))
            .connect_with(opts)
            .await?;
        Ok(Self { pool })
    }

    pub(crate) async fn migrate(&self) -> Result<(), AuthError> {
        MIGRATOR
            .run(&self.pool)
            .await
            .map_err(|e| AuthError::Internal(format!("sqlite migrations failed: {e}")))
    }

    pub(crate) fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    /// Public pool handle for `AuthStore::pool()`. Returns the raw
    /// `SqlitePool` for one-off queries (OIDC state persistence).
    pub fn pool_handle(&self) -> SqlitePool {
        self.pool.clone()
    }

    pub(crate) async fn get_user_by_id(
        &self,
        id: Uuid,
    ) -> Result<Option<AuthUserRecord>, AuthError> {
        let row = sqlx::query(
            "SELECT id, email, display_name, provider, password_hash, created_at \
             FROM users WHERE id = ? AND disabled_at IS NULL",
        )
        .bind(id.to_string())
        .fetch_optional(self.pool())
        .await?;
        Ok(row.map(row_to_user))
    }

    pub(crate) async fn get_user_by_email(
        &self,
        email: &str,
    ) -> Result<Option<AuthUserRecord>, AuthError> {
        // LOWER() comparison so `User@Example.com` and
        // `user@example.com` resolve to the same row. The DB
        // doesn't enforce case-insensitive uniqueness on the
        // `users.email` UNIQUE index — we normalise on write (see
        // `create_user`) so the column only ever contains
        // lower-case emails.
        let row = sqlx::query(
            "SELECT id, email, display_name, provider, password_hash, created_at \
             FROM users WHERE LOWER(email) = LOWER(?) AND disabled_at IS NULL",
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
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(id.to_string())
        .bind(&normalised)
        .bind(display_name)
        .bind(provider)
        .bind(password_hash)
        .execute(self.pool())
        .await;
        match res {
            Ok(_) => Ok(id),
            Err(sqlx::Error::Database(db_err)) if is_sqlite_unique_violation(&*db_err) => Err(
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
        let res = sqlx::query("UPDATE users SET password_hash = ? WHERE id = ?")
            .bind(password_hash)
            .bind(user_id.to_string())
            .execute(self.pool())
            .await?;
        Ok(res.rows_affected())
    }

    pub(crate) async fn delete_user(&self, user_id: Uuid) -> Result<u64, AuthError> {
        let res = sqlx::query("DELETE FROM users WHERE id = ?")
            .bind(user_id.to_string())
            .execute(self.pool())
            .await?;
        Ok(res.rows_affected())
    }

    pub(crate) async fn delete_user_by_email(
        &self,
        email: &str,
    ) -> Result<Option<Uuid>, AuthError> {
        let row = sqlx::query("SELECT id FROM users WHERE LOWER(email) = LOWER(?)")
            .bind(email)
            .fetch_optional(self.pool())
            .await?;
        let Some(r) = row else { return Ok(None) };
        let id: String = r.try_get("id")?;
        let id = Uuid::parse_str(&id).expect("DB UUID must parse");
        self.delete_user(id).await?;
        Ok(Some(id))
    }

    pub(crate) async fn count_users_by_provider(&self, provider: &str) -> Result<i64, AuthError> {
        let row = sqlx::query("SELECT COUNT(*) AS c FROM users WHERE provider = ?")
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
                "SELECT id, email, display_name, provider, password_hash, created_at \
                 FROM users WHERE provider LIKE ? ORDER BY created_at DESC",
            )
            .bind(format!("{prefix}%"))
            .fetch_all(self.pool())
            .await?
        } else {
            sqlx::query(
                "SELECT id, email, display_name, provider, password_hash, created_at \
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
        let id = crate::auth::session::new_session_id();
        let csrf = crate::auth::session::new_csrf_token();
        let now = Utc::now();
        let expires_at = now + chrono::Duration::from_std(ttl).unwrap_or(chrono::Duration::days(7));
        sqlx::query(
            "INSERT INTO sessions (id, user_id, csrf_token, created_at, expires_at, last_seen_at, ip, user_agent) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(id.to_string())
        .bind(user_id.to_string())
        .bind(&csrf)
        .bind(now.to_rfc3339())
        .bind(expires_at.to_rfc3339())
        .bind(now.to_rfc3339())
        .bind(ip)
        .bind(user_agent)
        .execute(self.pool())
        .await?;
        Ok(SessionRecord {
            id,
            user_id,
            csrf_token: csrf,
            expires_at,
            ip: ip.map(|s| s.to_string()),
            user_agent: user_agent.map(|s| s.to_string()),
        })
    }

    pub(crate) async fn lookup_session(
        &self,
        session_id: Uuid,
    ) -> Result<Option<(SessionRecord, AuthUserRecord)>, AuthError> {
        let row = sqlx::query(
            "SELECT s.id AS s_id, s.user_id AS s_user_id, s.csrf_token, \
                    s.expires_at AS s_expires_at, s.ip AS s_ip, s.user_agent AS s_user_agent, \
                    u.email AS u_email, u.display_name AS u_display_name, \
                    u.provider AS u_provider, u.created_at AS u_created_at, \
                    u.password_hash AS u_password_hash, u.id AS id \
             FROM sessions s JOIN users u ON u.id = s.user_id \
             WHERE s.id = ? AND u.disabled_at IS NULL",
        )
        .bind(session_id.to_string())
        .fetch_optional(self.pool())
        .await?;
        let Some(r) = row else { return Ok(None) };
        let expires_at_str: String = r.try_get("s_expires_at")?;
        let expires_at = parse_rfc3339(&expires_at_str);
        if expires_at <= Utc::now() {
            return Ok(None);
        }
        let session = SessionRecord {
            id: Uuid::parse_str(&r.try_get::<String, _>("s_id")?).expect("DB UUID must parse"),
            user_id: Uuid::parse_str(&r.try_get::<String, _>("s_user_id")?)
                .expect("DB UUID must parse"),
            csrf_token: r.try_get("csrf_token")?,
            expires_at,
            ip: r.try_get("s_ip")?,
            user_agent: r.try_get("s_user_agent")?,
        };
        // Inlined instead of `row_to_user(r)` because the join
        // SELECT aliases every user-side column (`u_email`,
        // `u_display_name`, etc.) so the helper's plain-name
        // lookups would not match.
        let user = AuthUserRecord {
            id: session.user_id,
            email: r.try_get("u_email")?,
            display_name: r.try_get("u_display_name")?,
            provider: r.try_get("u_provider")?,
            created_at: parse_rfc3339(&r.try_get::<String, _>("u_created_at")?),
            password_hash: r.try_get("u_password_hash")?,
        };
        Ok(Some((session, user)))
    }

    pub(crate) async fn touch_session(&self, session_id: Uuid) -> Result<(), AuthError> {
        sqlx::query("UPDATE sessions SET last_seen_at = ? WHERE id = ?")
            .bind(Utc::now().to_rfc3339())
            .bind(session_id.to_string())
            .execute(self.pool())
            .await?;
        Ok(())
    }

    pub(crate) async fn delete_session(&self, session_id: Uuid) -> Result<u64, AuthError> {
        let res = sqlx::query("DELETE FROM sessions WHERE id = ?")
            .bind(session_id.to_string())
            .execute(self.pool())
            .await?;
        Ok(res.rows_affected())
    }

    pub(crate) async fn delete_sessions_for_user(&self, user_id: Uuid) -> Result<u64, AuthError> {
        let res = sqlx::query("DELETE FROM sessions WHERE user_id = ?")
            .bind(user_id.to_string())
            .execute(self.pool())
            .await?;
        Ok(res.rows_affected())
    }

    pub(crate) async fn insert_passkey(&self, record: NewPasskeyRecord) -> Result<Uuid, AuthError> {
        let res = sqlx::query(
            "INSERT INTO passkeys (id, user_id, credential_id, public_key, counter, transports, aaguid, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(record.id.to_string())
        .bind(record.user_id.to_string())
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
            Err(sqlx::Error::Database(db_err)) if is_sqlite_unique_violation(&*db_err) => Err(
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
            "SELECT id, user_id, credential_id, public_key, counter, transports \
             FROM passkeys WHERE credential_id = ?",
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
        sqlx::query("UPDATE passkeys SET counter = ?, last_used_at = ? WHERE id = ?")
            .bind(new_counter as i64)
            .bind(Utc::now().to_rfc3339())
            .bind(passkey_id.to_string())
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
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(id.to_string())
        .bind(event.user_id.map(|u| u.to_string()))
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
        let user_id_str = user_id.to_string();
        let mut tx = self.pool.begin().await?;
        // Wipe first so a partial PUT cannot leave a service half
        // configured (the `UNIQUE (user_id, service_id, field_key)`
        // would otherwise reject a duplicate insert with a 409 we
        // would have to map manually).
        sqlx::query("DELETE FROM user_credentials WHERE user_id = ? AND service_id = ?")
            .bind(&user_id_str)
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
                 VALUES (?, ?, ?, ?, ?, ?, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP)",
            )
            .bind(&id)
            .bind(&user_id_str)
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
        let res = sqlx::query("DELETE FROM user_credentials WHERE user_id = ? AND service_id = ?")
            .bind(user_id.to_string())
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
             WHERE user_id = ? AND service_id = ? ORDER BY field_key",
        )
        .bind(user_id.to_string())
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
             WHERE user_id = ? AND service_id = ? AND field_key = ?",
        )
        .bind(user_id.to_string())
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

fn row_to_user(row: sqlx::sqlite::SqliteRow) -> AuthUserRecord {
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

fn is_sqlite_unique_violation(db_err: &dyn sqlx::error::DatabaseError) -> bool {
    // SQLite uses `SQLITE_CONSTRAINT_UNIQUE` (code 2067) and
    // `SQLITE_CONSTRAINT_PRIMARYKEY` (code 1555). Both translate
    // to a unique-constraint violation from the application's
    // point of view.
    db_err.code().as_deref() == Some("2067") || db_err.code().as_deref() == Some("1555")
}
