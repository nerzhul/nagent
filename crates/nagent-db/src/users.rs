//! Users repository — per-domain repository.
//!
//! Owns the SQL for the `users` table that used to live in
//! [`crate::auth::db_sqlite`] / [`crate::auth::db_postgres`]. The
//! legacy `AuthStore::get_user_by_id` / `…_by_email` / `create_user`
//! / etc. now delegate here so the auth layer stops being a "god
//! repository".
//!
//! The remaining methods (sessions, passkeys, audit, credentials,
//! preferences) stay on `AuthStore` for now — they will be moved in
//! follow-up commits to keep each commit reviewable. The
//! dispatch-via-enum pattern here is the template the others will
//! follow.

use uuid::Uuid;

use crate::error::Error;
use crate::pool::AnyPool;
use crate::types::AuthUserRecord;

/// Backend-agnostic users repository. Dispatch matches the
/// pattern used by the legacy [`crate::auth::store::AuthStore`].
#[derive(Debug, Clone)]
pub enum Users {
    Sqlite(sqlite::SqliteUsers),
    Postgres(postgres::PgUsers),
}

impl Users {
    /// Bind the repository to a shared pool.
    pub fn new(pool: &AnyPool) -> Self {
        match pool {
            AnyPool::Sqlite(p) => Self::Sqlite(sqlite::SqliteUsers::new(p.clone())),
            AnyPool::Postgres(p) => Self::Postgres(postgres::PgUsers::new(p.clone())),
        }
    }

    /// Look up a user by id. Returns `None` if the user does not
    /// exist or has been soft-disabled.
    pub async fn get_by_id(&self, id: Uuid) -> Result<Option<AuthUserRecord>, Error> {
        match self {
            Users::Sqlite(s) => s.get_by_id(id).await,
            Users::Postgres(s) => s.get_by_id(id).await,
        }
    }

    /// Look up a user by email (case-insensitive on both engines
    /// via the LOWER() comparison). Returns the active user only;
    /// soft-disabled users are excluded.
    pub async fn get_by_email(&self, email: &str) -> Result<Option<AuthUserRecord>, Error> {
        match self {
            Users::Sqlite(s) => s.get_by_email(email).await,
            Users::Postgres(s) => s.get_by_email(email).await,
        }
    }

    /// Create a new user. `password_hash` is `Some(bytes)` for the
    /// local backend and `None` for OIDC / passkey-only users.
    /// Fails with [`Error::Conflict`] if the email already
    /// exists.
    pub async fn create(
        &self,
        email: &str,
        display_name: &str,
        provider: &str,
        password_hash: Option<&[u8]>,
    ) -> Result<Uuid, Error> {
        match self {
            Users::Sqlite(s) => s.create(email, display_name, provider, password_hash).await,
            Users::Postgres(s) => s.create(email, display_name, provider, password_hash).await,
        }
    }

    /// Update the local password hash for an existing user.
    /// Returns the updated row count.
    pub async fn set_password(&self, user_id: Uuid, password_hash: &[u8]) -> Result<u64, Error> {
        match self {
            Users::Sqlite(s) => s.set_password(user_id, password_hash).await,
            Users::Postgres(s) => s.set_password(user_id, password_hash).await,
        }
    }

    /// Delete a user by id. Cascades to their sessions and passkeys
    /// via the FK constraints in the migration. Returns the number
    /// of rows removed (0 means the user did not exist).
    pub async fn delete(&self, user_id: Uuid) -> Result<u64, Error> {
        match self {
            Users::Sqlite(s) => s.delete(user_id).await,
            Users::Postgres(s) => s.delete(user_id).await,
        }
    }

    /// Delete a user by email. Returns the deleted user id if any.
    pub async fn delete_by_email(&self, email: &str) -> Result<Option<Uuid>, Error> {
        match self {
            Users::Sqlite(s) => s.delete_by_email(email).await,
            Users::Postgres(s) => s.delete_by_email(email).await,
        }
    }

