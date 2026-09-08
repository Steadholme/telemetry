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
use crate::handlers::{esc, fmt_duration, fmt_when, service_hue, shell};
use crate::handlers::{
    ICON_CHECK, ICON_CHEVRON, ICON_CLOCK, ICON_EXTERNAL, ICON_FILTER, ICON_REFRESH,
};
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
        self.min_ms
            .map(|ms| (ms * 1000.0) as i64)
            .unwrap_or(0)
            .max(0)
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
    let all = summarize(&spans);
    let traces = filter_traces(all.clone(), &q);

    let mut services: Vec<(String, usize, i64)> = Vec::new();
    for trace in &all {
        let name = short_service(&trace.root_service).to_string();
        match services
            .iter_mut()
            .find(|(existing, _, _)| *existing == name)
        {
            Some((_, count, slowest)) => {
                *count += 1;
                *slowest = (*slowest).max(trace.total_us);
            }
            None => services.push((name, 1, trace.total_us)),
        }
    }
    services.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

    let errors = traces.iter().filter(|t| t.has_error).count();
    let slowest = traces.iter().map(|t| t.total_us).max().unwrap_or(0);
    let p95 = percentile_us(&traces, 0.95);

    let summary = format!(
        "{} trace{} · slowest {} · {} with error{}",
        traces.len(),
        plural(traces.len()),
        fmt_duration(slowest),
        errors,
        plural(errors),
    );
    let head_sub = format!(
        "{} trace{} · {} service{} · {} span{} scanned",
        all.len(),
        plural(all.len()),
        services.len(),
        plural(services.len()),
        spans.len(),
        plural(spans.len()),
    );

    let rows = if traces.is_empty() {
        empty_traces()
    } else {
        traces.iter().map(render_trace_row).collect::<String>()
    };

    let mut slowest_rows = traces.clone();
    slowest_rows.sort_by(|a, b| b.total_us.cmp(&a.total_us));
    slowest_rows.truncate(4);

    let body = shell(TRACES_HTML, &headers, &email)
        .replace("{{HEAD_SUB}}", &esc(&head_sub))
        .replace("{{REFRESH_HREF}}", &esc(&refresh_href(&q)))
        .replace("{{T_TRACES}}", &traces.len().to_string())
        .replace("{{T_SERVICES}}", &services.len().to_string())
        .replace("{{T_ERRORS}}", &errors.to_string())
        .replace(
            "{{T_ERROR_CLASS}}",
            if errors > 0 { "tile--down" } else { "" },
        )
        .replace("{{T_P95}}", &esc(&fmt_duration(p95)))
        .replace("{{TRACE_COUNT}}", &traces.len().to_string())
        .replace("{{FILTERS}}", &render_filters(&q))
        .replace("{{SUMMARY}}", &esc(&summary))
        .replace("{{ROWS}}", &rows)
        .replace("{{SERVICE_COUNT}}", &services.len().to_string())
        .replace("{{SERVICES}}", &render_services(&services))
        .replace("{{SLOWEST}}", &render_slowest(&slowest_rows))
        .replace("{{ICON_REFRESH}}", ICON_REFRESH);
    Ok(Html(body).into_response())
}

/// The p-th percentile trace duration (nearest-rank), 0 when there is nothing to rank.
fn percentile_us(traces: &[TraceSummary], p: f64) -> i64 {
    if traces.is_empty() {
        return 0;
    }
    let mut durations: Vec<i64> = traces.iter().map(|t| t.total_us).collect();
    durations.sort_unstable();
    let rank = ((durations.len() as f64) * p).ceil() as usize;
    durations[rank.saturating_sub(1).min(durations.len() - 1)]
}

/// The current filter as a query string, so Refresh keeps the view.
fn refresh_href(q: &TracesQuery) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(service) = q
        .service
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        parts.push(format!("service={}", urlencode(service)));
    }
    if let Some(min) = q.min_ms {
        parts.push(format!("min_ms={min}"));
    }
    if parts.is_empty() {
        "/".to_string()
    } else {
        format!("/?{}", parts.join("&"))
    }
}

