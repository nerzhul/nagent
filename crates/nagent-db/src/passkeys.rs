//! Passkeys repository.

use uuid::Uuid;

use crate::error::Error;
use crate::pool::AnyPool;
use crate::types::{NewPasskeyRecord, PasskeyRecord};

#[derive(Debug, Clone)]
pub enum Passkeys {
    Sqlite(sqlite::SqlitePasskeys),
    Postgres(postgres::PgPasskeys),
}

impl Passkeys {
    pub fn new(pool: &AnyPool) -> Self {
        match pool {
            AnyPool::Sqlite(p) => Self::Sqlite(sqlite::SqlitePasskeys::new(p.clone())),
            AnyPool::Postgres(p) => Self::Postgres(postgres::PgPasskeys::new(p.clone())),
        }
    }

    /// Scope per-user operations to a single `user_id`. The
    /// returned [`ScopedPasskeys`] does not take a `user_id`
    /// argument on its per-row methods so a handler holding a
    /// scoped view cannot accidentally delete / list another
    /// user's passkeys (plan 4.A, plan S4).
    ///
    /// Operations that are inherently keyed by `credential_id` or
    /// `passkey_id` (the auth ceremony lookups) stay on the
    /// unscoped repository.
    pub fn for_user(&self, user_id: Uuid) -> ScopedPasskeys {
        ScopedPasskeys {
            inner: self.clone(),
            user_id,
        }
    }

    pub async fn insert(&self, record: NewPasskeyRecord) -> Result<Uuid, Error> {
        match self {
            Passkeys::Sqlite(s) => s.insert(record).await,
            Passkeys::Postgres(s) => s.insert(record).await,
        }
    }

    pub async fn get_by_credential_id(
        &self,
        credential_id: &[u8],
    ) -> Result<Option<PasskeyRecord>, Error> {
        match self {
            Passkeys::Sqlite(s) => s.get_by_credential_id(credential_id).await,
            Passkeys::Postgres(s) => s.get_by_credential_id(credential_id).await,
        }
    }

    pub async fn bump_counter(&self, passkey_id: Uuid, new_counter: u32) -> Result<(), Error> {
        match self {
            Passkeys::Sqlite(s) => s.bump_counter(passkey_id, new_counter).await,
            Passkeys::Postgres(s) => s.bump_counter(passkey_id, new_counter).await,
        }
    }

    /// All passkey rows for `user_id`. Used by the scoped view's
    /// `list` and the per-user account-deletion sweep.
    pub async fn list_for_user(&self, user_id: Uuid) -> Result<Vec<PasskeyRecord>, Error> {
        match self {
            Passkeys::Sqlite(s) => s.list_for_user(user_id).await,
            Passkeys::Postgres(s) => s.list_for_user(user_id).await,
        }
    }

    /// Delete every passkey row for `user_id`. Returns the number
    /// of rows removed. Used by the account-deletion path.
    pub async fn delete_for_user(&self, user_id: Uuid) -> Result<u64, Error> {
        match self {
            Passkeys::Sqlite(s) => s.delete_for_user(user_id).await,
            Passkeys::Postgres(s) => s.delete_for_user(user_id).await,
        }
    }
}

/// Per-user scoped view over [`Passkeys`].
///
/// Only the operations whose SQL has a `user_id` filter move to
/// the scoped view (`list`, `delete_for_user`). The auth-ceremony
/// lookups (`get_by_credential_id`, `bump_counter`) stay on the
/// unscoped repository because they are keyed by
/// `credential_id` / `passkey_id`, not `user_id`.
#[derive(Debug, Clone)]
pub struct ScopedPasskeys {
    inner: Passkeys,
    user_id: Uuid,
}

impl ScopedPasskeys {
    /// `user_id` this view is bound to.
    pub fn user_id(&self) -> Uuid {
        self.user_id
    }

    /// List every passkey for the scoped user.
    pub async fn list(&self) -> Result<Vec<PasskeyRecord>, Error> {
        self.inner.list_for_user(self.user_id).await
    }

    /// Delete every passkey for the scoped user.
    pub async fn delete_all(&self) -> Result<u64, Error> {
        self.inner.delete_for_user(self.user_id).await
    }
}

pub(crate) mod sqlite {
    use chrono::Utc;
    use sqlx::{Row, SqlitePool};
    use uuid::Uuid;

    use crate::error::Error;
    use crate::types::{NewPasskeyRecord, PasskeyRecord};

    #[derive(Clone, Debug)]
    pub struct SqlitePasskeys {
        pub pool: SqlitePool,
    }

    impl SqlitePasskeys {
        pub(crate) fn new(pool: SqlitePool) -> Self {
            Self { pool }
        }

        pub async fn insert(&self, record: NewPasskeyRecord) -> Result<Uuid, Error> {
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
            .execute(&self.pool)
            .await;
            match res {
                Ok(_) => Ok(record.id),
                Err(sqlx::Error::Database(db_err)) if is_sqlite_unique_violation(&*db_err) => {
                    Err(Error::Conflict("credential_id already registered".into()))
                }
                Err(e) => Err(Error::Database(e)),
            }
        }

