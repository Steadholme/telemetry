//! The SSO trace explorer (read-only).
//!
//! Mounted behind the Sluice `auth=sso` route at the subdomain root: the gateway injects the
//! verified `X-Auth-*`, which Filament reads only to label the app-bar. Every view is read-only
//! (no state-changing browser POST), so there is no CSRF surface here.
//!
//! - `GET /`             — recent traces, grouped by `trace_id`, filterable by service / min duration.
//! - `GET /trace/{id}`   — the waterfall: spans ordered by start, indented by parent, each a bar.
//! - `GET /api/traces`   — the same filtered summaries as JSON.

use axum::extract::{Path, Query, State};
use axum::response::{Html, IntoResponse, Json, Response};
use serde::Deserialize;

use crate::auth;
use crate::error::AppError;
use crate::handlers::{app_css, esc, fmt_duration, fmt_when, service_hue, topbar};
use crate::trace::{build_waterfall, summarize, TraceSummary};
use crate::AppState;

const TRACES_HTML: &str = include_str!("../../templates/traces.html");
const WATERFALL_HTML: &str = include_str!("../../templates/waterfall.html");

/// Filters for the trace list + `/api/traces`. Blank/garbled params are treated as absent.
#[derive(Debug, Deserialize, Default)]
pub struct TracesQuery {
    /// Case-insensitive substring match against a trace's root service.
    pub service: Option<String>,
    /// Minimum whole-trace duration, in milliseconds.
    pub min_ms: Option<f64>,
}

impl TracesQuery {
    fn service_filter(&self) -> Option<String> {
        self.service
            .as_ref()
            .map(|s| s.trim().to_ascii_lowercase())
            .filter(|s| !s.is_empty())
    }

    fn min_us(&self) -> i64 {
        self.min_ms.map(|ms| (ms * 1000.0) as i64).unwrap_or(0).max(0)
    }
}

/// Apply the query filters to a freshly summarized trace list (post-grouping, in Rust).
fn filter_traces(mut traces: Vec<TraceSummary>, q: &TracesQuery) -> Vec<TraceSummary> {
    let service = q.service_filter();
    let min_us = q.min_us();
    traces.retain(|t| {
        let svc_ok = service
            .as_ref()
            .map(|s| t.root_service.to_ascii_lowercase().contains(s))
            .unwrap_or(true);
        svc_ok && t.total_us >= min_us
    });
    traces
}

/// `GET /` — the recent-traces list.
pub async fn index(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Query(q): Query<TracesQuery>,
) -> Result<Response, AppError> {
    let email = auth::display_email(&headers);
    let spans = state.store.recent_spans().await?;
    let traces = filter_traces(summarize(&spans), &q);

    let summary = format!(
        "{} trace{} · {} span{} scanned",
        traces.len(),
        plural(traces.len()),
        spans.len(),
        plural(spans.len()),
    );

    let rows = if traces.is_empty() {
        r#"<div class="empty-state"><h2>No traces yet</h2><p>Spans arrive on <code>POST /ingest</code> (internal bearer). Once a service emits, its traces appear here.</p></div>"#.to_string()
    } else {
        traces.iter().map(render_trace_row).collect::<String>()
    };

    let body = TRACES_HTML
        .replace("{{CSS}}", app_css())
        .replace("{{TOPBAR}}", &topbar("Traces", &email))
        .replace("{{FILTERS}}", &render_filters(&q))
        .replace("{{SUMMARY}}", &esc(&summary))
        .replace("{{ROWS}}", &rows);
    Ok(Html(body).into_response())
}

/// `GET /api/traces` — the filtered summaries as JSON.
pub async fn api_traces(
    State(state): State<AppState>,
    Query(q): Query<TracesQuery>,
) -> Result<Json<Vec<TraceSummary>>, AppError> {
    let spans = state.store.recent_spans().await?;
    let traces = filter_traces(summarize(&spans), &q);
    Ok(Json(traces))
}

/// `GET /trace/{trace_id}` — the waterfall.
pub async fn waterfall(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Path(trace_id): Path<String>,
) -> Result<Response, AppError> {
    let email = auth::display_email(&headers);
    let spans = state.store.trace_spans(&trace_id).await?;
    if spans.is_empty() {
        return Err(AppError::NotFound("no such trace".to_string()));
    }
    let wf = build_waterfall(&spans);

    let meta = format!(
        "{} span{} · {} service{} · {}{}",
        wf.rows.len(),
        plural(wf.rows.len()),
        wf.service_count,
        plural(wf.service_count),
        fmt_duration(wf.total_us),
        if wf.has_error { " · errors" } else { "" },
    );

    let rows = wf.rows.iter().map(render_waterfall_row).collect::<String>();
    let legend = render_legend(&spans);

    let body = WATERFALL_HTML
        .replace("{{CSS}}", app_css())
        .replace("{{TOPBAR}}", &topbar("Waterfall", &email))
        .replace("{{TRACE_ID}}", &esc(&trace_id))
        .replace("{{META}}", &esc(&meta))
        .replace("{{LEGEND}}", &legend)
        .replace("{{ROWS}}", &rows);
    Ok(Html(body).into_response())
}

