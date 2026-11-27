//! Per-user credentials repository
//!
//! The credentials vault stores one row per `(user, service,
//! field)`. Callers that have already resolved a `user_id` should
//! prefer [`Credentials::for_user`] so the `WHERE user_id = ?`
//! filter cannot be accidentally dropped .

use uuid::Uuid;

use crate::error::Error;
use crate::pool::AnyPool;
use crate::types::UserCredentialRow;

#[derive(Debug, Clone)]
pub enum Credentials {
    Sqlite(sqlite::SqliteCredentials),
    Postgres(postgres::PgCredentials),
}

impl Credentials {
    pub fn new(pool: &AnyPool) -> Self {
        match pool {
            AnyPool::Sqlite(p) => Self::Sqlite(sqlite::SqliteCredentials::new(p.clone())),
            AnyPool::Postgres(p) => Self::Postgres(postgres::PgCredentials::new(p.clone())),
        }
    }

    /// Scope every per-row method to a single `user_id`. The
    /// returned [`ScopedCredentials`] does not take a `user_id`
    /// argument on its per-row methods, so a route handler that
    /// holds a `ScopedCredentials` cannot accidentally drop the
    /// `WHERE user_id = ?` filter .
    pub fn for_user(&self, user_id: Uuid) -> ScopedCredentials {
        ScopedCredentials {
            inner: self.clone(),
            user_id,
        }
    }

    pub async fn upsert(
        &self,
        user_id: Uuid,
        service_id: &str,
        fields: &[(String, Vec<u8>, Vec<u8>)],
    ) -> Result<(), Error> {
        match self {
            Credentials::Sqlite(s) => s.upsert(user_id, service_id, fields).await,
            Credentials::Postgres(s) => s.upsert(user_id, service_id, fields).await,
        }
    }

    pub async fn delete_service(&self, user_id: Uuid, service_id: &str) -> Result<u64, Error> {
        match self {
            Credentials::Sqlite(s) => s.delete_service(user_id, service_id).await,
            Credentials::Postgres(s) => s.delete_service(user_id, service_id).await,
        }
    }

    /// Insert-or-update the supplied fields **without** touching
    /// the service's other rows. Unlike [`Self::upsert`] (an
    /// atomic replace of every field), this is the write path for
    /// subsystems that own only a subset of a service's fields —
    /// e.g. the Shortcut OAuth callback writes `access_token` /
    /// `refresh_token` / … into the shared `shortcut` service
    /// without erasing the user's `api_token` / `username` /
    /// `default_team` rows.
    pub async fn merge_fields(
        &self,
        user_id: Uuid,
        service_id: &str,
        fields: &[(String, Vec<u8>, Vec<u8>)],
    ) -> Result<(), Error> {
        match self {
            Credentials::Sqlite(s) => s.merge_fields(user_id, service_id, fields).await,
            Credentials::Postgres(s) => s.merge_fields(user_id, service_id, fields).await,
        }
    }

    /// Delete only the listed `field_key`s of one `(user, service)`;
    /// the service's other rows survive. Used by the Shortcut OAuth
    /// disconnect handler, which clears the OAuth token rows but
    /// keeps the API-token fallback the user may still rely on.
    pub async fn delete_fields(
        &self,
        user_id: Uuid,
        service_id: &str,
        field_keys: &[String],
    ) -> Result<u64, Error> {
        match self {
            Credentials::Sqlite(s) => s.delete_fields(user_id, service_id, field_keys).await,
            Credentials::Postgres(s) => s.delete_fields(user_id, service_id, field_keys).await,
        }
    }

    pub async fn list_field_keys(
        &self,
        user_id: Uuid,
        service_id: &str,
    ) -> Result<Vec<String>, Error> {
        match self {
            Credentials::Sqlite(s) => s.list_field_keys(user_id, service_id).await,
            Credentials::Postgres(s) => s.list_field_keys(user_id, service_id).await,
        }
    }

