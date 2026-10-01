//! Sessions repository.
//!
//! Owns the SQL for the `sessions` table that previously lived in
//! [`crate::auth::db_sqlite`] / [`crate::auth::db_postgres`].
//! [`crate::auth::store::AuthStore::create_session`] and the related
//! lookups now delegate here.

use std::time::Duration;

use uuid::Uuid;

use crate::error::Error;
use crate::pool::AnyPool;
use crate::types::AuthUserRecord;
use crate::types::SessionRecord;

#[derive(Debug, Clone)]
pub enum Sessions {
    Sqlite(sqlite::SqliteSessions),
    Postgres(postgres::PgSessions),
}

impl Sessions {
    pub fn new(pool: &AnyPool) -> Self {
        match pool {
            AnyPool::Sqlite(p) => Self::Sqlite(sqlite::SqliteSessions::new(p.clone())),
            AnyPool::Postgres(p) => Self::Postgres(postgres::PgSessions::new(p.clone())),
        }
    }

    /// Scope per-user operations to a single `user_id`. The
    /// returned [`ScopedSessions`] does not take a `user_id`
    /// argument on its per-row methods so a handler holding a
    /// scoped view cannot accidentally delete another user's
    /// sessions (plan 4.A, plan S4).
    ///
    /// Operations that are inherently keyed by `token_hash` (the
    /// auth-ceremony lookups: `lookup_by_token_hash`, `touch`,
    /// `delete`) stay on the unscoped repository because they
    /// identify the session by its opaque cookie hash, not by
    /// `user_id`.
    pub fn for_user(&self, user_id: Uuid) -> ScopedSessions {
        ScopedSessions {
            inner: self.clone(),
            user_id,
        }
    }

    pub async fn create(
        &self,
        user_id: Uuid,
        ttl: Duration,
        ip: Option<&str>,
        user_agent: Option<&str>,
    ) -> Result<SessionRecord, Error> {
        match self {
            Sessions::Sqlite(s) => s.create(user_id, ttl, ip, user_agent).await,
            Sessions::Postgres(s) => s.create(user_id, ttl, ip, user_agent).await,
        }
    }

    pub async fn lookup_by_token_hash(
        &self,
        token_hash: &crate::types::SessionTokenHash,
    ) -> Result<Option<(SessionRecord, AuthUserRecord)>, Error> {
        match self {
            Sessions::Sqlite(s) => s.lookup_by_token_hash(token_hash).await,
            Sessions::Postgres(s) => s.lookup_by_token_hash(token_hash).await,
        }
    }

    pub async fn touch(&self, token_hash: &crate::types::SessionTokenHash) -> Result<(), Error> {
        match self {
            Sessions::Sqlite(s) => s.touch(token_hash).await,
            Sessions::Postgres(s) => s.touch(token_hash).await,
        }
    }

    pub async fn delete(&self, token_hash: &crate::types::SessionTokenHash) -> Result<u64, Error> {
        match self {
            Sessions::Sqlite(s) => s.delete(token_hash).await,
            Sessions::Postgres(s) => s.delete(token_hash).await,
        }
    }

    pub async fn delete_for_user(&self, user_id: Uuid) -> Result<u64, Error> {
        match self {
            Sessions::Sqlite(s) => s.delete_for_user(user_id).await,
            Sessions::Postgres(s) => s.delete_for_user(user_id).await,
        }
    }

    /// Number of sessions currently held by `user_id`. Used by
    /// the scoped view and by tests that assert on session
    /// lifecycle.
    pub async fn count_for_user(&self, user_id: Uuid) -> Result<u64, Error> {
        match self {
            Sessions::Sqlite(s) => s.count_for_user(user_id).await,
            Sessions::Postgres(s) => s.count_for_user(user_id).await,
        }
    }
}

/// Per-user scoped view over [`Sessions`].
///
/// Only the operations whose SQL has a `user_id` filter move to
/// the scoped view (`delete_all`, `count`). The auth-ceremony
/// operations (`lookup_by_token_hash`, `touch`, `delete`) stay on
/// the unscoped repository because they are keyed by `token_hash`,
/// not `user_id`; the cookie hash IS the scoping mechanism.
#[derive(Debug, Clone)]
pub struct ScopedSessions {
    inner: Sessions,
    user_id: Uuid,
}

