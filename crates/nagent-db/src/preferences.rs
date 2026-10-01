//! Per-user UI preferences repository.

use uuid::Uuid;

use crate::error::Error;
use crate::pool::AnyPool;
use crate::types::UserPreferences;

#[derive(Debug, Clone)]
pub enum Preferences {
    Sqlite(sqlite::SqlitePreferences),
    Postgres(postgres::PgPreferences),
}

impl Preferences {
    pub fn new(pool: &AnyPool) -> Self {
        match pool {
            AnyPool::Sqlite(p) => Self::Sqlite(sqlite::SqlitePreferences::new(p.clone())),
            AnyPool::Postgres(p) => Self::Postgres(postgres::PgPreferences::new(p.clone())),
        }
    }

    pub async fn get(&self, user_id: Uuid) -> Result<UserPreferences, Error> {
        match self {
            Preferences::Sqlite(s) => s.get(user_id).await,
            Preferences::Postgres(s) => s.get(user_id).await,
        }
    }

    pub async fn upsert(
        &self,
        user_id: Uuid,
        share_location_enabled: bool,
        share_timezone_enabled: bool,
    ) -> Result<UserPreferences, Error> {
        match self {
            Preferences::Sqlite(s) => {
                s.upsert(user_id, share_location_enabled, share_timezone_enabled)
                    .await
            }
            Preferences::Postgres(s) => {
                s.upsert(user_id, share_location_enabled, share_timezone_enabled)
                    .await
            }
        }
    }
}

pub(crate) mod sqlite {
    use chrono::{DateTime, Utc};
    use sqlx::{Row, SqlitePool};
    use uuid::Uuid;

    use crate::error::Error;
    use crate::types::UserPreferences;

    #[derive(Clone, Debug)]
    pub struct SqlitePreferences {
        pub pool: SqlitePool,
    }

    impl SqlitePreferences {
        pub(crate) fn new(pool: SqlitePool) -> Self {
            Self { pool }
        }

        pub async fn get(&self, user_id: Uuid) -> Result<UserPreferences, Error> {
            let row = sqlx::query(
                "SELECT share_location_enabled, share_timezone_enabled, updated_at \
                 FROM user_preferences WHERE user_id = ?",
            )
            .bind(user_id.to_string())
            .fetch_optional(&self.pool)
            .await?;
            let Some(r) = row else {
                return Ok(UserPreferences {
                    share_location_enabled: false,
                    share_timezone_enabled: false,
                    updated_at: Utc::now(),
                });
            };
            Ok(UserPreferences {
                share_location_enabled: row_to_bool(&r, "share_location_enabled")?,
                share_timezone_enabled: row_to_bool(&r, "share_timezone_enabled")?,
                updated_at: parse_rfc3339(&r.try_get::<String, _>("updated_at")?),
            })
        }

        pub async fn upsert(
            &self,
            user_id: Uuid,
            share_location_enabled: bool,
            share_timezone_enabled: bool,
        ) -> Result<UserPreferences, Error> {
            let user_id_str = user_id.to_string();
            sqlx::query(
                "INSERT INTO user_preferences \
                    (user_id, share_location_enabled, share_timezone_enabled, updated_at) \
                 VALUES (?, ?, ?, CURRENT_TIMESTAMP) \
                 ON CONFLICT(user_id) DO UPDATE SET \
                    share_location_enabled = excluded.share_location_enabled, \
                    share_timezone_enabled = excluded.share_timezone_enabled, \
                    updated_at = CURRENT_TIMESTAMP",
            )
            .bind(&user_id_str)
            .bind(if share_location_enabled { 1_i64 } else { 0_i64 })
            .bind(if share_timezone_enabled { 1_i64 } else { 0_i64 })
            .execute(&self.pool)
            .await?;
            self.get(user_id).await
        }
    }

    fn row_to_bool(row: &sqlx::sqlite::SqliteRow, col: &str) -> Result<bool, Error> {
        let n: i64 = row.try_get(col)?;
        Ok(n != 0)
    }

    fn parse_rfc3339(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s)
            .map(|d| d.with_timezone(&Utc))
            .unwrap_or_else(|_| Utc::now())
    }
}

pub(crate) mod postgres {
    use chrono::{DateTime, Utc};
    use sqlx::{PgPool, Row};
    use uuid::Uuid;

    use crate::error::Error;
    use crate::types::UserPreferences;

    #[derive(Clone, Debug)]
    pub struct PgPreferences {
        pub pool: PgPool,
    }

    impl PgPreferences {
        pub(crate) fn new(pool: PgPool) -> Self {
            Self { pool }
        }

        pub async fn get(&self, user_id: Uuid) -> Result<UserPreferences, Error> {
            let row = sqlx::query(
                "SELECT share_location_enabled, share_timezone_enabled, updated_at \
                 FROM user_preferences WHERE user_id = $1",
            )
            .bind(user_id)
            .fetch_optional(&self.pool)
            .await?;
            let Some(r) = row else {
                return Ok(UserPreferences {
                    share_location_enabled: false,
                    share_timezone_enabled: false,
                    updated_at: Utc::now(),
                });
            };
            Ok(UserPreferences {
                share_location_enabled: row_to_bool(&r, "share_location_enabled")?,
                share_timezone_enabled: row_to_bool(&r, "share_timezone_enabled")?,
                updated_at: parse_rfc3339(&r.try_get::<String, _>("updated_at")?),
            })
        }

        pub async fn upsert(
            &self,
            user_id: Uuid,
            share_location_enabled: bool,
            share_timezone_enabled: bool,
        ) -> Result<UserPreferences, Error> {
            sqlx::query(
                "INSERT INTO user_preferences \
                    (user_id, share_location_enabled, share_timezone_enabled, updated_at) \
                 VALUES ($1, $2, $3, CURRENT_TIMESTAMP) \
                 ON CONFLICT (user_id) DO UPDATE SET \
                    share_location_enabled = EXCLUDED.share_location_enabled, \
                    share_timezone_enabled = EXCLUDED.share_timezone_enabled, \
                    updated_at = CURRENT_TIMESTAMP",
            )
            .bind(user_id)
            .bind(if share_location_enabled { 1_i64 } else { 0_i64 })
            .bind(if share_timezone_enabled { 1_i64 } else { 0_i64 })
            .execute(&self.pool)
            .await?;
            self.get(user_id).await
        }
    }

    fn row_to_bool(row: &sqlx::postgres::PgRow, col: &str) -> Result<bool, Error> {
        let n: i64 = row.try_get(col)?;
        Ok(n != 0)
    }

    fn parse_rfc3339(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s)
            .map(|d| d.with_timezone(&Utc))
            .unwrap_or_else(|_| Utc::now())
    }
}
