//! HTTP handlers + shared server-render helpers.
//!
//! `health` is the unauthenticated liveness probe; `dashboard` carries the SSO-gated search UI +
//! JSON API; `ingest` is the own-bearer log intake. The shared design tokens / CSS are embedded
//! (via `include_str!`) and served as one immutable asset, matching the Steadholme enterprise brand (the
//! dark command-center palette shared with Keystone/Inkwell).

pub mod dashboard;
pub mod health;
pub mod ingest;

use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use std::sync::OnceLock;

/// Embedded service CSS layered after Odyssey's canonical Steadholme design system.
pub const SERVICE_CSS: &str = include_str!("../../static/service.css");
pub const APP_CSS_PATH: &str = "/assets/sift-20260908.css";
static APP_CSS: OnceLock<String> = OnceLock::new();

/// Cross-subdomain gateway logout (Sift lives at logs.w33d.xyz; the IdP is at id.w33d.xyz).
pub const LOGOUT_URL: &str = "https://sso.w33d.xyz/_gw/auth/logout";

pub fn app_css() -> &'static str {
    APP_CSS.get_or_init(|| {
        let mut css = String::with_capacity(odyssey::APP_CSS.len() + SERVICE_CSS.len() + 1);
        css.push_str(odyssey::APP_CSS);
        css.push('\n');
        css.push_str(SERVICE_CSS);
        css
    })
}

pub async fn app_css_asset() -> Response {
    let mut response = app_css().into_response();
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/css; charset=utf-8"),
    );
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=31536000, immutable"),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    response
}

/// Minimal HTML escaping for text/attribute interpolation (defense-in-depth on every field).
pub fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#x27;")
}

/// This crate serves one Telemetry surface.
pub const SURFACE: &str = "logs";
pub const SURFACE_HOST: &str = "logs.w33d.xyz";

/// Icons used across the console chrome (inline so no asset request is needed).
pub const ICON_MARK: &str = r##"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M2 12h4l2.5-7 5 14 2.5-7h6"/></svg>"##;
pub const ICON_GRID: &str = r##"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><rect x="3" y="3" width="7" height="7" rx="1.5"/><rect x="14" y="3" width="7" height="7" rx="1.5"/><rect x="3" y="14" width="7" height="7" rx="1.5"/><rect x="14" y="14" width="7" height="7" rx="1.5"/></svg>"##;
pub const ICON_LOGS: &str = r##"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M4 5h16M4 12h16M4 19h10"/><path d="M18 17l3 3-3 3" transform="translate(0,-4)"/></svg>"##;
pub const ICON_TRACES: &str = r##"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><rect x="3" y="4" width="12" height="4" rx="1"/><rect x="7" y="10" width="12" height="4" rx="1"/><rect x="5" y="16" width="9" height="4" rx="1"/></svg>"##;
pub const ICON_VITALS: &str = r##"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M2 12h4l2.5-7 5 14 2.5-7h6"/></svg>"##;
pub const ICON_REFRESH: &str = r##"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M20 11a8 8 0 1 0-2.3 5.7"/><path d="M20 5v6h-6"/></svg>"##;
pub const ICON_SEARCH: &str = r##"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><circle cx="11" cy="11" r="7"/><path d="m20 20-3.5-3.5"/></svg>"##;
pub const ICON_FILTER: &str = r##"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M3 5h18l-7 8v6l-4 2v-8L3 5Z"/></svg>"##;
pub const ICON_TEMPLATE: &str = r##"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><rect x="3" y="3" width="8" height="8" rx="1.5"/><rect x="13" y="3" width="8" height="8" rx="1.5"/><rect x="3" y="13" width="8" height="8" rx="1.5"/><rect x="13" y="13" width="8" height="8" rx="1.5"/></svg>"##;
pub const ICON_CHEVRON: &str = r##"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="m9 6 6 6-6 6"/></svg>"##;
pub const ICON_ARROW_LEFT: &str = r##"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M19 12H5"/><path d="m11 6-6 6 6 6"/></svg>"##;
pub const ICON_CAMERA: &str = r##"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M3 8h4l2-3h6l2 3h4v11H3V8Z"/><circle cx="12" cy="13" r="3.5"/></svg>"##;
pub const ICON_CHECK: &str = r##"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.4" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="m4 12 5 5L20 6"/></svg>"##;
pub const ICON_CLOCK: &str = r##"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><circle cx="12" cy="12" r="8.5"/><path d="M12 7.5V12l3 2"/></svg>"##;
pub const ICON_EXTERNAL: &str = r##"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M14 4h6v6"/><path d="M20 4 10 14"/><path d="M18 14v6H4V6h6"/></svg>"##;

/// The three demuxed surfaces, in app-bar order: (href, label, modifier, icon).
pub const SURFACES: [(&str, &str, &str, &str); 3] = [
    ("https://logs.w33d.xyz", "Logs", "logs", ICON_LOGS),
    ("https://traces.w33d.xyz", "Traces", "traces", ICON_TRACES),
    ("https://vitals.w33d.xyz", "Vitals", "vitals", ICON_VITALS),
];

