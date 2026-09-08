//! The SSO operator surface: the server-rendered dashboard (`GET /`) and the JSON search API
//! (`GET /api/search`). Both share one filter parse, source/time/level filters, keyset pagination,
//! and the keyword-overlap ranking.
//!
//! Identity comes from the gateway-injected `X-Auth-*` (Sift does no login of its own); the
//! handlers TRUST it because Sift is internal-only behind Sluice. The filter bar is a GET form, so
//! there is no state-changing POST on this surface (and thus no CSRF token to carry).

use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::response::{Html, IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::auth;
use crate::config::{DEFAULT_PAGE_LIMIT, SEARCH_LIMIT, TEMPLATE_PANEL_LIMIT};
use crate::handlers::{esc, fmt_datetime, severity_class, shell};
use crate::handlers::{ICON_CHEVRON, ICON_REFRESH, ICON_SEARCH, ICON_TEMPLATE};
use crate::store::{LogEntry, SearchFilter};
use crate::{now_secs, AppState};

const DASHBOARD_HTML: &str = include_str!("../../templates/dashboard.html");

/// Raw query string of the dashboard filter bar / search API. All fields optional.
#[derive(Debug, Default, Deserialize)]
pub struct SearchQuery {
    #[serde(default)]
    pub q: String,
    #[serde(default)]
    pub host: String,
    /// Query-string alias accepted for Datadog/Grafana-style "source" filters.
    #[serde(default)]
    pub source: String,
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
    pub before_ts: Option<i64>,
    #[serde(default)]
    pub before_id: String,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SearchCursor {
    pub before_ts: i64,
    pub before_id: String,
}

struct SearchPage {
    rows: Vec<LogEntry>,
    next_cursor: Option<SearchCursor>,
}

/// `GET /` — the dashboard: filter bar + recent log table + top-templates panel.
pub async fn index(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(qy): Query<SearchQuery>,
) -> Response {
    let email = auth::display_email(&headers);
    let page_limit = page_limit(&qy);
    let filter = to_filter(&qy, fetch_limit(page_limit));

    let rows = state.store.search(&filter).await.unwrap_or_default();
    let page = page_from_rows(rows, page_limit, filter.q.as_deref());

    let templates = state
        .store
        .top_templates(TEMPLATE_PANEL_LIMIT)
        .await
        .unwrap_or_default();
    let total_logs = state.store.count_logs().await.unwrap_or(0);
    let total_templates = state.store.count_templates().await.unwrap_or(0);

    // Active-template banner: when filtering by a template, show its sample with a clear link.
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
                r#"<div class="active-filter">{icon}<span>Filtered to one template</span><span class="active-filter__sample">{sample}</span><a class="active-filter__clear" href="/">Clear</a></div>"#,
                icon = ICON_TEMPLATE,
                sample = esc(&truncate(&label, 90)),
            )
        }
        None => String::new(),
    };

    let day_ago = now_secs() - 86_400;
    let severities = state
        .store
        .severity_counts(day_ago)
        .await
        .unwrap_or_default();
    let hosts = distinct_hosts(&page.rows);

    let head_sub = format!(
        "{} logs · {} templates · {} hosts · UTC",
        fmt_count(total_logs),
        fmt_count(total_templates),
        hosts,
    );
    let range_label = match page.next_cursor.as_ref() {
        Some(_) => format!("{} of {}", page.rows.len(), fmt_count(total_logs)),
        None => format!("{} shown", page.rows.len()),
    };

    let page_html = shell(DASHBOARD_HTML, &headers, &email)
        .replace("{{HEAD_SUB}}", &esc(&head_sub))
        .replace(
            "{{REFRESH_HREF}}",
            &esc(&format!("/?{}", build_query_string(&qy, None))),
        )
        .replace("{{T_LOGS}}", &fmt_count(total_logs))
        .replace("{{T_TEMPLATES}}", &fmt_count(total_templates))
        .replace("{{T_RESULTS}}", &fmt_count(page.rows.len() as i64))
        .replace("{{T_HOSTS}}", &hosts.to_string())
        .replace("{{RANGE_LABEL}}", &esc(&range_label))
        .replace("{{FILTER_BAR}}", &render_filter_bar(&qy))
        .replace("{{ACTIVE_FILTER}}", &active_filter)
        .replace("{{ROWS}}", &render_rows(&page.rows))
        .replace("{{PAGER}}", &render_pager(&qy, &page))
        .replace("{{TEMPLATE_COUNT}}", &templates.len().to_string())
        .replace(
            "{{TEMPLATES}}",
            &render_templates(&templates, filter.template_id.as_deref()),
        )
        .replace("{{SEVERITY_BARS}}", &render_severity_bars(&severities))
        .replace("{{ICON_REFRESH}}", ICON_REFRESH);
    Html(page_html).into_response()
}

