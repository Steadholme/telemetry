//! Unauthenticated liveness probe.

use axum::http::StatusCode;

/// `GET /healthz` -> 200 OK (plain text). Used by the container HEALTHCHECK. No auth.
pub async fn healthz() -> (StatusCode, &'static str) {
    (StatusCode::OK, "ok")
}