    /// Count users by provider (e.g. `"local"`, `"oidc:<issuer>"`).
    /// Used by the `auth delete-user` CLI to refuse removing the
    /// last local user when auth is enabled.
    pub async fn count_by_provider(&self, provider: &str) -> Result<i64, Error> {
        match self {
            Users::Sqlite(s) => s.count_by_provider(provider).await,
            Users::Postgres(s) => s.count_by_provider(provider).await,
        }
    }

    /// List all users, optionally filtered by `provider` prefix.
    /// Used by `auth list-users`.
    pub async fn list(&self, provider_prefix: Option<&str>) -> Result<Vec<AuthUserRecord>, Error> {
        match self {
            Users::Sqlite(s) => s.list(provider_prefix).await,
            Users::Postgres(s) => s.list(provider_prefix).await,
        }
    }
}

// ---- Per-engine implementations -----------------------------------------

pub(crate) mod sqlite {
    //! SQLite implementation of [`super::Users`]. Owns the SQL
    //! previously embedded in
    //! [`crate::auth::db_sqlite`].

    use chrono::{DateTime, Utc};
    use sqlx::{Row, SqlitePool};
    use uuid::Uuid;

    use crate::error::Error;
    use crate::types::AuthUserRecord;

    /// Cheap to clone — the underlying pool is `Arc`-backed.
    #[derive(Clone, Debug)]
    pub struct SqliteUsers {
        pub pool: SqlitePool,
    }

    impl SqliteUsers {
        pub(crate) fn new(pool: SqlitePool) -> Self {
            Self { pool }
        }

        pub async fn get_by_id(&self, id: Uuid) -> Result<Option<AuthUserRecord>, Error> {
            let row = sqlx::query(
                "SELECT id, email, display_name, provider, password_hash, created_at \
                 FROM users WHERE id = ? AND disabled_at IS NULL",
            )
            .bind(id.to_string())
            .fetch_optional(&self.pool)
            .await?;
            Ok(row.map(row_to_user))
        }

        pub async fn get_by_email(&self, email: &str) -> Result<Option<AuthUserRecord>, Error> {
            let row = sqlx::query(
                "SELECT id, email, display_name, provider, password_hash, created_at \
                 FROM users WHERE LOWER(email) = LOWER(?) AND disabled_at IS NULL",
            )
            .bind(email)
            .fetch_optional(&self.pool)
            .await?;
            Ok(row.map(row_to_user))
        }

        pub async fn create(
            &self,
            email: &str,
            display_name: &str,
            provider: &str,
            password_hash: Option<&[u8]>,
        ) -> Result<Uuid, Error> {
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
            .execute(&self.pool)
            .await;
            match res {
                Ok(_) => Ok(id),
                Err(sqlx::Error::Database(db_err)) if is_unique_violation(&*db_err) => Err(
                    Error::Conflict(format!("user with email {normalised:?} already exists")),
                ),
                Err(e) => Err(Error::Database(e)),
            }
        }

        pub async fn set_password(
            &self,
            user_id: Uuid,
            password_hash: &[u8],
        ) -> Result<u64, Error> {
            let res = sqlx::query("UPDATE users SET password_hash = ? WHERE id = ?")
                .bind(password_hash)
                .bind(user_id.to_string())
                .execute(&self.pool)
                .await?;
            Ok(res.rows_affected())
        }

        pub async fn delete(&self, user_id: Uuid) -> Result<u64, Error> {
            let res = sqlx::query("DELETE FROM users WHERE id = ?")
                .bind(user_id.to_string())
                .execute(&self.pool)
                .await?;
            Ok(res.rows_affected())
        }

        pub async fn delete_by_email(&self, email: &str) -> Result<Option<Uuid>, Error> {
            let row = sqlx::query("SELECT id FROM users WHERE LOWER(email) = LOWER(?)")
                .bind(email)
                .fetch_optional(&self.pool)
                .await?;
            let Some(r) = row else { return Ok(None) };
            let id: String = r.try_get("id")?;
            let id = Uuid::parse_str(&id).expect("DB UUID must parse");
            self.delete(id).await?;
            Ok(Some(id))
        }