// ---------------------------------------------------------------------------
// Render helpers (every interpolated field HTML-escaped)
// ---------------------------------------------------------------------------

fn render_filters(q: &TracesQuery) -> String {
    let service_val = q.service.as_deref().unwrap_or("");
    let min_val = q
        .min_ms
        .map(|m| {
            // Trim a trailing `.0` so the box reads `5` not `5.0`.
            let s = format!("{m}");
            s.trim_end_matches(".0").to_string()
        })
        .unwrap_or_default();
    format!(
        r#"<form class="filters" method="get" action="/">
  <div class="filters__field">
    <label for="f-svc">Service</label>
    <input type="text" id="f-svc" name="service" value="{svc}" placeholder="e.g. gateway">
  </div>
  <div class="filters__field">
    <label for="f-min">Min duration (ms)</label>
    <input type="text" id="f-min" name="min_ms" value="{min}" placeholder="0" inputmode="decimal">
  </div>
  <button class="btn btn-primary" type="submit">Filter</button>
  <a class="btn btn-ghost" href="/">Reset</a>
</form>"#,
        svc = esc(service_val),
        min = esc(&min_val),
    )
}

fn render_trace_row(t: &TraceSummary) -> String {
    let err = if t.has_error {
        r#"<span class="badge badge-error">Error</span>"#
    } else {
        ""
    };
    format!(
        r#"<a class="trace-row" href="/trace/{id_attr}">
  <span class="trace-row__svc" style="--hue:{hue}">{service}</span>
  <span class="trace-row__name">{name}{err}</span>
  <span class="trace-row__dur">{dur}</span>
  <span class="trace-row__count">{count} span{plural}</span>
  <span class="trace-row__when">{when}</span>
</a>"#,
        id_attr = esc(&t.trace_id),
        hue = service_hue(&t.root_service),
        service = esc(short_service(&t.root_service)),
        name = esc(&t.root_name),
        err = err,
        dur = esc(&fmt_duration(t.total_us)),
        count = t.span_count,
        plural = plural(t.span_count),
        when = esc(&fmt_when(t.start_us)),
    )
}

fn render_waterfall_row(r: &crate::trace::WaterfallRow) -> String {
    let indent = (r.depth.min(crate::config::MAX_TREE_DEPTH)) as f64 * 14.0;
    let status_class = if r.span.is_error() {
        "wf-bar wf-bar--error"
    } else {
        "wf-bar"
    };
    format!(
        r#"<div class="wf-row">
  <div class="wf-label" style="padding-left:{indent}px" title="{full}">
    <span class="wf-dot" style="--hue:{hue}"></span>
    <span class="wf-name">{name}</span>
    <span class="wf-svc">{service}</span>
  </div>
  <div class="wf-track">
    <div class="{status_class}" style="--hue:{hue};left:{left:.3}%;width:{width:.3}%"></div>
    <span class="wf-dur">{dur}</span>
  </div>
</div>"#,
        indent = indent,
        full = esc(&format!("{} ({})", r.span.name, r.span.service)),
        hue = service_hue(&r.span.service),
        name = esc(&r.span.name),
        service = esc(short_service(&r.span.service)),
        status_class = status_class,
        left = r.offset_pct,
        width = r.width_pct,
        dur = esc(&fmt_duration(r.dur_us)),
    )
}

/// Legend: each distinct service in the trace with its bar color.
fn render_legend(spans: &[crate::store::Span]) -> String {
    let mut services: Vec<&str> = spans.iter().map(|s| s.service.as_str()).collect();
    services.sort_unstable();
    services.dedup();
    let mut out = String::new();
    for s in services {
        out.push_str(&format!(
            r#"<span class="legend__item"><span class="wf-dot" style="--hue:{hue}"></span>{name}</span>"#,
            hue = service_hue(s),
            name = esc(short_service(s)),
        ));
    }
    out
}

/// Display label for an empty service name.
fn short_service(service: &str) -> &str {
    if service.is_empty() {
        "(unknown)"
    } else {
        service
    }
}

fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}
