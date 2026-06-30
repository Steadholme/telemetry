//! Span storage.
//!
//! `Store` is a small async trait with an in-memory and a PostgreSQL implementation, mirroring
//! the keystone/watchtower seam: handlers depend only on the trait, so a FusionDB-backed store
//! can drop in later. The PostgreSQL layer uses ONLY portable standard SQL (TEXT/BIGINT,
//! PRIMARY KEY/NOT NULL/DEFAULT, parameterized queries, `INSERT .. ON CONFLICT`, plain indexes)
//! and runtime queries (no compile-time macros), so the build needs NO database and the same
//! statements later run unchanged on FusionDB over pgwire.
//!
//! The methods are `async`: the axum handlers `.await` them directly on the serving runtime, and
//! `PgStore` drives sqlx natively — there is NO `block_in_place` and NO sync-over-async bridge, so
//! a DB round-trip never blocks a worker thread. Span ingest is idempotent: re-delivering a span
//! (same `span_id`) overwrites rather than duplicating, so a retrying collector is safe.

use std::sync::Mutex;

use async_trait::async_trait;
use thiserror::Error;

use crate::config::{SPAN_SCAN_LIMIT, TRACE_SPAN_LIMIT};

/// One span (maps 1:1 to a `spans` row).
#[derive(Clone, Debug, PartialEq)]
pub struct Span {
    pub span_id: String,
    pub trace_id: String,
    /// Empty string when this is a trace root (no parent).
    pub parent_id: String,
    pub name: String,
    pub service: String,
    /// Start time, epoch MICROSECONDS.
    pub start_us: i64,
    /// End time, epoch MICROSECONDS.
    pub end_us: i64,
    /// `ok` | `error` (anything non-`ok` is rendered as an error).
    pub status: String,
    /// Opaque, already-stringified attribute blob (kept as TEXT; never re-parsed for rendering).
    pub attrs: String,
}

impl Span {
    /// Duration in microseconds, clamped to `>= 0` (a malformed span with `end < start` reads 0).
    pub fn duration_us(&self) -> i64 {
        (self.end_us - self.start_us).max(0)
    }

    /// True when the span carries a non-`ok` status.
    pub fn is_error(&self) -> bool {
        !self.status.eq_ignore_ascii_case("ok") && !self.status.is_empty()
    }
}

/// Storage failure surfaced to the handler layer (mapped to a 500 `server_error`).
#[derive(Debug, Error)]
pub enum StoreError {
    #[error("store error: {0}")]
    Backend(String),
}

/// Pluggable span store.
#[async_trait]
pub trait Store: Send + Sync {
    /// Idempotently store a batch of spans (upsert by `span_id`). Returns the number accepted.
    async fn ingest_spans(&self, spans: &[Span]) -> Result<usize, StoreError>;

    /// A bounded window of the most recent spans (by `start_us` DESC, capped at
    /// [`SPAN_SCAN_LIMIT`]) — the input the dashboard groups into traces.
    async fn recent_spans(&self) -> Result<Vec<Span>, StoreError>;

    /// Every span of one trace, ordered by `start_us` ASC, capped at [`TRACE_SPAN_LIMIT`] — the
    /// input to the waterfall.
    async fn trace_spans(&self, trace_id: &str) -> Result<Vec<Span>, StoreError>;
}

// --------------------------------------------------------------------------------------
// In-memory store (the default; keeps the whole service database-free for dev + tests).
// --------------------------------------------------------------------------------------

#[derive(Default)]
pub struct InMemoryStore {
    spans: Mutex<Vec<Span>>,
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
    async fn ingest_spans(&self, spans: &[Span]) -> Result<usize, StoreError> {
        let mut store = self.spans.lock().expect("spans lock poisoned");
        for s in spans {
            match store.iter_mut().find(|e| e.span_id == s.span_id) {
                Some(existing) => *existing = s.clone(),
                None => store.push(s.clone()),
            }
        }
        Ok(spans.len())
    }

