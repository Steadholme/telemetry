//! Telemetry — one container hosting the HOLDFAST observability surfaces (logs / traces).
//!
//! Each surface is its OWN library crate (Sift/Filament), reused verbatim: same schema, same
//! routes, same templates, same OWN database, same subdomain, same OWN ingest bearer token. This
//! binary only adds a **Host-based vhost demux** so the estate runs ONE deployable instead of two.
//! Sluice points `logs.w33d.xyz` and `traces.w33d.xyz` at this container; each request is
//! dispatched to the matching surface's router by its `Host` header. Because the surfaces keep
//! their exact paths and separate databases, everything downstream is unchanged — in particular
//! Sift's `logs` table stays intact and reachable at `SIFT_DATABASE_URL`, where Hindsight /
//! Watchtower RCA reads it directly.
//!
//! Each surface's `POST /ingest` keeps its OWN bearer token (`SIFT_INGEST_TOKEN` /
//! `FILAMENT_INGEST_TOKEN`), reached via its subdomain. The demux forwards the FULL request
//! (headers + body) to the surface's own router via `oneshot`, so the per-surface bearer check is
//! evaluated exactly as it was standalone: `logs.w33d.xyz/ingest` -> Sift, `traces.w33d.xyz/ingest`
//! -> Filament.
//!
//! `healthcheck` subcommand: a dependency-free loopback `GET /healthz` (host-agnostic) used as the
//! container HEALTHCHECK, so the image needs no curl.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Request, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use tower::ServiceExt;

/// Default listen address — internal-only; Sluice fronts the two subdomains at this upstream.
const DEFAULT_BIND_ADDR: &str = "0.0.0.0:9100";

/// The two composed per-surface routers, dispatched by Host. Cheap to clone (each `Router` is
/// `Arc`-backed internally).
#[derive(Clone)]
struct Vhosts {
    logs: Router,
    traces: Router,
}

#[tokio::main]
async fn main() {
    // Container HEALTHCHECK path — handled before any setup, exits the process.
    if std::env::args().nth(1).as_deref() == Some("healthcheck") {
        std::process::exit(run_healthcheck());
    }

    tracing_subscriber::fmt::init();

    let bind_addr = std::env::var("BIND_ADDR").unwrap_or_else(|_| DEFAULT_BIND_ADDR.to_string());

    // Each surface connects to its OWN database and migrates idempotently — exactly what the
    // standalone service did. A failure here is fatal (the surface cannot serve without its DB).
    let logs = build_logs().await.unwrap_or_else(|e| fatal("logs (sift)", e));
    let traces = build_traces().await.unwrap_or_else(|e| fatal("traces (filament)", e));

    let app = Router::new()
        // Host-agnostic liveness for the container HEALTHCHECK + estate probes.
        .route("/healthz", get(|| async { "ok" }))
        .fallback(dispatch)
        .with_state(Vhosts { logs, traces });

    let addr: SocketAddr = bind_addr.parse().expect("invalid BIND_ADDR");
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .unwrap_or_else(|e| panic!("failed to bind {addr}: {e}"));
    tracing::info!(%addr, "Telemetry listening (logs/traces vhost demux)");
    axum::serve(listener, app).await.expect("server error");
}

/// Dispatch one request to the surface matching its `Host` header. An unknown host is a 404 — we
/// never silently serve one surface's content under another's vhost. The full request (headers +
/// body) is forwarded, so each surface's own `POST /ingest` bearer check still applies.
async fn dispatch(State(v): State<Vhosts>, req: Request) -> Response {
    let host = req
        .headers()
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    // Match on the leading label (`logs`/`traces`), ignoring any port.
    let label = host
        .split(':')
        .next()
        .unwrap_or("")
        .split('.')
        .next()
        .unwrap_or("");
    let router = match label {
        "logs" => v.logs,
        "traces" => v.traces,
        _ => return (StatusCode::NOT_FOUND, "unknown telemetry host").into_response(),
    };
    // `Router` is a tower `Service` (the exact `app(state).oneshot(req)` path the surfaces' own
    // tests use); its error type is `Infallible`.
    match router.oneshot(req).await {
        Ok(resp) => resp,
        Err(e) => match e {},
    }
}

