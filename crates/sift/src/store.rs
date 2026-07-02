//! Log + template storage.
//!
//! `Store` is a small async trait with an in-memory and a PostgreSQL implementation, mirroring
//! the keystone/inkwell seam: handlers depend only on the trait, so a FusionDB-backed store can
//! drop in later. The PostgreSQL layer uses ONLY portable standard SQL (TEXT/BIGINT, PK/UNIQUE/
//! NOT NULL/DEFAULT, `INSERT`, `UPDATE`, `CREATE INDEX`, `LOWER(..) LIKE ..`) and runtime queries
//! (no compile-time macros), so the build needs NO database and the same statements later run
//! unchanged on FusionDB over pgwire.
//!
//! The methods are `async`: the axum handlers + the syslog listeners `.await` them directly, and
//! `PgStore` drives sqlx natively — there is NO `block_in_place` and NO sync-over-async bridge.
//! Template upserts (SELECT-then-INSERT/UPDATE, which must report whether the template is brand
//! new) are serialized by a `tokio::sync::Mutex` so two concurrent ingests can never race on the
//! same `template_id`.

use std::sync::Mutex;

use async_trait::async_trait;
use serde::Serialize;
use thiserror::Error;

use crate::config::SEARCH_LIMIT;

/// One log line (maps 1:1 to a `logs` row).
#[derive(Clone, Debug, Serialize)]
pub struct LogEntry {
    pub id: String,
    pub ts: i64,
    pub host: String,
    pub app: String,
    pub severity: String,
    pub message: String,
    pub template_id: String,
}

/// One message template (maps 1:1 to a `templates` row).
#[derive(Clone, Debug, Serialize)]
pub struct Template {
    pub id: String,
    pub pattern: String,
    pub sample: String,
    pub count: i64,
    pub first_seen: i64,
    pub last_seen: i64,
}

/// Search/filter criteria for the dashboard + `/api/search`. Every field is optional; an unset
/// field does not constrain the result.
#[derive(Clone, Debug, Default)]
pub struct SearchFilter {
    /// Case-insensitive substring match on `message` (the `LIKE` half of the search).
    pub q: Option<String>,
    /// Exact source-host match. The dashboard also accepts `source=` as an alias for this field.
    pub host: Option<String>,
    pub app: Option<String>,
    pub severity: Option<String>,
    pub template_id: Option<String>,
    /// Inclusive lower/upper bounds on `ts` (epoch seconds).
    pub since: Option<i64>,
    pub until: Option<i64>,
    /// Strict keyset cursor for newest-first pages: return rows older than `(before_ts, before_id)`.
    pub before_ts: Option<i64>,
    pub before_id: Option<String>,
    /// Row cap (clamped to [`SEARCH_LIMIT`] by the caller).
    pub limit: usize,
}

/// Storage failure surfaced to the handler layer (always a 500).
#[derive(Debug, Error)]
pub enum StoreError {
    #[error("store error: {0}")]
    Backend(String),
}

/// Pluggable log store.
#[async_trait]
pub trait Store: Send + Sync {
    /// Insert one log row. Ids are unique, so this never conflicts.
    async fn insert_log(&self, log: &LogEntry) -> Result<(), StoreError>;

    /// Upsert a template: insert it (count = 1) when its id is new, otherwise bump `count` and
    /// advance `last_seen`. Returns `true` IFF the template was brand new (the caller emits the
    /// `sift.unknown_template` audit event in that case).
    async fn upsert_template(&self, tmpl: &Template) -> Result<bool, StoreError>;

    /// Filtered log rows, newest-first (`ts` DESC), capped at `filter.limit`.
    async fn search(&self, filter: &SearchFilter) -> Result<Vec<LogEntry>, StoreError>;

    /// Top templates by `count` DESC, capped at `limit`.
    async fn top_templates(&self, limit: usize) -> Result<Vec<Template>, StoreError>;

    /// One template by id (used to label a `?template_id=` filtered view).
    async fn get_template(&self, id: &str) -> Result<Option<Template>, StoreError>;

    /// Total stored log rows (a dashboard headline stat).
    async fn count_logs(&self) -> Result<i64, StoreError>;

