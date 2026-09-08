//! HTTP handlers. `health` is the unauthenticated liveness probe; `ingest` accepts agent
//! batches (bearer-guarded); `api` serves JSON time-series to the dashboard; `dashboard`
//! renders the server-side enterprise UI (gated by the gateway's `auth=sso` route).

pub mod api;
pub mod dashboard;
pub mod health;
pub mod ingest;

use axum::http::{header, HeaderValue};
use axum::response::{IntoResponse, Response};

pub const APP_CSS_PATH: &str = "/assets/vitals-20260908.css";

pub async fn app_css_asset() -> Response {
    let mut response = crate::render::app_css().into_response();
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

/// Cross-subdomain gateway logout (Vitals lives at vitals.w33d.xyz; the IdP is at id.w33d.xyz).
pub const LOGOUT_URL: &str = "https://sso.w33d.xyz/_gw/auth/logout";

/// This crate serves one Telemetry surface.
pub const SURFACE: &str = "vitals";
pub const SURFACE_HOST: &str = "vitals.w33d.xyz";

use crate::render::esc;

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
