//! SSR truth and accessibility contract for the Vitals Physiograph.
//!
//! These tests intentionally drive the real Router with an in-memory store. They assert
//! rendered truth rather than implementation details: no liveness language, no synthetic
//! chart data, truthful gap geometry, bounded empty/partial states, and accessible SVG names.

use std::collections::BTreeSet;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;
use vitals::metrics::Sample;
use vitals::{app, build_dev_state, now_secs, AppState};

async fn get_html(state: &AppState, uri: &str) -> String {
    let response = app(state.clone())
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(uri)
                .header("x-auth-email", "physiograph-gate@local")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK, "GET {uri}");
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}

fn body_markup(html: &str) -> &str {
    html.split_once("</head>").map_or(html, |(_, body)| body)
}

fn attribute(tag: &str, name: &str) -> Option<String> {
    let marker = format!(r#"{name}=""#);
    let start = tag.find(&marker)? + marker.len();
    let end = tag[start..].find('"')? + start;
    Some(tag[start..end].to_string())
}

fn opening_tags<'a>(html: &'a str, name: &str) -> Vec<&'a str> {
    let needle = format!("<{name}");
    let mut tags = Vec::new();
    let mut cursor = 0;
    while let Some(relative) = html[cursor..].find(&needle) {
        let start = cursor + relative;
        let Some(relative_end) = html[start..].find('>') else {
            break;
        };
        let end = start + relative_end + 1;
        tags.push(&html[start..end]);
        cursor = end;
    }
    tags
}

fn svg_by_id<'a>(html: &'a str, id: &str) -> &'a str {
    let marker = format!(r#"id="{id}""#);
    let id_pos = html
        .find(&marker)
        .unwrap_or_else(|| panic!("missing SVG id {id}"));
    let start = html[..id_pos]
        .rfind("<svg")
        .unwrap_or_else(|| panic!("missing SVG start for {id}"));
    let relative_end = html[id_pos..]
        .find("</svg>")
        .unwrap_or_else(|| panic!("missing SVG end for {id}"));
    &html[start..id_pos + relative_end + "</svg>".len()]
}

fn enclosing_section<'a>(html: &'a str, marker: &str) -> &'a str {
    let marker_pos = html
        .find(marker)
        .unwrap_or_else(|| panic!("missing section marker {marker}"));
    let start = html[..marker_pos]
        .rfind(r#"<section class="vt-section""#)
        .unwrap_or_else(|| panic!("missing section start for {marker}"));
    let relative_end = html[marker_pos..]
        .find("</section>")
        .unwrap_or_else(|| panic!("missing section end for {marker}"));
    &html[start..marker_pos + relative_end + "</section>".len()]
}

fn tag_texts(html: &str, name: &str) -> Vec<String> {
    let open = format!("<{name}>");
    let close = format!("</{name}>");
    let mut values = Vec::new();
    let mut cursor = 0;
    while let Some(relative) = html[cursor..].find(&open) {
        let start = cursor + relative + open.len();
        let Some(relative_end) = html[start..].find(&close) else {
            break;
        };
        let end = start + relative_end;
        values.push(html[start..end].to_string());
        cursor = end + close.len();
    }
    values
}

fn class_count(html: &str, tag: &str, class_value: &str) -> usize {
    opening_tags(html, tag)
        .into_iter()
        .filter(|candidate| {
            attribute(candidate, "class")
                .is_some_and(|classes| classes.split_ascii_whitespace().any(|c| c == class_value))
        })
        .count()
}

fn polyline_geometry(svg: &str, class_value: &str) -> (usize, usize) {
    let tags: Vec<&str> = opening_tags(svg, "polyline")
        .into_iter()
        .filter(|tag| {
            attribute(tag, "class")
                .is_some_and(|classes| classes.split_ascii_whitespace().any(|c| c == class_value))
        })
        .collect();
    let vertices = tags
        .iter()
        .map(|tag| {
            attribute(tag, "points")
                .unwrap_or_else(|| panic!("polyline {class_value} lacks points"))
                .split_ascii_whitespace()
                .count()
        })
        .sum();
    (tags.len(), vertices)
}

