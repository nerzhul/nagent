//! Per-user credentials repository — plan 5.D extraction.

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

pub mod sqlite {
    use sqlx::{Row, SqlitePool};
    use uuid::Uuid;

    use crate::error::Error;
    use crate::types::UserCredentialRow;

    #[derive(Clone, Debug)]
    pub struct SqliteCredentials {
        pub(crate) pool: SqlitePool,
    }

    impl SqliteCredentials {
        pub fn new(pool: SqlitePool) -> Self {
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

pub mod postgres {
    use sqlx::{PgPool, Row};
    use uuid::Uuid;

    use crate::error::Error;
    use crate::types::UserCredentialRow;

    #[derive(Clone, Debug)]
    pub struct PgCredentials {
        pub(crate) pool: PgPool,
    }

    impl PgCredentials {
        pub fn new(pool: PgPool) -> Self {
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
