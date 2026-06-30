//! Filament — distributed tracing (OTLP span ingest + trace waterfall) for the HOLDFAST stack.
//!
//! Library root: defines [`AppState`], wires the routes via [`app`], and provides
//! [`build_dev_state`] (in-memory store, audit off) and [`build_state_from_env`] (env-selected
//! store + Watchtower audit). Integration tests consume [`app`] directly via `tower::oneshot`,
//! exactly like the rest of the estate.
//!
//! Filament collapses Jaeger/Tempo into one DB: it stores spans, reconstructs traces, and renders
//! a waterfall, so request latency across services is queryable. It serves TWO surfaces:
//!
//! - The SSO dashboard at `/` (`auth=sso`, gateway-injected `X-Auth-*`): recent traces, the
//!   per-trace waterfall, and a JSON API. Read-only — no CSRF surface.
//! - An INTERNAL ingest at `POST /ingest` (NOT gateway-routed) with its OWN bearer token
//!   (`FILAMENT_INGEST_TOKEN`): accepts OTLP/HTTP-ish JSON or a flat span array.
//!
//! Endpoints:
//! - `GET  /healthz`            liveness (container HEALTHCHECK, no auth)
//! - `GET  /`                   recent traces (filter by service / min duration)
//! - `GET  /trace/{trace_id}`   the waterfall
//! - `GET  /api/traces`         the filtered summaries as JSON
//! - `POST /ingest`             span ingest (own bearer; internal-only)

pub mod audit;
pub mod auth;
pub mod config;
pub mod error;
pub mod handlers;
pub mod ingest;
pub mod store;
pub mod trace;

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::routing::{get, post};
use axum::Router;

use crate::audit::AuditSink;
use crate::config::{env_nonempty, Config};
use crate::store::{InMemoryStore, PgStore, Store};

/// Shared application state. Cheap to clone (everything behind `Arc` / cloneable handles).
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub store: Arc<dyn Store>,
    pub audit: AuditSink,
}

/// Build the router wiring all endpoints onto `state`.
///
/// The dashboard routes sit at the service root (Sluice forwards them unmodified); `/ingest` is
/// the internal, non-gateway-routed span sink (Filament enforces its own bearer auth there).
pub fn app(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(handlers::health::healthz))
        // --- SSO dashboard ---
        .route("/", get(handlers::dashboard::index))
        .route("/trace/{trace_id}", get(handlers::dashboard::waterfall))
        .route("/api/traces", get(handlers::dashboard::api_traces))
        // --- internal ingest (own bearer auth) ---
        .route("/ingest", post(handlers::ingest::ingest))
        .with_state(state)
}

/// Construct dev state: dev [`Config`], an empty [`InMemoryStore`], and a disabled audit sink (no
/// network). Used by `main`'s memory mode and the integration tests, so they need no database.
pub fn build_dev_state() -> AppState {
    AppState {
        config: Arc::new(Config::dev()),
        store: Arc::new(InMemoryStore::new()),
        audit: AuditSink::disabled(),
    }
}

/// Build runtime state from the environment.
///
/// [`Config`] comes from [`Config::from_env`]. The store is selected by `FILAMENT_STORE`:
/// - `memory` (default): empty [`InMemoryStore`] — no database required.
/// - `postgres`: connect `FILAMENT_DATABASE_URL` (or `DATABASE_URL`), run the idempotent
///   migration, wire [`PgStore`].
///
/// The audit sink is enabled by `AUDIT_ENABLED` + `WATCHTOWER_URL` + `AUDIT_INGEST_TOKEN`. Returns
/// an error string on misconfiguration so `main` can fail loudly.
pub async fn build_state_from_env() -> Result<AppState, String> {
    let config = Config::from_env();

    let store_kind = env_nonempty("FILAMENT_STORE").unwrap_or_else(|| "memory".to_string());
    let store: Arc<dyn Store> = match store_kind.as_str() {
        "postgres" => {
            let database_url = env_nonempty("FILAMENT_DATABASE_URL")
                .or_else(|| env_nonempty("DATABASE_URL"))
                .ok_or_else(|| "FILAMENT_STORE=postgres requires FILAMENT_DATABASE_URL".to_string())?;
            tracing::info!("FILAMENT_STORE=postgres — connecting to database");
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
        other => return Err(format!("unknown FILAMENT_STORE={other} (use memory|postgres)")),
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

/// Current wall-clock time in epoch microseconds (the span timestamp granularity).
pub fn now_us() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before UNIX epoch")
        .as_micros() as i64
}