/// Distinct source hosts in the current result page.
fn distinct_hosts(rows: &[LogEntry]) -> usize {
    let mut hosts: Vec<&str> = rows.iter().map(|row| row.host.as_str()).collect();
    hosts.sort_unstable();
    hosts.dedup();
    hosts.iter().filter(|host| !host.is_empty()).count()
}

/// `GET /api/search` — the same filter, as JSON, for programmatic callers.
pub async fn api_search(State(state): State<AppState>, Query(qy): Query<SearchQuery>) -> Response {
    let page_limit = page_limit(&qy);
    let filter = to_filter(&qy, fetch_limit(page_limit));
    match state.store.search(&filter).await {
        Ok(rows) => {
            let page = page_from_rows(rows, page_limit, filter.q.as_deref());
            let next_cursor = page.next_cursor.clone();
            let next = next_cursor
                .as_ref()
                .map(|cursor| format!("/api/search?{}", build_query_string(&qy, Some(cursor))));
            Json(json!({
                "count": page.rows.len(),
                "limit": page_limit,
                "next_cursor": next_cursor,
                "next": next,
                "results": page.rows,
            }))
            .into_response()
        }
        Err(e) => crate::error::AppError::from(e).into_json(),
    }
}

// ---------------------------------------------------------------------------
// Filter + ranking
// ---------------------------------------------------------------------------

/// Translate the raw query string into a [`SearchFilter`], clamping the row limit.
fn to_filter(qy: &SearchQuery, limit: usize) -> SearchFilter {
    SearchFilter {
        q: nonempty(&qy.q),
        host: effective_host(qy),
        app: nonempty(&qy.app),
        severity: nonempty(&qy.severity),
        template_id: nonempty(&qy.template_id),
        since: qy.since.trim().parse::<i64>().ok(),
        until: qy.until.trim().parse::<i64>().ok(),
        before_ts: qy.before_ts,
        before_id: nonempty(&qy.before_id),
        limit: limit.clamp(1, SEARCH_LIMIT),
    }
}

fn page_limit(qy: &SearchQuery) -> usize {
    qy.limit
        .unwrap_or(DEFAULT_PAGE_LIMIT)
        .clamp(1, SEARCH_LIMIT)
}

fn fetch_limit(page_limit: usize) -> usize {
    page_limit.saturating_add(1).min(SEARCH_LIMIT)
}