/// Render the shared app bar: brand lockup + host + the three surface pills on the left; All apps,
/// identity and Log out on the right. `active` is the surface modifier of the current vhost.
pub fn suite_bar(active: &str, host: &str, email: &str) -> String {
    let mut pills = String::new();
    for (href, label, modifier, icon) in SURFACES {
        let current = modifier == active;
        pills.push_str(&format!(
            r#"<a class="surf surf--{modifier}{state}" href="{href}"{aria}>{icon}{label}</a>"#,
            modifier = modifier,
            state = if current { " is-active" } else { "" },
            href = if current { "/" } else { href },
            aria = if current {
                r#" aria-current="page""#
            } else {
                ""
            },
            icon = icon,
            label = label,
        ));
    }
    let chip = match email.trim() {
        "" | "—" => {
            r#"<span class="user-email user-email--none">— (no gateway session)</span>"#.to_string()
        }
        value => {
            let initial = value
                .chars()
                .next()
                .map(|c| c.to_uppercase().to_string())
                .unwrap_or_else(|| "S".to_string());
            format!(
                r#"<span class="userchip"><span class="userchip__avatar" aria-hidden="true">{initial}</span><span class="user-email">{email}</span></span>"#,
                initial = esc(&initial),
                email = esc(value),
            )
        }
    };
    format!(
        r#"<header class="suitebar">
  <a class="suitebar__brand" href="/">
    <span class="brand-tile" aria-hidden="true">{mark}</span>
    <span class="suitebar__name"><b>Steadholme</b><span>Telemetry</span></span>
  </a>
  <span class="suitebar__host">{host}</span>
  <nav class="surfaces" aria-label="Telemetry surfaces">{pills}</nav>
  <span class="suitebar__spacer"></span>
  <div class="suitebar__right">
    <a class="allapps" href="https://w33d.xyz">{grid}<span>All apps</span></a>
    {chip}
    <a class="btn btn-ghost btn-sm" href="{logout}">Log out</a>
  </div>
</header>"#,
        mark = ICON_MARK,
        host = esc(host),
        pills = pills,
        grid = ICON_GRID,
        chip = chip,
        logout = LOGOUT_URL,
    )
}

/// The shared page footer.
pub const FOOTER: &str = r##"<footer class="v2-foot">
  <span class="v2-foot__lead">Steadholme Telemetry · logs · traces · vitals</span>
  <a href="https://status.w33d.xyz">Status</a>
  <a href="https://audit.w33d.xyz">Watchtower</a>
  <a href="https://w33d.xyz">All apps</a>
</footer>"##;

/// Fill a page template's chrome placeholders: theme attributes, stylesheet, app bar, footer.
pub fn shell(template: &str, headers: &axum::http::HeaderMap, email: &str) -> String {
    let cookie = headers
        .get(header::COOKIE)
        .and_then(|value| value.to_str().ok());
    let theme = odyssey::resolve_theme(cookie);
    template
        .replace("{{THEME_ATTR}}", odyssey::html_theme_attr(theme))
        .replace("{{COLOR_SCHEME}}", odyssey::color_scheme_meta(theme))
        .replace("{{CSS_PATH}}", APP_CSS_PATH)
        .replace("{{APPBAR}}", &suite_bar(SURFACE, SURFACE_HOST, email))
        .replace("{{FOOTER}}", FOOTER)
}

/// Format epoch seconds as a sortable UTC datetime `YYYY-MM-DD HH:MM:SS` — the log table reads
/// top-to-bottom, so the date has to line up character for character. std `time` only.
pub fn fmt_datetime(secs: i64) -> String {
    match time::OffsetDateTime::from_unix_timestamp(secs) {
        Ok(dt) => format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
            dt.year(),
            u8::from(dt.month()),
            dt.day(),
            dt.hour(),
            dt.minute(),
            dt.second()
        ),
        Err(_) => secs.to_string(),
    }
}

/// CSS modifier class for a severity chip, so the table colors emerg/err warm and debug cool.
pub fn severity_class(severity: &str) -> &'static str {
    match severity {
        "emerg" | "alert" | "crit" => "sev--crit",
        "err" => "sev--err",
        "warning" => "sev--warn",
        "notice" => "sev--notice",
        "debug" => "sev--debug",
        _ => "sev--info",
    }
}

/// A branded HTML error document: one status tile with the code, the reason and the message.
pub fn error_page(status: StatusCode, message: &str) -> String {
    let code = status.as_u16();
    let reason = status.canonical_reason().unwrap_or("Error");
    format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <meta name="color-scheme" content="light dark">
  <title>{code} {reason} · Sift</title>
  <link rel="stylesheet" href="{css_path}">
</head>
<body class="page-v2">
{appbar}
<main class="v2-page">
  <div class="status-wrap">
    <div class="status-tile">
      <div class="status-tile__code">{code}</div>
      <h1 class="status-tile__heading">{reason}</h1>
      <p class="status-tile__detail">{msg}</p>
      <div class="tags">
        <a class="btn btn-primary" href="/">{back}Back to logs</a>
        <a class="btn btn-secondary" href="https://w33d.xyz">All apps</a>
      </div>
    </div>
  </div>
  {footer}
</main>
</body>
</html>"#,
        css_path = APP_CSS_PATH,
        appbar = suite_bar(SURFACE, SURFACE_HOST, "—"),
        footer = FOOTER,
        back = ICON_ARROW_LEFT,
        code = code,
        reason = esc(reason),
        msg = esc(message),
    )
}
