//! `GET /` — the server-rendered enterprise dashboard.
//!
//! Identity comes from the gateway: Sluice runs the OIDC login and injects `X-Auth-Email`
//! on the `auth=sso` route, so this page does NO login of its own — it just reads the
//! header for the app-bar. The page shows every host's current CPU/mem/disk/load plus
//! recent sparklines, built from the TSDB.

use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::response::Html;
use serde::Deserialize;
use std::collections::BTreeMap;

use crate::metrics::SampleRow;
use crate::render::{build_host_views, render, render_host_detail, render_unknown_host};
use crate::store::Bucket;
use crate::{now_secs, AppState};

const SPARK_BUCKETS: i64 = 60;
const DETAIL_BUCKETS: i64 = 120;

#[derive(Debug, Default, Deserialize)]
pub struct DashQuery {
    pub host: Option<String>,
    pub range: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Range {
    H1,
    H6,
    D1,
    D7,
}

impl Range {
    pub const ALL: [Range; 4] = [Range::H1, Range::H6, Range::D1, Range::D7];

    pub fn parse(raw: Option<&str>) -> Self {
        match raw {
            Some("1h") => Range::H1,
            Some("6h") => Range::H6,
            Some("24h") => Range::D1,
            Some("7d") => Range::D7,
            _ => Range::H1,
        }
    }

    pub fn secs(self) -> i64 {
        match self {
            Range::H1 => 3_600,
            Range::H6 => 21_600,
            Range::D1 => 86_400,
            Range::D7 => 604_800,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Range::H1 => "1h",
            Range::H6 => "6h",
            Range::D1 => "24h",
            Range::D7 => "7d",
        }
    }

    fn overview_step(self) -> i64 {
        (self.secs() / SPARK_BUCKETS).max(crate::config::DEFAULT_SCRAPE_INTERVAL as i64)
    }

    fn detail_step(self) -> i64 {
        (self.secs() / DETAIL_BUCKETS).max(crate::config::DEFAULT_SCRAPE_INTERVAL as i64)
    }
}

pub async fn dashboard(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<DashQuery>,
) -> Html<String> {
    // Trusted because the gateway strips inbound X-Auth-* before injecting its own.
    let email = headers
        .get("x-auth-email")
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty())
        .unwrap_or("—");

    let now = now_secs();
    let range = Range::parse(q.range.as_deref());
    let since = now - range.secs();
    let latest = state.store.latest().await;
    let window = overview_window(&state, range, since, now).await;
    let anomalies = state
        .store
        .recent_anomalies(None, None, crate::config::ANOMALY_LIMIT)
        .await;
    let anomaly_counts = anomaly_counts_by_host(&anomalies, since);
    let hosts = build_host_views(
        &latest,
        &window,
        state.config.forecast_steps,
        &anomaly_counts,
        &state.config.host_aliases,
        now,
    );
    let requested_host = q.host.as_deref().map(str::trim).filter(|s| !s.is_empty());

    if let Some(host) = requested_host {
        if latest.iter().any(|row| row.host == host) {
            let detail_step = range.detail_step();
            let buckets = state
                .store
                .series_buckets(Some(host), None, since, now, detail_step)
                .await;
            return Html(render_host_detail(
                host,
                &hosts,
                &buckets,
                &anomalies,
                email,
                now,
                since,
                range.label(),
                detail_step,
                state.config.detect_secs,
                state.config.z_threshold,
                state.config.window,
            ));
        }
        return Html(render_unknown_host(email));
    }

    Html(render(
        &hosts,
        &anomalies,
        email,
        now,
        since,
        range.label(),
        state.config.detect_secs,
        state.config.z_threshold,
        state.config.window,
        state.config.klaxon_ready(),
    ))
}

async fn overview_window(state: &AppState, range: Range, since: i64, now: i64) -> Vec<SampleRow> {
    if range == Range::H1 {
        return state.store.query(None, None, since).await;
    }
    let step = range.overview_step();
    let buckets = state
        .store
        .series_buckets(None, None, since, now, step)
        .await;
    buckets_to_rows(&buckets, since, step)
}

fn buckets_to_rows(buckets: &[Bucket], since: i64, step: i64) -> Vec<SampleRow> {
    buckets
        .iter()
        .map(|b| SampleRow {
            host: b.host.clone(),
            metric: b.metric.clone(),
            value: b.avg,
            ts: since + b.bucket * step,
        })
        .collect()
}