impl ScopedSessions {
    /// `user_id` this view is bound to.
    pub fn user_id(&self) -> Uuid {
        self.user_id
    }

    /// Delete every session for the scoped user. Returns the number
    /// of rows removed.
    pub async fn delete_all(&self) -> Result<u64, Error> {
        self.inner.delete_for_user(self.user_id).await
    }

    /// Count sessions currently held by the scoped user.
    pub async fn count(&self) -> Result<u64, Error> {
        self.inner.count_for_user(self.user_id).await
    }
}

pub(crate) mod sqlite {
    use std::time::Duration;

    use chrono::{DateTime, Utc};
    use sqlx::{Row, SqlitePool};
    use uuid::Uuid;

    use crate::error::Error;
    use crate::types::AuthUserRecord;
    use crate::types::{SessionRecord, SessionTokenHash};

    #[derive(Clone, Debug)]
    pub struct SqliteSessions {
        pub pool: SqlitePool,
    }

    impl SqliteSessions {
        pub(crate) fn new(pool: SqlitePool) -> Self {
            Self { pool }
        }

        pub async fn create(
            &self,
            user_id: Uuid,
            ttl: Duration,
            ip: Option<&str>,
            user_agent: Option<&str>,
        ) -> Result<SessionRecord, Error> {
            let (token, token_hash) = crate::types::new_session_token();
            let csrf = crate::types::new_csrf_token();
            let now = Utc::now();
            let expires_at =
                now + chrono::Duration::from_std(ttl).unwrap_or(chrono::Duration::days(7));
            sqlx::query(
                "INSERT INTO sessions (token_hash, user_id, csrf_token, created_at, expires_at, last_seen_at, ip, user_agent) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(&token_hash[..])
            .bind(user_id.to_string())
            .bind(&csrf)
            .bind(now.to_rfc3339())
            .bind(expires_at.to_rfc3339())
            .bind(now.to_rfc3339())
            .bind(ip)
            .bind(user_agent)
            .execute(&self.pool)
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

        pub async fn lookup_by_token_hash(
            &self,
            token_hash: &SessionTokenHash,
        ) -> Result<Option<(SessionRecord, AuthUserRecord)>, Error> {
            let row = sqlx::query(
                "SELECT s.token_hash AS s_token_hash, s.user_id AS s_user_id, s.csrf_token, \
                        s.expires_at AS s_expires_at, s.ip AS s_ip, s.user_agent AS s_user_agent, \
                        u.email AS u_email, u.display_name AS u_display_name, \
                        u.provider AS u_provider, u.created_at AS u_created_at, \
                        u.password_hash AS u_password_hash, u.id AS id \
                 FROM sessions s JOIN users u ON u.id = s.user_id \
                 WHERE s.token_hash = ? AND u.disabled_at IS NULL",
            )
            .bind(&token_hash[..])
            .fetch_optional(&self.pool)
            .await?;
            let Some(r) = row else { return Ok(None) };
            let expires_at_str: String = r.try_get("s_expires_at")?;
            let expires_at = parse_rfc3339(&expires_at_str);
            if expires_at <= Utc::now() {
                return Ok(None);
            }
            let mut token_hash_row = [0u8; crate::types::SESSION_HASH_BYTES];
            let bytes: Vec<u8> = r.try_get("s_token_hash")?;
            if bytes.len() != token_hash_row.len() {
                return Ok(None);
            }
            token_hash_row.copy_from_slice(&bytes);
            let session = SessionRecord {
                token_hash: token_hash_row,
                user_id: Uuid::parse_str(&r.try_get::<String, _>("s_user_id")?)
                    .expect("DB UUID must parse"),
                csrf_token: r.try_get("csrf_token")?,
                expires_at,
                ip: r.try_get("s_ip")?,
                user_agent: r.try_get("s_user_agent")?,
                plaintext_token: None,
            };
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

        pub async fn touch(&self, token_hash: &SessionTokenHash) -> Result<(), Error> {
            sqlx::query("UPDATE sessions SET last_seen_at = ? WHERE token_hash = ?")
                .bind(Utc::now().to_rfc3339())
                .bind(&token_hash[..])
                .execute(&self.pool)
                .await?;
            Ok(())
        }

        pub async fn delete(&self, token_hash: &SessionTokenHash) -> Result<u64, Error> {
            let res = sqlx::query("DELETE FROM sessions WHERE token_hash = ?")
                .bind(&token_hash[..])
                .execute(&self.pool)
                .await?;
            Ok(res.rows_affected())
        }

        pub async fn delete_for_user(&self, user_id: Uuid) -> Result<u64, Error> {
            let res = sqlx::query("DELETE FROM sessions WHERE user_id = ?")
                .bind(user_id.to_string())
                .execute(&self.pool)
                .await?;
            Ok(res.rows_affected())
        }

        pub async fn count_for_user(&self, user_id: Uuid) -> Result<u64, Error> {
            let row = sqlx::query("SELECT COUNT(*) AS n FROM sessions WHERE user_id = ?")
                .bind(user_id.to_string())
                .fetch_one(&self.pool)
                .await?;
            let n: i64 = row.try_get("n")?;
            Ok(n as u64)
        }
    }

    fn parse_rfc3339(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s)
            .map(|d| d.with_timezone(&Utc))
            .unwrap_or_else(|_| Utc::now())
    }
}

pub(crate) mod postgres {
    use std::time::Duration;

