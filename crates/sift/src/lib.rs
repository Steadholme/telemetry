//! Sift — log aggregation + search with template clustering for the Steadholme stack.
//!
//! Library root: defines [`AppState`], wires the routes via [`app`], runs the HTTP server and the
//! syslog listeners concurrently via [`run`], and provides [`build_dev_state`] (in-memory store)
//! and [`build_state_from_env`] (env-selected store + Watchtower audit). Integration tests consume
//! [`app`] directly via `tower::oneshot`.
//!
//! Two listening surfaces, one tokio runtime:
//! - **HTTP `:9100`** — the operator dashboard + JSON search (gateway SSO at `/`, `/api/search`),
//!   the unauthenticated `/healthz`, and the OWN-bearer `POST /ingest` (internal-only; never
//!   routed through the public gateway).
//! - **Syslog `:5514` UDP + TCP** — best-effort RFC 5424 / RFC 3164 frames from other containers
//!   and the host, on the internal holdfast network (unauthenticated, network-segmented).
//!
//! Endpoints:
//! - `GET  /healthz`      liveness (public)
//! - `GET  /`             dashboard: filter bar + recent log table + top-templates panel (SSO)
//! - `GET  /api/search`   JSON search results for the same filters (SSO)
//! - `POST /ingest`       ingest one/many logs (own `Bearer SIFT_INGEST_TOKEN`)

pub mod audit;
pub mod auth;
pub mod config;
pub mod error;
pub mod handlers;
pub mod ingest;
pub mod store;
pub mod syslog;
pub mod template;

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::routing::{get, post};
use axum::Router;

use crate::audit::AuditSink;
use crate::config::{env_nonempty, Config};
use crate::store::{InMemoryStore, PgStore, Store};

/// Shared application state. Cheap to clone (everything behind `Arc` / a cloneable sink).
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub store: Arc<dyn Store>,
    pub audit: AuditSink,
}

/// Build the router wiring all HTTP endpoints onto `state`. Routes are explicit (no fallback):
/// Sluice forwards `/` and `/api/search` under SSO; `/ingest` is reached directly in-network.
pub fn app(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(handlers::health::healthz))
        .route("/", get(handlers::dashboard::index))
        .route("/api/search", get(handlers::dashboard::api_search))
        .route("/ingest", post(handlers::ingest::ingest))
        .with_state(state)
}

/// Construct dev state: dev [`Config`] + an empty [`InMemoryStore`] + a disabled audit sink. Used
/// by `main`'s memory mode and by the integration tests, so they need NO database and NO network.
pub fn build_dev_state() -> AppState {
    AppState {
        config: Arc::new(Config::dev()),
        store: Arc::new(InMemoryStore::new()),
        audit: AuditSink::disabled(),
    }
}

/// Build runtime state from the environment.
///
/// The store is selected by `SIFT_STORE`:
/// - `memory` (default): empty [`InMemoryStore`] — no database required.
/// - `postgres`: connect `SIFT_DATABASE_URL` (or `DATABASE_URL`), run the idempotent migration,
///   wire [`PgStore`].
///
/// The audit sink is enabled by `AUDIT_ENABLED` + `WATCHTOWER_URL` + `AUDIT_INGEST_TOKEN`. Returns
/// an error string on misconfiguration so `main` can fail loudly.
pub async fn build_state_from_env() -> Result<AppState, String> {
    let config = Config::from_env();

    let store_kind = env_nonempty("SIFT_STORE").unwrap_or_else(|| "memory".to_string());
    let store: Arc<dyn Store> = match store_kind.as_str() {
        "postgres" => {
            let database_url = env_nonempty("SIFT_DATABASE_URL")
                .or_else(|| env_nonempty("DATABASE_URL"))
                .ok_or_else(|| {
                    "SIFT_STORE=postgres requires SIFT_DATABASE_URL".to_string()
                })?;
            tracing::info!("SIFT_STORE=postgres — connecting to database");
            let pg = PgStore::connect(&database_url)
                .await
                .map_err(|e| format!("connect postgres: {e}"))?;
            pg.migrate()
                .await
                .map_err(|e| format!("run migration: {e}"))?;
            tracing::info!("postgres store ready (migrated)");
            Arc::new(pg)
        }
        "memory" => Arc::new(InMemoryStore::new()),
        other => return Err(format!("unknown SIFT_STORE={other} (use memory|postgres)")),
    };

    let audit = AuditSink::start(
        env_truthy("AUDIT_ENABLED"),
        &env_nonempty("WATCHTOWER_URL").unwrap_or_default(),
        env_nonempty("AUDIT_INGEST_TOKEN").as_deref(),
    );

    Ok(AppState {
        config: Arc::new(config),
        store,
        audit,
    })
}

/// Run the whole service: the HTTP server and the syslog UDP + TCP listeners, concurrently on the
/// current tokio runtime. Returns only if the HTTP listener fails to bind or the server exits.
pub async fn run(state: AppState) -> Result<(), String> {
    let http_addr: SocketAddr = state
        .config
        .bind_addr
        .parse()
        .map_err(|e| format!("invalid BIND_ADDR {}: {e}", state.config.bind_addr))?;
    let syslog_addr: SocketAddr = state
        .config
        .syslog_addr
        .parse()
        .map_err(|e| format!("invalid SIFT_SYSLOG_ADDR {}: {e}", state.config.syslog_addr))?;

    let listener = tokio::net::TcpListener::bind(http_addr)
        .await
        .map_err(|e| format!("failed to bind {http_addr}: {e}"))?;
    tracing::info!(%http_addr, %syslog_addr, "Sift listening (logs: dashboard + syslog + ingest)");

    let http = async {
        if let Err(e) = axum::serve(listener, app(state.clone())).await {
            tracing::error!(error = %e, "HTTP server error");
        }
    };
    let udp = syslog::serve_udp(state.clone(), syslog_addr);
    let tcp = syslog::serve_tcp(state.clone(), syslog_addr);

    // All three run forever; `join!` keeps the process alive as long as any is running.
    tokio::join!(http, udp, tcp);
    Ok(())
}

/// Interpret a boolean-ish env var (`on` / `true` / `1` / `yes`, case-insensitive).
fn env_truthy(key: &str) -> bool {
    matches!(
        std::env::var(key)
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str(),
        "on" | "true" | "1" | "yes"
    )
}

/// Current wall-clock time in epoch seconds (the log `ts` / template `first_seen`/`last_seen`).
pub fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before UNIX epoch")
        .as_secs() as i64
}

/// Monotonic per-process counter, combined with the wall clock, to mint a unique log id even
/// under high-concurrency ingest (two datagrams in the same second never collide).
static LOG_SEQ: AtomicU64 = AtomicU64::new(0);

/// A unique log row id: `log_<nanos>_<seq>`.
pub fn new_log_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before UNIX epoch")
        .as_nanos();
    let seq = LOG_SEQ.fetch_add(1, Ordering::Relaxed);
    format!("log_{nanos}_{seq}")
}
