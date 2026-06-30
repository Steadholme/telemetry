//! Error type + responses.
//!
//! Dashboard/page failures render a small branded HTML error page; the JSON paths (`/ingest`,
//! `/api/traces`) still get a sensible status code. 401s additionally carry `WWW-Authenticate`.
//! Keeping one enum mirrors the keystone/watchtower error seam.

use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum AppError {
    /// Malformed ingest body (not JSON / no recognizable spans).
    #[error("invalid_request: {0}")]
    InvalidRequest(String),

    /// Missing/invalid ingest bearer token, or no gateway SSO identity.
    #[error("unauthorized: {0}")]
    Unauthorized(String),

    /// No such trace.
    #[error("not_found: {0}")]
    NotFound(String),

    /// Unexpected internal failure (store I/O).
    #[error("server_error: {0}")]
    Internal(String),
}

impl AppError {
    fn parts(&self) -> (StatusCode, String, bool) {
        match self {
            AppError::InvalidRequest(d) => (StatusCode::BAD_REQUEST, d.clone(), false),
            AppError::Unauthorized(d) => (StatusCode::UNAUTHORIZED, d.clone(), true),
            AppError::NotFound(d) => (StatusCode::NOT_FOUND, d.clone(), false),
            AppError::Internal(d) => (StatusCode::INTERNAL_SERVER_ERROR, d.clone(), false),
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, description, www_authenticate) = self.parts();
        let body = crate::handlers::error_page(status, &description);
        let mut response = (status, Html(body)).into_response();
        if www_authenticate {
            response
                .headers_mut()
                .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
        }
        response
    }
}

/// Store failures collapse to a 500 `server_error`.
impl From<crate::store::StoreError> for AppError {
    fn from(e: crate::store::StoreError) -> Self {
        match e {
            crate::store::StoreError::Backend(m) => AppError::Internal(m),
        }
    }
}