fn page_from_rows(mut rows: Vec<LogEntry>, page_limit: usize, q: Option<&str>) -> SearchPage {
    let page_limit = page_limit.clamp(1, SEARCH_LIMIT);
    let next_cursor = if rows.len() > page_limit {
        rows.get(page_limit - 1).map(SearchCursor::from)
    } else {
        None
    };
    if rows.len() > page_limit {
        rows.truncate(page_limit);
    }
    SearchPage {
        rows: rank_by_overlap(rows, q),
        next_cursor,
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

fn effective_host(qy: &SearchQuery) -> Option<String> {
    nonempty(&qy.host).or_else(|| nonempty(&qy.source))
}

impl From<&LogEntry> for SearchCursor {
    fn from(log: &LogEntry) -> Self {
        SearchCursor {
            before_ts: log.ts,
            before_id: log.id.clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// Render the GET filter bar, preserving the current values (severity select keeps its choice).
fn render_filter_bar(qy: &SearchQuery) -> String {
    let severities = [
        "", "emerg", "alert", "crit", "err", "warning", "notice", "info", "debug",
    ];
    let host = effective_host(qy).unwrap_or_default();
    let limit = qy.limit.map(|n| n.to_string()).unwrap_or_default();
    let mut options = String::new();
    for value in severities {
        let label = if value.is_empty() {
            "Severity ≥ any"
        } else {
            value
        };
        options.push_str(&format!(
            r#"<option value="{val}"{sel}>{label}</option>"#,
            val = esc(value),
            sel = if value == qy.severity.trim() {
                " selected"
            } else {
                ""
            },
            label = esc(label),
        ));
    }
    format!(
        r#"<form class="filterbar filterbar--wrap" method="get" action="/">
  <div class="filterbar__field filterbar__field--grow"><label for="f-q">Search messages</label><input type="search" id="f-q" name="q" value="{q}" placeholder="connection reset" autocomplete="off" spellcheck="false"></div>
  <div class="filterbar__field"><label for="f-host">Source host</label><input type="text" id="f-host" name="host" value="{host}" placeholder="host-a1" autocomplete="off" spellcheck="false"></div>
  <div class="filterbar__field"><label for="f-app">App</label><input type="text" id="f-app" name="app" value="{app}" placeholder="gateway" autocomplete="off" spellcheck="false"></div>
  <div class="filterbar__field"><label for="f-sev">Severity</label><select id="f-sev" name="severity">{options}</select></div>
  <div class="filterbar__field"><label for="f-since">Since</label><input type="text" id="f-since" name="since" value="{since}" placeholder="epoch s" autocomplete="off" spellcheck="false" inputmode="numeric"></div>
  <div class="filterbar__field"><label for="f-until">Until</label><input type="text" id="f-until" name="until" value="{until}" placeholder="epoch s" autocomplete="off" spellcheck="false" inputmode="numeric"></div>
  <div class="filterbar__field filterbar__field--sm"><label for="f-limit">Limit</label><input type="text" id="f-limit" name="limit" value="{limit}" placeholder="200" autocomplete="off" spellcheck="false" inputmode="numeric"></div>
  <input type="hidden" name="template_id" value="{tid}">
  <div class="filterbar__actions"><button class="btn btn-primary" type="submit">{search}Search</button><a class="btn btn-ghost" href="/">Reset</a></div>
</form>"#,
        q = esc(qy.q.trim()),
        host = esc(&host),
        app = esc(qy.app.trim()),
        options = options,
        since = esc(qy.since.trim()),
        until = esc(qy.until.trim()),
        limit = esc(&limit),
        tid = esc(qy.template_id.trim()),
        search = ICON_SEARCH,
    )
}

/// The severity breakdown beside the table: one bar per severity, widest count sets the scale.
fn render_severity_bars(counts: &[(String, i64)]) -> String {
    if counts.is_empty() {
        return r#"<div class="card__pad"><div class="empty-tile">No logs in the last 24 hours</div></div>"#
            .to_string();
    }
    let order = ["crit", "err", "warning", "notice", "info", "debug"];
    let mut folded: Vec<(&str, i64)> = order.iter().map(|name| (*name, 0)).collect();
    for (severity, count) in counts {
        let bucket = match severity.as_str() {
            "emerg" | "alert" | "crit" => "crit",
            "err" => "err",
            "warning" => "warning",
            "notice" => "notice",
            "debug" => "debug",
            _ => "info",
        };
        if let Some(slot) = folded.iter_mut().find(|(name, _)| *name == bucket) {
            slot.1 += count;
        }
    }
    let max = folded.iter().map(|(_, n)| *n).max().unwrap_or(0).max(1);
    let mut out = String::from(r#"<div class="sevbars">"#);
    for (name, count) in folded {
        let width = (count as f64 / max as f64 * 100.0).round().max(0.0);
        out.push_str(&format!(
            r#"<div class="sevbar sevbar--{name}"><span class="sev sev--{name}">{label}</span><span class="sevbar__track"><span class="sevbar__fill" style="width:{width}%"></span></span><span class="sevbar__n">{count}</span></div>"#,
            name = name,
            label = esc(if name == "warning" { "warn" } else { name }),
            width = width,
            count = fmt_count(count),
        ));
    }
    out.push_str("</div>");
    out
}

fn render_pager(qy: &SearchQuery, page: &SearchPage) -> String {
    if page.rows.is_empty() {
        return String::new();
    }
    let label = format!(
        "Showing {} log{} · newest first",
        page.rows.len(),
        if page.rows.len() == 1 { "" } else { "s" },
    );
    let next = match &page.next_cursor {
        Some(cursor) => format!(
            r#"<a class="btn btn-secondary btn-sm" href="{href}">{icon}Next page</a>"#,
            href = esc(&format!("/?{}", build_query_string(qy, Some(cursor)))),
            icon = ICON_CHEVRON,
        ),
        None => r#"<span class="pagination__range">end of results</span>"#.to_string(),
    };
    format!(
        r#"<div class="table-meta"><span class="table-meta__showing">{label}</span><div class="pagination">{next}</div></div>"#,
        label = esc(&label),
        next = next,
    )
}

/// Render the log table body rows. Empty result -> an empty tile spanning the table.
fn render_rows(rows: &[LogEntry]) -> String {
    if rows.is_empty() {
        return r#"<tr><td class="empty" colspan="5"><div class="card__pad"><div class="empty-tile">No log line matches these filters. Widen the range, or wait for ingest.</div></div></td></tr>"#.to_string();
    }
    let mut out = String::with_capacity(rows.len() * 200);
    for l in rows {
        let class = match severity_class(&l.severity) {
            "sev--crit" => " class=\"is-crit\"",
            "sev--err" => " class=\"is-err\"",
            _ => "",
        };
        out.push_str(&format!(
            r#"<tr{class}>
  <td class="c-time">{time}</td>
  <td class="c-sev"><span class="sev {sevclass}">{sev}</span></td>
  <td class="c-host">{host}</td>
  <td class="c-app">{app}</td>
  <td class="c-msg{msgclass}"><a class="tmpl-link" href="/?template_id={tid}" title="filter to this template">{glyph}</a>{msg}</td>
</tr>"#,
            class = class,
            time = esc(&fmt_datetime(l.ts)),
            sevclass = severity_class(&l.severity),
            sev = esc(short_severity(&l.severity)),
            host = esc(if l.host.is_empty() { "—" } else { &l.host }),
            app = esc(if l.app.is_empty() { "—" } else { &l.app }),
            msgclass = if l.severity == "debug" { " c-msg--debug" } else { "" },
            tid = esc(&l.template_id),
            glyph = ICON_TEMPLATE,
            msg = esc(&l.message),
        ));
    }
    out
}

/// The badge label: syslog's six upper severities collapse onto the four the badge shows.
fn short_severity(severity: &str) -> &str {
    match severity {
        "emerg" | "alert" | "crit" => "crit",
        "warning" => "warn",
        other => other,
    }
}

/// Render the top-templates rail; each row links to its `?template_id=` filtered view.
fn render_templates(templates: &[crate::store::Template], active: Option<&str>) -> String {
    if templates.is_empty() {
        return r#"<div class="card__pad"><div class="empty-tile">No message template clustered yet</div></div>"#.to_string();
    }
    let mut out = String::with_capacity(templates.len() * 160);
    for t in templates {
        out.push_str(&format!(
            r#"<a class="tmpl{state}" href="/?template_id={id}"><span class="tmpl__count">{count}</span><span class="tmpl__sample">{sample}</span></a>"#,
            state = if active == Some(t.id.as_str()) {
                " is-active"
            } else {
                ""
            },
            id = esc(&t.id),
            count = fmt_count(t.count),
            sample = esc(&truncate(&t.sample, 90)),
        ));
    }
    out
}

/// Group-separated count (e.g. `12 408`, thin space), so big numbers read at a glance in mono.
fn fmt_count(n: i64) -> String {
    let s = n.abs().to_string();
    let mut out = String::new();
    let bytes = s.as_bytes();
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (bytes.len() - i) % 3 == 0 {
            out.push(' ');
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

fn build_query_string(qy: &SearchQuery, cursor: Option<&SearchCursor>) -> String {
    let mut pairs: Vec<(&str, String)> = Vec::new();
    push_nonempty(&mut pairs, "q", &qy.q);
    if let Some(host) = effective_host(qy) {
        pairs.push(("host", host));
    }
    push_nonempty(&mut pairs, "app", &qy.app);
    push_nonempty(&mut pairs, "severity", &qy.severity);
    push_nonempty(&mut pairs, "template_id", &qy.template_id);
    push_nonempty(&mut pairs, "since", &qy.since);
    push_nonempty(&mut pairs, "until", &qy.until);
    if let Some(limit) = qy.limit {
        pairs.push(("limit", limit.to_string()));
    }
    if let Some(cursor) = cursor {
        pairs.push(("before_ts", cursor.before_ts.to_string()));
        pairs.push(("before_id", cursor.before_id.clone()));
    }
    pairs
        .into_iter()
        .map(|(k, v)| format!("{}={}", k, percent_encode(&v)))
        .collect::<Vec<_>>()
        .join("&")
}

fn push_nonempty(pairs: &mut Vec<(&'static str, String)>, key: &'static str, value: &str) {
    if let Some(value) = nonempty(value) {
        pairs.push((key, value));
    }
}

fn percent_encode(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(value.len());
    for b in value.as_bytes() {
        let unreserved = b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~');
        if unreserved {
            out.push(*b as char);
        } else {
            out.push('%');
            out.push(HEX[(b >> 4) as usize] as char);
            out.push(HEX[(b & 0x0f) as usize] as char);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn log(id: &str, ts: i64, host: &str, msg: &str) -> LogEntry {
        LogEntry {
            id: id.to_string(),
            ts,
            host: host.to_string(),
            app: "web".to_string(),
            severity: "info".to_string(),
            message: msg.to_string(),
            template_id: "t".to_string(),
        }
    }

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
        assert_eq!(fmt_count(12408), "12 408");
        assert_eq!(fmt_count(1000000), "1 000 000");
    }

    #[test]
    fn page_cursor_uses_unranked_keyset_boundary() {
        let rows = vec![
            log("c", 300, "h1", "ordinary line"),
            log("b", 200, "h1", "database timeout"),
            log("a", 100, "h1", "database timeout"),
        ];
        let page = page_from_rows(rows, 2, Some("database"));

        assert_eq!(page.next_cursor.unwrap().before_id, "b");
        assert_eq!(
            page.rows.iter().map(|l| l.id.as_str()).collect::<Vec<_>>(),
            ["b", "c"]
        );
    }

    #[test]
    fn query_string_preserves_filters_and_encodes_values() {
        let qy = SearchQuery {
            q: "db timeout".to_string(),
            source: "edge/1".to_string(),
            app: "api".to_string(),
            severity: "err".to_string(),
            limit: Some(25),
            ..Default::default()
        };
        let qs = build_query_string(
            &qy,
            Some(&SearchCursor {
                before_ts: 200,
                before_id: "log_1".to_string(),
            }),
        );

        assert!(qs.contains("q=db%20timeout"));
        assert!(qs.contains("host=edge%2F1"));
        assert!(qs.contains("before_ts=200"));
        assert!(qs.contains("before_id=log_1"));
    }
}