fn percent_encode(raw: &str) -> String {
    let mut encoded = String::new();
    for byte in raw.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

fn assert_no_liveness_claim(body: &str) {
    for banned in [
        "实时",
        ">now<",
        "在线",
        "正常</span>",
        "过去 24 小时无异常",
        "Anomaly Watch",
        "vt-spark__now",
        "vt-host--offline",
    ] {
        assert!(!body.contains(banned), "banned liveness token: {banned}");
    }
}

fn assert_no_internal_material(body: &str) {
    for banned in ["10.77", "vitals:8300", "telemetry:9100", "Bearer "] {
        assert!(!body.contains(banned), "internal material leaked: {banned}");
    }
}

async fn seed_gap_fixture(state: &AppState, host: &str, now: i64) {
    let mut samples = Vec::new();
    for (delta, value) in [(-900, 10.0), (-870, 20.0), (-600, 30.0), (-570, 40.0)] {
        samples.push(Sample::new("cpu_pct", value, now + delta));
        samples.push(Sample::new("load1", value / 10.0, now + delta));
        samples.push(Sample::new("load5", value / 20.0, now + delta));
    }
    for (delta, value) in [(-900, 0.5), (-870, 0.7), (-300, 0.9), (-270, 1.1)] {
        samples.push(Sample::new("load15", value, now + delta));
    }
    for (delta, value) in [(-120, 45.0), (-90, 50.0), (-60, 55.0), (-30, 60.0)] {
        samples.push(Sample::new("mem_pct", value, now + delta));
        samples.push(Sample::new("disk_pct", value / 2.0, now + delta));
    }
    for (delta, value) in [
        (-900, 1000.0),
        (-870, 1200.0),
        (-600, 1600.0),
        (-570, 1800.0),
    ] {
        samples.push(Sample::new("net_rx_bps", value, now + delta));
    }
    for (delta, value) in [(-900, 800.0), (-870, 900.0), (-300, 1300.0), (-270, 1500.0)] {
        samples.push(Sample::new("net_tx_bps", value, now + delta));
    }
    samples.extend([
        Sample::new("mem_used_bytes", 8_000_000_000.0, now - 30),
        Sample::new("mem_total_bytes", 16_000_000_000.0, now - 30),
        Sample::new("disk_used_bytes", 80_000_000_000.0, now - 30),
        Sample::new("disk_total_bytes", 160_000_000_000.0, now - 30),
        Sample::new("uptime_secs", 90_061.0, now - 30),
    ]);
    state.store.insert_samples(host, &samples).await;
}

#[tokio::test]
async fn bounded_copy_and_window_scoped_quiet_state_replace_liveness_claims() {
    let state = build_dev_state();
    let empty = get_html(&state, "/").await;
    let empty_body = body_markup(&empty);
    assert!(!empty.contains("<script"));
    assert!(empty_body.contains("No readings"));
    assert!(empty_body.contains("Start vitals-agent to populate the fleet."));
    assert_no_liveness_claim(empty_body);
    assert_no_internal_material(empty_body);

    let now = now_secs();
    state
        .store
        .insert_samples("edge", &[Sample::new("cpu_pct", 42.0, now - 30)])
        .await;
    for range in ["1h", "7d"] {
        let html = get_html(&state, &format!("/?range={range}")).await;
        let body = body_markup(&html);
        assert!(!html.contains("<script"));
        assert!(body.contains(r#"class="vt-matrix""#));
        assert!(body.contains(r#"class="vt-summary""#));
        assert!(body.contains(r#"aria-label="Time range""#));
        assert!(body.contains("Statistical deviations"));
        assert!(body.contains(&format!("No deviations in {range}")));
        assert!(body.contains("Updated "));
        assert!(body.contains("UTC"));
        assert!(body.contains(">Refresh</a>"));
        assert_no_liveness_claim(body);
        assert_no_internal_material(body);
    }
}

#[tokio::test]
async fn freshness_and_partial_metric_states_are_textual_and_never_zero_filled() {
    let state = build_dev_state();
    let now = now_secs();
    for (host, age, value) in [
        ("fresh-probe", 30, 41.0),
        ("recent-probe", 300, 75.0),
        ("stale-probe", 1200, 95.0),
    ] {
        state
            .store
            .insert_samples(host, &[Sample::new("cpu_pct", value, now - age)])
            .await;
    }

    let html = get_html(&state, "/").await;
    let body = body_markup(&html);
    assert!(body.contains("vt-host--healthy"));
    assert!(body.contains("vt-host--warning"));
    assert!(body.contains("vt-host--stale"));
    assert!(body.contains("CPU 75.0%"));
    assert!(body.contains("No report for"));
    assert!(body.contains(">—</span>"));
    assert!(
        !body.contains("width:0.0%"),
        "missing metric was rendered as zero"
    );
    assert_no_liveness_claim(body);
}

#[tokio::test]
async fn detail_geometry_keeps_real_vertices_and_merges_gap_union() {
    let state = build_dev_state();
    let now = now_secs();
    seed_gap_fixture(&state, "gap-host", now).await;
    let html = get_html(&state, "/?host=gap-host&range=1h").await;
    let body = body_markup(&html);

    for id in ["vt-fld-cpu", "vt-fld-load", "vt-fld-net"] {
        let svg = svg_by_id(body, id);
        assert_eq!(
            class_count(svg, "rect", "vt-chart__gapband"),
            1,
            "{id} must render one merged gap union"
        );
        assert!(
            attribute(opening_tags(svg, "svg")[0], "aria-label")
                .unwrap()
                .contains("gaps present"),
            "{id} accessible name must disclose its rendered gap"
        );
        assert!(svg.contains("Missing data") || body.contains("Missing data"));
    }

    let cpu = svg_by_id(body, "vt-fld-cpu");
    assert_eq!(polyline_geometry(cpu, "vt-tone-calm"), (2, 4));
    let load = svg_by_id(body, "vt-fld-load");
    assert_eq!(polyline_geometry(load, "vt-load-1"), (2, 4));
    assert_eq!(polyline_geometry(load, "vt-load-5"), (2, 4));
    assert_eq!(polyline_geometry(load, "vt-load-15"), (2, 4));
    let net = svg_by_id(body, "vt-fld-net");
    assert_eq!(polyline_geometry(net, "vt-net-rx"), (2, 4));
    assert_eq!(polyline_geometry(net, "vt-net-tx"), (2, 4));
    assert!(!body.contains("vt-spark__baseline"));
}

#[tokio::test]
async fn svg_names_are_unique_and_share_visible_field_summary_values() {
    let state = build_dev_state();
    let now = now_secs();
    seed_gap_fixture(&state, "named-host", now).await;
    let html = get_html(&state, "/?host=named-host&range=1h").await;
    let body = body_markup(&html);
    let svg_tags: Vec<&str> = opening_tags(body, "svg")
        .into_iter()
        .filter(|tag| {
            attribute(tag, "class").is_some_and(|classes| {
                classes
                    .split_ascii_whitespace()
                    .any(|class| class == "vt-chart__svg")
            })
        })
        .collect();
    assert_eq!(
        svg_tags.len(),
        5,
        "detail page has one SVG per metric field"
    );

    let mut ids = BTreeSet::new();
    let mut labels = BTreeSet::new();
    for tag in svg_tags {
        assert_eq!(attribute(tag, "role").as_deref(), Some("img"));
        let id = attribute(tag, "id").expect("SVG id");
        let label = attribute(tag, "aria-label").expect("SVG accessible name");
        assert!(!label.is_empty());
        assert!(ids.insert(id.clone()), "duplicate SVG id {id}");
        assert!(labels.insert(label.clone()), "duplicate SVG name {label}");
        for required in [" samples", "latest ", "min ", "avg ", "max "] {
            assert!(
                label.contains(required),
                "{id} missing {required} in {label}"
            );
        }

        let svg = svg_by_id(body, &id);
        assert_eq!(
            label.contains("gaps present"),
            svg.contains("vt-chart__gapband"),
            "gap disclosure mismatch for {id}"
        );
        let section = enclosing_section(body, &format!(r#"id="{id}""#));
        let values = tag_texts(section, "b");
        assert_eq!(values.len(), 4, "{id} visible latest/min/avg/max rail");
        for value in values {
            assert!(
                label.contains(&value),
                "{id} accessible summary diverges from visible value {value}: {label}"
            );
        }
    }
    assert!(labels.iter().any(|label| label.contains("latest 40.0%")));
    assert!(labels.iter().any(|label| label.contains("min 10.0%")));
    assert!(labels.iter().any(|label| label.contains("avg 25.0%")));
    assert!(labels.iter().any(|label| label.contains("max 40.0%")));
    assert!(labels.iter().any(|label| label.contains("4 samples")));
}

#[tokio::test]
async fn hostile_identity_is_escaped_and_semantic_hooks_remain_intact() {
    let state = build_dev_state();
    let now = now_secs();
    let hostile = "<script>alert(1)</script>\u{202e}\"&very-long-host-identifier";
    seed_gap_fixture(&state, hostile, now).await;
    let uri = format!("/?host={}&range=1h", percent_encode(hostile));
    let html = get_html(&state, &uri).await;
    let css = get_html(&state, "/assets/vitals-20260908.css").await;
    let body = body_markup(&html);

    assert!(!body.contains("<script>alert(1)</script>"));
    assert!(body.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
    assert!(html.contains("/assets/vitals-20260908.css"));
    assert!(css.contains("unicode-bidi: isolate"));
    assert_eq!(body.matches(r#"class="vt-section""#).count(), 5);
    for title in ["CPU", "Memory", "Disk", "Load", "Network"] {
        assert!(
            body.contains(&format!(">{title}</h2>")),
            "missing field title {title}"
        );
    }
    assert!(body.contains(r#"class="vt-range""#));
    assert!(body.contains(r#"aria-current="page""#));
    assert!(body.contains(r#"class="vt-hostrail""#));
    assert!(body.contains(r#"class="vt-workbench""#));
    assert!(body.contains(r##"href="#vt-main""##));
    assert!(body.contains("<polyline"));
    assert_no_liveness_claim(body);
    assert_no_internal_material(body);
}
