//! Per-user long-term memory repository
//!
//! (plan 1791267136806, §1.2–§1.4). Mirrors [`crate::credentials`]
//! in shape: an engine-agnostic [`Memories`] enum that owns the
//! per-engine SQL, a [`ScopedMemories`] view that drops the
//! `user_id` argument from its per-row methods, and
//! ciphertext-only row shapes — the encryption key lives in
//! `nagent-server` (`[auth.credentials].key`) and decryption happens
//! in the `MemorySource` adapter over there.
//!
//! The taxonomy columns (`subject`, `predicate`, `tags`) stay
//! plaintext so the recall query can stay a `WHERE … LIKE …` scan
//! without per-row decryption. The plan §1.2 documents the
//! tradeoff (a subject/predicate leak still leaks the shape of the
//! user's facts) and reserves a blind-index PR for v2.

use uuid::Uuid;

use crate::error::Error;
use crate::pool::AnyPool;
use crate::types::{MemoryMeta, MemoryRow, NewMemoryRequest};

/// Maximum number of rows the recall path is allowed to return.
/// Matches the plan §1.4 `LIMIT 64` so a corrupt `value_ciphertext`
/// row cannot blow the per-chat memory budget by an unbounded
/// decrypt loop. The top-K for the auto-injected system block is a
/// smaller number (`[memory].recalled_top_k`, default 10) — the
/// repository still caps the decryption set at this number so a
/// future API caller cannot use a smaller `limit` to bypass it.
pub const RECALL_HARD_LIMIT: usize = 64;

#[derive(Debug, Clone)]
pub enum Memories {
    Sqlite(sqlite::SqliteMemories),
    Postgres(postgres::PgMemories),
}

impl Memories {
    pub fn new(pool: &AnyPool) -> Self {
        match pool {
            AnyPool::Sqlite(p) => Self::Sqlite(sqlite::SqliteMemories::new(p.clone())),
            AnyPool::Postgres(p) => Self::Postgres(postgres::PgMemories::new(p.clone())),
        }
    }

    /// Scope every per-row method to a single `user_id`. The
    /// returned [`ScopedMemories`] drops the `user_id` argument on
    /// its per-row methods, so a route handler holding a scoped
    /// view cannot accidentally drop the `WHERE user_id = ?`
    /// filter.
    pub fn for_user(&self, user_id: Uuid) -> ScopedMemories {
        ScopedMemories {
            inner: self.clone(),
            user_id,
        }
    }

    /// Store one (already-encrypted) memory for `user_id`.
    ///
    /// Enforces the dedup-by-value contract from plan §1.3:
    /// `(user_id, subject, predicate)` is treated as a logical
    /// triple and the row's `value_nonce` / `value_ciphertext` are
    /// replaced atomically when the same subject+predicate pair
    /// already exists for this user. The existing row's
    /// `confidence` is preserved unless the new request carries an
    /// explicit value, in which case the caller's value wins (so a
    /// store with `confidence = 1.0` keeps the row fresh, while a
    /// store that omits the field refreshes only the value).
    pub async fn upsert(&self, user_id: Uuid, req: NewMemoryRequest) -> Result<Uuid, Error> {
        match self {
            Memories::Sqlite(s) => s.upsert(user_id, req).await,
            Memories::Postgres(s) => s.upsert(user_id, req).await,
        }
    }

    /// Recall up to `limit` rows for `user_id`, optionally filtered
    /// by `subject`, `predicate`, and `tags` (each a case-
    /// insensitive `LIKE` pattern with `%` wildcards). The returned
    /// rows carry raw ciphertext — decryption is the caller's job
    /// (the `MemorySource` adapter in `nagent-server`).
    ///
    /// The recall query bumps `last_used_at` to the current
    /// timestamp on every row it surfaces (plan §1.4); the bump
    /// happens in the same transaction so the audit / ranking stay
    /// consistent.
    pub async fn recall(
        &self,
        user_id: Uuid,
        subject: Option<&str>,
        predicate: Option<&str>,
        tags: Option<&str>,
        limit: usize,
    ) -> Result<Vec<MemoryRow>, Error> {
        let bounded = limit.min(RECALL_HARD_LIMIT);
        match self {
            Memories::Sqlite(s) => s.recall(user_id, subject, predicate, tags, bounded).await,
            Memories::Postgres(s) => s.recall(user_id, subject, predicate, tags, bounded).await,
        }
    }

