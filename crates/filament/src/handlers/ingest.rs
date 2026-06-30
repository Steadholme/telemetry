//! Internal span ingest (`POST /ingest`).
//!
//! This path is INTERNAL-only and NOT gateway-routed, so Filament authenticates it itself with a
//! bearer token (`Authorization: Bearer <FILAMENT_INGEST_TOKEN>`). The body is parsed LIBERALLY
//! (OTLP/HTTP `{resourceSpans:[...]}` OR a flat span array — see [`crate::ingest`]) and stored
//! idempotently by `span_id`. When an ingested trace carries an error span, a sampled
//! `filament.trace.error` event is emitted to Watchtower (non-blocking; a down Watchtower never
//! affects the response).

use std::collections::HashMap;
use std::hash::{Hash, Hasher};

use axum::body::Bytes;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::Json;
use serde::Serialize;

use crate::audit::AuditEvent;
use crate::auth::require_ingest;
use crate::error::AppError;
use crate::ingest::parse_spans;
use crate::store::Span;
use crate::trace::error_trace_ids;
use crate::AppState;

/// `POST /ingest` response.
#[derive(Serialize)]
pub struct IngestResponse {
    pub accepted: usize,
}

/// `POST /ingest` -> store spans, returns `{ accepted }`.
pub async fn ingest(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<IngestResponse>, AppError> {
    require_ingest(&headers, &state.config.ingest_token)?;

    let value: serde_json::Value = serde_json::from_slice(&body)
        .map_err(|e| AppError::InvalidRequest(format!("body is not valid JSON: {e}")))?;

    let spans = parse_spans(&value);
    if spans.is_empty() {
        return Err(AppError::InvalidRequest(
            "no recognizable spans in body (expected resourceSpans or a span array)".to_string(),
        ));
    }

    let accepted = state.store.ingest_spans(&spans).await?;

    // Audit error traces (sampled, non-blocking). Computed from THIS batch only.
    emit_error_audits(&state, &spans);

    tracing::info!(accepted, "spans ingested");
    Ok(Json(IngestResponse { accepted }))
}

/// Emit a `filament.trace.error` for each error-bearing trace in the batch that passes the
/// deterministic 1-in-N sampler. `emit` is non-blocking, so this never delays the response.
fn emit_error_audits(state: &AppState, spans: &[Span]) {
    let n = state.config.error_sample_n;
    for trace_id in error_trace_ids(spans) {
        if !should_sample(&trace_id, n) {
            continue;
        }
        let (service, error_count) = error_summary(&trace_id, spans);
        state.audit.emit(AuditEvent::warning(
            "filament.trace.error",
            &service,
            &trace_id,
            &format!("{error_count} error span(s) in trace"),
        ));
    }
}

/// Deterministic 1-in-N sampler keyed on `trace_id` (so the same trace samples consistently and an
/// error burst from one trace is not re-emitted on every retry). `n <= 1` => always sample.
fn should_sample(trace_id: &str, n: u64) -> bool {
    if n <= 1 {
        return true;
    }
    let mut h = std::collections::hash_map::DefaultHasher::new();
    trace_id.hash(&mut h);
    h.finish() % n == 0
}

/// For one trace within the batch: the root-ish service to attribute the event to + the number of
/// error spans. Service is the first error span's service (a stable, value-free label).
fn error_summary(trace_id: &str, spans: &[Span]) -> (String, usize) {
    let mut count = 0usize;
    let mut service: Option<&str> = None;
    // Track each service's earliest start so we attribute to the trace's lead service.
    let mut earliest: HashMap<&str, i64> = HashMap::new();
    for s in spans.iter().filter(|s| s.trace_id == trace_id) {
        earliest
            .entry(s.service.as_str())
            .and_modify(|e| *e = (*e).min(s.start_us))
            .or_insert(s.start_us);
        if s.is_error() {
            count += 1;
            if service.is_none() {
                service = Some(s.service.as_str());
            }
        }
    }
    let service = service
        .filter(|s| !s.is_empty())
        .unwrap_or("unknown")
        .to_string();
    (service, count)
}
