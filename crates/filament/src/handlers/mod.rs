//! HTTP handlers + shared server-render helpers.
//!
//! - [`health`] — unauthenticated liveness probe (`/healthz`).
//! - [`ingest`] — internal span ingest (`POST /ingest`, own bearer token).
//! - [`dashboard`] — the server-rendered SSO trace explorer (`GET /`, `/trace/{id}`, `/api/traces`).
//!
//! The shared design tokens / CSS are embedded (via `include_str!`) and inlined into every page,
//! matching the HOLDFAST enterprise brand (the same look as the Keystone/Inkwell UI): brand
//! gradient, indigo accent, cards, app-bar.

pub mod dashboard;
pub mod health;
pub mod ingest;

use std::hash::{Hash, Hasher};

use axum::http::StatusCode;

/// Embedded design system, inlined into each rendered page's `<style>`.
pub const APP_CSS: &str = include_str!("../../static/app.css");

/// Cross-subdomain gateway logout (Filament lives at traces.w33d.xyz; the IdP is at id.w33d.xyz).
pub const LOGOUT_URL: &str = "https://id.w33d.xyz/_gw/auth/logout";

/// The HOLDFAST shield glyph (small, for the app-bar brand lockup).
pub const SHIELD_SVG: &str = r##"<svg viewBox="0 0 48 48" fill="none" xmlns="http://www.w3.org/2000/svg"><defs><linearGradient id="hf-shield-sm" x1="8" y1="4" x2="40" y2="44" gradientUnits="userSpaceOnUse"><stop stop-color="#818CF8"/><stop offset="1" stop-color="#4F46E5"/></linearGradient></defs><path d="M24 4 8 9.5V22c0 11 7 17.4 16 21.5C33 39.4 40 33 40 22V9.5L24 4Z" fill="url(#hf-shield-sm)"/><rect x="20" y="19" width="8" height="13" rx="1" fill="#fff" fill-opacity="0.92"/><path d="M20 19v-2.5a4 4 0 0 1 8 0V19" stroke="#fff" stroke-width="2" stroke-opacity="0.92" fill="none"/></svg>"##;

/// Minimal HTML escaping for text/attribute interpolation (defense-in-depth on every field).
pub fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#x27;")
}

/// Render the shared app-bar: shield + HOLDFAST wordmark on the left; the page title, an
/// "All apps" link back to the apex portal, the signed-in user chip (avatar initial + email),
/// and a Logout link to the gateway on the right. A blank/placeholder email (public or error
/// pages with no gateway identity) renders the chrome without a user chip.
pub fn topbar(page_title: &str, email: &str) -> String {
    let chip = if email.is_empty() || email == "—" {
        String::new()
    } else {
        let initial = email
            .chars()
            .next()
            .map(|c| c.to_uppercase().to_string())
            .unwrap_or_else(|| "H".to_string());
        format!(
            "<span class=\"userchip\"><span class=\"userchip__avatar\" aria-hidden=\"true\">{}</span><span class=\"user-email\">{}</span></span>",
            esc(&initial),
            esc(email),
        )
    };
    format!(
        r#"<header class="topbar">
  <div class="topbar__inner">
    <a class="brand" href="/" aria-label="HOLDFAST Filament">
      <span class="brand__glyph" aria-hidden="true">{shield}</span>
      <span class="brand__word">HOLDFAST</span>
    </a>
    <div class="topbar__right">
      <span class="topbar__title">{title}</span>
      <a class="allapps" href="https://w33d.xyz" title="All apps"><svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><rect x="3" y="3" width="7" height="7" rx="1.5"/><rect x="14" y="3" width="7" height="7" rx="1.5"/><rect x="3" y="14" width="7" height="7" rx="1.5"/><rect x="14" y="14" width="7" height="7" rx="1.5"/></svg>All apps</a>
      {chip}
      <a class="btn btn-ghost btn-sm" href="{logout}">Log out</a>
    </div>
  </div>
</header>"#,
        shield = SHIELD_SVG,
        title = esc(page_title),
        chip = chip,
        logout = LOGOUT_URL,
    )
}

/// Format a microsecond duration compactly: `420µs`, `12.4ms`, `3.21s`.
pub fn fmt_duration(us: i64) -> String {
    let us = us.max(0);
    if us < 1_000 {
        format!("{us}µs")
    } else if us < 1_000_000 {
        format!("{:.1}ms", us as f64 / 1_000.0)
    } else {
        format!("{:.2}s", us as f64 / 1_000_000.0)
    }
}

/// Format an epoch-MICROSECONDS instant as a compact UTC `Mon D, HH:MM:SS` (e.g. `Jun 30,
/// 14:05:09`). std `time` only, no extra C deps.
pub fn fmt_when(us: i64) -> String {
    let secs = us / 1_000_000;
    match time::OffsetDateTime::from_unix_timestamp(secs) {
        Ok(dt) => format!(
            "{} {}, {:02}:{:02}:{:02}",
            month_abbr(dt.month()),
            dt.day(),
            dt.hour(),
            dt.minute(),
            dt.second()
        ),
        Err(_) => us.to_string(),
    }
}

fn month_abbr(m: time::Month) -> &'static str {
    use time::Month::*;
    match m {
        January => "Jan",
        February => "Feb",
        March => "Mar",
        April => "Apr",
        May => "May",
        June => "Jun",
        July => "Jul",
        August => "Aug",
        September => "Sep",
        October => "Oct",
        November => "Nov",
        December => "Dec",
    }
}

/// A deterministic hue (0..360) for a service name, so the same service keeps one bar color across
/// the waterfall and the legend. Pure (no global state).
pub fn service_hue(service: &str) -> u16 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    service.hash(&mut h);
    (h.finish() % 360) as u16
}

/// A small, branded HTML error page (used by [`crate::error::AppError`]).
pub fn error_page(status: StatusCode, message: &str) -> String {
    let code = status.as_u16();
    let reason = status.canonical_reason().unwrap_or("Error");
    format!(
        r#"<!DOCTYPE html>
<html lang="en"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta name="color-scheme" content="light">
<title>{code} {reason} · Filament</title><style>{css}</style></head>
<body class="page-reading">
{topbar}
<main class="reader">
  <div class="error-card">
    <div class="error-card__code">{code}</div>
    <h1 class="error-card__title">{reason}</h1>
    <p class="error-card__msg">{msg}</p>
    <a class="btn btn-primary" href="/">Back to traces</a>
  </div>
</main>
</body></html>"#,
        css = APP_CSS,
        topbar = topbar("Filament", "—"),
        code = code,
        reason = esc(reason),
        msg = esc(message),
    )
}