    /// Total distinct templates (a dashboard headline stat).
    async fn count_templates(&self) -> Result<i64, StoreError>;
}

// --------------------------------------------------------------------------------------
// In-memory store (the default; keeps the whole service database-free for dev + tests).
// --------------------------------------------------------------------------------------

#[derive(Default)]
pub struct InMemoryStore {
    logs: Mutex<Vec<LogEntry>>,
    templates: Mutex<Vec<Template>>,
}

impl InMemoryStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl Store for InMemoryStore {
    // The std `Mutex` is fine throughout: each critical section is fully synchronous (no `.await`
    // inside), so a guard is never held across a yield point.
    async fn insert_log(&self, log: &LogEntry) -> Result<(), StoreError> {
        self.logs
            .lock()
            .expect("logs lock poisoned")
            .push(log.clone());
        Ok(())
    }

    async fn upsert_template(&self, tmpl: &Template) -> Result<bool, StoreError> {
        let mut templates = self.templates.lock().expect("templates lock poisoned");
        match templates.iter_mut().find(|t| t.id == tmpl.id) {
            Some(existing) => {
                existing.count += 1;
                existing.last_seen = tmpl.last_seen;
                Ok(false)
            }
            None => {
                templates.push(tmpl.clone());
                Ok(true)
            }
        }
    }

    async fn search(&self, filter: &SearchFilter) -> Result<Vec<LogEntry>, StoreError> {
        let logs = self.logs.lock().expect("logs lock poisoned");
        let q_lower = filter.q.as_ref().map(|s| s.to_lowercase());
        let mut hits: Vec<LogEntry> = logs
            .iter()
            .filter(|l| match_filter(l, filter, q_lower.as_deref()))
            .cloned()
            .collect();
        // Newest-first; ties broken by id so output is stable.
        hits.sort_by(|a, b| b.ts.cmp(&a.ts).then_with(|| b.id.cmp(&a.id)));
        hits.truncate(filter.limit.min(SEARCH_LIMIT));
        Ok(hits)
    }

    async fn top_templates(&self, limit: usize) -> Result<Vec<Template>, StoreError> {
        let templates = self.templates.lock().expect("templates lock poisoned");
        let mut v: Vec<Template> = templates.clone();
        v.sort_by(|a, b| {
            b.count
                .cmp(&a.count)
                .then_with(|| b.last_seen.cmp(&a.last_seen))
        });
        v.truncate(limit);
        Ok(v)
    }

    async fn get_template(&self, id: &str) -> Result<Option<Template>, StoreError> {
        Ok(self
            .templates
            .lock()
            .expect("templates lock poisoned")
            .iter()
            .find(|t| t.id == id)
            .cloned())
    }

    async fn count_logs(&self) -> Result<i64, StoreError> {
        Ok(self.logs.lock().expect("logs lock poisoned").len() as i64)
    }

    async fn count_templates(&self) -> Result<i64, StoreError> {
        Ok(self
            .templates
            .lock()
            .expect("templates lock poisoned")
            .len() as i64)
    }
}

/// True when `log` satisfies every set field of `filter`. `q_lower` is the pre-lowercased query.
fn match_filter(log: &LogEntry, f: &SearchFilter, q_lower: Option<&str>) -> bool {
    if let Some(q) = q_lower {
        if !log.message.to_lowercase().contains(q) {
            return false;
        }
    }
    if let Some(host) = &f.host {
        if &log.host != host {
            return false;
        }
    }
    if let Some(app) = &f.app {
        if &log.app != app {
            return false;
        }
    }
    if let Some(sev) = &f.severity {
        if &log.severity != sev {
            return false;
        }
    }
    if let Some(tid) = &f.template_id {
        if &log.template_id != tid {
            return false;
        }
    }
    if let Some(since) = f.since {
        if log.ts < since {
            return false;
        }
    }
    if let Some(until) = f.until {
        if log.ts > until {
            return false;
        }
    }
    if let (Some(before_ts), Some(before_id)) = (f.before_ts, f.before_id.as_ref()) {
        if !(log.ts < before_ts || (log.ts == before_ts && log.id.as_str() < before_id.as_str())) {
            return false;
        }
    }
    true
}