/// Percent-encode a query value.
fn urlencode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn empty_traces() -> String {
    format!(
        r#"<div class="card__pad"><div class="empty-tile">No trace matches this filter</div><div class="steps">
  <div class="step step--done"><span class="step__mark" aria-hidden="true">{check}</span><div><div class="step__label">Emit spans</div><div class="step__who">POST /ingest · internal bearer</div></div></div>
  <div class="step"><span class="step__mark" aria-hidden="true">{clock}</span><div><div class="step__label">Group by trace_id</div><div class="step__who">the root span names the trace</div></div></div>
  <div class="step"><span class="step__mark" aria-hidden="true">{clock}</span><div><div class="step__label">Open a waterfall</div><div class="step__who">spans indented by parent</div></div></div>
</div></div>"#,
        check = ICON_CHECK,
        clock = ICON_CLOCK,
    )
}

/// The rail's per-service breakdown: chip, trace count, slowest trace.
fn render_services(services: &[(String, usize, i64)]) -> String {
    if services.is_empty() {
        return r#"<div class="card__pad"><div class="empty-tile">No service has reported a span</div></div>"#.to_string();
    }
    let mut out = String::new();
    for (name, count, slowest) in services {
        out.push_str(&format!(
            r#"<div class="objrow"><span class="svc svc--{slot}">{name}</span><span class="objrow__via"><span class="trace__spans">{count}</span><span class="trace__dur">{slowest}</span></span></div>"#,
            slot = service_slot(name),
            name = esc(name),
            count = count,
            slowest = esc(&fmt_duration(*slowest)),
        ));
    }
    out
}