        pub async fn count_by_provider(&self, provider: &str) -> Result<i64, Error> {
            let row = sqlx::query("SELECT COUNT(*) AS c FROM users WHERE provider = ?")
                .bind(provider)
                .fetch_one(&self.pool)
                .await?;
            let c: i64 = row.try_get("c")?;
            Ok(c)
        }

        pub async fn list(
            &self,
            provider_prefix: Option<&str>,
        ) -> Result<Vec<AuthUserRecord>, Error> {
            let rows = if let Some(prefix) = provider_prefix {
                sqlx::query(
                    "SELECT id, email, display_name, provider, password_hash, created_at \
                     FROM users WHERE provider LIKE ? ORDER BY created_at DESC",
                )
                .bind(format!("{prefix}%"))
                .fetch_all(&self.pool)
                .await?
            } else {
                sqlx::query(
                    "SELECT id, email, display_name, provider, password_hash, created_at \
                     FROM users ORDER BY created_at DESC",
                )
                .fetch_all(&self.pool)
                .await?
            };
            Ok(rows.into_iter().map(row_to_user).collect())
        }
    }

    fn row_to_user(r: sqlx::sqlite::SqliteRow) -> AuthUserRecord {
        let created_at_str: String = r.try_get("created_at").expect("created_at");
        let created_at = parse_rfc3339(&created_at_str);
        AuthUserRecord {
            id: Uuid::parse_str(&r.try_get::<String, _>("id").expect("id"))
                .expect("DB UUID must parse"),
            email: r.try_get("email").expect("email"),
            display_name: r.try_get("display_name").expect("display_name"),
            provider: r.try_get("provider").expect("provider"),
            created_at,
            password_hash: r.try_get("password_hash").expect("password_hash"),
        }
    }

    fn parse_rfc3339(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s)
            .map(|d| d.with_timezone(&Utc))
            .unwrap_or_else(|_| Utc::now())
    }

    fn is_unique_violation(db_err: &dyn sqlx::error::DatabaseError) -> bool {
        // SQLite uses SQLSTATE 23000 for integrity constraint
        // violations (which includes UNIQUE). The message also
        // contains "UNIQUE constraint failed" — both signals are
        // portable across recent sqlx versions.
        db_err.code().as_deref() == Some("23000")
            || db_err.message().to_ascii_uppercase().contains("UNIQUE")
    }
}

pub(crate) mod postgres {
    //! Postgres implementation of [`super::Users`]. Owns the SQL
    //! previously embedded in [`crate::auth::db_postgres`].

    use chrono::{DateTime, Utc};
    use sqlx::{PgPool, Row};
    use uuid::Uuid;

    use crate::error::Error;
    use crate::types::AuthUserRecord;

    /// Cheap to clone — the underlying pool is `Arc`-backed.
    #[derive(Clone, Debug)]
    pub struct PgUsers {
        pub pool: PgPool,
    }

    impl PgUsers {
        pub(crate) fn new(pool: PgPool) -> Self {
            Self { pool }
        }

        pub async fn get_by_id(&self, id: Uuid) -> Result<Option<AuthUserRecord>, Error> {
            let row = sqlx::query(
                "SELECT id::text AS id, email, display_name, provider, password_hash, \
                        created_at::text AS created_at \
                 FROM users WHERE id = $1 AND disabled_at IS NULL",
            )
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
            Ok(row.map(row_to_user))
        }

        pub async fn get_by_email(&self, email: &str) -> Result<Option<AuthUserRecord>, Error> {
            let row = sqlx::query(
                "SELECT id::text AS id, email, display_name, provider, password_hash, \
                        created_at::text AS created_at \
                 FROM users WHERE LOWER(email) = LOWER($1) AND disabled_at IS NULL",
            )
            .bind(email)
            .fetch_optional(&self.pool)
            .await?;
            Ok(row.map(row_to_user))
        }