/// Build the logs (Sift) surface router against `SIFT_DATABASE_URL`, with Sift's own audit sink.
///
/// State is built EXPLICITLY (not via `sift::build_state_from_env`) so this single process holds
/// both surfaces under one bind: connect + migrate the OWN database, start the OWN audit emitter
/// exactly as Sift does (`AUDIT_ENABLED` + `WATCHTOWER_URL` + `AUDIT_INGEST_TOKEN`), and assemble
/// the `AppState` Sift's `app()` expects. The config is `Config::from_env()`, so `SIFT_INGEST_TOKEN`
/// still guards `POST /ingest`.
async fn build_logs() -> Result<Router, String> {
    let dsn = require_env("SIFT_DATABASE_URL")?;
    let pg = sift::store::PgStore::connect(&dsn)
        .await
        .map_err(|e| format!("connect: {e}"))?;
    pg.migrate().await.map_err(|e| format!("migrate: {e}"))?;
    tracing::info!("logs (sift) store ready");
    let audit = sift::audit::AuditSink::start(
        env_truthy("AUDIT_ENABLED"),
        &sift::config::env_nonempty("WATCHTOWER_URL").unwrap_or_default(),
        sift::config::env_nonempty("AUDIT_INGEST_TOKEN").as_deref(),
    );
    let state = sift::AppState {
        config: Arc::new(sift::config::Config::from_env()),
        store: Arc::new(pg),
        audit,
    };
    Ok(sift::app(state))
}

/// Build the traces (Filament) surface router against `FILAMENT_DATABASE_URL`, with Filament's own
/// audit sink. Same explicit construction as [`build_logs`], against Filament's OWN database, audit
/// emitter, config and `FILAMENT_INGEST_TOKEN`.
async fn build_traces() -> Result<Router, String> {
    let dsn = require_env("FILAMENT_DATABASE_URL")?;
    let pg = filament::store::PgStore::connect(&dsn)
        .await
        .map_err(|e| format!("connect: {e}"))?;
    pg.migrate().await.map_err(|e| format!("migrate: {e}"))?;
    tracing::info!("traces (filament) store ready");
    let audit = filament::audit::AuditSink::start(
        env_truthy("AUDIT_ENABLED"),
        &filament::config::env_nonempty("WATCHTOWER_URL").unwrap_or_default(),
        filament::config::env_nonempty("AUDIT_INGEST_TOKEN").as_deref(),
    );
    let state = filament::AppState {
        config: Arc::new(filament::config::Config::from_env()),
        store: Arc::new(pg),
        audit,
    };
    Ok(filament::app(state))
}

/// Interpret a boolean-ish env var (`on` / `true` / `1` / `yes`, case-insensitive). Mirrors the
/// private `env_truthy` each surface uses in its own `build_state_from_env`, so audit is gated
/// identically.
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

/// Read a required env var, returning a descriptive error when unset/empty.
fn require_env(key: &str) -> Result<String, String> {
    match std::env::var(key) {
        Ok(v) if !v.is_empty() => Ok(v),
        _ => Err(format!("{key} is required")),
    }
}

/// Log a fatal startup error for one surface and exit.
fn fatal(surface: &str, err: String) -> ! {
    tracing::error!(surface, error = %err, "failed to build telemetry surface");
    std::process::exit(1);
}

/// GET `/healthz` over a raw TCP socket on the loopback. Returns the process exit code.
fn run_healthcheck() -> i32 {
    let bind_addr = std::env::var("BIND_ADDR").unwrap_or_else(|_| DEFAULT_BIND_ADDR.to_string());
    let port = bind_addr.rsplit(':').next().unwrap_or("9100");
    let target = format!("127.0.0.1:{port}");
    match healthcheck_once(&target) {
        Ok(true) => 0,
        Ok(false) => {
            eprintln!("healthcheck: {target} did not return 200");
            1
        }
        Err(e) => {
            eprintln!("healthcheck: {target} error: {e}");
            1
        }
    }
}

fn healthcheck_once(target: &str) -> std::io::Result<bool> {
    let addr: SocketAddr = target
        .parse()
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, format!("{e}")))?;
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(2))?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    stream.write_all(b"GET /healthz HTTP/1.0\r\nHost: localhost\r\nConnection: close\r\n\r\n")?;
    let mut buf = String::new();
    stream.read_to_string(&mut buf)?;
    Ok(buf.lines().next().unwrap_or("").contains("200"))
}