/// The rail's slowest traces.
fn render_slowest(traces: &[TraceSummary]) -> String {
    if traces.is_empty() {
        return r#"<div class="card__pad"><div class="empty-tile">Nothing timed yet</div></div>"#
            .to_string();
    }
    let mut out = String::new();
    for trace in traces {
        out.push_str(&format!(
            r#"<a class="objrow" href="/trace/{id}"><span class="wf__dot" style="color:var(--svc-{slot})"></span><span class="trace__name">{name}</span><span class="trace__dur">{dur}</span></a>"#,
            id = esc(&trace.trace_id),
            slot = service_slot(short_service(&trace.root_service)),
            name = esc(&trace.root_name),
            dur = esc(&fmt_duration(trace.total_us)),
        ));
    }
    out
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
    let root = wf.rows.first();
    let root_service = root
        .map(|row| short_service(&row.span.service).to_string())
        .unwrap_or_else(|| "(unknown)".to_string());
    let root_name = root
        .map(|row| row.span.name.clone())
        .unwrap_or_else(|| "(root)".to_string());
    let errors = wf.rows.iter().filter(|row| row.span.is_error()).count();

    let pills = format!(
        r#"<span class="svc svc--{slot}">{service}</span><span class="pill {status_class}">{status}</span><span class="countpill countpill--plain">{spans} span{plural}</span><span class="countpill countpill--plain">{dur}</span><span class="countpill countpill--plain">{when}</span>"#,
        slot = service_slot(&root_service),
        service = esc(&root_service),
        status_class = if wf.has_error { "pill-down" } else { "pill-ok" },
        status = if wf.has_error { "error" } else { "ok" },
        spans = wf.rows.len(),
        plural = plural(wf.rows.len()),
        dur = esc(&fmt_duration(wf.total_us)),
        when = esc(&fmt_when(wf.start_us)),
    );

    let banner = if wf.has_error {
        format!(
            r#"<div class="banner"><span class="banner__msg">{errors} span{plural} ended in error · the failing bars are red</span></div>"#,
            errors = errors,
            plural = plural(errors),
        )
    } else {
        String::new()
    };

    let mut slowest: Vec<&crate::trace::WaterfallRow> = wf.rows.iter().collect();
    slowest.sort_by(|a, b| b.dur_us.cmp(&a.dur_us));
    slowest.truncate(4);
    let mut slowest_rows = String::new();
    for row in slowest {
        slowest_rows.push_str(&format!(
            r#"<div class="objrow"><span class="wf__dot" style="color:var(--svc-{slot})"></span><span class="trace__name">{name}</span><span class="objrow__via">{service}</span><span class="trace__dur">{dur}</span></div>"#,
            slot = service_slot(short_service(&row.span.service)),
            name = esc(&row.span.name),
            service = esc(short_service(&row.span.service)),
            dur = esc(&fmt_duration(row.dur_us)),
        ));
    }

    let defs = format!(
        r#"<div class="defs">
  <div class="defs__row"><span class="defs__term">Trace id</span><span class="defs__value mono">{id}</span></div>
  <div class="defs__row"><span class="defs__term">Root</span><span class="defs__value mono">{root}</span></div>
  <div class="defs__row"><span class="defs__term">Service</span><span class="defs__value">{service}</span></div>
  <div class="defs__row"><span class="defs__term">Started</span><span class="defs__value">{when} UTC</span></div>
  <div class="defs__row"><span class="defs__term">Duration</span><span class="defs__value mono">{dur}</span></div>
  <div class="defs__row"><span class="defs__term">Services</span><span class="defs__value">{services}</span></div>
  <div class="defs__row"><span class="defs__term">Spans</span><span class="defs__value">{spans}</span></div>
  <div class="defs__row"><span class="defs__term">Errors</span><span class="defs__value">{errors}</span></div>
</div>"#,
        id = esc(&trace_id),
        root = esc(&root_name),
        service = esc(&root_service),
        when = esc(&fmt_when(wf.start_us)),
        dur = esc(&fmt_duration(wf.total_us)),
        services = wf.service_count,
        spans = wf.rows.len(),
        errors = errors,
    );

    let rows = wf.rows.iter().map(render_waterfall_row).collect::<String>();
    let logs_href = format!("https://logs.w33d.xyz/?q={}", urlencode(&trace_id));

    let body = shell(WATERFALL_HTML, &headers, &email)
        .replace("{{TRACE_ID}}", &esc(&trace_id))
        .replace("{{TRACE_PILLS}}", &pills)
        .replace("{{LOGS_HREF}}", &esc(&logs_href))
        .replace("{{BANNER}}", &banner)
        .replace("{{SPAN_COUNT}}", &format!("{} spans", wf.rows.len()))
        .replace("{{LEGEND}}", &render_legend(&spans))
        .replace("{{RULER}}", &render_ruler(wf.total_us))
        .replace("{{ROWS}}", &rows)
        .replace("{{TRACE_DEFS}}", &defs)
        .replace("{{SLOWEST_SPANS}}", &slowest_rows)
        .replace("{{ICON_EXTERNAL}}", ICON_EXTERNAL);
    Ok(Html(body).into_response())
}

/// The time ruler above the bars: five ticks across the trace duration.
fn render_ruler(total_us: i64) -> String {
    let mut cells = String::new();
    for step in 0..5 {
        let at = total_us * step / 4;
        cells.push_str(&format!(
            r#"<span class="ruler__cell">{}</span>"#,
            esc(&if step == 0 {
                "0".to_string()
            } else {
                fmt_duration(at)
            })
        ));
    }
    format!(r#"<div class="ruler">{cells}</div>"#)
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
        r#"<form class="filterbar" method="get" action="/">
  <div class="filterbar__field filterbar__field--grow"><label for="f-svc">Service</label><input type="text" id="f-svc" name="service" value="{svc}" placeholder="gateway" autocomplete="off" spellcheck="false"></div>
  <div class="filterbar__field"><label for="f-min">Min duration (ms)</label><input type="text" id="f-min" name="min_ms" value="{min}" placeholder="0" inputmode="decimal" autocomplete="off"></div>
  <div class="filterbar__actions"><button class="btn btn-primary" type="submit">{icon}Filter</button><a class="btn btn-ghost" href="/">Reset</a></div>
</form>"#,
        svc = esc(service_val),
        min = esc(&min_val),
        icon = ICON_FILTER,
    )
}

