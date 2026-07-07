//! Server-side rendering of the Vitals dashboard.
//!
//! Pure functions: a `&[HostView]` + the signed-in email in, an HTML `String` out. The CSS
//! is embedded (`include_str!`) so the slim image never misses an asset and the page is one
//! self-contained document. The brand lockup, tokens, app-bar, cards, status pills and
//! tables match the shared HOLDFAST enterprise design.

use std::{collections::BTreeMap, sync::OnceLock};

use crate::analytics;
use crate::chart::{self, Domain, Line, SparkOpts, Tone};
use crate::metrics::{self, SampleRow};
use crate::store::{Anomaly, Bucket};

const SERVICE_CSS: &str = include_str!("../static/service.css");
static APP_CSS: OnceLock<String> = OnceLock::new();

/// Full CSS payload: canonical Odyssey first, Vitals' service layer second.
pub fn app_css() -> &'static str {
    APP_CSS.get_or_init(|| {
        let mut css = String::with_capacity(odyssey::APP_CSS.len() + SERVICE_CSS.len() + 1);
        css.push_str(odyssey::APP_CSS);
        css.push('\n');
        css.push_str(SERVICE_CSS);
        css
    })
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
pub fn render(
    hosts: &[HostView],
    anomalies: &[Anomaly],
    email: &str,
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
    let summary = summary_strip(hosts, anomalies, since, now);
    let rangebar = rangebar(None, range_label);

    format!(
        r#"<!DOCTYPE html>
<html lang="zh-CN" data-density="compact">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta name="color-scheme" content="light">
<title>Vitals · HOLDFAST</title>
<style>{css}</style>
</head>
<body>
<header class="topbar">
  <div class="topbar__inner">
    <a class="brand" href="/" aria-label="HOLDFAST Vitals">
      <span class="brand__glyph" aria-hidden="true">{shield}</span>
      <span class="brand__word">HOLDFAST</span>
    </a>
    <div class="topbar__right">{userbox}</div>
  </div>
</header>
<main class="wrap">
  <div class="page-head">
    <div>
      <h1>主机探针 · Host Vitals</h1>
      <p class="muted">实时 CPU / 内存 / 磁盘 / 负载，每台主机采样上报。</p>
    </div>
  </div>
  {summary}
  {rangebar}
  <div class="vt-fleet">{cards}</div>
  {anomaly_panel}
</main>
</body>
</html>"#,
        css = app_css(),
        shield = SHIELD_SVG,
        userbox = userbox(email),
        summary = summary,
        rangebar = rangebar,
        anomaly_panel = anomaly_panel,
        cards = cards,
    )
}

/// Distinct empty state for a `?host=` value that is not in the current host set. The raw query
/// string is intentionally not echoed.
pub fn render_unknown_host(email: &str) -> String {
    format!(
        r#"<!DOCTYPE html>
<html lang="zh-CN">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta name="color-scheme" content="light">
<title>Vitals · HOLDFAST</title>
<style>{css}</style>
</head>
<body>
<header class="topbar">
  <div class="topbar__inner">
    <a class="brand" href="/" aria-label="HOLDFAST Vitals">
      <span class="brand__glyph" aria-hidden="true">{shield}</span>
      <span class="brand__word">HOLDFAST</span>
    </a>
    <div class="topbar__right">{userbox}</div>
  </div>
</header>
<main class="wrap">
  <section class="card vitals-card vitals-card--empty">
    <h2>未知主机 · Unknown host</h2>
    <p class="muted">此主机不在当前探针集合中。返回总览查看已上报主机。</p>
    <p><a class="btn btn-ghost btn-sm" href="/">返回总览</a></p>
  </section>
</main>
</body>
</html>"#,
        css = app_css(),
        shield = SHIELD_SVG,
        userbox = userbox(email),
    )
}

