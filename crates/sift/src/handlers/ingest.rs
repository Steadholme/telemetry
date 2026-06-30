//! `POST /ingest` — the own-bearer log intake (internal-only; NOT routed through the gateway).
//!
//! Other containers/hosts on the holdfast network POST here directly, so this path does its OWN
//! auth: `Authorization: Bearer <SIFT_INGEST_TOKEN>` (constant-time). The body is liberal — a JSON
//! log object, a JSON array, an OTLP-ish `{resourceLogs...}` envelope, or newline-delimited text.

use axum::body::Bytes;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

use crate::auth;
use crate::ingest::ingest_body;
use crate::AppState;

/// Accept one or many logs. Returns `{"accepted": N}` on success; a JSON error envelope otherwise.
pub async fn ingest(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    if let Err(e) = auth::require_ingest_bearer(&headers, &state.config.ingest_token) {
        return e.into_json();
    }
    match ingest_body(&state, &body).await {
        Ok(accepted) => {
            tracing::debug!(accepted, "ingest batch accepted");
            (axum::http::StatusCode::OK, Json(json!({ "accepted": accepted }))).into_response()
        }
        Err(e) => e.into_json(),
    }
}