        pub async fn create(
            &self,
            email: &str,
            display_name: &str,
            provider: &str,
            password_hash: Option<&[u8]>,
        ) -> Result<Uuid, Error> {
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
            .execute(&self.pool)
            .await;
            match res {
                Ok(_) => Ok(id),
                Err(sqlx::Error::Database(db_err)) if db_err.is_unique_violation() => Err(
                    Error::Conflict(format!("user with email {normalised:?} already exists")),
                ),
                Err(e) => Err(Error::Database(e)),
            }
        }

        pub async fn set_password(
            &self,
            user_id: Uuid,
            password_hash: &[u8],
        ) -> Result<u64, Error> {
            let res = sqlx::query("UPDATE users SET password_hash = $1 WHERE id = $2")
                .bind(password_hash)
                .bind(user_id)
                .execute(&self.pool)
                .await?;
            Ok(res.rows_affected())
        }

        pub async fn delete(&self, user_id: Uuid) -> Result<u64, Error> {
            let res = sqlx::query("DELETE FROM users WHERE id = $1")
                .bind(user_id)
                .execute(&self.pool)
                .await?;
            Ok(res.rows_affected())
        }

        pub async fn delete_by_email(&self, email: &str) -> Result<Option<Uuid>, Error> {
            let row =
                sqlx::query("SELECT id::text AS id FROM users WHERE LOWER(email) = LOWER($1)")
                    .bind(email)
                    .fetch_optional(&self.pool)
                    .await?;
            let Some(r) = row else { return Ok(None) };
            let id_str: String = r.try_get("id")?;
            let id = Uuid::parse_str(&id_str).expect("DB UUID must parse");
            self.delete(id).await?;
            Ok(Some(id))
        }

        pub async fn count_by_provider(&self, provider: &str) -> Result<i64, Error> {
            let row = sqlx::query("SELECT COUNT(*) AS c FROM users WHERE provider = $1")
                .bind(provider)
                .fetch_one(&self.pool)
                .await?;
            let c: i64 = row.try_get("c")?;
            Ok(c)
        }

        pub async fn list(
            &self,
            provider_prefix: Option<&str>,
        ) -> Result<Vec<AuthUserRecord>, Error> {
            let rows = if let Some(prefix) = provider_prefix {
                sqlx::query(
                    "SELECT id::text AS id, email, display_name, provider, password_hash, \
                            created_at::text AS created_at \
                     FROM users WHERE provider LIKE $1 ORDER BY created_at DESC",
                )
                .bind(format!("{prefix}%"))
                .fetch_all(&self.pool)
                .await?
            } else {
                sqlx::query(
                    "SELECT id::text AS id, email, display_name, provider, password_hash, \
                            created_at::text AS created_at \
                     FROM users ORDER BY created_at DESC",
                )
                .fetch_all(&self.pool)
                .await?
            };
            Ok(rows.into_iter().map(row_to_user).collect())
        }
    }

    fn row_to_user(r: sqlx::postgres::PgRow) -> AuthUserRecord {
        let created_at_str: String = r.try_get("created_at").expect("created_at");
        let created_at = parse_rfc3339(&created_at_str);
        AuthUserRecord {
            id: Uuid::parse_str(&r.try_get::<String, _>("id").expect("id"))
                .expect("DB UUID must parse"),
            email: r.try_get("email").expect("email"),
            display_name: r.try_get("display_name").expect("display_name"),
            provider: r.try_get("provider").expect("provider"),
            created_at,
            password_hash: r.try_get("password_hash").expect("password_hash"),
        }
    }

    fn parse_rfc3339(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s)
            .map(|d| d.with_timezone(&Utc))
            .unwrap_or_else(|_| Utc::now())
    }
}

// ---- Tests ---------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dispatch_enum_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Users>();
        assert_send_sync::<sqlite::SqliteUsers>();
        assert_send_sync::<postgres::PgUsers>();
    }
}
