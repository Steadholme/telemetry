//! The SSO operator surface: the server-rendered dashboard (`GET /`) and the JSON search API
//! (`GET /api/search`). Both share one filter parse + the keyword-overlap ranking.
//!
//! Identity comes from the gateway-injected `X-Auth-*` (Sift does no login of its own); the
//! handlers TRUST it because Sift is internal-only behind Sluice. The filter bar is a GET form, so
//! there is no state-changing POST on this surface (and thus no CSRF token to carry).

use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::response::{Html, IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::json;

use crate::auth;
use crate::config::{DEFAULT_PAGE_LIMIT, SEARCH_LIMIT, TEMPLATE_PANEL_LIMIT};
use crate::handlers::{esc, fmt_datetime, severity_class, topbar, APP_CSS};
use crate::store::{LogEntry, SearchFilter};
use crate::AppState;

const DASHBOARD_HTML: &str = include_str!("../../templates/dashboard.html");

/// Raw query string of the dashboard filter bar / search API. All fields optional.
#[derive(Debug, Default, Deserialize)]
pub struct SearchQuery {
    #[serde(default)]
    pub q: String,
    #[serde(default)]
    pub app: String,
    #[serde(default)]
    pub severity: String,
    #[serde(default)]
    pub template_id: String,
    /// Epoch-second lower/upper bounds (as strings so a blank field is tolerated).
    #[serde(default)]
    pub since: String,
    #[serde(default)]
    pub until: String,
    #[serde(default)]
    pub limit: Option<usize>,
}

/// `GET /` — the dashboard: filter bar + recent log table + top-templates panel.
pub async fn index(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(qy): Query<SearchQuery>,
) -> Response {
    let email = auth::display_email(&headers);
    let filter = to_filter(&qy);

    let mut rows = state.store.search(&filter).await.unwrap_or_default();
    rows = rank_by_overlap(rows, filter.q.as_deref());

    let templates = state
        .store
        .top_templates(TEMPLATE_PANEL_LIMIT)
        .await
        .unwrap_or_default();
    let total_logs = state.store.count_logs().await.unwrap_or(0);
    let total_templates = state.store.count_templates().await.unwrap_or(0);

    // Active-template chip: when filtering by a template, show its sample with a clear link.
    let active_filter = match &filter.template_id {
        Some(tid) => {
            let label = state
                .store
                .get_template(tid)
                .await
                .ok()
                .flatten()
                .map(|t| t.sample)
                .unwrap_or_else(|| tid.clone());
            format!(
                r#"<div class="active-filter">Filtered to template <code>{tid}</code> · <span class="active-filter__sample">{sample}</span> <a class="active-filter__clear" href="/">clear ✕</a></div>"#,
                tid = esc(tid),
                sample = esc(&truncate(&label, 80)),
            )
        }
        None => String::new(),
    };

    let page = DASHBOARD_HTML
        .replace("{{CSS}}", APP_CSS)
        .replace("{{TOPBAR}}", &topbar("Log search", &email))
        .replace("{{STAT_LOGS}}", &fmt_count(total_logs))
        .replace("{{STAT_TEMPLATES}}", &fmt_count(total_templates))
        .replace("{{STAT_RESULTS}}", &fmt_count(rows.len() as i64))
        .replace("{{FILTER_BAR}}", &render_filter_bar(&qy))
        .replace("{{ACTIVE_FILTER}}", &active_filter)
        .replace("{{ROWS}}", &render_rows(&rows))
        .replace("{{TEMPLATES}}", &render_templates(&templates));
    Html(page).into_response()
}

/// `GET /api/search` — the same filter, as JSON, for programmatic callers.
pub async fn api_search(
    State(state): State<AppState>,
    Query(qy): Query<SearchQuery>,
) -> Response {
    let filter = to_filter(&qy);
    match state.store.search(&filter).await {
        Ok(rows) => {
            let rows = rank_by_overlap(rows, filter.q.as_deref());
            Json(json!({ "count": rows.len(), "results": rows })).into_response()
        }
        Err(e) => crate::error::AppError::from(e).into_json(),
    }
}

// ---------------------------------------------------------------------------
// Filter + ranking
// ---------------------------------------------------------------------------

/// Translate the raw query string into a [`SearchFilter`], clamping the row limit.
fn to_filter(qy: &SearchQuery) -> SearchFilter {
    SearchFilter {
        q: nonempty(&qy.q),
        app: nonempty(&qy.app),
        severity: nonempty(&qy.severity),
        template_id: nonempty(&qy.template_id),
        since: qy.since.trim().parse::<i64>().ok(),
        until: qy.until.trim().parse::<i64>().ok(),
        limit: qy
            .limit
            .unwrap_or(DEFAULT_PAGE_LIMIT)
            .clamp(1, SEARCH_LIMIT),
    }
}

/// Rank the LIKE-matched rows by keyword overlap with the query, newest-first within equal scores.
///
/// SEMANTIC-ISH SEARCH SEAM: this is lexical keyword-overlap scoring layered on top of the SQL
/// `LIKE` prefilter. The embedding upgrade slots in HERE — store a per-message vector at ingest,
/// and replace [`overlap_score`] with a cosine similarity against the query embedding, keeping the
/// same `(score, ts)` ordering so the rest of the pipeline is untouched.
fn rank_by_overlap(mut rows: Vec<LogEntry>, q: Option<&str>) -> Vec<LogEntry> {
    let Some(q) = q else { return rows };
    let keywords = keywords(q);
    if keywords.is_empty() {
        return rows;
    }
    rows.sort_by(|a, b| {
        overlap_score(&b.message, &keywords)
            .cmp(&overlap_score(&a.message, &keywords))
            .then_with(|| b.ts.cmp(&a.ts))
            .then_with(|| b.id.cmp(&a.id))
    });
    rows
}

/// Distinct lowercase alphanumeric keywords (>= 2 chars) of a query string.
fn keywords(q: &str) -> Vec<String> {
    let mut seen: Vec<String> = Vec::new();
    for raw in q.split(|c: char| !c.is_alphanumeric()) {
        let w = raw.to_lowercase();
        if w.len() >= 2 && !seen.contains(&w) {
            seen.push(w);
        }
    }
    seen
}

/// How many of `keywords` appear (as substrings) in `message`.
fn overlap_score(message: &str, keywords: &[String]) -> usize {
    let m = message.to_lowercase();
    keywords.iter().filter(|k| m.contains(k.as_str())).count()
}

fn nonempty(s: &str) -> Option<String> {
    let t = s.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// Render the GET filter bar, preserving the current values (severity select keeps its choice).
fn render_filter_bar(qy: &SearchQuery) -> String {
    let severities = ["", "emerg", "alert", "crit", "err", "warning", "notice", "info", "debug"];
    let mut options = String::new();
    for s in severities {
        let label = if s.is_empty() { "all severities" } else { s };
        let selected = if s == qy.severity.trim() { " selected" } else { "" };
        options.push_str(&format!(
            r#"<option value="{val}"{sel}>{label}</option>"#,
            val = esc(s),
            sel = selected,
            label = esc(label),
        ));
    }
    format!(
        r#"<form class="filter-bar" method="get" action="/">
  <input class="filter-bar__text" type="text" name="q" value="{q}" placeholder="search messages…" aria-label="search text">
  <input class="filter-bar__app" type="text" name="app" value="{app}" placeholder="app" aria-label="app">
  <select class="filter-bar__sev" name="severity" aria-label="severity">{options}</select>
  <input class="filter-bar__time" type="text" name="since" value="{since}" placeholder="since (epoch s)" aria-label="since">
  <input class="filter-bar__time" type="text" name="until" value="{until}" placeholder="until (epoch s)" aria-label="until">
  <input type="hidden" name="template_id" value="{tid}">
  <button class="btn btn-primary" type="submit">Search</button>
  <a class="btn btn-ghost" href="/">Reset</a>
</form>"#,
        q = esc(qy.q.trim()),
        app = esc(qy.app.trim()),
        options = options,
        since = esc(qy.since.trim()),
        until = esc(qy.until.trim()),
        tid = esc(qy.template_id.trim()),
    )
}

/// Render the log table body rows. Empty result -> a friendly empty state spanning the table.
fn render_rows(rows: &[LogEntry]) -> String {
    if rows.is_empty() {
        return r#"<tr><td class="logtable__empty" colspan="5">No matching log lines. Adjust the filters above, or wait for ingest.</td></tr>"#.to_string();
    }
    let mut out = String::with_capacity(rows.len() * 160);
    for l in rows {
        out.push_str(&format!(
            r#"<tr>
  <td class="col-time">{time}</td>
  <td class="col-sev"><span class="sev {sevclass}">{sev}</span></td>
  <td class="col-host">{host}</td>
  <td class="col-app">{app}</td>
  <td class="col-msg"><a class="col-msg__tmpl" href="/?template_id={tid}" title="filter to this template">▦</a> {msg}</td>
</tr>"#,
            time = esc(&fmt_datetime(l.ts)),
            sevclass = severity_class(&l.severity),
            sev = esc(&l.severity),
            host = esc(if l.host.is_empty() { "—" } else { &l.host }),
            app = esc(if l.app.is_empty() { "—" } else { &l.app }),
            tid = esc(&l.template_id),
            msg = esc(&l.message),
        ));
    }
    out
}

/// Render the top-templates side panel; each row links to its `?template_id=` filtered view.
fn render_templates(templates: &[crate::store::Template]) -> String {
    if templates.is_empty() {
        return r#"<p class="panel__empty">No templates yet.</p>"#.to_string();
    }
    let mut out = String::with_capacity(templates.len() * 120);
    for t in templates {
        out.push_str(&format!(
            r#"<a class="tmpl" href="/?template_id={id}">
  <span class="tmpl__count">{count}</span>
  <span class="tmpl__sample">{sample}</span>
</a>"#,
            id = esc(&t.id),
            count = fmt_count(t.count),
            sample = esc(&truncate(&t.sample, 90)),
        ));
    }
    out
}

/// Group-separated count (e.g. `12,408`), so big numbers read at a glance.
fn fmt_count(n: i64) -> String {
    let s = n.abs().to_string();
    let mut out = String::new();
    let bytes = s.as_bytes();
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (bytes.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(*b as char);
    }
    if n < 0 {
        format!("-{out}")
    } else {
        out
    }
}

/// Truncate to `n` chars on a char boundary, appending `…` when cut.
fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let t: String = s.chars().take(n).collect();
        format!("{t}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keywords_are_distinct_and_filtered() {
        assert_eq!(keywords("DB db timeout 5"), vec!["db", "timeout"]);
        assert!(keywords("a , . ;").is_empty());
    }

    #[test]
    fn overlap_scores_by_keyword_hits() {
        let kws = keywords("database timeout");
        assert_eq!(overlap_score("database connection timeout", &kws), 2);
        assert_eq!(overlap_score("database ok", &kws), 1);
        assert_eq!(overlap_score("all good", &kws), 0);
    }

    #[test]
    fn fmt_count_groups_thousands() {
        assert_eq!(fmt_count(0), "0");
        assert_eq!(fmt_count(42), "42");
        assert_eq!(fmt_count(12408), "12,408");
        assert_eq!(fmt_count(1000000), "1,000,000");
    }
}