        pub async fn get_by_credential_id(
            &self,
            credential_id: &[u8],
        ) -> Result<Option<PasskeyRecord>, Error> {
            let row = sqlx::query(
                "SELECT id, user_id, credential_id, public_key, counter, transports \
                 FROM passkeys WHERE credential_id = ?",
            )
            .bind(credential_id)
            .fetch_optional(&self.pool)
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

        pub async fn bump_counter(&self, passkey_id: Uuid, new_counter: u32) -> Result<(), Error> {
            sqlx::query("UPDATE passkeys SET counter = ?, last_used_at = ? WHERE id = ?")
                .bind(new_counter as i64)
                .bind(Utc::now().to_rfc3339())
                .bind(passkey_id.to_string())
                .execute(&self.pool)
                .await?;
            Ok(())
        }

        pub async fn list_for_user(&self, user_id: Uuid) -> Result<Vec<PasskeyRecord>, Error> {
            let rows = sqlx::query(
                "SELECT id, user_id, credential_id, public_key, counter, transports \
                 FROM passkeys WHERE user_id = ? ORDER BY created_at ASC",
            )
            .bind(user_id.to_string())
            .fetch_all(&self.pool)
            .await?;
            rows.into_iter()
                .map(|r| {
                    Ok(PasskeyRecord {
                        id: Uuid::parse_str(&r.try_get::<String, _>("id")?)
                            .expect("DB UUID must parse"),
                        user_id: Uuid::parse_str(&r.try_get::<String, _>("user_id")?)
                            .expect("DB UUID must parse"),
                        credential_id: r.try_get("credential_id")?,
                        public_key: r.try_get("public_key")?,
                        counter: r.try_get::<i64, _>("counter")? as u32,
                        transports: r.try_get("transports")?,
                    })
                })
                .collect()
        }

        pub async fn delete_for_user(&self, user_id: Uuid) -> Result<u64, Error> {
            let res = sqlx::query("DELETE FROM passkeys WHERE user_id = ?")
                .bind(user_id.to_string())
                .execute(&self.pool)
                .await?;
            Ok(res.rows_affected())
        }
    }

    fn is_sqlite_unique_violation(db_err: &dyn sqlx::error::DatabaseError) -> bool {
        db_err.code().as_deref() == Some("2067") || db_err.code().as_deref() == Some("1555")
    }
}

pub(crate) mod postgres {
    use chrono::Utc;
    use sqlx::{PgPool, Row};
    use uuid::Uuid;

    use crate::error::Error;
    use crate::types::{NewPasskeyRecord, PasskeyRecord};

    #[derive(Clone, Debug)]
    pub struct PgPasskeys {
        pub pool: PgPool,
    }

    impl PgPasskeys {
        pub(crate) fn new(pool: PgPool) -> Self {
            Self { pool }
        }

        pub async fn insert(&self, record: NewPasskeyRecord) -> Result<Uuid, Error> {
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
            .execute(&self.pool)
            .await;
            match res {
                Ok(_) => Ok(record.id),
                Err(sqlx::Error::Database(db_err)) if db_err.is_unique_violation() => {
                    Err(Error::Conflict("credential_id already registered".into()))
                }
                Err(e) => Err(Error::Database(e)),
            }
        }

        pub async fn get_by_credential_id(
            &self,
            credential_id: &[u8],
        ) -> Result<Option<PasskeyRecord>, Error> {
            let row = sqlx::query(
                "SELECT id::text AS id, user_id::text AS user_id, credential_id, public_key, \
                        counter, transports \
                 FROM passkeys WHERE credential_id = $1",
            )
            .bind(credential_id)
            .fetch_optional(&self.pool)
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

        pub async fn bump_counter(&self, passkey_id: Uuid, new_counter: u32) -> Result<(), Error> {
            sqlx::query("UPDATE passkeys SET counter = $1, last_used_at = $2 WHERE id = $3")
                .bind(new_counter as i64)
                .bind(Utc::now().to_rfc3339())
                .bind(passkey_id)
                .execute(&self.pool)
                .await?;
            Ok(())
        }

        pub async fn list_for_user(&self, user_id: Uuid) -> Result<Vec<PasskeyRecord>, Error> {
            let rows = sqlx::query(
                "SELECT id, user_id::text AS user_id, credential_id, public_key, counter, transports \
                 FROM passkeys WHERE user_id = $1 ORDER BY created_at ASC",
            )
            .bind(user_id)
            .fetch_all(&self.pool)
            .await?;
            rows.into_iter()
                .map(|r| {
                    Ok(PasskeyRecord {
                        id: Uuid::parse_str(&r.try_get::<String, _>("id")?)
                            .expect("DB UUID must parse"),
                        user_id: Uuid::parse_str(&r.try_get::<String, _>("user_id")?)
                            .expect("DB UUID must parse"),
                        credential_id: r.try_get("credential_id")?,
                        public_key: r.try_get("public_key")?,
                        counter: r.try_get::<i64, _>("counter")? as u32,
                        transports: r.try_get("transports")?,
                    })
                })
                .collect()
        }

        pub async fn delete_for_user(&self, user_id: Uuid) -> Result<u64, Error> {
            let res = sqlx::query("DELETE FROM passkeys WHERE user_id = $1")
                .bind(user_id)
                .execute(&self.pool)
                .await?;
            Ok(res.rows_affected())
        }
    }
}