// --------------------------------------------------------------------------------------
// PostgreSQL-backed store (portable: standard SQL, runtime queries, no macros).
// --------------------------------------------------------------------------------------
//
// Selected at runtime by `SIFT_STORE=postgres`. Each method drives sqlx natively and the callers
// `.await` it — NO `block_in_place`, NO sync-over-async. Template upserts are serialized by an
// in-process `tokio::sync::Mutex` so the SELECT-then-INSERT/UPDATE is atomic and two ingests
// never race for the same `template_id`.

use sqlx::postgres::{PgPool, PgPoolOptions};
use sqlx::{QueryBuilder, Row};

/// PostgreSQL-backed [`Store`]. Holds a `PgPool` + the template-upsert serial guard.
pub struct PgStore {
    pool: PgPool,
    upsert_guard: tokio::sync::Mutex<()>,
}

impl PgStore {
    /// Open a pooled connection. Async; call from within a Tokio runtime.
    pub async fn connect(database_url: &str) -> Result<Self, sqlx::Error> {
        let pool = PgPoolOptions::new()
            .max_connections(8)
            .connect(database_url)
            .await?;
        Ok(Self::from_pool(pool))
    }

    /// Construct from an existing pool (used by tests that share a pool).
    pub fn from_pool(pool: PgPool) -> Self {
        Self {
            pool,
            upsert_guard: tokio::sync::Mutex::new(()),
        }
    }