    /// List metadata for `user_id`, newest first. Powers
    /// `GET /api/memories` (the Settings-tab audit list) and the
    /// `memory_list` agent. Metadata-only — the row does NOT
    /// include the encrypted bytes.
    pub async fn list_meta(&self, user_id: Uuid, limit: usize) -> Result<Vec<MemoryMeta>, Error> {
        let bounded = limit.min(RECALL_HARD_LIMIT);
        match self {
            Memories::Sqlite(s) => s.list_meta(user_id, bounded).await,
            Memories::Postgres(s) => s.list_meta(user_id, bounded).await,
        }
    }

    /// Forget one memory by id. The cross-user attempt
    /// (forgetting another user's row) is refused with `Ok(0)` so
    /// a route handler cannot tell "exists but other user" from
    /// "does not exist" (matches the `Credentials` cross-user
    /// pattern — see plan §4 "Risks and mitigations").
    pub async fn forget(&self, user_id: Uuid, id: Uuid) -> Result<u64, Error> {
        match self {
            Memories::Sqlite(s) => s.forget(user_id, id).await,
            Memories::Postgres(s) => s.forget(user_id, id).await,
        }
    }
}

/// Per-user scoped view over [`Memories`].
///
/// Per-row methods do not take a `user_id` argument; the filter is
/// fixed at construction.
#[derive(Debug, Clone)]
pub struct ScopedMemories {
    inner: Memories,
    user_id: Uuid,
}

impl ScopedMemories {
    pub fn user_id(&self) -> Uuid {
        self.user_id
    }

    /// Store one memory. Dedup-by-value: replaces the existing row
    /// when `(user_id, subject, predicate)` already matches.
    pub async fn upsert(&self, req: NewMemoryRequest) -> Result<Uuid, Error> {
        self.inner.upsert(self.user_id, req).await
    }

    /// Recall up to `limit` rows. See [`Memories::recall`].
    pub async fn recall(
        &self,
        subject: Option<&str>,
        predicate: Option<&str>,
        tags: Option<&str>,
        limit: usize,
    ) -> Result<Vec<MemoryRow>, Error> {
        self.inner
            .recall(self.user_id, subject, predicate, tags, limit)
            .await
    }

    /// List metadata for the bound user. See [`Memories::list_meta`].
    pub async fn list_meta(&self, limit: usize) -> Result<Vec<MemoryMeta>, Error> {
        self.inner.list_meta(self.user_id, limit).await
    }

    /// Forget one memory. Returns the affected-row count; `Ok(0)`
    /// means either "no such row" or "another user's row".
    pub async fn forget(&self, id: Uuid) -> Result<u64, Error> {
        self.inner.forget(self.user_id, id).await
    }
}

pub(crate) mod sqlite {
    use sqlx::{Row, SqlitePool};
    use uuid::Uuid;

    use crate::error::Error;
    use crate::types::{MemoryMeta, MemoryRow, NewMemoryRequest};

    #[derive(Clone, Debug)]
    pub struct SqliteMemories {
        pub pool: SqlitePool,
    }

    impl SqliteMemories {
        pub(crate) fn new(pool: SqlitePool) -> Self {
            Self { pool }
        }

