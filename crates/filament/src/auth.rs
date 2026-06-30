//! Two authentication surfaces, deliberately split.
//!
//! - The SSO dashboard (`traces.w33d.xyz/`, `/trace/{id}`, `/api/traces`) is `auth=sso` at the
//!   Sluice gateway: the gateway runs the OIDC browser login against Keystone, STRIPS any inbound
//!   `X-Auth-*`, and injects the verified `X-Auth-Subject` / `X-Auth-Email` / `X-Auth-Scope`.
//!   Filament is internal-only, so it TRUSTS those headers for display; the dashboard is read-only
//!   (no state-changing browser POST, so no CSRF surface).
//!
//! - The ingest path (`POST /ingest`) is INTERNAL-only and NOT gateway-routed, so Filament does
//!   its OWN bearer auth there (`Authorization: Bearer <FILAMENT_INGEST_TOKEN>`), compared in
//!   constant time so a timing side-channel cannot recover the token byte by byte.

use axum::http::{header, HeaderMap};

use crate::error::AppError;

pub const HEADER_SUBJECT: &str = "x-auth-subject";
pub const HEADER_EMAIL: &str = "x-auth-email";

/// The dashboard viewer's email for display, falling back to a neutral label when no gateway
/// session is present (e.g. a local `cargo run` or the DB-free test suite).
pub fn display_email(headers: &HeaderMap) -> String {
    header_value(headers, HEADER_EMAIL).unwrap_or_else(|| "—".to_string())
}

fn header_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Verify the request carries the configured ingest bearer token. Returns `Unauthorized`
/// (401 + `WWW-Authenticate: Bearer`) when the header is missing, malformed, or mismatched.
pub fn require_ingest(headers: &HeaderMap, expected_token: &str) -> Result<(), AppError> {
    let presented = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer ").or_else(|| v.strip_prefix("bearer ")))
        .map(str::trim);

    match presented {
        Some(token) if ct_eq(token.as_bytes(), expected_token.as_bytes()) => Ok(()),
        _ => Err(AppError::Unauthorized(
            "missing or invalid ingest bearer token".to_string(),
        )),
    }
}

/// Constant-time byte equality. Folds the length difference into the accumulator so neither the
/// comparison time nor an early return reveals where two values diverge.
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    let mut diff = (a.len() ^ b.len()) as u8;
    let n = a.len().min(b.len());
    for i in 0..n {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ct_eq_matches_and_rejects() {
        assert!(ct_eq(b"token", b"token"));
        assert!(!ct_eq(b"token", b"tokeN"));
        assert!(!ct_eq(b"token", b"token-longer"));
        assert!(!ct_eq(b"", b"x"));
    }

    #[test]
    fn require_ingest_checks_bearer() {
        let mut h = HeaderMap::new();
        assert!(require_ingest(&h, "secret").is_err());
        h.insert(header::AUTHORIZATION, "Bearer secret".parse().unwrap());
        assert!(require_ingest(&h, "secret").is_ok());
        let mut bad = HeaderMap::new();
        bad.insert(header::AUTHORIZATION, "Bearer wrong".parse().unwrap());
        assert!(require_ingest(&bad, "secret").is_err());
    }

    #[test]
    fn display_email_falls_back() {
        assert_eq!(display_email(&HeaderMap::new()), "—");
        let mut h = HeaderMap::new();
        h.insert(HEADER_EMAIL, "a@w33d.xyz".parse().unwrap());
        assert_eq!(display_email(&h), "a@w33d.xyz");
    }
}