    /// Idempotent, portable migration. Standard SQL only — safe to run on every startup.
    pub async fn migrate(&self) -> Result<(), sqlx::Error> {
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS logs (\
                 id TEXT PRIMARY KEY, \
                 ts BIGINT NOT NULL, \
                 host TEXT NOT NULL DEFAULT '', \
                 app TEXT NOT NULL DEFAULT '', \
                 severity TEXT NOT NULL DEFAULT 'info', \
                 message TEXT NOT NULL, \
                 template_id TEXT NOT NULL DEFAULT ''\
             )",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS templates (\
                 id TEXT PRIMARY KEY, \
                 pattern TEXT NOT NULL, \
                 sample TEXT NOT NULL, \
                 count BIGINT NOT NULL DEFAULT 0, \
                 first_seen BIGINT NOT NULL, \
                 last_seen BIGINT NOT NULL\
             )",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_logs_ts ON logs (ts)")
            .execute(&self.pool)
            .await?;
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_logs_keyset ON logs (ts, id)")
            .execute(&self.pool)
            .await?;
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_logs_host ON logs (host)")
            .execute(&self.pool)
            .await?;
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_logs_app ON logs (app)")
            .execute(&self.pool)
            .await?;
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_logs_template_id ON logs (template_id)")
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    fn log_from_row(row: &sqlx::postgres::PgRow) -> Result<LogEntry, sqlx::Error> {
        Ok(LogEntry {
            id: row.try_get("id")?,
            ts: row.try_get("ts")?,
            host: row.try_get("host")?,
            app: row.try_get("app")?,
            severity: row.try_get("severity")?,
            message: row.try_get("message")?,
            template_id: row.try_get("template_id")?,
        })
    }

    fn template_from_row(row: &sqlx::postgres::PgRow) -> Result<Template, sqlx::Error> {
        Ok(Template {
            id: row.try_get("id")?,
            pattern: row.try_get("pattern")?,
            sample: row.try_get("sample")?,
            count: row.try_get("count")?,
            first_seen: row.try_get("first_seen")?,
            last_seen: row.try_get("last_seen")?,
        })
    }

    async fn insert_log_async(&self, l: &LogEntry) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO logs (id, ts, host, app, severity, message, template_id) \
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(&l.id)
        .bind(l.ts)
        .bind(&l.host)
        .bind(&l.app)
        .bind(&l.severity)
        .bind(&l.message)
        .bind(&l.template_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn upsert_template_async(&self, t: &Template) -> Result<bool, sqlx::Error> {
        // Serialize the read-modify-write so `is_new` is accurate and no two ingests race.
        let _guard = self.upsert_guard.lock().await;
        let existing = sqlx::query("SELECT id FROM templates WHERE id = $1")
            .bind(&t.id)
            .fetch_optional(&self.pool)
            .await?;
        if existing.is_some() {
            sqlx::query("UPDATE templates SET count = count + 1, last_seen = $1 WHERE id = $2")
                .bind(t.last_seen)
                .bind(&t.id)
                .execute(&self.pool)
                .await?;
            Ok(false)
        } else {
            sqlx::query(
                "INSERT INTO templates (id, pattern, sample, count, first_seen, last_seen) \
                 VALUES ($1, $2, $3, $4, $5, $6)",
            )
            .bind(&t.id)
            .bind(&t.pattern)
            .bind(&t.sample)
            .bind(1_i64)
            .bind(t.first_seen)
            .bind(t.last_seen)
            .execute(&self.pool)
            .await?;
            Ok(true)
        }
    }

    async fn search_async(&self, f: &SearchFilter) -> Result<Vec<LogEntry>, sqlx::Error> {
        // Built with `QueryBuilder` so every value is a bound parameter (portable, injection-safe)
        // and only the set filters appear in the WHERE clause.
        let mut qb: QueryBuilder<sqlx::Postgres> = QueryBuilder::new(
            "SELECT id, ts, host, app, severity, message, template_id FROM logs WHERE 1 = 1",
        );
        if let Some(host) = &f.host {
            qb.push(" AND host = ").push_bind(host);
        }
        if let Some(app) = &f.app {
            qb.push(" AND app = ").push_bind(app);
        }
        if let Some(sev) = &f.severity {
            qb.push(" AND severity = ").push_bind(sev);
        }
        if let Some(tid) = &f.template_id {
            qb.push(" AND template_id = ").push_bind(tid);
        }
        if let Some(since) = f.since {
            qb.push(" AND ts >= ").push_bind(since);
        }
        if let Some(until) = f.until {
            qb.push(" AND ts <= ").push_bind(until);
        }
        if let Some(q) = &f.q {
            let like = format!("%{}%", q.to_lowercase());
            qb.push(" AND LOWER(message) LIKE ").push_bind(like);
        }
        if let (Some(before_ts), Some(before_id)) = (f.before_ts, f.before_id.as_ref()) {
            qb.push(" AND (ts < ")
                .push_bind(before_ts)
                .push(" OR (ts = ")
                .push_bind(before_ts)
                .push(" AND id < ")
                .push_bind(before_id)
                .push("))");
        }
        qb.push(" ORDER BY ts DESC, id DESC LIMIT ")
            .push_bind(f.limit.min(SEARCH_LIMIT) as i64);
        let rows = qb.build().fetch_all(&self.pool).await?;
        rows.iter().map(Self::log_from_row).collect()
    }

    async fn top_templates_async(&self, limit: usize) -> Result<Vec<Template>, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT id, pattern, sample, count, first_seen, last_seen \
             FROM templates ORDER BY count DESC, last_seen DESC LIMIT $1",
        )
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(Self::template_from_row).collect()
    }

    async fn get_template_async(&self, id: &str) -> Result<Option<Template>, sqlx::Error> {
        let row = sqlx::query(
            "SELECT id, pattern, sample, count, first_seen, last_seen FROM templates WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        match row {
            Some(r) => Ok(Some(Self::template_from_row(&r)?)),
            None => Ok(None),
        }
    }

    async fn count_async(&self, table: &str) -> Result<i64, sqlx::Error> {
        // `table` is a fixed internal literal (`logs` | `templates`), never user input.
        let sql = format!("SELECT COUNT(*) AS n FROM {table}");
        let row = sqlx::query(&sql).fetch_one(&self.pool).await?;
        row.try_get("n")
    }
}

