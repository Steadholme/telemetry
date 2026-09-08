//! Server-side rendering of the Vitals dashboard.
//!
//! Pure functions: a `&[HostView]` + the signed-in email in, an HTML `String` out. The CSS
//! is embedded (`include_str!`) so the slim image never misses an asset and the page is one
//! self-contained document. Vitals owns the signal-matrix and host-workbench presentation;
//! only the estate navigation endpoints are shared with other Steadholme products.

use std::collections::BTreeMap;

use crate::analytics;
use crate::chart::{self, Access, Domain, Line, SparkOpts, Tone};
use crate::handlers::APP_CSS_PATH;
use crate::metrics::{self, SampleRow};
use crate::store::{Anomaly, Bucket};

const SERVICE_CSS: &str = include_str!("../static/service.css");
static APP_CSS: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// Canonical Odyssey base first, the Vitals service layer second — the same layering the other
/// two Telemetry surfaces use, so one estate-wide design system reaches every vhost.
pub fn app_css() -> &'static str {
    APP_CSS
        .get_or_init(|| {
            let mut css = String::with_capacity(odyssey::APP_CSS.len() + SERVICE_CSS.len() + 1);
            css.push_str(odyssey::APP_CSS);
            css.push('\n');
            css.push_str(SERVICE_CSS);
            css
        })
        .as_str()
}

/// Everything the dashboard shows for one host.
#[derive(Clone, Debug, Default)]
pub struct HostView {
    pub host: String,
    pub display_name: String,
    /// metric name -> latest value (headline gauges + raw figures).
    pub gauges: BTreeMap<String, f64>,
    /// Recent cpu_pct series (oldest -> newest) for the sparkline.
    pub spark_cpu: Vec<(i64, f64)>,
    /// Recent mem_pct series (oldest -> newest) for the sparkline.
    pub spark_mem: Vec<(i64, f64)>,
    /// Recent disk_pct series (oldest -> newest) for the sparkline.
    pub spark_disk: Vec<(i64, f64)>,
    /// Recent load1 series (oldest -> newest) for the fleet matrix.
    pub spark_load: Vec<(i64, f64)>,
    /// Recent net_rx_bps series (oldest -> newest) for the sparkline.
    pub spark_net_rx: Vec<(i64, f64)>,
    /// Recent net_tx_bps series (oldest -> newest) for the sparkline.
    pub spark_net_tx: Vec<(i64, f64)>,
    /// Short-term linear projection of cpu_pct beyond the window (dashed sparkline tail).
    pub forecast_cpu: Vec<(i64, f64)>,
    /// Short-term linear projection of mem_pct beyond the window.
    pub forecast_mem: Vec<(i64, f64)>,
    /// Short-term linear projection of disk_pct beyond the window.
    pub forecast_disk: Vec<(i64, f64)>,
    /// Count of in-range anomalies for the host.
    pub anomaly_count: usize,
    /// Epoch seconds of this host's most recent sample (freshness).
    pub last_ts: i64,
}

impl HostView {
    fn g(&self, metric: &str) -> Option<f64> {
        self.gauges.get(metric).copied()
    }

    fn hotness(&self) -> f64 {
        [
            self.g(metrics::M_CPU_PCT),
            self.g(metrics::M_MEM_PCT),
            self.g(metrics::M_DISK_PCT),
        ]
        .into_iter()
        .flatten()
        .fold(0.0, f64::max)
    }
}