        pub async fn upsert(&self, user_id: Uuid, req: NewMemoryRequest) -> Result<Uuid, Error> {
            let user_id_str = user_id.to_string();
            // Try INSERT. On the unique `(user_id, subject,
            // predicate)` collision we UPDATE in place to preserve
            // the row id (so callers that capture the returned id
            // before the call can still re-query by it).
            let new_id = Uuid::new_v4();
            let existing_id: Option<String> = sqlx::query(
                "SELECT id FROM memories \
                 WHERE user_id = ? AND subject = ? AND predicate = ?",
            )
            .bind(&user_id_str)
            .bind(&req.subject)
            .bind(&req.predicate)
            .fetch_optional(&self.pool)
            .await?
            .map(|r| r.try_get::<String, _>("id"))
            .transpose()?;

            match existing_id {
                Some(id) => {
                    sqlx::query(
                        "UPDATE memories SET \
                            value_nonce = ?, value_ciphertext = ?, \
                            notes_nonce = ?, notes_ciphertext = ?, \
                            tags = ?, confidence = ?, \
                            source_session_id = ?, source_kind = ?, \
                            last_used_at = NULL \
                     WHERE id = ?",
                    )
                    .bind(&req.value_nonce)
                    .bind(&req.value_ciphertext)
                    .bind(req.notes_nonce.as_deref())
                    .bind(req.notes_ciphertext.as_deref())
                    .bind(&req.tags)
                    .bind(req.confidence)
                    .bind(req.source_session_id.as_deref())
                    .bind(&req.source_kind)
                    .bind(&id)
                    .execute(&self.pool)
                    .await?;
                    Ok(Uuid::parse_str(&id)?)
                }
                None => {
                    sqlx::query(
                        "INSERT INTO memories \
                            (id, user_id, subject, predicate, \
                             value_nonce, value_ciphertext, \
                             notes_nonce, notes_ciphertext, \
                             tags, confidence, \
                             source_session_id, source_kind) \
                         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                    )
                    .bind(new_id.to_string())
                    .bind(&user_id_str)
                    .bind(&req.subject)
                    .bind(&req.predicate)
                    .bind(&req.value_nonce)
                    .bind(&req.value_ciphertext)
                    .bind(req.notes_nonce.as_deref())
                    .bind(req.notes_ciphertext.as_deref())
                    .bind(&req.tags)
                    .bind(req.confidence)
                    .bind(req.source_session_id.as_deref())
                    .bind(&req.source_kind)
                    .execute(&self.pool)
                    .await?;
                    Ok(new_id)
                }
            }
        }

        pub async fn recall(
            &self,
            user_id: Uuid,
            subject: Option<&str>,
            predicate: Option<&str>,
            tags: Option<&str>,
            limit: usize,
        ) -> Result<Vec<MemoryRow>, Error> {
            let user_id_str = user_id.to_string();
            // Escape `\`, `%`, `_` so a caller-supplied filter
            // cannot poison the `LIKE` pattern (defence in depth;
            // the agent-side JSON contract already constrains the
            // values, but the repository must not trust it).
            let subject_pat: Option<String> = subject.map(escape_like);
            let predicate_pat: Option<String> = predicate.map(escape_like);
            let tags_pat: Option<String> = tags.map(escape_like);
            let subj_ref = subject_pat.as_deref();
            let pred_ref = predicate_pat.as_deref();
            let tags_ref = tags_pat.as_deref();

            // SQLite lacks a boolean type — the LIKE match is
            // expressed as `(pat IS NULL OR col LIKE pat ESCAPE '\')`.
            let rows = sqlx::query(
                "SELECT id, subject, predicate, \
                        value_nonce, value_ciphertext, \
                        notes_nonce, notes_ciphertext, \
                        tags, confidence, \
                        source_session_id, source_kind, \
                        created_at, last_used_at, expires_at \
                 FROM memories \
                 WHERE user_id = ? \
                   AND (? IS NULL OR subject LIKE ? ESCAPE '\\') \
                   AND (? IS NULL OR predicate LIKE ? ESCAPE '\\') \
                   AND (? IS NULL OR tags LIKE ? ESCAPE '\\') \
                 ORDER BY confidence DESC, last_used_at DESC NULLS LAST, created_at DESC \
                 LIMIT ?",
            )
            .bind(&user_id_str)
            .bind(subj_ref)
            .bind(subj_ref)
            .bind(pred_ref)
            .bind(pred_ref)
            .bind(tags_ref)
            .bind(tags_ref)
            .bind(limit as i64)
            .fetch_all(&self.pool)
            .await?;

            let parsed = rows
                .into_iter()
                .map(|r| {
                    Ok::<MemoryRow, Error>(MemoryRow {
                        id: Uuid::parse_str(r.try_get::<String, _>("id")?.as_str())?,
                        subject: r.try_get("subject")?,
                        predicate: r.try_get("predicate")?,
                        value_nonce: r.try_get("value_nonce")?,
                        value_ciphertext: r.try_get("value_ciphertext")?,
                        notes_nonce: r.try_get("notes_nonce")?,
                        notes_ciphertext: r.try_get("notes_ciphertext")?,
                        tags: r.try_get("tags")?,
                        confidence: r.try_get::<f64, _>("confidence")? as f32,
                        source_session_id: r.try_get("source_session_id")?,
                        source_kind: r.try_get("source_kind")?,
                        created_at: parse_rfc3339(&r.try_get::<String, _>("created_at")?),
                        last_used_at: r
                            .try_get::<Option<String>, _>("last_used_at")?
                            .map(|s| parse_rfc3339(&s)),
                        expires_at: r
                            .try_get::<Option<String>, _>("expires_at")?
                            .map(|s| parse_rfc3339(&s)),
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;

            // Bump `last_used_at` on the rows we just returned —
            // one UPDATE per id keeps the rank-bucket path correct
            // without holding a long-lived transaction.
            if !parsed.is_empty() {
                let ids: Vec<String> = parsed.iter().map(|r| r.id.to_string()).collect();
                let placeholders = std::iter::repeat_n("?", ids.len())
                    .collect::<Vec<_>>()
                    .join(",");
                let sql = format!(
                    "UPDATE memories SET last_used_at = CURRENT_TIMESTAMP \
                     WHERE user_id = ? AND id IN ({})",
                    placeholders
                );
                let mut q = sqlx::query(&sql).bind(&user_id_str);
                for id in &ids {
                    q = q.bind(id);
                }
                q.execute(&self.pool).await?;
            }
            Ok(parsed)
        }

        pub async fn list_meta(
            &self,
            user_id: Uuid,
            limit: usize,
        ) -> Result<Vec<MemoryMeta>, Error> {
            let rows = sqlx::query(
                "SELECT id, subject, predicate, tags, confidence, \
                        source_session_id, source_kind, \
                        created_at, last_used_at, expires_at \
                 FROM memories \
                 WHERE user_id = ? \
                 ORDER BY created_at DESC, id DESC \
                 LIMIT ?",
            )
            .bind(user_id.to_string())
            .bind(limit as i64)
            .fetch_all(&self.pool)
            .await?;
            rows.into_iter()
                .map(|r| {
                    Ok(MemoryMeta {
                        id: Uuid::parse_str(r.try_get::<String, _>("id")?.as_str())?,
                        subject: r.try_get("subject")?,
                        predicate: r.try_get("predicate")?,
                        tags: r.try_get("tags")?,
                        confidence: r.try_get::<f64, _>("confidence")? as f32,
                        source_session_id: r.try_get("source_session_id")?,
                        source_kind: r.try_get("source_kind")?,
                        created_at: parse_rfc3339(&r.try_get::<String, _>("created_at")?),
                        last_used_at: r
                            .try_get::<Option<String>, _>("last_used_at")?
                            .map(|s| parse_rfc3339(&s)),
                        expires_at: r
                            .try_get::<Option<String>, _>("expires_at")?
                            .map(|s| parse_rfc3339(&s)),
                    })
                })
                .collect::<Result<Vec<_>, _>>()
        }

        pub async fn forget(&self, user_id: Uuid, id: Uuid) -> Result<u64, Error> {
            let res = sqlx::query("DELETE FROM memories WHERE user_id = ? AND id = ?")
                .bind(user_id.to_string())
                .bind(id.to_string())
                .execute(&self.pool)
                .await?;
            Ok(res.rows_affected())
        }
    }

    fn escape_like(s: &str) -> String {
        // Escape `\`, `%`, `_` so a caller-supplied LIKE pattern
        // cannot match more rows than asked for. The pair
        // (`ESCAPE '\'` in the SQL, `\` escapes here) is the SQLite
        // default.
        let mut out = String::with_capacity(s.len());
        for c in s.chars() {
            match c {
                '\\' | '%' | '_' => {
                    out.push('\\');
                    out.push(c);
                }
                other => out.push(other),
            }
        }
        out
    }

    fn parse_rfc3339(s: &str) -> chrono::DateTime<chrono::Utc> {
        use chrono::{DateTime, Utc};
        DateTime::parse_from_rfc3339(s)
            .map(|d| d.with_timezone(&Utc))
            .unwrap_or_else(|_| Utc::now())
    }
}

pub(crate) mod postgres {
    use sqlx::{PgPool, Row};
    use uuid::Uuid;

    use crate::error::Error;
    use crate::types::{MemoryMeta, MemoryRow, NewMemoryRequest};

    #[derive(Clone, Debug)]
    pub struct PgMemories {
        pub pool: PgPool,
    }

    impl PgMemories {
        pub(crate) fn new(pool: PgPool) -> Self {
            Self { pool }
        }

        pub async fn upsert(&self, user_id: Uuid, req: NewMemoryRequest) -> Result<Uuid, Error> {
            // Postgres lacks `INSERT OR REPLACE`; emulate via
            // `WITH existing AS (SELECT …) UPDATE … RETURNING id
            // ELSE INSERT … RETURNING id` in a single statement so
            // the dedup + write stays atomic.
            let row = sqlx::query(
                "WITH existing AS ( \
                     SELECT id FROM memories \
                     WHERE user_id = $1 AND subject = $2 AND predicate = $3 \
                 ) \
                 UPDATE memories SET \
                     value_nonce = $4, value_ciphertext = $5, \
                     notes_nonce = $6, notes_ciphertext = $7, \
                     tags = $8, confidence = $9, \
                     source_session_id = $10, source_kind = $11, \
                     last_used_at = NULL \
                 WHERE id IN (SELECT id FROM existing) \
                 RETURNING id",
            )
            .bind(user_id)
            .bind(&req.subject)
            .bind(&req.predicate)
            .bind(&req.value_nonce)
            .bind(&req.value_ciphertext)
            .bind(req.notes_nonce.as_deref())
            .bind(req.notes_ciphertext.as_deref())
            .bind(&req.tags)
            .bind(req.confidence)
            .bind(req.source_session_id.as_deref())
            .bind(&req.source_kind)
            .fetch_optional(&self.pool)
            .await?;

            if let Some(r) = row {
                let id: String = r.try_get("id")?;
                return Ok(Uuid::parse_str(&id)?);
            }
            let new_id = Uuid::new_v4();
            sqlx::query(
                "INSERT INTO memories \
                    (id, user_id, subject, predicate, \
                     value_nonce, value_ciphertext, \
                     notes_nonce, notes_ciphertext, \
                     tags, confidence, \
                     source_session_id, source_kind) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)",
            )
            .bind(new_id)
            .bind(user_id)
            .bind(&req.subject)
            .bind(&req.predicate)
            .bind(&req.value_nonce)
            .bind(&req.value_ciphertext)
            .bind(req.notes_nonce.as_deref())
            .bind(req.notes_ciphertext.as_deref())
            .bind(&req.tags)
            .bind(req.confidence)
            .bind(req.source_session_id.as_deref())
            .bind(&req.source_kind)
            .execute(&self.pool)
            .await?;
            Ok(new_id)
        }

        pub async fn recall(
            &self,
            user_id: Uuid,
            subject: Option<&str>,
            predicate: Option<&str>,
            tags: Option<&str>,
            limit: usize,
        ) -> Result<Vec<MemoryRow>, Error> {
            let subject_pat = subject.map(escape_like);
            let predicate_pat = predicate.map(escape_like);
            let tags_pat = tags.map(escape_like);

            let rows = sqlx::query(
                "SELECT id, subject, predicate, \
                        value_nonce, value_ciphertext, \
                        notes_nonce, notes_ciphertext, \
                        tags, confidence, \
                        source_session_id, source_kind, \
                        created_at, last_used_at, expires_at \
                 FROM memories \
                 WHERE user_id = $1 \
                   AND ($2::text IS NULL OR subject LIKE $2 ESCAPE '\\') \
                   AND ($3::text IS NULL OR predicate LIKE $3 ESCAPE '\\') \
                   AND ($4::text IS NULL OR tags LIKE $4 ESCAPE '\\') \
                 ORDER BY confidence DESC, last_used_at DESC NULLS LAST, created_at DESC \
                 LIMIT $5",
            )
            .bind(user_id)
            .bind(subject_pat.as_deref())
            .bind(predicate_pat.as_deref())
            .bind(tags_pat.as_deref())
            .bind(limit as i64)
            .fetch_all(&self.pool)
            .await?;

            let parsed = rows
                .into_iter()
                .map(|r| {
                    Ok::<MemoryRow, Error>(MemoryRow {
                        id: Uuid::parse_str(r.try_get::<String, _>("id")?.as_str())?,
                        subject: r.try_get("subject")?,
                        predicate: r.try_get("predicate")?,
                        value_nonce: r.try_get("value_nonce")?,
                        value_ciphertext: r.try_get("value_ciphertext")?,
                        notes_nonce: r.try_get("notes_nonce")?,
                        notes_ciphertext: r.try_get("notes_ciphertext")?,
                        tags: r.try_get("tags")?,
                        confidence: r.try_get::<f64, _>("confidence")? as f32,
                        source_session_id: r.try_get("source_session_id")?,
                        source_kind: r.try_get("source_kind")?,
                        created_at: parse_rfc3339(&r.try_get::<String, _>("created_at")?),
                        last_used_at: r
                            .try_get::<Option<String>, _>("last_used_at")?
                            .map(|s| parse_rfc3339(&s)),
                        expires_at: r
                            .try_get::<Option<String>, _>("expires_at")?
                            .map(|s| parse_rfc3339(&s)),
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;

            if !parsed.is_empty() {
                let ids: Vec<Uuid> = parsed.iter().map(|r| r.id).collect();
                sqlx::query(
                    "UPDATE memories SET last_used_at = CURRENT_TIMESTAMP \
                     WHERE user_id = $1 AND id = ANY($2::uuid[])",
                )
                .bind(user_id)
                .bind(&ids)
                .execute(&self.pool)
                .await?;
            }
            Ok(parsed)
        }

        pub async fn list_meta(
            &self,
            user_id: Uuid,
            limit: usize,
        ) -> Result<Vec<MemoryMeta>, Error> {
            let rows = sqlx::query(
                "SELECT id, subject, predicate, tags, confidence, \
                        source_session_id, source_kind, \
                        created_at, last_used_at, expires_at \
                 FROM memories \
                 WHERE user_id = $1 \
                 ORDER BY created_at DESC, id DESC \
                 LIMIT $2",
            )
            .bind(user_id)
            .bind(limit as i64)
            .fetch_all(&self.pool)
            .await?;
            rows.into_iter()
                .map(|r| {
                    Ok(MemoryMeta {
                        id: Uuid::parse_str(r.try_get::<String, _>("id")?.as_str())?,
                        subject: r.try_get("subject")?,
                        predicate: r.try_get("predicate")?,
                        tags: r.try_get("tags")?,
                        confidence: r.try_get::<f64, _>("confidence")? as f32,
                        source_session_id: r.try_get("source_session_id")?,
                        source_kind: r.try_get("source_kind")?,
                        created_at: parse_rfc3339(&r.try_get::<String, _>("created_at")?),
                        last_used_at: r
                            .try_get::<Option<String>, _>("last_used_at")?
                            .map(|s| parse_rfc3339(&s)),
                        expires_at: r
                            .try_get::<Option<String>, _>("expires_at")?
                            .map(|s| parse_rfc3339(&s)),
                    })
                })
                .collect::<Result<Vec<_>, _>>()
        }

        pub async fn forget(&self, user_id: Uuid, id: Uuid) -> Result<u64, Error> {
            let res = sqlx::query("DELETE FROM memories WHERE user_id = $1 AND id = $2")
                .bind(user_id)
                .bind(id)
                .execute(&self.pool)
                .await?;
            Ok(res.rows_affected())
        }
    }

    fn escape_like(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        for c in s.chars() {
            match c {
                '\\' | '%' | '_' => {
                    out.push('\\');
                    out.push(c);
                }
                other => out.push(other),
            }
        }
        out
    }

    fn parse_rfc3339(s: &str) -> chrono::DateTime<chrono::Utc> {
        use chrono::{DateTime, Utc};
        DateTime::parse_from_rfc3339(s)
            .map(|d| d.with_timezone(&Utc))
            .unwrap_or_else(|_| Utc::now())
    }
}
