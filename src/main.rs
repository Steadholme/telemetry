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

/// Second listen address — four services hardcode `http://vitals:8300`, so the SAME demux app is
/// also bound here. A request to `vitals:8300` resolves its leading label `vitals` and reaches the
/// vitals arm exactly as `vitals.w33d.xyz` does through :9100.
const VITALS_BIND_ADDR: &str = "0.0.0.0:8300";

/// The composed per-surface routers, dispatched by Host. Cheap to clone (each `Router` is
/// `Arc`-backed internally).
#[derive(Clone)]
struct Vhosts {
    logs: Router,
    traces: Router,
    vitals: Router,
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
    let vitals = build_vitals().await.unwrap_or_else(|e| fatal("vitals (metrics)", e));

    let app = Router::new()
        // Host-agnostic liveness for the container HEALTHCHECK + estate probes.
        .route("/healthz", get(|| async { "ok" }))
        .fallback(dispatch)
        .with_state(Vhosts {
            logs,
            traces,
            vitals,
        });

    // The estate reaches this demux on TWO ports with the SAME app:
    //   :9100  — Sluice fronts logs/traces/vitals subdomains at this upstream.
    //   :8300  — four services POST metrics to the hardcoded `http://vitals:8300`; that label
    //            resolves to the vitals arm. One app, two TcpListeners.
    let primary_addr: SocketAddr = bind_addr.parse().expect("invalid BIND_ADDR");
    let vitals_addr: SocketAddr = VITALS_BIND_ADDR.parse().expect("invalid VITALS_BIND_ADDR");

    let primary = tokio::net::TcpListener::bind(primary_addr)
        .await
        .unwrap_or_else(|e| panic!("failed to bind {primary_addr}: {e}"));
    let secondary = tokio::net::TcpListener::bind(vitals_addr)
        .await
        .unwrap_or_else(|e| panic!("failed to bind {vitals_addr}: {e}"));

    tracing::info!(%primary_addr, %vitals_addr, "Telemetry listening (logs/traces/vitals vhost demux on both ports)");

    // Serve the same app on both listeners; either ending is fatal.
    let app_secondary = app.clone();
    let primary_task = tokio::spawn(async move { axum::serve(primary, app).await });
    let secondary_task =
        tokio::spawn(async move { axum::serve(secondary, app_secondary).await });
    let (primary_res, secondary_res) = tokio::join!(primary_task, secondary_task);
    primary_res.expect("primary serve task panicked").expect("primary server error");
    secondary_res.expect("secondary serve task panicked").expect("secondary server error");
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
        // Gateway subdomain `vitals.w33d.xyz` AND the internal `http://vitals:8300` service name
        // both present the leading label `vitals`.
        "vitals" => v.vitals,
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

    // Preserve Sift's syslog UDP+TCP listeners (`SIFT_SYSLOG_ADDR`, default :5514). The standalone
    // Sift ran them alongside its HTTP server (`sift::run`); the demux composes only the HTTP router,
    // so spawn the listeners here so logs.w33d.xyz keeps BOTH ingest paths (HTTP /ingest + syslog).
    match state.config.syslog_addr.parse::<SocketAddr>() {
        Ok(syslog_addr) => {
            tokio::spawn(sift::syslog::serve_udp(state.clone(), syslog_addr));
            tokio::spawn(sift::syslog::serve_tcp(state.clone(), syslog_addr));
            tracing::info!(%syslog_addr, "sift syslog listeners started (udp + tcp)");
        }
        Err(e) => tracing::warn!(
            addr = %state.config.syslog_addr,
            error = %e,
            "invalid SIFT_SYSLOG_ADDR — syslog listeners not started"
        ),
    }

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

/// Build the vitals (metrics TSDB + dashboard) surface router against `VITALS_DATABASE_URL`.
///
/// State is built EXPLICITLY (not via `vitals::build_state_from_env`) because that path is selected
/// by `VITALS_STORE` and reads the BARE `DATABASE_URL`, which collides when multiple surfaces share
/// one process. Here vitals always runs on Postgres against its OWN `VITALS_DATABASE_URL`, connected
/// + migrated with vitals' own `PgStore`. `ServerConfig::from_env()` keeps `INGEST_TOKEN`,
/// `RETENTION_HOURS`, `VITALS_DETECT*`, `VITALS_Z`, `KLAXON_*` etc. exactly as standalone. The audit
/// sink is wired identically to vitals' own `build_state_from_env` (`AUDIT_ENABLED` + `WATCHTOWER_URL`
/// + `AUDIT_INGEST_TOKEN`).
///
/// Vitals' TWO background tasks are preserved by spawning them explicitly (the demux composes only
/// the HTTP router): the hourly retention pruner AND `detector::spawn_detector` (the self-baselining
/// anomaly detector folded in from the retired Augur). Without them metrics never prune and anomalies
/// never fire — a silent regression.
async fn build_vitals() -> Result<Router, String> {
    let dsn = require_env("VITALS_DATABASE_URL")?;
    let pg = vitals::store::PgStore::connect(&dsn)
        .await
        .map_err(|e| format!("connect: {e}"))?;
    pg.migrate().await.map_err(|e| format!("migrate: {e}"))?;
    tracing::info!("vitals (metrics) store ready");
    let audit = vitals::audit::AuditSink::start(
        env_truthy("AUDIT_ENABLED"),
        &std::env::var("WATCHTOWER_URL").unwrap_or_default(),
        std::env::var("AUDIT_INGEST_TOKEN").ok().as_deref(),
    );
    let state = vitals::AppState {
        config: Arc::new(vitals::config::ServerConfig::from_env()),
        store: Arc::new(pg),
        audit,
    };

    // Preserve vitals' hourly retention pruner (mirrors vitals/src/main.rs::spawn_retention_pruner).
    spawn_vitals_pruner(state.clone());

    // Preserve vitals' background anomaly detector (gated by VITALS_DETECT, default on).
    if state.config.detect_enabled {
        vitals::detector::spawn_detector(state.clone());
    }

    Ok(vitals::app(state))
}

/// Background timer replicating vitals' standalone retention pruner: every hour, delete samples
/// older than `RETENTION_HOURS`. Detached for the process lifetime.
fn spawn_vitals_pruner(state: vitals::AppState) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(3600));
        loop {
            tick.tick().await;
            let cutoff = vitals::now_secs() - state.config.retention_secs();
            let removed = state.store.prune(cutoff).await;
            if removed > 0 {
                tracing::info!(removed, cutoff, "vitals retention prune");
            }
        }
    });
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