    pub async fn fetch(
        &self,
        user_id: Uuid,
        service_id: &str,
        field_key: &str,
    ) -> Result<Option<UserCredentialRow>, Error> {
        match self {
            Credentials::Sqlite(s) => s.fetch(user_id, service_id, field_key).await,
            Credentials::Postgres(s) => s.fetch(user_id, service_id, field_key).await,
        }
    }
}

/// Per-user scoped view over [`Credentials`].
#[derive(Debug, Clone)]
pub struct ScopedCredentials {
    inner: Credentials,
    user_id: Uuid,
}

impl ScopedCredentials {
    pub fn user_id(&self) -> Uuid {
        self.user_id
    }

    /// Atomic replace of every configured field for one
    /// `(user, service)`. Caller encrypts each field before
    /// passing it in.
    pub async fn upsert(
        &self,
        service_id: &str,
        fields: &[(String, Vec<u8>, Vec<u8>)],
    ) -> Result<(), Error> {
        self.inner.upsert(self.user_id, service_id, fields).await
    }

    /// Remove every configured field for one `(user, service)`.
    pub async fn delete_service(&self, service_id: &str) -> Result<u64, Error> {
        self.inner.delete_service(self.user_id, service_id).await
    }

    /// Insert-or-update the supplied fields without touching the
    /// service's other rows — see [`Credentials::merge_fields`].
    pub async fn merge_fields(
        &self,
        service_id: &str,
        fields: &[(String, Vec<u8>, Vec<u8>)],
    ) -> Result<(), Error> {
        self.inner
            .merge_fields(self.user_id, service_id, fields)
            .await
    }

    /// Delete only the listed fields of one `(user, service)` —
    /// see [`Credentials::delete_fields`].
    pub async fn delete_fields(
        &self,
        service_id: &str,
        field_keys: &[String],
    ) -> Result<u64, Error> {
        self.inner
            .delete_fields(self.user_id, service_id, field_keys)
            .await
    }

    /// List every `field_key` currently configured for
    /// `(user, service)`. Used by `GET /api/integrations/:id`.
    pub async fn list_field_keys(&self, service_id: &str) -> Result<Vec<String>, Error> {
        self.inner.list_field_keys(self.user_id, service_id).await
    }

    /// Fetch the `(nonce, ciphertext)` row for `(user, service,
    /// field)`. Returns `None` when the field is not configured.
    pub async fn fetch(
        &self,
        service_id: &str,
        field_key: &str,
    ) -> Result<Option<UserCredentialRow>, Error> {
        self.inner.fetch(self.user_id, service_id, field_key).await
    }
}

pub(crate) mod sqlite {
    use sqlx::{QueryBuilder, Row, Sqlite, SqlitePool};
    use uuid::Uuid;

    use crate::error::Error;
    use crate::types::UserCredentialRow;

    #[derive(Clone, Debug)]
    pub struct SqliteCredentials {
        pub pool: SqlitePool,
    }

    impl SqliteCredentials {
        pub(crate) fn new(pool: SqlitePool) -> Self {
            Self { pool }
        }