fn anomaly_counts_by_host(
    anomalies: &[crate::store::Anomaly],
    since: i64,
) -> BTreeMap<String, usize> {
    let mut counts = BTreeMap::new();
    for anomaly in anomalies.iter().filter(|a| a.ts >= since) {
        *counts.entry(anomaly.host.clone()).or_insert(0) += 1;
    }
    counts
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    use super::*;
    use crate::config::ServerConfig;
    use crate::metrics::Sample;
    use crate::{app, build_dev_state};

    #[test]
    fn range_parse_whitelists_known_values() {
        assert_eq!(Range::parse(Some("1h")), Range::H1);
        assert_eq!(Range::parse(Some("6h")), Range::H6);
        assert_eq!(Range::parse(Some("24h")), Range::D1);
        assert_eq!(Range::parse(Some("7d")), Range::D7);
        assert_eq!(Range::parse(Some("<script>")), Range::H1);
        assert_eq!(Range::parse(None), Range::H1);
    }

    #[test]
    fn buckets_materialize_to_rows_at_bucket_wall_clock() {
        let rows = buckets_to_rows(
            &[Bucket {
                host: "h".to_string(),
                metric: "cpu_pct".to_string(),
                bucket: 3,
                avg: 42.0,
                max: 50.0,
                min: 30.0,
                n: 2,
            }],
            100,
            10,
        );
        assert_eq!(rows[0].ts, 130);
        assert_eq!(rows[0].value, 42.0);
    }

    #[tokio::test]
    async fn unknown_host_state_does_not_echo_or_reuse_overview_empty_copy() {
        let state = build_dev_state();
        let now = now_secs();
        state
            .store
            .insert_samples("edge", &[Sample::new("cpu_pct", 10.0, now)])
            .await;
        let resp = app(state)
            .oneshot(
                Request::builder()
                    .uri("/?host=%3Cscript%3E&range=garbage")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let html = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(html.contains("未知主机 · Unknown host"));
        assert!(!html.contains("<script>"));
        assert!(!html.contains("无读数 · No readings"));
    }

    #[tokio::test]
    async fn known_host_detail_renders_sections_range_tabs_and_charts() {
        let state = build_dev_state();
        let now = now_secs();
        state
            .store
            .insert_samples(
                "edge",
                &[
                    Sample::new("cpu_pct", 40.0, now - 60),
                    Sample::new("cpu_pct", 45.0, now),
                    Sample::new("mem_pct", 50.0, now - 60),
                    Sample::new("mem_pct", 55.0, now),
                    Sample::new("mem_used_bytes", 8_000_000_000.0, now),
                    Sample::new("mem_total_bytes", 16_000_000_000.0, now),
                    Sample::new("disk_pct", 60.0, now - 60),
                    Sample::new("disk_pct", 65.0, now),
                    Sample::new("disk_used_bytes", 80_000_000_000.0, now),
                    Sample::new("disk_total_bytes", 160_000_000_000.0, now),
                    Sample::new("load1", 1.0, now - 60),
                    Sample::new("load1", 1.2, now),
                    Sample::new("load5", 0.8, now - 60),
                    Sample::new("load5", 0.9, now),
                    Sample::new("load15", 0.7, now - 60),
                    Sample::new("load15", 0.8, now),
                    Sample::new("net_rx_bps", 1000.0, now - 60),
                    Sample::new("net_rx_bps", 2000.0, now),
                    Sample::new("net_tx_bps", 800.0, now - 60),
                    Sample::new("net_tx_bps", 1200.0, now),
                    Sample::new("uptime_secs", 120.0, now),
                ],
            )
            .await;
        let resp = app(state)
            .oneshot(
                Request::builder()
                    .uri("/?host=edge&range=1h")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let html = String::from_utf8(bytes.to_vec()).unwrap();
        assert_eq!(html.matches("class=\"card vt-section\"").count(), 5);
        assert!(html.contains("处理器 · CPU"));
        assert!(html.contains("内存 · Memory"));
        assert!(html.contains("磁盘 · Disk"));
        assert!(html.contains("负载 · Load"));
        assert!(html.contains("网络 · Network"));
        assert!(html.contains("tabs--window"));
        assert!(html.contains("aria-current=\"page\""));
        assert!(html.contains("<polyline"));
    }

    #[tokio::test]
    async fn host_alias_is_render_only_and_raw_id_stays_available() {
        let mut state = build_dev_state();
        let mut cfg = ServerConfig::dev();
        cfg.host_aliases
            .insert("docker-abc123".to_string(), "Edge Alias".to_string());
        state.config = Arc::new(cfg);
        let now = now_secs();
        state
            .store
            .insert_samples("docker-abc123", &[Sample::new("cpu_pct", 12.0, now)])
            .await;
        let resp = app(state)
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let html = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(html.contains("Edge Alias"));
        assert!(html.contains("title=\"docker-abc123\""));
    }
}