    async fn recent_spans(&self) -> Result<Vec<Span>, StoreError> {
        let store = self.spans.lock().expect("spans lock poisoned");
        let mut v: Vec<Span> = store.clone();
        // Newest-first; ties broken by span_id so output is stable.
        v.sort_by(|a, b| {
            b.start_us
                .cmp(&a.start_us)
                .then_with(|| b.span_id.cmp(&a.span_id))
        });
        v.truncate(SPAN_SCAN_LIMIT);
        Ok(v)
    }

    async fn trace_spans(&self, trace_id: &str) -> Result<Vec<Span>, StoreError> {
        let store = self.spans.lock().expect("spans lock poisoned");
        let mut v: Vec<Span> = store
            .iter()
            .filter(|s| s.trace_id == trace_id)
            .cloned()
            .collect();
        v.sort_by(|a, b| {
            a.start_us
                .cmp(&b.start_us)
                .then_with(|| a.span_id.cmp(&b.span_id))
        });
        v.truncate(TRACE_SPAN_LIMIT);
        Ok(v)
    }
}

// --------------------------------------------------------------------------------------
// PostgreSQL-backed store (portable: standard SQL, runtime queries, no macros).
// --------------------------------------------------------------------------------------
//
// Selected at runtime by `FILAMENT_STORE=postgres`. Each method drives sqlx natively and the
// handlers `.await` it on the serving runtime — NO `block_in_place`, NO sync-over-async. Ingest
// is serialized by an async `Mutex` so a batch upsert never interleaves a half-applied row set;
// reads never take that guard, so they run fully concurrently.

use sqlx::postgres::{PgPool, PgPoolOptions};
use sqlx::Row;

/// PostgreSQL-backed [`Store`]. Holds a `PgPool` and an async write serializer.
pub struct PgStore {
    pool: PgPool,
    /// Serializes ingest batches (a `tokio::sync::Mutex` so it can be held across the per-span
    /// `.await` without blocking a worker thread). Reads never take it.
    write_guard: tokio::sync::Mutex<()>,
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
            write_guard: tokio::sync::Mutex::new(()),
        }
    }

    /// Idempotent, portable migration. Standard SQL only — safe to run on every startup.
    pub async fn migrate(&self) -> Result<(), sqlx::Error> {
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS spans (\
                 span_id TEXT PRIMARY KEY, \
                 trace_id TEXT NOT NULL, \
                 parent_id TEXT NOT NULL DEFAULT '', \
                 name TEXT NOT NULL, \
                 service TEXT NOT NULL DEFAULT '', \
                 start_us BIGINT NOT NULL, \
                 end_us BIGINT NOT NULL, \
                 status TEXT NOT NULL DEFAULT 'ok', \
                 attrs TEXT NOT NULL DEFAULT ''\
             )",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_spans_trace_id ON spans (trace_id)")
            .execute(&self.pool)
            .await?;
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_spans_service_start ON spans (service, start_us)")
            .execute(&self.pool)
            .await?;
        // Backs the newest-first recent-spans scan that feeds the dashboard grouping.
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_spans_start_us ON spans (start_us)")
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    fn span_from_row(row: &sqlx::postgres::PgRow) -> Result<Span, sqlx::Error> {
        Ok(Span {
            span_id: row.try_get("span_id")?,
            trace_id: row.try_get("trace_id")?,
            parent_id: row.try_get("parent_id")?,
            name: row.try_get("name")?,
            service: row.try_get("service")?,
            start_us: row.try_get("start_us")?,
            end_us: row.try_get("end_us")?,
            status: row.try_get("status")?,
            attrs: row.try_get("attrs")?,
        })
    }

    async fn ingest_one(&self, s: &Span) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO spans \
                 (span_id, trace_id, parent_id, name, service, start_us, end_us, status, attrs) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) \
             ON CONFLICT (span_id) DO UPDATE SET \
                 trace_id = EXCLUDED.trace_id, parent_id = EXCLUDED.parent_id, \
                 name = EXCLUDED.name, service = EXCLUDED.service, \
                 start_us = EXCLUDED.start_us, end_us = EXCLUDED.end_us, \
                 status = EXCLUDED.status, attrs = EXCLUDED.attrs",
        )
        .bind(&s.span_id)
        .bind(&s.trace_id)
        .bind(&s.parent_id)
        .bind(&s.name)
        .bind(&s.service)
        .bind(s.start_us)
        .bind(s.end_us)
        .bind(&s.status)
        .bind(&s.attrs)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn recent_spans_async(&self) -> Result<Vec<Span>, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT span_id, trace_id, parent_id, name, service, start_us, end_us, status, attrs \
             FROM spans ORDER BY start_us DESC, span_id DESC LIMIT $1",
        )
        .bind(SPAN_SCAN_LIMIT as i64)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(Self::span_from_row).collect()
    }

    async fn trace_spans_async(&self, trace_id: &str) -> Result<Vec<Span>, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT span_id, trace_id, parent_id, name, service, start_us, end_us, status, attrs \
             FROM spans WHERE trace_id = $1 ORDER BY start_us ASC, span_id ASC LIMIT $2",
        )
        .bind(trace_id)
        .bind(TRACE_SPAN_LIMIT as i64)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(Self::span_from_row).collect()
    }
}

