//! Two identity surfaces + double-submit CSRF.
//!
//! 1. **Dashboard (`/`, `/api/search`) — gateway SSO.** Sift does NO login of its own. It sits
//!    behind a Sluice `auth=sso` route, where the gateway runs the OIDC browser login against
//!    Keystone, STRIPS any inbound `X-Auth-*`, and injects the verified `X-Auth-Subject` /
//!    `X-Auth-Email` / `X-Auth-Scope`. Because Sift is internal-only, it TRUSTS those headers as
//!    the signed-in operator.
//!
//! 2. **Ingest (`POST /ingest`) — own bearer.** The ingest path is NOT routed through the public
//!    gateway: other containers/hosts on the holdfast network POST directly. So it does its OWN
//!    auth — an HTTP `Authorization: Bearer <SIFT_INGEST_TOKEN>` checked in constant time. The
//!    syslog UDP/TCP listeners are network-segmented (internal :5514) and unauthenticated, per
//!    the syslog protocol.
//!
//! State-changing browser POSTs would be double-submit CSRF protected via the helpers below; the
//! dashboard's only form is a GET filter, so CSRF is currently unused on the SSO surface but kept
//! for parity with the estate auth seam.

use axum::http::{header, HeaderMap};

use crate::error::AppError;

pub const HEADER_SUBJECT: &str = "x-auth-subject";
pub const HEADER_EMAIL: &str = "x-auth-email";
pub const HEADER_SCOPE: &str = "x-auth-scope";

/// Double-submit CSRF cookie. `__Host-` prefix => Secure + Path=/ + no Domain.
pub const CSRF_COOKIE: &str = "__Host-csrf";
/// CSRF cookie lifetime, seconds.
const CSRF_TTL: u64 = 3600;

/// The signed-in operator's subject (stable user id), if the gateway injected one.
pub fn subject(headers: &HeaderMap) -> Option<String> {
    header_value(headers, HEADER_SUBJECT)
}

/// The signed-in operator's email, if the gateway injected one.
pub fn email(headers: &HeaderMap) -> Option<String> {
    header_value(headers, HEADER_EMAIL)
}

/// The operator's scope claim, if the gateway injected one.
pub fn scope(headers: &HeaderMap) -> Option<String> {
    header_value(headers, HEADER_SCOPE)
}

/// The operator's email for display, falling back to a neutral label when unauthenticated.
pub fn display_email(headers: &HeaderMap) -> String {
    email(headers).unwrap_or_else(|| "—".to_string())
}

/// Require an authenticated operator. Returns `(subject, email)`, or `Unauthorized` when no SSO
/// identity is present — defense in depth behind the gateway.
pub fn require_operator(headers: &HeaderMap) -> Result<(String, String), AppError> {
    let sub = subject(headers).ok_or_else(|| {
        AppError::Unauthorized("no gateway SSO identity (X-Auth-Subject missing)".to_string())
    })?;
    Ok((sub, email(headers).unwrap_or_default()))
}

fn header_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

// ---------------------------------------------------------------------------
// Ingest bearer auth (own credential — NOT gateway SSO)
// ---------------------------------------------------------------------------

/// Verify `Authorization: Bearer <token>` against the configured ingest token, constant-time.
/// Returns `Unauthorized` on a missing/malformed header or a mismatch.
pub fn require_ingest_bearer(headers: &HeaderMap, expected: &str) -> Result<(), AppError> {
    let raw = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| AppError::Unauthorized("missing Authorization header".to_string()))?;
    let token = raw
        .strip_prefix("Bearer ")
        .or_else(|| raw.strip_prefix("bearer "))
        .ok_or_else(|| AppError::Unauthorized("expected Bearer token".to_string()))?;
    if ct_eq(token.trim().as_bytes(), expected.as_bytes()) {
        Ok(())
    } else {
        Err(AppError::Unauthorized("invalid ingest token".to_string()))
    }
}

// ---------------------------------------------------------------------------
// Cookies + CSRF (double-submit)
// ---------------------------------------------------------------------------

/// Read a single cookie value from the request's `Cookie` header(s).
pub fn get_cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    for hv in headers.get_all(header::COOKIE).iter() {
        let Ok(raw) = hv.to_str() else { continue };
        for pair in raw.split(';') {
            let pair = pair.trim();
            if let Some((k, v)) = pair.split_once('=') {
                if k.trim() == name {
                    return Some(v.trim().to_string());
                }
            }
        }
    }
    None
}

/// `Set-Cookie` value for the (JS-readable) CSRF cookie.
pub fn csrf_cookie(value: &str) -> String {
    format!("{CSRF_COOKIE}={value}; Path=/; Secure; SameSite=Lax; Max-Age={CSRF_TTL}")
}

/// Mint a fresh CSRF token: 32 CSPRNG bytes, hex-encoded.
pub fn new_csrf_token() -> String {
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes).expect("OS CSPRNG unavailable");
    hex::encode(bytes)
}

/// Resolve the CSRF token to embed in this render's forms. Reuses the existing cookie token when
/// present (so it is stable across pages/tabs); otherwise mints one and returns the matching
/// `Set-Cookie` to attach to the response.
pub fn ensure_csrf(headers: &HeaderMap) -> (String, Option<String>) {
    match get_cookie(headers, CSRF_COOKIE) {
        Some(c) if !c.is_empty() => (c, None),
        _ => {
            let token = new_csrf_token();
            let set = csrf_cookie(&token);
            (token, Some(set))
        }
    }
}

/// Double-submit check: the `submitted` form token must equal the `__Host-csrf` cookie.
pub fn verify_csrf(headers: &HeaderMap, submitted: &str) -> Result<(), AppError> {
    let ok = match get_cookie(headers, CSRF_COOKIE) {
        Some(cookie) if !cookie.is_empty() => ct_eq(cookie.as_bytes(), submitted.as_bytes()),
        _ => false,
    };
    if ok {
        Ok(())
    } else {
        Err(AppError::Unauthorized("CSRF token mismatch".to_string()))
    }
}

/// Length-checked constant-time byte comparison (no early return on the first differing byte).
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ingest_bearer_matches_and_rejects() {
        let mut h = HeaderMap::new();
        h.insert(header::AUTHORIZATION, "Bearer s3cr3t".parse().unwrap());
        assert!(require_ingest_bearer(&h, "s3cr3t").is_ok());
        assert!(require_ingest_bearer(&h, "wrong").is_err());
        assert!(require_ingest_bearer(&HeaderMap::new(), "s3cr3t").is_err());
    }

    #[test]
    fn operator_needs_subject() {
        assert!(require_operator(&HeaderMap::new()).is_err());
        let mut h = HeaderMap::new();
        h.insert(HEADER_SUBJECT, "u_1".parse().unwrap());
        h.insert(HEADER_EMAIL, "a@steadholme.local".parse().unwrap());
        let (sub, em) = require_operator(&h).unwrap();
        assert_eq!(sub, "u_1");
        assert_eq!(em, "a@steadholme.local");
    }

    #[test]
    fn csrf_double_submit_matches_and_rejects() {
        let token = new_csrf_token();
        let mut headers = HeaderMap::new();
        headers.append(
            header::COOKIE,
            format!("{CSRF_COOKIE}={token}").parse().unwrap(),
        );
        assert!(verify_csrf(&headers, &token).is_ok());
        assert!(verify_csrf(&headers, "not-the-token").is_err());
        assert!(verify_csrf(&HeaderMap::new(), &token).is_err());
    }
}