/// Per-host drill-down. Filled out in the redesign steps after routing is in place.
#[allow(clippy::too_many_arguments)]
pub fn render_host_detail(
    host: &str,
    hosts: &[HostView],
    buckets: &[Bucket],
    anomalies: &[Anomaly],
    email: &str,
    now: i64,
    since: i64,
    range_label: &str,
    detail_step: i64,
    detect_secs: u64,
    z_threshold: f64,
    _window: usize,
) -> String {
    let Some(current) = hosts.iter().find(|h| h.host == host) else {
        return render_unknown_host(email);
    };
    let display = current.display_name.as_str();
    let age = now - current.last_ts;
    let (pill_class, pill_text) = if age <= 60 {
        ("pill--ok", "在线".to_string())
    } else if age <= 600 {
        ("pill--warn", format!("{} 前", human_age(age)))
    } else {
        ("pill--down", format!("{} 前", human_age(age)))
    };
    let idx = hosts.iter().position(|h| h.host == host).unwrap_or(0);
    let prev = idx
        .checked_sub(1)
        .and_then(|i| hosts.get(i))
        .map(|h| host_nav_link("← Prev", &h.host, range_label))
        .unwrap_or_default();
    let next = hosts
        .get(idx + 1)
        .map(|h| host_nav_link("Next →", &h.host, range_label))
        .unwrap_or_default();
    let sections = [
        metric_section(
            "处理器 · CPU",
            current,
            buckets,
            anomalies,
            host,
            metrics::M_CPU_PCT,
            &[metrics::M_CPU_PCT],
            Domain::Pct,
            (since, now),
            detail_step,
        ),
        metric_section(
            "内存 · Memory",
            current,
            buckets,
            anomalies,
            host,
            metrics::M_MEM_PCT,
            &[metrics::M_MEM_PCT],
            Domain::Pct,
            (since, now),
            detail_step,
        ),
        metric_section(
            "磁盘 · Disk",
            current,
            buckets,
            anomalies,
            host,
            metrics::M_DISK_PCT,
            &[metrics::M_DISK_PCT],
            Domain::Pct,
            (since, now),
            detail_step,
        ),
        metric_section(
            "负载 · Load",
            current,
            buckets,
            anomalies,
            host,
            metrics::M_LOAD1,
            &[metrics::M_LOAD1, metrics::M_LOAD5, metrics::M_LOAD15],
            Domain::Auto { headroom: 1.2 },
            (since, now),
            detail_step,
        ),
        metric_section(
            "网络 · Network",
            current,
            buckets,
            anomalies,
            host,
            metrics::M_NET_RX,
            &[metrics::M_NET_RX, metrics::M_NET_TX],
            Domain::Auto { headroom: 1.2 },
            (since, now),
            detail_step,
        ),
    ]
    .join("");
    let events = host_eventline(host, anomalies, since, detect_secs, z_threshold, now);
    format!(
        r#"<!DOCTYPE html>
<html lang="zh-CN" data-density="compact">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta name="color-scheme" content="light">
<title>Vitals · HOLDFAST</title>
<style>{css}</style>
</head>
<body>
<header class="topbar">
  <div class="topbar__inner">
    <a class="brand" href="/" aria-label="HOLDFAST Vitals">
      <span class="brand__glyph" aria-hidden="true">{shield}</span>
      <span class="brand__word">HOLDFAST</span>
    </a>
    <div class="topbar__right">{userbox}</div>
  </div>
</header>
<main class="wrap">
  <nav class="breadcrumb"><a href="/">主机 Hosts</a> / <span>{display}</span></nav>
  <div class="vt-detailhead">
    {tile}
    <div class="vt-detailhead__title">
      <h1>{display}</h1>
      <div class="vt-host__id mono" title="{host_attr}">{host}</div>
    </div>
    <div class="vt-detailhead__meta">
      <span class="pill {pill_class}">{pill_text}</span>
      <span class="pill pill--muted">{uptime}</span>
      <span class="pill pill--muted">last seen {last_seen}</span>
    </div>
    <div class="vt-hostnav">{prev}{next}</div>
  </div>
  {rangebar}
  {sections}
  {events}
</main>
</body>
</html>"#,
        css = app_css(),
        shield = SHIELD_SVG,
        userbox = userbox(email),
        tile = odyssey::identity::letter_tile(display, host),
        host = esc(host),
        host_attr = esc(host),
        display = esc(display),
        pill_class = pill_class,
        pill_text = esc(&pill_text),
        uptime = esc(&current
            .g(metrics::M_UPTIME)
            .map(|v| human_uptime(v as i64))
            .unwrap_or_else(|| "—".to_string())),
        last_seen = esc(&format!("{} 前", human_age(age))),
        prev = prev,
        next = next,
        rangebar = rangebar(Some(host), range_label),
        sections = sections,
        events = events,
    )
}