/// Build per-host views from the store's `latest()` rows + a recent window of samples.
///
/// `latest` carries one row per `(host, metric)`; `window` carries the recent cpu_pct /
/// mem_pct series used for sparklines (ordered by `(host, metric, ts)`).
pub fn build_host_views(
    latest: &[SampleRow],
    window: &[SampleRow],
    forecast_steps: usize,
    anomaly_counts: &BTreeMap<String, usize>,
    host_aliases: &BTreeMap<String, String>,
    now: i64,
) -> Vec<HostView> {
    let mut by_host: BTreeMap<String, HostView> = BTreeMap::new();
    for r in latest {
        let hv = by_host.entry(r.host.clone()).or_insert_with(|| HostView {
            host: r.host.clone(),
            display_name: host_aliases
                .get(&r.host)
                .cloned()
                .unwrap_or_else(|| r.host.clone()),
            ..Default::default()
        });
        hv.gauges.insert(r.metric.clone(), r.value);
        hv.last_ts = hv.last_ts.max(r.ts);
    }
    for r in window {
        // Skip hosts that have no latest row (shouldn't happen, but stay defensive).
        let Some(hv) = by_host.get_mut(&r.host) else {
            continue;
        };
        match r.metric.as_str() {
            metrics::M_CPU_PCT => hv.spark_cpu.push((r.ts, r.value)),
            metrics::M_MEM_PCT => hv.spark_mem.push((r.ts, r.value)),
            metrics::M_DISK_PCT => hv.spark_disk.push((r.ts, r.value)),
            metrics::M_LOAD1 => hv.spark_load.push((r.ts, r.value)),
            metrics::M_NET_RX => hv.spark_net_rx.push((r.ts, r.value)),
            metrics::M_NET_TX => hv.spark_net_tx.push((r.ts, r.value)),
            _ => {}
        }
    }
    // Short-term linear forecast over each host's recent percent series (Augur's projection,
    // folded in). Skipped when the window is too thin to fit a line.
    if forecast_steps > 0 {
        for hv in by_host.values_mut() {
            hv.forecast_cpu = forecast_series(&hv.spark_cpu, forecast_steps);
            hv.forecast_mem = forecast_series(&hv.spark_mem, forecast_steps);
            hv.forecast_disk = forecast_series(&hv.spark_disk, forecast_steps);
        }
    }
    for hv in by_host.values_mut() {
        hv.anomaly_count = anomaly_counts.get(&hv.host).copied().unwrap_or(0);
    }
    let mut hosts: Vec<HostView> = by_host.into_values().collect();
    hosts.sort_by(|a, b| {
        let a_online = now - a.last_ts <= 600;
        let b_online = now - b.last_ts <= 600;
        b_online
            .cmp(&a_online)
            .then_with(|| {
                b.hotness()
                    .partial_cmp(&a.hotness())
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| a.display_name.cmp(&b.display_name))
            .then_with(|| a.host.cmp(&b.host))
    });
    hosts
}

fn forecast_series(series: &[(i64, f64)], steps: usize) -> Vec<(i64, f64)> {
    if series.len() < 2 || steps == 0 {
        return Vec::new();
    }
    let values: Vec<f64> = series.iter().map(|(_, value)| *value).collect();
    let projected = analytics::forecast(&values, steps).points;
    let last_ts = series.last().map(|(ts, _)| *ts).unwrap_or(0);
    let prev_ts = series
        .get(series.len().saturating_sub(2))
        .map(|(ts, _)| *ts)
        .unwrap_or(last_ts - crate::config::DEFAULT_SCRAPE_INTERVAL as i64);
    let step = (last_ts - prev_ts).max(1);
    projected
        .into_iter()
        .enumerate()
        .map(|(i, value)| (last_ts + (i as i64 + 1) * step, value))
        .collect()
}

/// Render the whole dashboard document.
#[allow(clippy::too_many_arguments)]
pub fn render(
    hosts: &[HostView],
    anomalies: &[Anomaly],
    email: &str,
    theme: &str,
    now: i64,
    since: i64,
    range_label: &str,
    detect_secs: u64,
    z_threshold: f64,
    window: usize,
    klaxon_ready: bool,
) -> String {
    let cards: String = if hosts.is_empty() {
        empty_state()
    } else {
        hosts
            .iter()
            .map(|h| host_card(h, now, since, range_label))
            .collect()
    };
    let anomaly_panel = anomaly_watch(
        anomalies,
        now,
        since,
        range_label,
        detect_secs,
        z_threshold,
        window,
        klaxon_ready,
        None,
    );
    let summary = summary_strip(hosts, now);
    let rangebar = rangebar(None, range_label);
    let snapshot = snapshot_controls(&format!("/?range={range_label}"), now);
    let fresh = hosts.iter().filter(|host| now - host.last_ts <= 60).count();
    let host_word = if hosts.len() == 1 { "host" } else { "hosts" };

    format!(
        r##"<!DOCTYPE html>
<html lang="en" data-density="compact"{theme_attr}>
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1, viewport-fit=cover">
<meta name="color-scheme" content="{color_scheme}">
<title>Fleet · Telemetry · Steadholme</title>
<link rel="stylesheet" href="{css_path}">
</head>
<body class="page-v2 vt-app">
<a class="vt-skip sr-only" href="#vt-main">Skip to fleet</a>
{appbar}
<main class="vt-shell" id="vt-main">
  <header class="vt-pagehead">
    <div>
      <div class="vt-eyebrow">Infrastructure</div>
      <h1>Fleet</h1>
      <p class="vt-pagehead__meta">{host_count} {host_word} · {fresh} updated &lt;1m · scrape every {scrape} s</p>
    </div>
    <div class="vt-pagehead__actions">{rangebar}{snapshot}</div>
  </header>
  {summary}
  <section class="vt-matrix" aria-label="Hosts">
    <div class="vt-matrix__head" aria-hidden="true">
      <span>Host</span><span>CPU</span><span>Memory</span><span>Disk</span><span>Load</span><span>Network</span><span>Deviations</span>
    </div>
    <div class="vt-fleet">{cards}</div>
  </section>
  {anomaly_panel}
  {footer}
</main>
</body>
</html>"##,
        css_path = APP_CSS_PATH,
        theme_attr = odyssey::html_theme_attr(theme),
        color_scheme = odyssey::color_scheme_meta(theme),
        appbar = crate::handlers::suite_bar(
            crate::handlers::SURFACE,
            crate::handlers::SURFACE_HOST,
            email
        ),
        footer = crate::handlers::FOOTER,
        scrape = crate::config::DEFAULT_SCRAPE_INTERVAL,
        host_count = hosts.len(),
        host_word = host_word,
        fresh = fresh,
        summary = summary,
        snapshot = snapshot,
        rangebar = rangebar,
        anomaly_panel = anomaly_panel,
        cards = cards,
    )
}

/// Distinct empty state for a `?host=` value that is not in the current host set. The raw query
/// string is intentionally not echoed.
pub fn render_unknown_host(email: &str, theme: &str) -> String {
    format!(
        r##"<!DOCTYPE html>
<html lang="en"{theme_attr}>
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1, viewport-fit=cover">
<meta name="color-scheme" content="{color_scheme}">
<title>Unknown host · Telemetry · Steadholme</title>
<link rel="stylesheet" href="{css_path}">
</head>
<body class="page-v2 vt-app">
<a class="vt-skip sr-only" href="#vt-main">Skip to content</a>
{appbar}
<main class="vt-shell" id="vt-main">
  <section class="vt-empty">
    <div class="vt-empty__mark" aria-hidden="true">?</div>
    <h1>Unknown host</h1>
    <p>This host is not reporting to Vitals.</p>
    <a class="vt-button" href="/">Back to fleet</a>
  </section>
  {footer}
</main>
</body>
</html>"##,
        css_path = APP_CSS_PATH,
        theme_attr = odyssey::html_theme_attr(theme),
        color_scheme = odyssey::color_scheme_meta(theme),
        appbar = crate::handlers::suite_bar(
            crate::handlers::SURFACE,
            crate::handlers::SURFACE_HOST,
            email
        ),
        footer = crate::handlers::FOOTER,
    )
}

/// Per-host drill-down: breadcrumb, detail head, the five metric sections, and the host
/// eventline (when the window holds events).
#[allow(clippy::too_many_arguments)]
pub fn render_host_detail(
    host: &str,
    hosts: &[HostView],
    buckets: &[Bucket],
    anomalies: &[Anomaly],
    email: &str,
    theme: &str,
    now: i64,
    since: i64,
    range_label: &str,
    detail_step: i64,
    detect_secs: u64,
    z_threshold: f64,
    _window: usize,
) -> String {
    let Some(current) = hosts.iter().find(|h| h.host == host) else {
        return render_unknown_host(email, theme);
    };
    let display = current.display_name.as_str();
    let age = now - current.last_ts;
    let status = host_status(current, now);
    let status_reason = status_reason_markup(current, now);
    let sections = [
        metric_section(
            "CPU",
            current,
            buckets,
            anomalies,
            host,
            metrics::M_CPU_PCT,
            &[metrics::M_CPU_PCT],
            Domain::Pct,
            (since, now),
            detail_step,
            range_label,
        ),
        metric_section(
            "Memory",
            current,
            buckets,
            anomalies,
            host,
            metrics::M_MEM_PCT,
            &[metrics::M_MEM_PCT],
            Domain::Pct,
            (since, now),
            detail_step,
            range_label,
        ),
        metric_section(
            "Disk",
            current,
            buckets,
            anomalies,
            host,
            metrics::M_DISK_PCT,
            &[metrics::M_DISK_PCT],
            Domain::Pct,
            (since, now),
            detail_step,
            range_label,
        ),
        metric_section(
            "Load",
            current,
            buckets,
            anomalies,
            host,
            metrics::M_LOAD1,
            &[metrics::M_LOAD1, metrics::M_LOAD5, metrics::M_LOAD15],
            Domain::Auto { headroom: 1.2 },
            (since, now),
            detail_step,
            range_label,
        ),
        metric_section(
            "Network",
            current,
            buckets,
            anomalies,
            host,
            metrics::M_NET_RX,
            &[metrics::M_NET_RX, metrics::M_NET_TX],
            Domain::Auto { headroom: 1.2 },
            (since, now),
            detail_step,
            range_label,
        ),
    ]
    .join("");
    let events = host_eventline(host, anomalies, since, detect_secs, z_threshold, now);
    let rail = host_rail(hosts, host, range_label, now);
    let rangebar = rangebar(Some(host), range_label);
    let refresh_href = format!("/?host={}&range={range_label}", pct_encode(host));
    let snapshot = snapshot_controls(&refresh_href, now);
    let host_id = host_id_block(display, host, "vt-host__id");
    format!(
        r##"<!DOCTYPE html>
<html lang="en" data-density="compact"{theme_attr}>
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1, viewport-fit=cover">
<meta name="color-scheme" content="{color_scheme}">
<title>{display} · Telemetry · Steadholme</title>
<link rel="stylesheet" href="{css_path}">
</head>
<body class="page-v2 vt-app">
<a class="vt-skip sr-only" href="#vt-main">Skip to host details</a>
{appbar}
<main class="vt-shell vt-shell--detail" id="vt-main">
  <div class="vt-workbench">
    {rail}
    <section class="vt-detailpane" aria-labelledby="vt-host-title">
      <header class="vt-detailhead">
        <div class="vt-detailhead__title">
          <a class="vt-back" href="/">Fleet</a>
          <h1 id="vt-host-title">{display}</h1>
          {host_id}
        </div>
        <div class="vt-detailhead__state">
          <span class="vt-status vt-status--{status_class}"><i aria-hidden="true"></i>{status_label}{status_reason}</span>
          <span>{uptime}</span>
          <span>Seen {last_seen} ago</span>
          {snapshot}
        </div>
        {rangebar}
      </header>
      <div class="vt-detailquick" aria-label="Latest readings">
        <div><span>CPU</span><strong>{cpu}</strong></div>
        <div><span>Memory</span><strong>{mem}</strong></div>
        <div><span>Disk</span><strong>{disk}</strong></div>
        <div><span>Load</span><strong>{load}</strong></div>
        <div class="vt-detailquick__network"><span>Network</span><strong>{network}</strong></div>
      </div>
      <div class="vt-detailcharts">{sections}</div>
      {events}
    </section>
  </div>
  {footer}
</main>
</body>
</html>"##,
        css_path = APP_CSS_PATH,
        theme_attr = odyssey::html_theme_attr(theme),
        color_scheme = odyssey::color_scheme_meta(theme),
        appbar = crate::handlers::suite_bar(
            crate::handlers::SURFACE,
            crate::handlers::SURFACE_HOST,
            email
        ),
        footer = crate::handlers::FOOTER,
        rail = rail,
        host_id = host_id,
        display = esc(display),
        status_class = status.class(),
        status_label = status.label(),
        status_reason = status_reason,
        uptime = esc(&current
            .g(metrics::M_UPTIME)
            .map(|v| human_uptime(v as i64))
            .unwrap_or_else(|| "—".to_string())),
        last_seen = esc(&human_age(age)),
        snapshot = snapshot,
        rangebar = rangebar,
        cpu = esc(&pct_fmt(current.g(metrics::M_CPU_PCT))),
        mem = esc(&pct_fmt(current.g(metrics::M_MEM_PCT))),
        disk = esc(&pct_fmt(current.g(metrics::M_DISK_PCT))),
        load = esc(&current
            .g(metrics::M_LOAD1)
            .map(|v| format!("{v:.2}"))
            .unwrap_or_else(|| "—".to_string())),
        network = esc(&net_now(current)),
        sections = sections,
        events = events,
    )
}

fn host_rail(hosts: &[HostView], active_host: &str, range_label: &str, now: i64) -> String {
    let links = hosts
        .iter()
        .map(|host| {
            let status = host_status(host, now);
            let current = if host.host == active_host { " is-active" } else { "" };
            let aria = if host.host == active_host { r#" aria-current="page""# } else { "" };
            let href = format!("/?host={}&range={}", pct_encode(&host.host), range_label);
            let host_id = host_id_inline(&host.display_name, &host.host);
            format!(
                r#"<a class="vt-hostrail__item{current}" href="{href}"{aria}>
  <span class="vt-statusdot vt-statusdot--{status_class}" aria-hidden="true"></span>
  <span class="vt-hostrail__identity"><strong>{display}</strong>{host_id}</span>
  <span class="vt-hostrail__readings"><span>CPU {cpu}</span><span>MEM {mem}</span><span>DISK {disk}</span></span>
</a>"#,
                current = current,
                href = esc(&href),
                aria = aria,
                status_class = status.class(),
                display = esc(&host.display_name),
                host_id = host_id,
                cpu = esc(&pct_fmt(host.g(metrics::M_CPU_PCT))),
                mem = esc(&pct_fmt(host.g(metrics::M_MEM_PCT))),
                disk = esc(&pct_fmt(host.g(metrics::M_DISK_PCT))),
            )
        })
        .collect::<String>();
    format!(
        r#"<aside class="vt-hostrail" aria-label="Hosts">
  <div class="vt-hostrail__head"><span>Hosts</span><span>{count}</span></div>
  <nav>{links}</nav>
</aside>"#,
        count = hosts.len(),
        links = links,
    )
}

#[allow(clippy::too_many_arguments)]
fn metric_section(
    title: &str,
    host: &HostView,
    buckets: &[Bucket],
    anomalies: &[Anomaly],
    host_id: &str,
    primary_metric: &str,
    metrics: &[&str],
    domain: Domain,
    range: (i64, i64),
    step: i64,
    range_label: &str,
) -> String {
    let series_by_metric: Vec<(&str, Vec<(i64, f64)>)> = metrics
        .iter()
        .map(|metric| (*metric, bucket_series(buckets, metric, range.0, step)))
        .collect();
    let lines: Vec<Line<'_>> = series_by_metric
        .iter()
        .map(|(metric, series)| Line {
            points: series.as_slice(),
            class: metric_line_class(metric),
            area: metrics.len() == 1,
        })
        .collect();
    let primary_series = series_by_metric
        .iter()
        .find(|(metric, _)| *metric == primary_metric)
        .map(|(_, series)| series.as_slice())
        .unwrap_or(&[]);
    let anoms = anomaly_points(anomalies, host_id, primary_metric, range.0);
    let summary = field_summary(primary_metric, host.g(primary_metric), primary_series);
    // The merged gap union is computed by the authoritative emitter (`default_gap` stays
    // chart-private and must not be duplicated here), so probe the real geometry to learn
    // whether the union is non-empty for the accessible name and the gap legend key.
    let has_gaps = detail_has_gap_bands(&lines, &domain, range, 720.0, 180.0);
    let slug = field_slug(primary_metric);
    let name_id = format!("vt-fld-{slug}");
    let mut name = format!(
        "{title} trace · {range_label} window · {n} samples · latest {latest} · min {min} · avg {avg} · max {max}",
        n = summary.n,
        latest = summary.latest,
        min = summary.min,
        avg = summary.avg,
        max = summary.max,
    );
    if has_gaps {
        name.push_str(" · gaps present");
    }
    let access = Access {
        label: &name,
        desc: None,
        name_id: &name_id,
    };
    let svg = chart::detail_svg(&lines, &anoms, &domain, range, 720.0, 180.0, &access);
    let stats = stats_strip(&summary, &format!("{name_id}-rail"));
    let legend = legend_for(metrics, &domain, has_gaps, summary.n < 2);
    let subrow = section_subrow(host, primary_metric);
    let chart = chart_frame(primary_metric, primary_series, &domain, range, &svg);
    format!(
        r#"<section class="vt-section">
  <div class="vt-section__head">
    <div>
      <h2 id="{title_id}">{title}</h2>
      {subrow}
    </div>
  </div>
  {chart}
  {stats}
  {legend}
</section>"#,
        title_id = esc(&format!("{name_id}-title")),
        title = esc(title),
        subrow = subrow,
        stats = stats,
        chart = chart,
        legend = legend,
    )
}

fn bucket_series(buckets: &[Bucket], metric: &str, since: i64, step: i64) -> Vec<(i64, f64)> {
    buckets
        .iter()
        .filter(|b| b.metric == metric)
        .map(|b| (since + b.bucket * step, b.avg))
        .collect()
}

/// The one field-summary source: latest/min/avg/max as already-formatted strings plus the
/// rendered-sample count, computed once from the rendered series only. Both the visible
/// `.vt-stats` rail and the SVG accessible name read these same bytes, so they cannot
/// diverge; `chart.rs` formats no label number of its own.
struct FieldSummary {
    latest: String,
    min: String,
    avg: String,
    max: String,
    n: usize,
}

fn field_summary(metric: &str, current: Option<f64>, series: &[(i64, f64)]) -> FieldSummary {
    let values: Vec<f64> = series.iter().map(|(_, value)| *value).collect();
    let min = values.iter().copied().reduce(f64::min);
    let max = values.iter().copied().reduce(f64::max);
    let avg = if values.is_empty() {
        None
    } else {
        Some(values.iter().sum::<f64>() / values.len() as f64)
    };
    let fmt = |value: Option<f64>| {
        value
            .map(|v| fmt_metric(metric, v))
            .unwrap_or_else(|| "—".to_string())
    };
    FieldSummary {
        latest: fmt(current),
        min: fmt(min),
        avg: fmt(avg),
        max: fmt(max),
        n: series.len(),
    }
}

fn stats_strip(summary: &FieldSummary, rail_id: &str) -> String {
    format!(
        r#"<div class="vt-stats" id="{rail_id}">
  {latest}
  {min}
  {avg}
  {max}
</div>"#,
        rail_id = esc(rail_id),
        latest = stat_cell("latest", &summary.latest),
        min = stat_cell("min", &summary.min),
        avg = stat_cell("avg", &summary.avg),
        max = stat_cell("max", &summary.max),
    )
}

fn stat_cell(label: &str, value: &str) -> String {
    format!(
        r#"<div class="vt-stat"><span>{label}</span><b>{value}</b></div>"#,
        label = esc(label),
        value = esc(value),
    )
}

fn chart_frame(
    metric: &str,
    series: &[(i64, f64)],
    domain: &Domain,
    range: (i64, i64),
    svg: &str,
) -> String {
    let axis = y_axis(metric, series, domain);
    format!(
        r#"<div class="vt-chart">
  <div class="vt-chart__yaxis">{axis}</div>
  <div class="vt-chart__plot">{svg}</div>
  <div class="vt-chart__xaxis"><span>{start} ago</span><span>Now</span></div>
</div>"#,
        axis = axis,
        svg = svg,
        start = esc(&human_age(range.1 - range.0)),
    )
}

fn y_axis(metric: &str, series: &[(i64, f64)], domain: &Domain) -> String {
    let labels: Vec<String> = match domain {
        Domain::Pct => vec![
            "100.0%".to_string(),
            "75.0%".to_string(),
            "50.0%".to_string(),
            "25.0%".to_string(),
            "0.0%".to_string(),
        ],
        Domain::Auto { headroom } => {
            let max = series
                .iter()
                .map(|(_, value)| *value)
                .fold(0.0, f64::max)
                .max(1.0)
                * headroom.max(1.0);
            [max, max * 0.75, max * 0.5, max * 0.25, 0.0]
                .into_iter()
                .map(|v| fmt_metric(metric, v))
                .collect()
        }
    };
    labels
        .iter()
        .map(|label| format!(r#"<span>{}</span>"#, esc(label)))
        .collect()
}

/// Detail legend: multi-series line keys, the threshold key (Pct domains only), the gap
/// key when the field renders merged gap bands, and the empty-series micro-key when the
/// primary series has nothing drawable in the window.
fn legend_for(metrics: &[&str], domain: &Domain, has_gaps: bool, primary_empty: bool) -> String {
    let mut items = String::new();
    if metrics.len() >= 2 {
        for metric in metrics {
            let label = match *metric {
                metrics::M_LOAD1 => "1m",
                metrics::M_LOAD5 => "5m",
                metrics::M_LOAD15 => "15m",
                metrics::M_NET_RX => "↓ RX",
                metrics::M_NET_TX => "↑ TX",
                _ => *metric,
            };
            items.push_str(&format!(
                r#"<span class="{class}"><i></i>{label}</span>"#,
                class = esc(metric_line_class(metric)),
                label = esc(label),
            ));
        }
    }
    if matches!(domain, Domain::Pct) {
        items.push_str(r#"<span class="vt-legend__threshold"><i></i>90% threshold</span>"#);
    }
    if has_gaps {
        items.push_str(r#"<span class="vt-legend__gap"><i></i>Missing data</span>"#);
    }
    if primary_empty {
        items.push_str(r#"<span class="vt-legend__empty">No samples in range</span>"#);
    }
    if items.is_empty() {
        String::new()
    } else {
        format!(r#"<div class="vt-legend">{items}</div>"#)
    }
}

fn section_subrow(host: &HostView, metric: &str) -> String {
    let text = match metric {
        metrics::M_MEM_PCT => match (host.g(metrics::M_MEM_USED), host.g(metrics::M_MEM_TOTAL)) {
            (Some(u), Some(t)) => format!("{} / {}", human_bytes(u), human_bytes(t)),
            _ => String::new(),
        },
        metrics::M_DISK_PCT => {
            match (host.g(metrics::M_DISK_USED), host.g(metrics::M_DISK_TOTAL)) {
                (Some(u), Some(t)) => format!("{} / {}", human_bytes(u), human_bytes(t)),
                _ => String::new(),
            }
        }
        metrics::M_LOAD1 => match (
            host.g(metrics::M_LOAD1),
            host.g(metrics::M_LOAD5),
            host.g(metrics::M_LOAD15),
        ) {
            (Some(a), Some(b), Some(c)) => format!("{a:.2} · {b:.2} · {c:.2}"),
            _ => String::new(),
        },
        metrics::M_NET_RX => net_now(host),
        _ => String::new(),
    };
    if text.is_empty() {
        String::new()
    } else {
        format!(r#"<p class="muted">{}</p>"#, esc(&text))
    }
}

fn metric_line_class(metric: &str) -> &'static str {
    match metric {
        metrics::M_LOAD1 => "vt-load-1",
        metrics::M_LOAD5 => "vt-load-5",
        metrics::M_LOAD15 => "vt-load-15",
        metrics::M_NET_RX => "vt-net-rx",
        metrics::M_NET_TX => "vt-net-tx",
        _ => "vt-tone-calm",
    }
}

fn anomaly_points(anomalies: &[Anomaly], host: &str, metric: &str, since: i64) -> Vec<(i64, f64)> {
    anomalies
        .iter()
        .filter(|a| a.host == host && a.metric == metric && a.ts >= since)
        .map(|a| (a.ts, a.value))
        .collect()
}

fn host_eventline(
    host: &str,
    anomalies: &[Anomaly],
    since: i64,
    detect_secs: u64,
    z_threshold: f64,
    now: i64,
) -> String {
    let host_rows: Vec<Anomaly> = anomalies
        .iter()
        .filter(|a| a.host == host && a.ts >= since)
        .cloned()
        .collect();
    let events = group_anomalies_with_threshold(&host_rows, detect_secs, z_threshold);
    if events.is_empty() {
        return String::new();
    }
    let items = events
        .iter()
        .map(|event| {
            let dot = match event.tier {
                EventTier::Warn => "eventline__dot eventline__dot--warn",
                EventTier::Crit => "eventline__dot eventline__dot--down",
            };
            format!(
                r#"<li class="eventline__item">
  <span class="{dot}" aria-hidden="true"></span>
  <time class="eventline__time">{when}</time>
  <div class="eventline__title">{title} · z = {z}</div>
  <div class="eventline__body">{value}</div>
</li>"#,
                dot = dot,
                when = esc(&format!("{} ago", human_age(now - event.last_ts))),
                title = esc(&event.title),
                z = esc(&format!("{:.2}", event.peak_z)),
                value = esc(&event.value_text),
            )
        })
        .collect::<String>();
    format!(
        r#"<section class="vt-section vt-eventsline">
  <div class="vt-section__head"><h2>Statistical deviations</h2></div>
  <div class="vt-section__body"><ol class="eventline">{items}</ol></div>
</section>"#,
        items = items,
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EventTier {
    Warn,
    Crit,
}

#[derive(Clone, Debug, PartialEq)]
pub struct VtEvent {
    pub host: String,
    pub family: String,
    pub title: String,
    pub value_text: String,
    pub peak_z: f64,
    pub count: usize,
    pub first_ts: i64,
    pub last_ts: i64,
    pub tier: EventTier,
}

/// Group recent raw anomaly rows into render-level events with default detector threshold.
pub fn group_anomalies(anomalies: &[Anomaly], detect_secs: u64) -> Vec<VtEvent> {
    group_anomalies_with_threshold(anomalies, detect_secs, crate::config::DEFAULT_Z)
}

fn group_anomalies_with_threshold(
    anomalies: &[Anomaly],
    detect_secs: u64,
    z_threshold: f64,
) -> Vec<VtEvent> {
    #[derive(Clone)]
    struct SameTs {
        host: String,
        family: String,
        title: String,
        ts: i64,
        peak_z: f64,
        values: Vec<(String, String)>,
    }

    let mut by_family_ts: BTreeMap<(String, String, i64), SameTs> = BTreeMap::new();
    for anomaly in anomalies {
        let (family, title) = metric_family(&anomaly.metric);
        let entry = by_family_ts
            .entry((anomaly.host.clone(), family.to_string(), anomaly.ts))
            .or_insert_with(|| SameTs {
                host: anomaly.host.clone(),
                family: family.to_string(),
                title: title.to_string(),
                ts: anomaly.ts,
                peak_z: anomaly.score,
                values: Vec::new(),
            });
        if anomaly.score.abs() > entry.peak_z.abs() {
            entry.peak_z = anomaly.score;
        }
        entry.values.push((
            anomaly.metric.clone(),
            fmt_metric(&anomaly.metric, anomaly.value),
        ));
    }

    let mut folded: Vec<VtEvent> = Vec::new();
    let gap = (detect_secs.max(1) * 2) as i64;
    for same_ts in by_family_ts.into_values() {
        let value_text = synth_value_text(&same_ts.values);
        match folded.last_mut() {
            Some(prev)
                if prev.host == same_ts.host
                    && prev.family == same_ts.family
                    && same_ts.ts - prev.last_ts <= gap =>
            {
                prev.count += 1;
                prev.last_ts = same_ts.ts;
                prev.value_text = value_text;
                if same_ts.peak_z.abs() > prev.peak_z.abs() {
                    prev.peak_z = same_ts.peak_z;
                }
                prev.tier = event_tier(prev.peak_z, z_threshold);
            }
            _ => folded.push(VtEvent {
                host: same_ts.host,
                family: same_ts.family,
                title: same_ts.title,
                value_text,
                peak_z: same_ts.peak_z,
                count: 1,
                first_ts: same_ts.ts,
                last_ts: same_ts.ts,
                tier: event_tier(same_ts.peak_z, z_threshold),
            }),
        }
    }
    folded.sort_by(|a, b| {
        b.last_ts
            .cmp(&a.last_ts)
            .then(a.host.cmp(&b.host))
            .then(a.family.cmp(&b.family))
    });
    folded
}

#[allow(clippy::too_many_arguments)]
fn anomaly_watch(
    anomalies: &[Anomaly],
    now: i64,
    since: i64,
    range_label: &str,
    detect_secs: u64,
    z_threshold: f64,
    window: usize,
    klaxon_ready: bool,
    active_host: Option<&str>,
) -> String {
    let in_range: Vec<Anomaly> = anomalies
        .iter()
        .filter(|a| a.ts >= since)
        .filter(|a| active_host.is_none_or(|host| a.host == host))
        .cloned()
        .collect();
    let events = group_anomalies_with_threshold(&in_range, detect_secs, z_threshold);
    let crit = events.iter().filter(|e| e.tier == EventTier::Crit).count();
    let warn = events.len().saturating_sub(crit);
    let klaxon = if klaxon_ready {
        r#"<span class="vt-chip">Klaxon</span>"#
    } else {
        ""
    };
    let body = if events.is_empty() {
        format!(
            r#"<div class="vt-quiet">
  <strong>No deviations in {range_label}</strong>
  <span>z ≥ {z:.1} · {window} samples · every {detect_secs}s</span>
</div>"#,
            range_label = esc(range_label),
            z = z_threshold,
            window = window,
            detect_secs = detect_secs,
        )
    } else {
        let rows = events
            .iter()
            .enumerate()
            .map(|(i, e)| event_row(e, now, i + 1))
            .collect::<String>();
        format!(r#"<div class="vt-events">{rows}</div>"#)
    };

    format!(
        r#"<section id="vt-anomaly" class="vt-anomaly" data-density="compact">
  <header class="vt-anomaly__head">
    <div>
      <h2>Statistical deviations</h2>
      <p class="vt-anomaly__meta">Unusual change · z ≥ {z:.1} · {window} samples · every {detect_secs}s</p>
    </div>
    <div class="vt-anomaly__counts"><span class="vt-chip vt-chip--critical">{crit} critical</span><span class="vt-chip vt-chip--warning">{warn} warning</span>{klaxon}</div>
  </header>
  <div class="vt-anomaly__body">
    {filters}
    {body}
  </div>
</section>"#,
        z = z_threshold,
        window = window,
        detect_secs = detect_secs,
        crit = crit,
        warn = warn,
        klaxon = klaxon,
        filters = anomaly_filters(anomalies, since, range_label, active_host),
        body = body,
    )
}

fn anomaly_filters(
    anomalies: &[Anomaly],
    since: i64,
    range_label: &str,
    active_host: Option<&str>,
) -> String {
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for a in anomalies.iter().filter(|a| a.ts >= since) {
        *counts.entry(a.host.clone()).or_insert(0) += 1;
    }
    let total: usize = counts.values().sum();
    let (all_class, all_current) = if active_host.is_none() {
        ("vt-chip is-active", r#" aria-current="true""#)
    } else {
        ("vt-chip", "")
    };
    let mut out = format!(
        r#"<div class="vt-anomaly__filters"><a class="{all_class}" href="/?range={range}"{all_current}>All <span class="countpill">{total}</span></a>"#,
        all_class = all_class,
        all_current = all_current,
        range = esc(range_label),
        total = total,
    );
    for (host, count) in counts {
        let (class, current) = if active_host == Some(host.as_str()) {
            ("vt-chip is-active", r#" aria-current="true""#)
        } else {
            ("vt-chip", "")
        };
        let href = format!("/?host={}&range={}", pct_encode(&host), range_label);
        out.push_str(&format!(
            r#"<a class="{class}" href="{href}"{current}>{tile}<span>{host}</span><span class="countpill">{count}</span></a>"#,
            class = class,
            current = current,
            href = esc(&href),
            tile = node_glyph(&host),
            host = esc(&host),
            count = count,
        ));
    }
    out.push_str("</div>");
    out
}

fn event_row(event: &VtEvent, now: i64, ordinal: usize) -> String {
    let tier_class = match event.tier {
        EventTier::Warn => "vt-event--warn",
        EventTier::Crit => "vt-event--crit",
    };
    let dot_class = match event.tier {
        EventTier::Warn => "eventline__dot--warn",
        EventTier::Crit => "eventline__dot--down",
    };
    let tier_word = match event.tier {
        EventTier::Warn => "Warning",
        EventTier::Crit => "Critical",
    };
    let direction = if event.peak_z >= 0.0 {
        "↑ surge"
    } else {
        "↓ drop"
    };
    let count = if event.count > 1 {
        format!(r#"<span class="countpill">×{}</span>"#, event.count)
    } else {
        String::new()
    };
    format!(
        r#"<article class="vt-event {tier_class}">
  <span class="vt-event__dot {dot_class}" aria-hidden="true"></span>
  <span class="vt-event__no mono">#{ordinal:02}</span>
  <div class="vt-event__main">
    <div class="vt-event__host">{tile}<span class="mono">{host}</span>{count}</div>
    <div class="vt-event__title">{title} · {direction}</div>
    <div class="vt-event__val">{value}</div>
  </div>
  <span class="vt-event__tier">{tier_word}</span>
  <div class="vt-event__z">z = {z}</div>
  <time class="vt-event__time">{when}</time>
</article>"#,
        tier_class = tier_class,
        dot_class = dot_class,
        ordinal = ordinal,
        tier_word = tier_word,
        tile = node_glyph(&event.host),
        host = esc(&event.host),
        count = count,
        title = esc(&event.title),
        direction = direction,
        value = esc(&event.value_text),
        z = esc(&format!("{:.2}", event.peak_z)),
        when = esc(&format!("{} ago", human_age(now - event.last_ts))),
    )
}

fn metric_family(metric: &str) -> (&'static str, &'static str) {
    match metric {
        metrics::M_CPU_PCT => ("cpu", "CPU"),
        metrics::M_MEM_PCT | metrics::M_MEM_USED | metrics::M_MEM_TOTAL => ("mem", "Memory"),
        metrics::M_DISK_PCT | metrics::M_DISK_USED | metrics::M_DISK_TOTAL => ("disk", "Disk"),
        metrics::M_LOAD1 | metrics::M_LOAD5 | metrics::M_LOAD15 => ("load", "Load"),
        metrics::M_NET_RX | metrics::M_NET_TX => ("net", "Network"),
        metrics::M_UPTIME => ("uptime", "Uptime"),
        _ => ("other", "Metric"),
    }
}

fn synth_value_text(values: &[(String, String)]) -> String {
    let mut sorted = values.to_vec();
    sorted.sort_by_key(|(metric, _)| metric_order(metric));
    let pct: Vec<String> = sorted
        .iter()
        .filter(|(metric, _)| metric.ends_with("_pct"))
        .map(|(_, value)| value.clone())
        .collect();
    let rest: Vec<String> = sorted
        .iter()
        .filter(|(metric, _)| !metric.ends_with("_pct"))
        .map(|(_, value)| value.clone())
        .collect();
    match (pct.is_empty(), rest.is_empty()) {
        (false, false) => format!("{} ({})", pct.join(" · "), rest.join(" · ")),
        (false, true) => pct.join(" · "),
        (true, false) => rest.join(" · "),
        (true, true) => "—".to_string(),
    }
}

fn metric_order(metric: &str) -> usize {
    match metric {
        metrics::M_CPU_PCT | metrics::M_MEM_PCT | metrics::M_DISK_PCT => 0,
        metrics::M_MEM_USED | metrics::M_DISK_USED => 1,
        metrics::M_MEM_TOTAL | metrics::M_DISK_TOTAL => 2,
        metrics::M_LOAD1 | metrics::M_NET_RX => 3,
        metrics::M_LOAD5 | metrics::M_NET_TX => 4,
        metrics::M_LOAD15 => 5,
        _ => 9,
    }
}

fn event_tier(score: f64, z_threshold: f64) -> EventTier {
    if score.abs() >= z_threshold * 2.0 {
        EventTier::Crit
    } else {
        EventTier::Warn
    }
}

/// Cross-subdomain SSO logout (terminated at the gateway / Keystone IdP).
fn node_glyph(label: &str) -> String {
    let initial = label
        .chars()
        .find(|character| character.is_alphanumeric())
        .map(|character| character.to_uppercase().to_string())
        .unwrap_or_else(|| "·".to_string());
    format!(
        r#"<span class="vt-nodeglyph" aria-hidden="true">{}</span>"#,
        esc(&initial),
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HostStatus {
    Healthy,
    Warning,
    Critical,
    Stale,
}

impl HostStatus {
    fn class(self) -> &'static str {
        match self {
            HostStatus::Healthy => "healthy",
            HostStatus::Warning => "warning",
            HostStatus::Critical => "critical",
            HostStatus::Stale => "stale",
        }
    }

    fn label(self) -> &'static str {
        match self {
            HostStatus::Healthy => "Healthy",
            HostStatus::Warning => "Warning",
            HostStatus::Critical => "Critical",
            HostStatus::Stale => "Stale",
        }
    }
}

fn host_status(host: &HostView, now: i64) -> HostStatus {
    if now - host.last_ts > 600 {
        HostStatus::Stale
    } else if host.hotness() >= 90.0 {
        HostStatus::Critical
    } else if host.hotness() >= 70.0 || host.anomaly_count > 0 {
        HostStatus::Warning
    } else {
        HostStatus::Healthy
    }
}

fn host_status_reason(host: &HostView, now: i64) -> Option<String> {
    if now - host.last_ts > 600 {
        return Some(format!("No report for {}", human_age(now - host.last_ts)));
    }
    let hottest = [
        ("CPU", host.g(metrics::M_CPU_PCT)),
        ("Memory", host.g(metrics::M_MEM_PCT)),
        ("Disk", host.g(metrics::M_DISK_PCT)),
    ]
    .into_iter()
    .filter_map(|(label, value)| value.map(|value| (label, value)))
    .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
    if let Some((label, value)) = hottest.filter(|(_, value)| *value >= 70.0) {
        return Some(format!("{label} {value:.1}%"));
    }
    if host.anomaly_count > 0 {
        let noun = if host.anomaly_count == 1 {
            "deviation"
        } else {
            "deviations"
        };
        return Some(format!("{} {noun}", host.anomaly_count));
    }
    None
}

fn status_reason_markup(host: &HostView, now: i64) -> String {
    host_status_reason(host, now)
        .map(|reason| format!(r#"<span class="vt-status__reason">{}</span>"#, esc(&reason)))
        .unwrap_or_default()
}

fn summary_strip(hosts: &[HostView], now: i64) -> String {
    let mut healthy = 0;
    let mut warning = 0;
    let mut critical = 0;
    let mut stale = 0;
    for host in hosts {
        match host_status(host, now) {
            HostStatus::Healthy => healthy += 1,
            HostStatus::Warning => warning += 1,
            HostStatus::Critical => critical += 1,
            HostStatus::Stale => stale += 1,
        }
    }
    format!(
        r#"<section class="vt-summary" aria-label="Fleet summary">
  <div class="vt-summary__item vt-summary__item--healthy"><span class="vt-summary__label"><i aria-hidden="true"></i>Healthy</span><strong>{healthy}</strong><span>of {total}</span></div>
  <div class="vt-summary__item vt-summary__item--warning"><span class="vt-summary__label"><i aria-hidden="true"></i>Warning</span><strong>{warning}</strong><span>of {total}</span></div>
  <div class="vt-summary__item vt-summary__item--critical"><span class="vt-summary__label"><i aria-hidden="true"></i>Critical</span><strong>{critical}</strong><span>of {total}</span></div>
  <div class="vt-summary__item vt-summary__item--stale"><span class="vt-summary__label"><i aria-hidden="true"></i>Stale</span><strong>{stale}</strong><span>of {total}</span></div>
</section>"#,
        healthy = healthy,
        warning = warning,
        critical = critical,
        stale = stale,
        total = hosts.len(),
    )
}

fn rangebar(host: Option<&str>, active: &str) -> String {
    let tabs = ["1h", "6h", "24h", "7d"]
        .iter()
        .map(|label| {
            let href = match host {
                Some(host) => format!("/?host={}&range={label}", pct_encode(host)),
                None => format!("/?range={label}"),
            };
            let class = if *label == active {
                "vt-range__item is-active"
            } else {
                "vt-range__item"
            };
            let current = if *label == active {
                r#" aria-current="page""#
            } else {
                ""
            };
            format!(
                r#"<a class="{class}" href="{href}"{current}>{label}</a>"#,
                class = class,
                href = esc(&href),
                current = current,
                label = esc(label),
            )
        })
        .collect::<String>();
    format!(r#"<nav class="vt-range" aria-label="Time range">{tabs}</nav>"#)
}

fn snapshot_controls(href: &str, now: i64) -> String {
    let seconds = now.rem_euclid(86_400);
    let hour = seconds / 3_600;
    let minute = (seconds % 3_600) / 60;
    let second = seconds % 60;
    format!(
        r#"<div class="vt-snapshot"><span>Updated {hour:02}:{minute:02}:{second:02} UTC</span><a class="vt-refresh" href="{href}">Refresh</a></div>"#,
        href = esc(href),
    )
}

fn host_id_block(display: &str, host: &str, class: &str) -> String {
    if display == host {
        String::new()
    } else {
        format!(
            r#"<div class="{class} mono" title="{host}">{host}</div>"#,
            class = esc(class),
            host = esc(host),
        )
    }
}

fn host_id_inline(display: &str, host: &str) -> String {
    if display == host {
        String::new()
    } else {
        format!(r#"<span class="mono">{}</span>"#, esc(host))
    }
}

/// One aligned fleet row: host identity, five comparable signals, and anomaly count.
fn host_card(h: &HostView, now: i64, since: i64, range_label: &str) -> String {
    let age = now - h.last_ts;
    let status = host_status(h, now);
    let status_reason = status_reason_markup(h, now);
    let host_id = host_id_block(&h.display_name, &h.host, "vt-host__id");

    let cpu = h.g(metrics::M_CPU_PCT);
    let mem = h.g(metrics::M_MEM_PCT);
    let disk = h.g(metrics::M_DISK_PCT);
    let load = h.g(metrics::M_LOAD1);
    let mem_detail = match (h.g(metrics::M_MEM_USED), h.g(metrics::M_MEM_TOTAL)) {
        (Some(used), Some(total)) => format!("{} / {}", human_bytes(used), human_bytes(total)),
        _ => "—".to_string(),
    };
    let disk_detail = match (h.g(metrics::M_DISK_USED), h.g(metrics::M_DISK_TOTAL)) {
        (Some(used), Some(total)) => format!("{} / {}", human_bytes(used), human_bytes(total)),
        _ => "—".to_string(),
    };
    let uptime_detail = h
        .g(metrics::M_UPTIME)
        .map(|s| human_uptime(s as i64))
        .unwrap_or_else(|| "—".to_string());
    let href = format!("/?host={}&range={}", pct_encode(&h.host), range_label);
    let spark_range = (since, now);
    let gap = ((now - since).max(1) / 60).max(crate::config::DEFAULT_SCRAPE_INTERVAL as i64) * 2;

    // Per-spark accessible identities: the caller composes every name from the identical
    // formatted head string shown beside the trace; `chart.rs` formats no label number.
    let cpu_head = pct_fmt(cpu);
    let mem_head = pct_fmt(mem);
    let disk_head = pct_fmt(disk);
    let load_head = load
        .map(|value| format!("{value:.2}"))
        .unwrap_or_else(|| "—".to_string());
    let net_head = net_now(h);
    let cpu_gaps = !chart::gap_spans(&h.spark_cpu, gap).is_empty();
    let mem_gaps = !chart::gap_spans(&h.spark_mem, gap).is_empty();
    let disk_gaps = !chart::gap_spans(&h.spark_disk, gap).is_empty();

    let (cpu_name_id, cpu_name) = pct_spark_access(
        &h.host,
        "CPU %",
        "cpu",
        h.spark_cpu.len(),
        &cpu_head,
        cpu_gaps,
        range_label,
    );
    let cpu_access = Access {
        label: &cpu_name,
        desc: None,
        name_id: &cpu_name_id,
    };
    let cpu_svg = pct_spark(
        &h.spark_cpu,
        &h.forecast_cpu,
        spark_range,
        cpu,
        gap,
        &cpu_access,
    );
    let cpu_foot = spark_foot(h.spark_cpu.len(), !h.forecast_cpu.is_empty(), cpu_gaps);

    let (mem_name_id, mem_name) = pct_spark_access(
        &h.host,
        "MEM %",
        "mem",
        h.spark_mem.len(),
        &mem_head,
        mem_gaps,
        range_label,
    );
    let mem_access = Access {
        label: &mem_name,
        desc: None,
        name_id: &mem_name_id,
    };
    let mem_svg = pct_spark(
        &h.spark_mem,
        &h.forecast_mem,
        spark_range,
        mem,
        gap,
        &mem_access,
    );
    let mem_foot = format!(
        r#"<div class="vt-spark__context">{}</div>{}"#,
        esc(&mem_detail),
        spark_foot(h.spark_mem.len(), !h.forecast_mem.is_empty(), mem_gaps),
    );

    let (disk_name_id, disk_name) = pct_spark_access(
        &h.host,
        "DISK %",
        "disk",
        h.spark_disk.len(),
        &disk_head,
        disk_gaps,
        range_label,
    );
    let disk_access = Access {
        label: &disk_name,
        desc: None,
        name_id: &disk_name_id,
    };
    let disk_svg = pct_spark(
        &h.spark_disk,
        &h.forecast_disk,
        spark_range,
        disk,
        gap,
        &disk_access,
    );
    let disk_foot = format!(
        r#"<div class="vt-spark__context">{}</div>{}"#,
        esc(&disk_detail),
        spark_foot(h.spark_disk.len(), !h.forecast_disk.is_empty(), disk_gaps,),
    );

    let load_lines = [Line {
        points: h.spark_load.as_slice(),
        class: "vt-load-1",
        area: false,
    }];
    let load_gaps = detail_has_gap_bands(
        &load_lines,
        &Domain::Auto { headroom: 1.2 },
        spark_range,
        120.0,
        36.0,
    );
    let load_name_id = format!("vt-spark-{}-load", h.host);
    let mut load_name = format!(
        "{} · load trace · {range_label} window · {} samples · latest {load_head}",
        h.host,
        h.spark_load.len(),
    );
    if load_gaps {
        load_name.push_str(" · gaps present");
    }
    let load_access = Access {
        label: &load_name,
        desc: None,
        name_id: &load_name_id,
    };
    let load_svg = net_spark(&load_lines, spark_range, &load_access);
    let load_foot = spark_foot(h.spark_load.len(), false, load_gaps);

    // NET is a two-series detail chart: the merged gap union covers RX and TX while each
    // polyline stays independently segmented.
    let net_lines = [
        Line {
            points: h.spark_net_rx.as_slice(),
            class: "vt-net-rx",
            area: false,
        },
        Line {
            points: h.spark_net_tx.as_slice(),
            class: "vt-net-tx",
            area: false,
        },
    ];
    let net_gaps = detail_has_gap_bands(
        &net_lines,
        &Domain::Auto { headroom: 1.2 },
        spark_range,
        120.0,
        36.0,
    );
    let net_samples = h.spark_net_rx.len() + h.spark_net_tx.len();
    let net_name_id = format!("vt-spark-{}-net", h.host);
    let mut net_name = format!(
        "{} · NET trace · {range_label} window · RX/TX · {net_samples} samples",
        h.host
    );
    if net_gaps {
        net_name.push_str(" · gaps present");
    }
    let net_access = Access {
        label: &net_name,
        desc: None,
        name_id: &net_name_id,
    };
    let net_svg = net_spark(&net_lines, spark_range, &net_access);
    let net_foot = spark_foot(net_samples, false, net_gaps);
    let anomaly_tone = if h.anomaly_count > 0 {
        "warning"
    } else {
        "healthy"
    };

    format!(
        r#"<article class="vt-host vt-host--{status_class}">
  <div class="vt-host__identity">
    <span class="vt-status vt-status--{status_class}"><i aria-hidden="true"></i>{status_label}{status_reason}</span>
    <a class="vt-host__name" href="{href}">{display}</a>
    {host_id}
    <div class="vt-host__meta">Seen {last_seen} ago · {uptime}</div>
  </div>
  <div class="vt-host__signal">{cpu_spark}</div>
  <div class="vt-host__signal">{mem_spark}</div>
  <div class="vt-host__signal">{disk_spark}</div>
  <div class="vt-host__signal">{load_spark}</div>
  <div class="vt-host__signal">{net_spark}</div>
  <div class="vt-host__alerts vt-host__alerts--{anomaly_tone}">
    <span class="vt-host__anomaly"><strong>{anomaly_count}</strong><span>Deviations</span></span>
    <a class="vt-open" href="{href}" aria-label="Open {display}">Open <span aria-hidden="true">→</span></a>
  </div>
</article>"#,
        host_id = host_id,
        display = esc(&h.display_name),
        href = esc(&href),
        status_class = status.class(),
        status_label = status.label(),
        status_reason = status_reason,
        last_seen = esc(&human_age(age)),
        uptime = esc(&uptime_detail),
        cpu_spark = spark_panel("CPU", &cpu_head, &cpu_svg, &cpu_foot),
        mem_spark = spark_panel("Memory", &mem_head, &mem_svg, &mem_foot),
        disk_spark = spark_panel("Disk", &disk_head, &disk_svg, &disk_foot),
        load_spark = spark_panel("Load", &load_head, &load_svg, &load_foot),
        net_spark = spark_panel("Network", &net_head, &net_svg, &net_foot),
        anomaly_tone = anomaly_tone,
        anomaly_count = h.anomaly_count,
    )
}

fn pct_spark(
    points: &[(i64, f64)],
    forecast: &[(i64, f64)],
    range: (i64, i64),
    current: Option<f64>,
    gap_secs: i64,
    access: &Access<'_>,
) -> String {
    chart::spark_svg(
        points,
        forecast,
        range,
        &SparkOpts {
            w: 120.0,
            h: 36.0,
            domain: Domain::Pct,
            tone: pct_chart_tone(current),
            gap_secs,
            with_dot: true,
            access: *access,
        },
    )
}

fn net_spark(lines: &[Line<'_>], range: (i64, i64), access: &Access<'_>) -> String {
    chart::detail_svg(
        lines,
        &[],
        &Domain::Auto { headroom: 1.2 },
        range,
        120.0,
        36.0,
        access,
    )
}

fn spark_panel(label: &str, latest: &str, svg: &str, foot: &str) -> String {
    let class = if label == "Network" {
        "vt-sparkbox vt-sparkbox--network"
    } else {
        "vt-sparkbox"
    };
    format!(
        r#"<div class="{class}">
  <div class="vt-spark__head"><span class="vt-spark__label">{label}</span><span class="vt-spark__latest">{latest}</span></div>
  {svg}
{foot}
</div>"#,
        class = class,
        label = esc(label),
        latest = esc(latest),
        svg = svg,
        foot = foot,
    )
}

fn pct_chart_tone(v: Option<f64>) -> Tone {
    match v {
        Some(x) if x >= 90.0 => Tone::Down,
        Some(x) if x >= 70.0 => Tone::Warn,
        Some(_) => Tone::Calm,
        None => Tone::Mute,
    }
}

fn net_now(h: &HostView) -> String {
    match (h.g(metrics::M_NET_RX), h.g(metrics::M_NET_TX)) {
        (Some(rx), Some(tx)) => {
            format!("↓ {}/s ·\u{200b} ↑ {}/s", human_bytes(rx), human_bytes(tx))
        }
        _ => "—".to_string(),
    }
}

fn empty_state() -> String {
    r#"<section class="vt-empty vt-empty--matrix">
  <div class="vt-empty__mark" aria-hidden="true">—</div>
  <h2>No readings</h2>
  <p>Start vitals-agent to populate the fleet.</p>
</section>"#
        .to_string()
}

// --- formatting helpers ----------------------------------------------------------------

fn pct_fmt(v: Option<f64>) -> String {
    v.map(|x| format!("{x:.1}%"))
        .unwrap_or_else(|| "—".to_string())
}

// --- Physiograph additive helpers ------------------------------------------------------
// Delimited region for the presentation-contract additions (freshness pills, accessible
// names, spark feet, gap probes). Nothing frozen above is re-implemented here; these
// helpers only compose caller-owned strings and borrow them for the chart calls.

/// Stable slug for a detail field's primary metric — drives the `vt-fld-*` ids.
fn field_slug(primary_metric: &str) -> &'static str {
    match primary_metric {
        metrics::M_CPU_PCT => "cpu",
        metrics::M_MEM_PCT => "mem",
        metrics::M_DISK_PCT => "disk",
        metrics::M_LOAD1 => "load",
        metrics::M_NET_RX => "net",
        _ => "metric",
    }
}

/// Whether `chart::detail_svg` renders merged gap bands for these lines over `range`.
/// `default_gap` is chart-private by contract and must not be duplicated here, so the
/// authoritative emitter itself is probed with a throwaway accessible name; only the
/// boolean is consumed, for the accessible-name suffix and the gap legend/foot keys.
fn detail_has_gap_bands(
    lines: &[Line<'_>],
    domain: &Domain,
    range: (i64, i64),
    w: f64,
    h: f64,
) -> bool {
    let probe = Access {
        label: "",
        desc: None,
        name_id: "",
    };
    chart::detail_svg(lines, &[], domain, range, w, h, &probe).contains("vt-chart__gapband")
}

/// Overview pct-spark accessible identity (S21): returns the owned `name_id` and `name`
/// so the borrows in `Access` outlive the `spark_svg` call. The `{host}` prefix is the
/// raw host id, which keeps names unique across the page even with identical readings;
/// `head` is the identical formatted string shown in the spark head.
fn pct_spark_access(
    host: &str,
    label: &str,
    slug: &str,
    samples: usize,
    head: &str,
    has_gaps: bool,
    range_label: &str,
) -> (String, String) {
    let name_id = format!("vt-spark-{host}-{slug}");
    let mut name = if samples < 2 {
        format!("{host} · {label} trace · {range_label} window · no samples")
    } else {
        format!("{host} · {label} trace · {range_label} window · {samples} samples · latest {head}")
    };
    if has_gaps {
        name.push_str(" · gaps present");
    }
    (name_id, name)
}

/// Spark foot keys (S12/S22/S23): empty-window micro-key, projection key when a forecast
/// is drawn, gap key when the trace has gap spans. Empty string when no key applies.
fn spark_foot(samples: usize, has_forecast: bool, has_gaps: bool) -> String {
    let mut keys = String::new();
    if samples < 2 {
        keys.push_str(r#"<span class="vt-spark__key">No samples in range</span>"#);
    } else {
        keys.push_str(&format!(
            r#"<span class="vt-spark__key">{samples} samples</span>"#
        ));
        if has_forecast {
            keys.push_str(
                r#"<span class="vt-spark__key vt-spark__key--proj"><i></i>Projected</span>"#,
            );
        }
        if has_gaps {
            keys.push_str(
                r#"<span class="vt-spark__key vt-spark__key--gap"><i></i>Missing data</span>"#,
            );
        }
    }
    if keys.is_empty() {
        String::new()
    } else {
        format!(r#"<div class="vt-spark__foot">{keys}</div>"#)
    }
}

/// Format one metric value with the unit conventions used across Vitals render surfaces.
pub fn fmt_metric(metric: &str, v: f64) -> String {
    if metric.ends_with("_pct") {
        format!("{v:.1}%")
    } else if metric.ends_with("_bytes") {
        human_bytes(v)
    } else if metric.ends_with("_bps") {
        format!("{}/s", human_bytes(v))
    } else if metric.starts_with("load") {
        format!("{v:.2}")
    } else if metric == metrics::M_UPTIME {
        human_uptime(v as i64)
    } else {
        format!("{v:.2}")
    }
}

/// Percent-encode query values for href attributes. HTML escaping is still applied separately
/// when the final href is interpolated.
pub fn pct_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Human-readable byte size (binary units).
pub fn human_bytes(bytes: f64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut v = bytes.max(0.0);
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{v:.0} {}", UNITS[i])
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

/// Human-readable uptime / freshness from seconds.
pub fn human_uptime(secs: i64) -> String {
    let s = secs.max(0);
    let d = s / 86400;
    let h = (s % 86400) / 3600;
    let m = (s % 3600) / 60;
    if d > 0 {
        format!("{d}d {h}h {m}m")
    } else if h > 0 {
        format!("{h}h {m}m")
    } else {
        format!("{m}m")
    }
}

fn human_age(secs: i64) -> String {
    let s = secs.max(0);
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m", s / 60)
    } else {
        format!("{}h", s / 3600)
    }
}

/// HTML-escape untrusted text (host ids, the signed-in email).
pub fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#x27;")
}

/// Product mark: a single continuous pulse, kept semantic and gradient-free.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fmt_metric_uses_vitals_units() {
        assert_eq!(
            fmt_metric(metrics::M_DISK_USED, 804.0 * 1024.0 * 1024.0 * 1024.0),
            "804.0 GiB"
        );
        assert_eq!(fmt_metric(metrics::M_CPU_PCT, 85.227), "85.2%");
        assert_eq!(fmt_metric(metrics::M_LOAD1, 1.25), "1.25");
        assert_eq!(fmt_metric(metrics::M_UPTIME, 90_061.0), "1d 1h 1m");
    }

    #[test]
    fn pct_encode_keeps_query_values_href_safe() {
        assert_eq!(pct_encode("edge-1"), "edge-1");
        assert_eq!(pct_encode("bad host/<x>"), "bad%20host%2F%3Cx%3E");
    }

    #[test]
    fn group_anomalies_merges_families_and_folds_consecutive_events() {
        let rows = vec![
            Anomaly::new("box", metrics::M_DISK_PCT, 100, 79.8, 3.2, "pct".into()),
            Anomaly::new(
                "box",
                metrics::M_DISK_USED,
                100,
                803.9 * 1024.0 * 1024.0 * 1024.0,
                3.4,
                "bytes".into(),
            ),
            Anomaly::new("box", metrics::M_DISK_PCT, 210, 92.0, 6.0, "crit".into()),
            Anomaly::new("box", metrics::M_CPU_PCT, 400, 91.0, 4.0, "cpu".into()),
        ];

        let grouped = group_anomalies_with_threshold(&rows, 60, 3.0);
        let disk = grouped
            .iter()
            .find(|event| event.host == "box" && event.family == "disk")
            .unwrap();
        assert_eq!(
            disk.count, 2,
            "same-family events within 2x detect_secs fold"
        );
        assert_eq!(disk.first_ts, 100);
        assert_eq!(disk.last_ts, 210);
        assert_eq!(disk.tier, EventTier::Crit, "2x z threshold is critical");
        assert!(disk.value_text.contains("92.0%"));

        let same_ts = group_anomalies_with_threshold(&rows[..2], 60, 3.0);
        assert_eq!(same_ts.len(), 1);
        assert_eq!(same_ts[0].family, "disk");
        assert_eq!(same_ts[0].count, 1);
        assert!(same_ts[0].value_text.contains("79.8%"));
        assert!(same_ts[0].value_text.contains("803.9 GiB"));
    }
}