    use chrono::{DateTime, Utc};
    use sqlx::{PgPool, Row};
    use uuid::Uuid;

    use crate::error::Error;
    use crate::types::AuthUserRecord;
    use crate::types::{SessionRecord, SessionTokenHash};

    #[derive(Clone, Debug)]
    pub struct PgSessions {
        pub pool: PgPool,
    }

    impl PgSessions {
        pub(crate) fn new(pool: PgPool) -> Self {
            Self { pool }
        }

        pub async fn create(
            &self,
            user_id: Uuid,
            ttl: Duration,
            ip: Option<&str>,
            user_agent: Option<&str>,
        ) -> Result<SessionRecord, Error> {
            let (token, token_hash) = crate::types::new_session_token();
            let csrf = crate::types::new_csrf_token();
            let now = Utc::now();
            let expires_at =
                now + chrono::Duration::from_std(ttl).unwrap_or(chrono::Duration::days(7));
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
            .execute(&self.pool)
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

        pub async fn lookup_by_token_hash(
            &self,
            token_hash: &SessionTokenHash,
        ) -> Result<Option<(SessionRecord, AuthUserRecord)>, Error> {
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
            .fetch_optional(&self.pool)
            .await?;
            let Some(r) = row else { return Ok(None) };
            let expires_at = parse_rfc3339(&r.try_get::<String, _>("s_expires_at")?);
            if expires_at <= Utc::now() {
                return Ok(None);
            }
            let mut token_hash_row = [0u8; crate::types::SESSION_HASH_BYTES];
            let bytes: Vec<u8> = r.try_get("s_token_hash")?;
            if bytes.len() != token_hash_row.len() {
                return Ok(None);
            }
            token_hash_row.copy_from_slice(&bytes);
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

        pub async fn touch(&self, token_hash: &SessionTokenHash) -> Result<(), Error> {
            sqlx::query("UPDATE sessions SET last_seen_at = $1 WHERE token_hash = $2")
                .bind(Utc::now().to_rfc3339())
                .bind(&token_hash[..])
                .execute(&self.pool)
                .await?;
            Ok(())
        }

        pub async fn delete(&self, token_hash: &SessionTokenHash) -> Result<u64, Error> {
            let res = sqlx::query("DELETE FROM sessions WHERE token_hash = $1")
                .bind(&token_hash[..])
                .execute(&self.pool)
                .await?;
            Ok(res.rows_affected())
        }

        pub async fn delete_for_user(&self, user_id: Uuid) -> Result<u64, Error> {
            let res = sqlx::query("DELETE FROM sessions WHERE user_id = $1")
                .bind(user_id)
                .execute(&self.pool)
                .await?;
            Ok(res.rows_affected())
        }

        pub async fn count_for_user(&self, user_id: Uuid) -> Result<u64, Error> {
            let row = sqlx::query("SELECT COUNT(*)::bigint AS n FROM sessions WHERE user_id = $1")
                .bind(user_id)
                .fetch_one(&self.pool)
                .await?;
            let n: i64 = row.try_get("n")?;
            Ok(n as u64)
        }
    }

    fn parse_rfc3339(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s)
            .map(|d| d.with_timezone(&Utc))
            .unwrap_or_else(|_| Utc::now())
    }
}