#[async_trait]
impl Store for PgStore {
    async fn ingest_spans(&self, spans: &[Span]) -> Result<usize, StoreError> {
        let _guard = self.write_guard.lock().await;
        let mut n = 0;
        for s in spans {
            self.ingest_one(s)
                .await
                .map_err(|e| StoreError::Backend(e.to_string()))?;
            n += 1;
        }
        Ok(n)
    }

    async fn recent_spans(&self) -> Result<Vec<Span>, StoreError> {
        self.recent_spans_async()
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))
    }

    async fn trace_spans(&self, trace_id: &str) -> Result<Vec<Span>, StoreError> {
        self.trace_spans_async(trace_id)
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span(id: &str, trace: &str, start: i64, end: i64, status: &str) -> Span {
        Span {
            span_id: id.to_string(),
            trace_id: trace.to_string(),
            parent_id: String::new(),
            name: "op".to_string(),
            service: "svc".to_string(),
            start_us: start,
            end_us: end,
            status: status.to_string(),
            attrs: String::new(),
        }
    }

    #[tokio::test]
    async fn ingest_is_idempotent_by_span_id() {
        let store = InMemoryStore::new();
        store.ingest_spans(&[span("a", "t1", 0, 10, "ok")]).await.unwrap();
        // Re-deliver the same span_id with a new status -> overwrite, not duplicate.
        store.ingest_spans(&[span("a", "t1", 0, 99, "error")]).await.unwrap();
        let all = store.recent_spans().await.unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].end_us, 99);
        assert!(all[0].is_error());
    }

    #[tokio::test]
    async fn trace_spans_filters_and_orders() {
        let store = InMemoryStore::new();
        store
            .ingest_spans(&[
                span("b", "t1", 20, 30, "ok"),
                span("a", "t1", 0, 10, "ok"),
                span("c", "t2", 5, 6, "ok"),
            ])
            .await
            .unwrap();
        let t1 = store.trace_spans("t1").await.unwrap();
        assert_eq!(t1.len(), 2);
        assert_eq!(t1[0].span_id, "a", "ordered by start_us ASC");
        assert_eq!(t1[1].span_id, "b");
    }

    #[test]
    fn span_duration_and_error() {
        assert_eq!(span("x", "t", 100, 350, "ok").duration_us(), 250);
        assert_eq!(span("x", "t", 100, 50, "ok").duration_us(), 0, "clamped");
        assert!(span("x", "t", 0, 1, "ERROR").is_error());
        assert!(!span("x", "t", 0, 1, "ok").is_error());
        assert!(!span("x", "t", 0, 1, "").is_error());
    }
}