#[async_trait]
impl Store for PgStore {
    async fn insert_log(&self, log: &LogEntry) -> Result<(), StoreError> {
        self.insert_log_async(log)
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))
    }

    async fn upsert_template(&self, tmpl: &Template) -> Result<bool, StoreError> {
        self.upsert_template_async(tmpl)
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))
    }

    async fn search(&self, filter: &SearchFilter) -> Result<Vec<LogEntry>, StoreError> {
        self.search_async(filter)
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))
    }

    async fn top_templates(&self, limit: usize) -> Result<Vec<Template>, StoreError> {
        self.top_templates_async(limit)
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))
    }

    async fn get_template(&self, id: &str) -> Result<Option<Template>, StoreError> {
        self.get_template_async(id)
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))
    }

    async fn count_logs(&self) -> Result<i64, StoreError> {
        self.count_async("logs")
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))
    }

    async fn count_templates(&self) -> Result<i64, StoreError> {
        self.count_async("templates")
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn log(id: &str, ts: i64, app: &str, sev: &str, msg: &str, tid: &str) -> LogEntry {
        LogEntry {
            id: id.to_string(),
            ts,
            host: "h".to_string(),
            app: app.to_string(),
            severity: sev.to_string(),
            message: msg.to_string(),
            template_id: tid.to_string(),
        }
    }

    #[tokio::test]
    async fn upsert_reports_new_then_existing_and_counts() {
        let s = InMemoryStore::new();
        let t = Template {
            id: "t_1".to_string(),
            pattern: "<NUM> errors".to_string(),
            sample: "5 errors".to_string(),
            count: 1,
            first_seen: 10,
            last_seen: 10,
        };
        assert!(
            s.upsert_template(&t).await.unwrap(),
            "first sighting is new"
        );
        assert!(!s.upsert_template(&t).await.unwrap(), "second is not new");
        let top = s.top_templates(10).await.unwrap();
        assert_eq!(top.len(), 1);
        assert_eq!(top[0].count, 2);
    }

    #[tokio::test]
    async fn search_filters_and_orders_newest_first() {
        let s = InMemoryStore::new();
        s.insert_log(&log("a", 100, "web", "info", "user login ok", "t_a"))
            .await
            .unwrap();
        s.insert_log(&log("b", 200, "web", "error", "DB timeout", "t_b"))
            .await
            .unwrap();
        s.insert_log(&log("c", 150, "api", "info", "login retry", "t_a"))
            .await
            .unwrap();

        let f = SearchFilter {
            q: Some("login".to_string()),
            limit: 50,
            ..Default::default()
        };
        let hits = s.search(&f).await.unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].id, "c", "newest-first within the LIKE match");

        let f = SearchFilter {
            app: Some("web".to_string()),
            limit: 50,
            ..Default::default()
        };
        assert_eq!(s.search(&f).await.unwrap().len(), 2);

        let f = SearchFilter {
            host: Some("h".to_string()),
            limit: 50,
            ..Default::default()
        };
        assert_eq!(s.search(&f).await.unwrap().len(), 3);

        let f = SearchFilter {
            template_id: Some("t_a".to_string()),
            limit: 50,
            ..Default::default()
        };
        assert_eq!(s.search(&f).await.unwrap().len(), 2);

        let f = SearchFilter {
            since: Some(150),
            until: Some(150),
            limit: 50,
            ..Default::default()
        };
        let hits = s.search(&f).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, "c");
    }

    #[tokio::test]
    async fn search_keyset_pages_after_ts_and_id() {
        let s = InMemoryStore::new();
        s.insert_log(&log("a", 100, "web", "info", "oldest", "t_a"))
            .await
            .unwrap();
        s.insert_log(&log("b", 200, "web", "info", "middle", "t_b"))
            .await
            .unwrap();
        s.insert_log(&log("c", 300, "web", "info", "newest", "t_c"))
            .await
            .unwrap();
        s.insert_log(&log("d", 200, "web", "info", "same-second newer id", "t_d"))
            .await
            .unwrap();

        let f = SearchFilter {
            limit: 3,
            ..Default::default()
        };
        let first = s.search(&f).await.unwrap();
        assert_eq!(
            first.iter().map(|l| l.id.as_str()).collect::<Vec<_>>(),
            ["c", "d", "b"]
        );

        let f = SearchFilter {
            before_ts: Some(200),
            before_id: Some("b".to_string()),
            limit: 3,
            ..Default::default()
        };
        let next = s.search(&f).await.unwrap();
        assert_eq!(
            next.iter().map(|l| l.id.as_str()).collect::<Vec<_>>(),
            ["a"]
        );
    }
}