fn render_trace_row(t: &TraceSummary) -> String {
    let service = short_service(&t.root_service);
    format!(
        r#"<a class="trace" href="/trace/{id_attr}">
  <span class="trace__svc"><span class="svc svc--{slot}">{service}</span></span>
  <span class="trace__name">{name}</span>
  {err}
  <span class="trace__dur{dur_class}">{dur}</span>
  <span class="trace__spans">{count} span{plural}</span>
  <span class="trace__when">{when}</span>
  <span class="trace__go">{chevron}</span>
</a>"#,
        id_attr = esc(&t.trace_id),
        slot = service_slot(service),
        service = esc(service),
        name = esc(&t.root_name),
        err = if t.has_error {
            r#"<span class="trace__err"><span class="badge-error">Error</span></span>"#
        } else {
            ""
        },
        dur_class = if t.has_error {
            " trace__dur--error"
        } else {
            ""
        },
        dur = esc(&fmt_duration(t.total_us)),
        count = t.span_count,
        plural = plural(t.span_count),
        when = esc(&fmt_when(t.start_us)),
        chevron = ICON_CHEVRON,
    )
}

fn render_waterfall_row(r: &crate::trace::WaterfallRow) -> String {
    let indent = (r.depth.min(crate::config::MAX_TREE_DEPTH)) as f64 * 14.0;
    let service = short_service(&r.span.service);
    let slot = service_slot(service);
    // The duration label sits after the bar, unless the bar reaches the right edge — then it is
    // flipped inside so it never falls off the track.
    let flip = r.offset_pct + r.width_pct > 82.0;
    let label = if flip {
        // Wide bars carry the label inside; short bars near the right edge put it to their left.
        if r.width_pct > 30.0 {
            format!(
                r#"<span class="wf__dur wf__dur--inside" style="right:{right:.3}%">{dur}</span>"#,
                right = (100.0 - r.offset_pct - r.width_pct + 0.8).max(0.6),
                dur = esc(&fmt_duration(r.dur_us)),
            )
        } else {
            format!(
                r#"<span class="wf__dur" style="right:{right:.3}%">{dur}</span>"#,
                right = (100.0 - r.offset_pct + 0.8).min(99.0),
                dur = esc(&fmt_duration(r.dur_us)),
            )
        }
    } else {
        format!(
            r#"<span class="wf__dur" style="left:{left:.3}%">{dur}</span>"#,
            left = (r.offset_pct + r.width_pct + 0.8).min(99.0),
            dur = esc(&fmt_duration(r.dur_us)),
        )
    };
    format!(
        r#"<div class="wf__row">
  <span class="wf__label" style="padding-left:{indent}px" title="{full}">
    <span class="wf__dot" style="color:var(--svc-{slot})"></span>
    <span class="wf__name">{name}</span>
    <span class="wf__svc">{service}</span>
  </span>
  <span class="wf__track">
    <span class="{bar_class}" style="background:var(--svc-{slot});left:{left:.3}%;width:{width:.3}%"></span>
    {label}
  </span>
</div>"#,
        indent = indent,
        full = esc(&format!("{} ({})", r.span.name, r.span.service)),
        slot = slot,
        name = esc(&r.span.name),
        service = esc(service),
        bar_class = if r.span.is_error() {
            "wf__bar wf__bar--error"
        } else {
            "wf__bar"
        },
        left = r.offset_pct,
        width = r.width_pct,
        label = label,
    )
}

/// Legend: each distinct service in the trace with its bar colour.
fn render_legend(spans: &[crate::store::Span]) -> String {
    let mut services: Vec<&str> = spans.iter().map(|s| s.service.as_str()).collect();
    services.sort_unstable();
    services.dedup();
    let mut out = String::from(r#"<span class="legend__item">Services</span>"#);
    for service in services {
        let label = short_service(service);
        out.push_str(&format!(
            r#"<span class="legend__item"><span class="legend__swatch" style="background:var(--svc-{slot})"></span>{name}</span>"#,
            slot = service_slot(label),
            name = esc(label),
        ));
    }
    out
}

/// Map a service name onto one of the six service hues, so the same service keeps one colour
/// across the list, the waterfall and the legend. Pure (no global state).
pub fn service_slot(service: &str) -> u8 {
    (service_hue(service) % 6) as u8 + 1
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
