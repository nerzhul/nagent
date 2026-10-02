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

    /// Scope every per-user method to a single `user_id`. The
    /// returned [`ScopedPreferences`] drops the `user_id` argument
    /// on its per-row methods so a route handler holding a scoped
    /// view cannot accidentally target another user's row (plan
    /// 4.A, plan S4).
    pub fn for_user(&self, user_id: Uuid) -> ScopedPreferences {
        ScopedPreferences {
            inner: self.clone(),
            user_id,
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
        reply_language: Option<String>,
        additional_instructions: Option<String>,
        temperature: Option<f32>,
    ) -> Result<UserPreferences, Error> {
        match self {
            Preferences::Sqlite(s) => {
                s.upsert(
                    user_id,
                    share_location_enabled,
                    share_timezone_enabled,
                    reply_language,
                    additional_instructions,
                    temperature,
                )
                .await
            }
            Preferences::Postgres(s) => {
                s.upsert(
                    user_id,
                    share_location_enabled,
                    share_timezone_enabled,
                    reply_language,
                    additional_instructions,
                    temperature,
                )
                .await
            }
        }
    }
}

/// Per-user scoped view over [`Preferences`].
///
/// The scoped methods (`get`, `upsert`) do NOT take a `user_id`
/// argument; the filter is fixed at construction. Plan 4.A / S4
/// (per-user isolation enforced by construction).
#[derive(Debug, Clone)]
pub struct ScopedPreferences {
    inner: Preferences,
    user_id: Uuid,
}

impl ScopedPreferences {
    /// `user_id` this view is bound to. Surfaced for tests +
    /// diagnostic logs.
    pub fn user_id(&self) -> Uuid {
        self.user_id
    }

    /// Read the preferences row for the scoped user.
    pub async fn get(&self) -> Result<UserPreferences, Error> {
        self.inner.get(self.user_id).await
    }

    /// Replace the preferences row for the scoped user.
    ///
    /// All five fields must be supplied — partial PUTs are
    /// rejected at the HTTP boundary so we can treat this signature
    /// as a strict atomic replace (matches the `share_location_enabled` /
    /// `share_timezone_enabled` "all fields required" contract
    /// documented in `auth::routes::PutPreferencesBody`). Pass
    /// `None` for `reply_language` to clear the preference ("Auto"
    /// mode — fall back to the user's input language); same for
    /// `additional_instructions` (no user-supplied suffix) and
    /// `temperature` (fall back to the upstream model default).
    pub async fn upsert(
        &self,
        share_location_enabled: bool,
        share_timezone_enabled: bool,
        reply_language: Option<String>,
        additional_instructions: Option<String>,
        temperature: Option<f32>,
    ) -> Result<UserPreferences, Error> {
        self.inner
            .upsert(
                self.user_id,
                share_location_enabled,
                share_timezone_enabled,
                reply_language,
                additional_instructions,
                temperature,
            )
            .await
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
                "SELECT share_location_enabled, share_timezone_enabled, reply_language, \
                        additional_instructions, temperature, updated_at \
                 FROM user_preferences WHERE user_id = ?",
            )
            .bind(user_id.to_string())
            .fetch_optional(&self.pool)
            .await?;
            let Some(r) = row else {
                return Ok(UserPreferences {
                    share_location_enabled: false,
                    share_timezone_enabled: false,
                    reply_language: None,
                    additional_instructions: None,
                    temperature: None,
                    updated_at: Utc::now(),
                });
            };
            Ok(UserPreferences {
                share_location_enabled: row_to_bool(&r, "share_location_enabled")?,
                share_timezone_enabled: row_to_bool(&r, "share_timezone_enabled")?,
                reply_language: r.try_get::<Option<String>, _>("reply_language")?,
                additional_instructions: r
                    .try_get::<Option<String>, _>("additional_instructions")?,
                temperature: r.try_get::<Option<f32>, _>("temperature")?,
                updated_at: parse_rfc3339(&r.try_get::<String, _>("updated_at")?),
            })
        }

        pub async fn upsert(
            &self,
            user_id: Uuid,
            share_location_enabled: bool,
            share_timezone_enabled: bool,
            reply_language: Option<String>,
            additional_instructions: Option<String>,
            temperature: Option<f32>,
        ) -> Result<UserPreferences, Error> {
            let user_id_str = user_id.to_string();
            sqlx::query(
                "INSERT INTO user_preferences \
                    (user_id, share_location_enabled, share_timezone_enabled, \
                     reply_language, additional_instructions, temperature, updated_at) \
                 VALUES (?, ?, ?, ?, ?, ?, CURRENT_TIMESTAMP) \
                 ON CONFLICT(user_id) DO UPDATE SET \
                     share_location_enabled = excluded.share_location_enabled, \
                     share_timezone_enabled = excluded.share_timezone_enabled, \
                     reply_language = excluded.reply_language, \
                     additional_instructions = excluded.additional_instructions, \
                     temperature = excluded.temperature, \
                     updated_at = CURRENT_TIMESTAMP",
            )
            .bind(&user_id_str)
            .bind(if share_location_enabled { 1_i64 } else { 0_i64 })
            .bind(if share_timezone_enabled { 1_i64 } else { 0_i64 })
            .bind(reply_language)
            .bind(additional_instructions)
            .bind(temperature)
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
                "SELECT share_location_enabled, share_timezone_enabled, reply_language, \
                        additional_instructions, temperature, updated_at \
                 FROM user_preferences WHERE user_id = $1",
            )
            .bind(user_id)
            .fetch_optional(&self.pool)
            .await?;
            let Some(r) = row else {
                return Ok(UserPreferences {
                    share_location_enabled: false,
                    share_timezone_enabled: false,
                    reply_language: None,
                    additional_instructions: None,
                    temperature: None,
                    updated_at: Utc::now(),
                });
            };
            Ok(UserPreferences {
                share_location_enabled: row_to_bool(&r, "share_location_enabled")?,
                share_timezone_enabled: row_to_bool(&r, "share_timezone_enabled")?,
                reply_language: r.try_get::<Option<String>, _>("reply_language")?,
                additional_instructions: r
                    .try_get::<Option<String>, _>("additional_instructions")?,
                temperature: r.try_get::<Option<f32>, _>("temperature")?,
                updated_at: parse_rfc3339(&r.try_get::<String, _>("updated_at")?),
            })
        }

        pub async fn upsert(
            &self,
            user_id: Uuid,
            share_location_enabled: bool,
            share_timezone_enabled: bool,
            reply_language: Option<String>,
            additional_instructions: Option<String>,
            temperature: Option<f32>,
        ) -> Result<UserPreferences, Error> {
            sqlx::query(
                "INSERT INTO user_preferences \
                    (user_id, share_location_enabled, share_timezone_enabled, \
                     reply_language, additional_instructions, temperature, updated_at) \
                 VALUES ($1, $2, $3, $4, $5, $6, CURRENT_TIMESTAMP) \
                 ON CONFLICT (user_id) DO UPDATE SET \
                     share_location_enabled = EXCLUDED.share_location_enabled, \
                     share_timezone_enabled = EXCLUDED.share_timezone_enabled, \
                     reply_language = EXCLUDED.reply_language, \
                     additional_instructions = EXCLUDED.additional_instructions, \
                     temperature = EXCLUDED.temperature, \
                     updated_at = CURRENT_TIMESTAMP",
            )
            .bind(user_id)
            .bind(if share_location_enabled { 1_i64 } else { 0_i64 })
            .bind(if share_timezone_enabled { 1_i64 } else { 0_i64 })
            .bind(reply_language)
            .bind(additional_instructions)
            .bind(temperature)
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
