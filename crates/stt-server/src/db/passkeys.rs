//! Passkeys repository — plan 5.D extraction.

use uuid::Uuid;

use crate::auth::error::AuthError;
use crate::auth::store::{NewPasskeyRecord, PasskeyRecord};
use crate::db::pool::AnyPool;

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

    pub async fn insert(&self, record: NewPasskeyRecord) -> Result<Uuid, AuthError> {
        match self {
            Passkeys::Sqlite(s) => s.insert(record).await,
            Passkeys::Postgres(s) => s.insert(record).await,
        }
    }

    pub async fn get_by_credential_id(
        &self,
        credential_id: &[u8],
    ) -> Result<Option<PasskeyRecord>, AuthError> {
        match self {
            Passkeys::Sqlite(s) => s.get_by_credential_id(credential_id).await,
            Passkeys::Postgres(s) => s.get_by_credential_id(credential_id).await,
        }
    }

    pub async fn bump_counter(&self, passkey_id: Uuid, new_counter: u32) -> Result<(), AuthError> {
        match self {
            Passkeys::Sqlite(s) => s.bump_counter(passkey_id, new_counter).await,
            Passkeys::Postgres(s) => s.bump_counter(passkey_id, new_counter).await,
        }
    }
}

pub mod sqlite {
    use chrono::Utc;
    use sqlx::{Row, SqlitePool};
    use uuid::Uuid;

    use crate::auth::error::AuthError;
    use crate::auth::store::{NewPasskeyRecord, PasskeyRecord};

    #[derive(Clone, Debug)]
    pub struct SqlitePasskeys {
        pub(crate) pool: SqlitePool,
    }

    impl SqlitePasskeys {
        pub(crate) fn new(pool: SqlitePool) -> Self {
            Self { pool }
        }

        pub async fn insert(&self, record: NewPasskeyRecord) -> Result<Uuid, AuthError> {
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
                Err(sqlx::Error::Database(db_err)) if is_sqlite_unique_violation(&*db_err) => Err(
                    AuthError::Conflict("credential_id already registered".into()),
                ),
                Err(e) => Err(AuthError::Database(e)),
            }
        }

        pub async fn get_by_credential_id(
            &self,
            credential_id: &[u8],
        ) -> Result<Option<PasskeyRecord>, AuthError> {
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

        pub async fn bump_counter(
            &self,
            passkey_id: Uuid,
            new_counter: u32,
        ) -> Result<(), AuthError> {
            sqlx::query("UPDATE passkeys SET counter = ?, last_used_at = ? WHERE id = ?")
                .bind(new_counter as i64)
                .bind(Utc::now().to_rfc3339())
                .bind(passkey_id.to_string())
                .execute(&self.pool)
                .await?;
            Ok(())
        }
    }

    fn is_sqlite_unique_violation(db_err: &dyn sqlx::error::DatabaseError) -> bool {
        db_err.code().as_deref() == Some("2067") || db_err.code().as_deref() == Some("1555")
    }
}

pub mod postgres {
    use chrono::Utc;
    use sqlx::{PgPool, Row};
    use uuid::Uuid;

    use crate::auth::error::AuthError;
    use crate::auth::store::{NewPasskeyRecord, PasskeyRecord};

    #[derive(Clone, Debug)]
    pub struct PgPasskeys {
        pub(crate) pool: PgPool,
    }

    impl PgPasskeys {
        pub(crate) fn new(pool: PgPool) -> Self {
            Self { pool }
        }

        pub async fn insert(&self, record: NewPasskeyRecord) -> Result<Uuid, AuthError> {
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
                Err(sqlx::Error::Database(db_err)) if db_err.is_unique_violation() => Err(
                    AuthError::Conflict("credential_id already registered".into()),
                ),
                Err(e) => Err(AuthError::Database(e)),
            }
        }

        pub async fn get_by_credential_id(
            &self,
            credential_id: &[u8],
        ) -> Result<Option<PasskeyRecord>, AuthError> {
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

        pub async fn bump_counter(
            &self,
            passkey_id: Uuid,
            new_counter: u32,
        ) -> Result<(), AuthError> {
            sqlx::query("UPDATE passkeys SET counter = $1, last_used_at = $2 WHERE id = $3")
                .bind(new_counter as i64)
                .bind(Utc::now().to_rfc3339())
                .bind(passkey_id)
                .execute(&self.pool)
                .await?;
            Ok(())
        }
    }
}
