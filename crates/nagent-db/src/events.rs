//! Auth events repository — plan 5.D extraction.

use crate::pool::AnyPool;
use crate::types::NewAuthEvent;

#[derive(Debug, Clone)]
pub enum Events {
    Sqlite(sqlite::SqliteEvents),
    Postgres(postgres::PgEvents),
}

impl Events {
    pub fn new(pool: &AnyPool) -> Self {
        match pool {
            AnyPool::Sqlite(p) => Self::Sqlite(sqlite::SqliteEvents::new(p.clone())),
            AnyPool::Postgres(p) => Self::Postgres(postgres::PgEvents::new(p.clone())),
        }
    }

    /// Best-effort: a failure to insert the audit row is logged but
    /// never propagated back to the caller.
    pub fn record(&self, event: NewAuthEvent) {
        match self {
            Events::Sqlite(s) => s.record(event),
            Events::Postgres(s) => s.record(event),
        }
    }
}

pub(crate) mod sqlite {
    use chrono::Utc;
    use sqlx::SqlitePool;
    use uuid::Uuid;

    use crate::types::NewAuthEvent;

    #[derive(Clone, Debug)]
    pub struct SqliteEvents {
        pub pool: SqlitePool,
    }

    impl SqliteEvents {
        pub(crate) fn new(pool: SqlitePool) -> Self {
            Self { pool }
        }

        pub fn record(&self, event: NewAuthEvent) {
            let pool = self.pool.clone();
            tokio::spawn(async move {
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
                .execute(&pool)
                .await;
                if let Err(e) = res {
                    tracing::warn!(error = %e, "failed to write auth_events row");
                }
            });
        }
    }
}

pub(crate) mod postgres {
    use chrono::Utc;
    use sqlx::PgPool;
    use uuid::Uuid;

    use crate::types::NewAuthEvent;

    #[derive(Clone, Debug)]
    pub struct PgEvents {
        pub pool: PgPool,
    }

    impl PgEvents {
        pub(crate) fn new(pool: PgPool) -> Self {
            Self { pool }
        }

        pub fn record(&self, event: NewAuthEvent) {
            let pool = self.pool.clone();
            tokio::spawn(async move {
                let id = Uuid::new_v4();
                let res = sqlx::query(
                    "INSERT INTO auth_events (id, user_id, kind, provider, ip, user_agent, target_service, occurred_at) \
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
                )
                .bind(id.to_string())
                .bind(event.user_id.map(|u| u.to_string()))
                .bind(&event.kind)
                .bind(&event.provider)
                .bind(event.ip.as_deref())
                .bind(event.user_agent.as_deref())
                .bind(event.target_service.as_deref())
                .bind(Utc::now().to_rfc3339())
                .execute(&pool)
                .await;
                if let Err(e) = res {
                    tracing::warn!(error = %e, "failed to write auth_events row");
                }
            });
        }
    }
}