        pub async fn upsert(
            &self,
            user_id: Uuid,
            service_id: &str,
            fields: &[(String, Vec<u8>, Vec<u8>)],
        ) -> Result<(), Error> {
            let user_id_str = user_id.to_string();
            let mut tx = self.pool.begin().await?;
            sqlx::query("DELETE FROM user_credentials WHERE user_id = ? AND service_id = ?")
                .bind(&user_id_str)
                .bind(service_id)
                .execute(&mut *tx)
                .await?;
            for (field_key, nonce, ciphertext) in fields {
                if nonce.len() != 12 {
                    return Err(Error::BadRequest(format!(
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

        pub async fn delete_service(&self, user_id: Uuid, service_id: &str) -> Result<u64, Error> {
            let res =
                sqlx::query("DELETE FROM user_credentials WHERE user_id = ? AND service_id = ?")
                    .bind(user_id.to_string())
                    .bind(service_id)
                    .execute(&self.pool)
                    .await?;
            Ok(res.rows_affected())
        }

        pub async fn merge_fields(
            &self,
            user_id: Uuid,
            service_id: &str,
            fields: &[(String, Vec<u8>, Vec<u8>)],
        ) -> Result<(), Error> {
            let user_id_str = user_id.to_string();
            let mut tx = self.pool.begin().await?;
            for (field_key, nonce, ciphertext) in fields {
                if nonce.len() != 12 {
                    return Err(Error::BadRequest(format!(
                        "nonce for field {field_key:?} must be 12 bytes, got {}",
                        nonce.len()
                    )));
                }
                let id = Uuid::new_v4().to_string();
                sqlx::query(
                    "INSERT INTO user_credentials \
                     (id, user_id, service_id, field_key, nonce, ciphertext, created_at, updated_at) \
                     VALUES (?, ?, ?, ?, ?, ?, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP) \
                     ON CONFLICT (user_id, service_id, field_key) DO UPDATE SET \
                       nonce = excluded.nonce, \
                       ciphertext = excluded.ciphertext, \
                       updated_at = CURRENT_TIMESTAMP",
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

        pub async fn delete_fields(
            &self,
            user_id: Uuid,
            service_id: &str,
            field_keys: &[String],
        ) -> Result<u64, Error> {
            let mut qb: QueryBuilder<Sqlite> =
                QueryBuilder::new("DELETE FROM user_credentials WHERE user_id = ");
            qb.push_bind(user_id.to_string());
            qb.push(" AND service_id = ");
            qb.push_bind(service_id);
            qb.push(" AND field_key IN (");
            let mut sep = qb.separated(", ");
            for key in field_keys {
                sep.push_bind(key);
            }
            qb.push(")");
            let res = qb.build().execute(&self.pool).await?;
            Ok(res.rows_affected())
        }

        pub async fn list_field_keys(
            &self,
            user_id: Uuid,
            service_id: &str,
        ) -> Result<Vec<String>, Error> {
            let rows = sqlx::query(
                "SELECT field_key FROM user_credentials \
                 WHERE user_id = ? AND service_id = ? ORDER BY field_key",
            )
            .bind(user_id.to_string())
            .bind(service_id)
            .fetch_all(&self.pool)
            .await?;
            rows.into_iter()
                .map(|r| r.try_get::<String, _>("field_key"))
                .collect::<Result<Vec<_>, _>>()
                .map_err(Error::Database)
        }

        pub async fn fetch(
            &self,
            user_id: Uuid,
            service_id: &str,
            field_key: &str,
        ) -> Result<Option<UserCredentialRow>, Error> {
            let row = sqlx::query(
                "SELECT nonce, ciphertext FROM user_credentials \
                 WHERE user_id = ? AND service_id = ? AND field_key = ?",
            )
            .bind(user_id.to_string())
            .bind(service_id)
            .bind(field_key)
            .fetch_optional(&self.pool)
            .await?;
            let Some(r) = row else { return Ok(None) };
            Ok(Some(UserCredentialRow {
                nonce: r.try_get("nonce")?,
                ciphertext: r.try_get("ciphertext")?,
            }))
        }
    }
}

pub(crate) mod postgres {
    use sqlx::{PgPool, Postgres, QueryBuilder, Row};
    use uuid::Uuid;

    use crate::error::Error;
    use crate::types::UserCredentialRow;

    #[derive(Clone, Debug)]
    pub struct PgCredentials {
        pub pool: PgPool,
    }

    impl PgCredentials {
        pub(crate) fn new(pool: PgPool) -> Self {
            Self { pool }
        }

        pub async fn upsert(
            &self,
            user_id: Uuid,
            service_id: &str,
            fields: &[(String, Vec<u8>, Vec<u8>)],
        ) -> Result<(), Error> {
            let mut tx = self.pool.begin().await?;
            sqlx::query("DELETE FROM user_credentials WHERE user_id = $1 AND service_id = $2")
                .bind(user_id)
                .bind(service_id)
                .execute(&mut *tx)
                .await?;
            for (field_key, nonce, ciphertext) in fields {
                if nonce.len() != 12 {
                    return Err(Error::BadRequest(format!(
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

        pub async fn delete_service(&self, user_id: Uuid, service_id: &str) -> Result<u64, Error> {
            let res =
                sqlx::query("DELETE FROM user_credentials WHERE user_id = $1 AND service_id = $2")
                    .bind(user_id)
                    .bind(service_id)
                    .execute(&self.pool)
                    .await?;
            Ok(res.rows_affected())
        }

        pub async fn merge_fields(
            &self,
            user_id: Uuid,
            service_id: &str,
            fields: &[(String, Vec<u8>, Vec<u8>)],
        ) -> Result<(), Error> {
            let mut tx = self.pool.begin().await?;
            for (field_key, nonce, ciphertext) in fields {
                if nonce.len() != 12 {
                    return Err(Error::BadRequest(format!(
                        "nonce for field {field_key:?} must be 12 bytes, got {}",
                        nonce.len()
                    )));
                }
                let id = Uuid::new_v4().to_string();
                sqlx::query(
                    "INSERT INTO user_credentials \
                     (id, user_id, service_id, field_key, nonce, ciphertext, created_at, updated_at) \
                     VALUES ($1, $2, $3, $4, $5, $6, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP) \
                     ON CONFLICT (user_id, service_id, field_key) DO UPDATE SET \
                       nonce = excluded.nonce, \
                       ciphertext = excluded.ciphertext, \
                       updated_at = CURRENT_TIMESTAMP",
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

        pub async fn delete_fields(
            &self,
            user_id: Uuid,
            service_id: &str,
            field_keys: &[String],
        ) -> Result<u64, Error> {
            let mut qb: QueryBuilder<Postgres> =
                QueryBuilder::new("DELETE FROM user_credentials WHERE user_id = ");
            qb.push_bind(user_id);
            qb.push(" AND service_id = ");
            qb.push_bind(service_id);
            qb.push(" AND field_key IN (");
            let mut sep = qb.separated(", ");
            for key in field_keys {
                sep.push_bind(key);
            }
            qb.push(")");
            let res = qb.build().execute(&self.pool).await?;
            Ok(res.rows_affected())
        }

        pub async fn list_field_keys(
            &self,
            user_id: Uuid,
            service_id: &str,
        ) -> Result<Vec<String>, Error> {
            let rows = sqlx::query(
                "SELECT field_key FROM user_credentials \
                 WHERE user_id = $1 AND service_id = $2 ORDER BY field_key",
            )
            .bind(user_id)
            .bind(service_id)
            .fetch_all(&self.pool)
            .await?;
            rows.into_iter()
                .map(|r| r.try_get::<String, _>("field_key"))
                .collect::<Result<Vec<_>, _>>()
                .map_err(Error::Database)
        }

        pub async fn fetch(
            &self,
            user_id: Uuid,
            service_id: &str,
            field_key: &str,
        ) -> Result<Option<UserCredentialRow>, Error> {
            let row = sqlx::query(
                "SELECT nonce, ciphertext FROM user_credentials \
                 WHERE user_id = $1 AND service_id = $2 AND field_key = $3",
            )
            .bind(user_id)
            .bind(service_id)
            .bind(field_key)
            .fetch_optional(&self.pool)
            .await?;
            let Some(r) = row else { return Ok(None) };
            Ok(Some(UserCredentialRow {
                nonce: r.try_get("nonce")?,
                ciphertext: r.try_get("ciphertext")?,
            }))
        }
    }
}