fn host_nav_link(label: &str, host: &str, range_label: &str) -> String {
    let href = format!("/?host={}&range={}", pct_encode(host), range_label);
    format!(
        r#"<a class="btn btn-ghost btn-sm" href="{href}">{label}</a>"#,
        href = esc(&href),
        label = esc(label),
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
    let svg = chart::detail_svg(&lines, &anoms, &domain, range, 720.0, 180.0);
    let stats = stats_strip(primary_metric, host.g(primary_metric), primary_series);
    let legend = legend_for(metrics);
    let subrow = section_subrow(host, primary_metric);
    let chart = chart_frame(primary_metric, primary_series, &domain, range, &svg);
    format!(
        r#"<section class="card vt-section">
  <div class="vt-section__head">
    <div>
      <h2>{title}</h2>
      {subrow}
    </div>
    {stats}
  </div>
  {chart}
  {legend}
</section>"#,
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

fn stats_strip(metric: &str, current: Option<f64>, series: &[(i64, f64)]) -> String {
    let values: Vec<f64> = series.iter().map(|(_, value)| *value).collect();
    let min = values.iter().copied().reduce(f64::min);
    let max = values.iter().copied().reduce(f64::max);
    let avg = if values.is_empty() {
        None
    } else {
        Some(values.iter().sum::<f64>() / values.len() as f64)
    };
    format!(
        r#"<div class="vt-stats">
  {now}
  {min}
  {avg}
  {max}
</div>"#,
        now = stat_cell("now", current.map(|v| fmt_metric(metric, v))),
        min = stat_cell("min", min.map(|v| fmt_metric(metric, v))),
        avg = stat_cell("avg", avg.map(|v| fmt_metric(metric, v))),
        max = stat_cell("max", max.map(|v| fmt_metric(metric, v))),
    )
}

fn stat_cell(label: &str, value: Option<String>) -> String {
    format!(
        r#"<div class="vt-stat"><span>{label}</span><b>{value}</b></div>"#,
        label = esc(label),
        value = esc(value.as_deref().unwrap_or("—")),
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
  <div class="vt-chart__xaxis"><span>{start}</span><span>now</span></div>
</div>"#,
        axis = axis,
        svg = svg,
        start = esc(&format!("{} 前", human_age(range.1 - range.0))),
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

fn legend_for(metrics: &[&str]) -> String {
    if metrics.len() < 2 {
        return String::new();
    }
    let items = metrics
        .iter()
        .map(|metric| {
            let label = match *metric {
                metrics::M_LOAD1 => "1m",
                metrics::M_LOAD5 => "5m",
                metrics::M_LOAD15 => "15m",
                metrics::M_NET_RX => "↓ RX",
                metrics::M_NET_TX => "↑ TX",
                _ => *metric,
            };
            format!(
                r#"<span class="{class}"><i></i>{label}</span>"#,
                class = esc(metric_line_class(metric)),
                label = esc(label),
            )
        })
        .collect::<String>();
    format!(r#"<div class="vt-legend">{items}</div>"#)
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
                when = esc(&format!("{} 前", human_age(now - event.last_ts))),
                title = esc(&event.title),
                z = esc(&format!("{:.2}", event.peak_z)),
                value = esc(&event.value_text),
            )
        })
        .collect::<String>();
    format!(
        r#"<section class="card vt-section">
  <div class="vt-section__head"><h2>异常事件 · Events</h2></div>
  <div class="card__body"><ol class="eventline">{items}</ol></div>
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
        r#"<span class="pill pill--muted">已接 Klaxon 推送</span>"#
    } else {
        ""
    };
    let body = if events.is_empty() {
        format!(
            r#"<div class="vt-quiet">
  <div><span class="pill pill--ok">正常</span> 过去 24 小时无异常 · No anomalies</div>
  <div class="vt-quiet__note">z ≥ {z:.1} · window {window} · 每 {detect_secs}s 扫描</div>
</div>"#,
            z = z_threshold,
            window = window,
            detect_secs = detect_secs,
        )
    } else {
        let rows = events.iter().map(|e| event_row(e, now)).collect::<String>();
        format!(r#"<div class="vt-events">{rows}</div>"#)
    };

    format!(
        r#"<section id="vt-anomaly" class="card vt-anomaly" data-density="compact">
  <div class="card__head">
    <div class="card__title">
      <h2>异常监测 · Anomaly Watch</h2>
      <p class="vt-anomaly__meta">z ≥ {z:.1} · window {window} · 每 {detect_secs}s 扫描</p>
    </div>
    <span class="pill pill--down">{crit} critical</span>
    <span class="pill pill--warn">{warn} warn</span>
    {klaxon}
  </div>
  <div class="card__body">
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
    let all_class = if active_host.is_none() {
        "chip chip--solid"
    } else {
        "chip chip--outline"
    };
    let mut out = format!(
        r#"<div class="vt-anomaly__filters"><a class="{all_class}" href="/?range={range}">全部 All <span class="countpill">{total}</span></a>"#,
        all_class = all_class,
        range = esc(range_label),
        total = total,
    );
    for (host, count) in counts {
        let class = if active_host == Some(host.as_str()) {
            "chip chip--solid chip--dot"
        } else {
            "chip chip--outline chip--dot"
        };
        let href = format!("/?host={}&range={}", pct_encode(&host), range_label);
        out.push_str(&format!(
            r#"<a class="{class}" href="{href}">{tile}<span>{host}</span><span class="countpill">{count}</span></a>"#,
            class = class,
            href = esc(&href),
            tile = odyssey::identity::letter_tile(&host, &host),
            host = esc(&host),
            count = count,
        ));
    }
    out.push_str("</div>");
    out
}

fn event_row(event: &VtEvent, now: i64) -> String {
    let tier_class = match event.tier {
        EventTier::Warn => "vt-event--warn",
        EventTier::Crit => "vt-event--crit",
    };
    let dot_class = match event.tier {
        EventTier::Warn => "eventline__dot--warn",
        EventTier::Crit => "eventline__dot--down",
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
  <div class="vt-event__main">
    <div class="vt-event__host">{tile}<span class="mono">{host}</span>{count}</div>
    <div class="vt-event__title">{title} · {direction}</div>
    <div class="vt-event__val">{value}</div>
  </div>
  <div class="vt-event__z">z = {z}</div>
  <time class="vt-event__time">{when}</time>
</article>"#,
        tier_class = tier_class,
        dot_class = dot_class,
        tile = odyssey::identity::letter_tile(&event.host, &event.host),
        host = esc(&event.host),
        count = count,
        title = esc(&event.title),
        direction = direction,
        value = esc(&event.value_text),
        z = esc(&format!("{:.2}", event.peak_z)),
        when = esc(&format!("{} 前", human_age(now - event.last_ts))),
    )
}

fn metric_family(metric: &str) -> (&'static str, &'static str) {
    match metric {
        metrics::M_CPU_PCT => ("cpu", "处理器 · CPU"),
        metrics::M_MEM_PCT | metrics::M_MEM_USED | metrics::M_MEM_TOTAL => ("mem", "内存 · Memory"),
        metrics::M_DISK_PCT | metrics::M_DISK_USED | metrics::M_DISK_TOTAL => {
            ("disk", "磁盘 · Disk")
        }
        metrics::M_LOAD1 | metrics::M_LOAD5 | metrics::M_LOAD15 => ("load", "负载 · Load"),
        metrics::M_NET_RX | metrics::M_NET_TX => ("net", "网络 · Network"),
        metrics::M_UPTIME => ("uptime", "运行时间 · Uptime"),
        _ => ("other", "指标 · Metric"),
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
const LOGOUT_URL: &str = "/_gw/auth/logout";

/// The right side of the app-bar, shared with every HOLDFAST service: a page title, an
/// "All apps" pill back to the apex portal, the signed-in user chip (avatar initial + email),
/// and the cross-subdomain logout. `email` is the gateway-injected identity; the unknown
/// placeholder (`—`) or an empty string renders no user chip (public-page friendly).
fn userbox(email: &str) -> String {
    let has_identity = !email.is_empty() && email != "—";
    let chip = if has_identity {
        let initial = email
            .chars()
            .next()
            .map(|c| c.to_uppercase().to_string())
            .unwrap_or_else(|| "H".to_string());
        format!(
            "<span class=\"userchip\"><span class=\"userchip__avatar\" aria-hidden=\"true\">{}</span><span class=\"user-email\" title=\"signed in\">{}</span></span>",
            esc(&initial),
            esc(email),
        )
    } else {
        String::new()
    };
    format!(
        concat!(
            "<span class=\"topbar__title\">Vitals</span>",
            "<a class=\"allapps\" href=\"https://w33d.xyz\" title=\"All apps\">",
            "<svg viewBox=\"0 0 24 24\" fill=\"none\" stroke=\"currentColor\" stroke-width=\"2\" stroke-linecap=\"round\" stroke-linejoin=\"round\" aria-hidden=\"true\">",
            "<rect x=\"3\" y=\"3\" width=\"7\" height=\"7\" rx=\"1.5\"/><rect x=\"14\" y=\"3\" width=\"7\" height=\"7\" rx=\"1.5\"/>",
            "<rect x=\"3\" y=\"14\" width=\"7\" height=\"7\" rx=\"1.5\"/><rect x=\"14\" y=\"14\" width=\"7\" height=\"7\" rx=\"1.5\"/></svg>All apps</a>",
            "{chip}",
            "<a class=\"btn btn-ghost btn-sm\" href=\"{logout}\">Logout</a>",
        ),
        chip = chip,
        logout = LOGOUT_URL,
    )
}

fn summary_strip(hosts: &[HostView], anomalies: &[Anomaly], since: i64, now: i64) -> String {
    let online = hosts.iter().filter(|h| now - h.last_ts <= 60).count();
    let active_anomalies = anomalies.iter().filter(|a| a.ts >= since).count();
    let host_tone = if online == hosts.len() {
        "stat__val--ok"
    } else {
        "stat__val--warn"
    };
    format!(
        r##"<section class="stat-grid vt-summary" aria-label="fleet summary">
  <div class="stat">
    <div class="stat__label">在线主机 · Online</div>
    <div class="stat__value {host_tone}">{online} / {total}</div>
    <div class="stat__meta">≤60s fresh</div>
  </div>
  {cpu}
  {mem}
  {disk}
  <a class="stat vt-summary__link" href="#vt-anomaly">
    <div class="stat__label">异常 · Anomalies</div>
    <div class="stat__value">{active_anomalies}</div>
    <div class="stat__meta">in selected range</div>
  </a>
</section>"##,
        host_tone = host_tone,
        online = online,
        total = hosts.len(),
        cpu = summary_pct_tile("最热 CPU · Worst CPU", hosts, metrics::M_CPU_PCT),
        mem = summary_pct_tile("最高内存 · Worst MEM", hosts, metrics::M_MEM_PCT),
        disk = summary_pct_tile("最高磁盘 · Worst DISK", hosts, metrics::M_DISK_PCT),
        active_anomalies = active_anomalies,
    )
}

fn summary_pct_tile(label: &str, hosts: &[HostView], metric: &str) -> String {
    let worst = hosts
        .iter()
        .filter_map(|host| host.g(metric).map(|value| (host, value)))
        .max_by(|(_, a), (_, b)| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let (value, meta, pct, tone) = match worst {
        Some((host, value)) => (
            fmt_metric(metric, value),
            host.display_name.clone(),
            value.clamp(0.0, 100.0),
            pct_tone(Some(value)),
        ),
        None => ("—".to_string(), "no data".to_string(), 0.0, "is-muted"),
    };
    format!(
        r#"<div class="stat">
  <div class="stat__label">{label}</div>
  <div class="stat__value">{value}</div>
  <div class="stat__meter"><i class="{tone}" style="width:{pct:.1}%"></i></div>
  <div class="stat__meta">{meta}</div>
</div>"#,
        label = esc(label),
        value = esc(&value),
        tone = tone,
        pct = pct,
        meta = esc(&meta),
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
                "tab is-active"
            } else {
                "tab"
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
    format!(r#"<nav class="tabs tabs--window vt-rangebar" aria-label="time range">{tabs}</nav>"#)
}

/// One host card: freshness pill, htop-density meters, and 2x2 sparklines.
fn host_card(h: &HostView, now: i64, since: i64, range_label: &str) -> String {
    let age = now - h.last_ts;
    let (pill_class, pill_text) = if age <= 60 {
        ("pill--ok", "在线".to_string())
    } else if age <= 600 {
        ("pill--warn", format!("{} 前", human_age(age)))
    } else {
        ("pill--down", format!("{} 前", human_age(age)))
    };

    let cpu = h.g(metrics::M_CPU_PCT);
    let mem = h.g(metrics::M_MEM_PCT);
    let disk = h.g(metrics::M_DISK_PCT);

    let mem_detail = match (h.g(metrics::M_MEM_USED), h.g(metrics::M_MEM_TOTAL)) {
        (Some(u), Some(t)) => format!("{} / {}", human_bytes(u), human_bytes(t)),
        _ => "—".to_string(),
    };
    let disk_detail = match (h.g(metrics::M_DISK_USED), h.g(metrics::M_DISK_TOTAL)) {
        (Some(u), Some(t)) => format!("{} / {}", human_bytes(u), human_bytes(t)),
        _ => "—".to_string(),
    };
    let load_detail = match (
        h.g(metrics::M_LOAD1),
        h.g(metrics::M_LOAD5),
        h.g(metrics::M_LOAD15),
    ) {
        (Some(a), Some(b), Some(c)) => format!("{a:.2} · {b:.2} · {c:.2}"),
        _ => "—".to_string(),
    };
    let net_detail = match (h.g(metrics::M_NET_RX), h.g(metrics::M_NET_TX)) {
        (Some(rx), Some(tx)) => format!("↓ {}/s · ↑ {}/s", human_bytes(rx), human_bytes(tx)),
        _ => "—".to_string(),
    };
    let uptime_detail = h
        .g(metrics::M_UPTIME)
        .map(|s| human_uptime(s as i64))
        .unwrap_or_else(|| "—".to_string());
    let href = format!("/?host={}&range={}", pct_encode(&h.host), range_label);
    let tile = odyssey::identity::letter_tile(&h.display_name, &h.host).to_string();
    let anomaly_badge = if h.anomaly_count > 0 {
        format!(
            r#"<span class="pill pill--warn">{n} 异常</span>"#,
            n = h.anomaly_count
        )
    } else {
        String::new()
    };
    let offline = age > 600;
    let offline_class = if offline { " vt-host--offline" } else { "" };
    let last_seen = if offline {
        format!(
            r#"<div class="vt-lastseen">最后上报 · last seen {}</div>"#,
            esc(&format!("{} 前", human_age(age)))
        )
    } else {
        String::new()
    };
    let spark_range = (since, now);
    let gap = ((now - since).max(1) / 60).max(crate::config::DEFAULT_SCRAPE_INTERVAL as i64) * 2;
    let cpu_svg = pct_spark(&h.spark_cpu, &h.forecast_cpu, spark_range, cpu, gap);
    let mem_svg = pct_spark(&h.spark_mem, &h.forecast_mem, spark_range, mem, gap);
    let disk_svg = pct_spark(&h.spark_disk, &h.forecast_disk, spark_range, disk, gap);
    let net_svg = net_spark(&h.spark_net_rx, &h.spark_net_tx, spark_range);

    format!(
        r#"<section class="card vt-host{offline_class}">
  <div class="vt-host__head">
    {tile}
    <div class="vt-host__title">
      <a class="vt-host__name" href="{href}">{display}</a>
      <div class="vt-host__id mono" title="{host_attr}">{host}</div>
    </div>
    <div class="vt-host__side">
      <span class="pill {pill_class}">{pill_text}</span>
      <span class="pill pill--muted">{uptime_detail}</span>
      {anomaly_badge}
    </div>
  </div>
  {last_seen}
  <div class="vt-host__body">
    {cpu_meter}
    {mem_meter}
    {disk_meter}
    <div class="vt-kv">
      <span>负载 1/5/15 <b>{load_detail}</b></span>
      <span>网络 <b>{net_detail}</b></span>
    </div>
  </div>
  <div class="vt-sparks">
    {cpu_spark}
    {mem_spark}
    {disk_spark}
    {net_spark}
  </div>
</section>"#,
        host_attr = esc(&h.host),
        host = esc(&h.host),
        display = esc(&h.display_name),
        href = esc(&href),
        tile = tile,
        offline_class = offline_class,
        pill_class = pill_class,
        pill_text = esc(&pill_text),
        anomaly_badge = anomaly_badge,
        uptime_detail = esc(&uptime_detail),
        last_seen = last_seen,
        cpu_meter = meter("CPU", cpu, ""),
        mem_meter = meter("内存", mem, &mem_detail),
        disk_meter = meter("磁盘", disk, &disk_detail),
        load_detail = esc(&load_detail),
        net_detail = esc(&net_detail),
        cpu_spark = spark_panel("CPU %", &pct_fmt(cpu), &cpu_svg),
        mem_spark = spark_panel("MEM %", &pct_fmt(mem), &mem_svg),
        disk_spark = spark_panel("DISK %", &pct_fmt(disk), &disk_svg),
        net_spark = spark_panel("NET", &net_now(h), &net_svg),
    )
}

fn meter(label: &str, pct: Option<f64>, detail: &str) -> String {
    let fill = pct.unwrap_or(0.0).clamp(0.0, 100.0);
    format!(
        r#"<div class="vt-meter">
  <div class="vt-meter__label">{label}</div>
  <div class="vt-meter__bar"><span class="vt-meter__fill {tone}" style="width:{fill:.1}%"></span></div>
  <div class="vt-meter__val">{value}</div>
  <div class="vt-meter__detail">{detail}</div>
</div>"#,
        label = esc(label),
        tone = pct_tone(pct),
        fill = fill,
        value = esc(&pct_fmt(pct)),
        detail = esc(detail),
    )
}

fn pct_spark(
    points: &[(i64, f64)],
    forecast: &[(i64, f64)],
    range: (i64, i64),
    current: Option<f64>,
    gap_secs: i64,
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
        },
    )
}

fn net_spark(rx: &[(i64, f64)], tx: &[(i64, f64)], range: (i64, i64)) -> String {
    let lines = [
        Line {
            points: rx,
            class: "vt-net-rx",
            area: false,
        },
        Line {
            points: tx,
            class: "vt-net-tx",
            area: false,
        },
    ];
    chart::detail_svg(
        &lines,
        &[],
        &Domain::Auto { headroom: 1.2 },
        range,
        120.0,
        36.0,
    )
}

fn spark_panel(label: &str, now: &str, svg: &str) -> String {
    format!(
        r#"<div class="vt-sparkbox">
  <div class="vt-spark__head"><span class="vt-spark__label">{label}</span><span class="vt-spark__now">{now}</span></div>
  {svg}
</div>"#,
        label = esc(label),
        now = esc(now),
        svg = svg,
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
        (Some(rx), Some(tx)) => format!("↓ {}/s · ↑ {}/s", human_bytes(rx), human_bytes(tx)),
        _ => "—".to_string(),
    }
}

fn empty_state() -> String {
    r#"<section class="card vitals-card vitals-card--empty">
  <h2>暂无数据</h2>
  <p class="muted">尚未收到任何探针上报。确认 vitals-agent 正在运行并指向本服务的 /ingest。</p>
</section>"#
        .to_string()
}

// --- formatting helpers ----------------------------------------------------------------

fn pct_fmt(v: Option<f64>) -> String {
    v.map(|x| format!("{x:.1}%"))
        .unwrap_or_else(|| "—".to_string())
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

/// Tone class for a percentage gauge: green < 70 < amber < 90 < red.
fn pct_tone(v: Option<f64>) -> &'static str {
    match v {
        Some(x) if x >= 90.0 => "is-danger",
        Some(x) if x >= 70.0 => "is-warn",
        Some(_) => "is-ok",
        None => "is-muted",
    }
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

/// HOLDFAST shield glyph (indigo gradient), shared with the Keystone/console app-bar.
const SHIELD_SVG: &str = r##"<svg viewBox="0 0 48 48" fill="none" xmlns="http://www.w3.org/2000/svg">
<defs><linearGradient id="hf-shield-v" x1="8" y1="4" x2="40" y2="44" gradientUnits="userSpaceOnUse">
<stop stop-color="#818CF8"/><stop offset="1" stop-color="#4F46E5"/></linearGradient></defs>
<path d="M24 4 8 9.5V22c0 11 7 17.4 16 21.5C33 39.4 40 33 40 22V9.5L24 4Z" fill="url(#hf-shield-v)"/>
<rect x="20" y="19" width="8" height="13" rx="1" fill="#fff" fill-opacity="0.92"/>
<path d="M20 19v-2.5a4 4 0 0 1 8 0V19" stroke="#fff" stroke-width="2" stroke-opacity="0.92" fill="none"/>
</svg>"##;

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
