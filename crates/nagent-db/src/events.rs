//! Auth events repository.
//!
//! ## Audit row kinds
//!
//! Every per-user action through the auth subtree writes one row
//! to `auth_events`. The `kind` column is the discriminator; the
//! `target_service` column carries the integration id when the
//! event is scoped to a credential vault row (currently `caldav`,
//! `x_account`, or — plan 1791267136806 — `memory`).
//!
//! | `kind`                        | `target_service` | Emitted by                                |
//! |-------------------------------|------------------|-------------------------------------------|
//! | `login_success`               | (none)           | `auth::routes::login_handler`             |
//! | `login_failure`               | (none)           | `auth::routes::login_handler`             |
//! | `logout`                      | (none)           | `auth::routes::logout_handler`            |
//! | `credential_access`          | service id       | `credentials::resolver` (read or write)   |
//! | `credential_missing`          | service id       | `credentials::resolver` (no row)         |
//! | `credential_decrypt_failed`   | service id       | `credentials::resolver` (AES-GCM auth)    |
//! | `memory_store`                | `memory`         | `memories::adapter` (plan 1791267136806) |
//! | `memory_recall`               | `memory`         | `memories::adapter` (plan 1791267136806) |
//! | `memory_forget`               | `memory`         | `memories::adapter` (plan 1791267136806) |
//! | `memory_inject`               | `memory`         | reserved for follow-up per-row throttling|
//! | `memory_decrypt_failed`       | `memory`         | `memories::adapter` (AES-GCM auth)        |
//!
//! Rows are appended by the `record_event` helper, which fires the
//! insert on a `tokio::spawn` so a slow DB never blocks the hot path.
//! All errors are logged at WARN; the caller's response is
//! unaffected.

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
