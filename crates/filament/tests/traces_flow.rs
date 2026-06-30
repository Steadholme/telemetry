//! End-to-end HTTP flow over the in-memory store (NO database).
//!
//! Drives the real `app` router via `tower::oneshot`, exactly like the rest of the estate.
//! Covers: health, empty dashboard, the ingest bearer guard, liberal flat + OTLP ingest, trace
//! reconstruction, the waterfall, filters, the JSON API, and the 404 path.

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use filament::config::DEFAULT_INGEST_TOKEN;
use filament::{app, build_dev_state};
use tower::ServiceExt;

#[tokio::test]
async fn full_trace_flow_in_memory() {
    let state = build_dev_state();

    // --- health ------------------------------------------------------------
    let (status, _) = call(&state, get("/healthz")).await;
    assert_eq!(status, StatusCode::OK);

    // --- empty dashboard ---------------------------------------------------
    let (status, body) = call(&state, get("/")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("No traces yet"), "empty dashboard placeholder");

    // --- ingest without bearer -> 401 --------------------------------------
    let flat = r#"[
        {"trace_id":"t1","span_id":"a","name":"GET /checkout","service":"gateway","start_us":1000,"end_us":9000,"status":"ok"},
        {"trace_id":"t1","span_id":"b","parent_id":"a","name":"SELECT carts","service":"db","start_us":2000,"end_us":4000,"status":"error"}
    ]"#;
    let (status, _) = call(&state, post_ingest(flat, None)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "no bearer -> 401");

    // --- ingest with invalid JSON -> 400 -----------------------------------
    let (status, _) = call(&state, post_ingest("not json", Some(DEFAULT_INGEST_TOKEN))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "bad JSON -> 400");

    // --- real flat ingest --------------------------------------------------
    let (status, body) = call(&state, post_ingest(flat, Some(DEFAULT_INGEST_TOKEN))).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("\"accepted\":2"), "two spans accepted: {body}");

    // --- dashboard lists the trace, with the error badge -------------------
    let (status, body) = call(&state, get("/")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("GET /checkout"), "root name shown");
    assert!(body.contains("gateway"), "root service shown");
    assert!(body.contains("badge-error"), "error trace flagged");

    // --- filter by service: a match keeps it, a miss drops it --------------
    let (_, body) = call(&state, get("/?service=gateway")).await;
    assert!(body.contains("GET /checkout"), "service filter matches root service");
    let (_, body) = call(&state, get("/?service=nope")).await;
    assert!(!body.contains("GET /checkout"), "non-matching service filtered out");

    // --- filter by min duration (trace total is 8ms) -----------------------
    let (_, body) = call(&state, get("/?min_ms=5")).await;
    assert!(body.contains("GET /checkout"), "5ms <= 8ms total: kept");
    let (_, body) = call(&state, get("/?min_ms=50")).await;
    assert!(!body.contains("GET /checkout"), "50ms > 8ms total: dropped");

    // --- waterfall: spans rendered, child indented -------------------------
    let (status, body) = call(&state, get("/trace/t1")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("GET /checkout"));
    assert!(body.contains("SELECT carts"));
    assert!(body.contains("wf-bar--error"), "error span bar styled");
    assert!(body.contains("padding-left:14px"), "child span indented one level");

    // --- missing trace -> 404 ----------------------------------------------
    let (status, _) = call(&state, get("/trace/does-not-exist")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // --- JSON API ----------------------------------------------------------
    let (status, body) = call(&state, get("/api/traces")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("\"trace_id\":\"t1\""));
    assert!(body.contains("\"has_error\":true"));
    assert!(body.contains("\"span_count\":2"));
}

#[tokio::test]
async fn otlp_ingest_is_accepted() {
    let state = build_dev_state();
    let otlp = r#"{
        "resourceSpans": [{
            "resource": {"attributes": [{"key":"service.name","value":{"stringValue":"checkout"}}]},
            "scopeSpans": [{
                "spans": [{
                    "traceId":"otlp-1","spanId":"s1","parentSpanId":"",
                    "name":"POST /pay",
                    "startTimeUnixNano":"1700000000000000000",
                    "endTimeUnixNano":"1700000000003000000",
                    "status":{"code":2}
                }]
            }]
        }]
    }"#;
    let (status, body) = call(&state, post_ingest(otlp, Some(DEFAULT_INGEST_TOKEN))).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("\"accepted\":1"));

    // The OTLP span (3ms, nanos -> micros) shows on the dashboard with its resource service.
    let (_, body) = call(&state, get("/")).await;
    assert!(body.contains("POST /pay"));
    assert!(body.contains("checkout"));
    assert!(body.contains("3.0ms"), "nanos converted to a 3ms duration: {body}");
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

async fn call(state: &filament::AppState, req: Request<Body>) -> (StatusCode, String) {
    let resp = app(state.clone()).oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    (status, String::from_utf8_lossy(&bytes).to_string())
}

fn get(uri: &str) -> Request<Body> {
    Request::builder().uri(uri).body(Body::empty()).unwrap()
}

fn post_ingest(body: &str, token: Option<&str>) -> Request<Body> {
    let mut b = Request::builder()
        .method("POST")
        .uri("/ingest")
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(t) = token {
        b = b.header(header::AUTHORIZATION, format!("Bearer {t}"));
    }
    b.body(Body::from(body.to_string())).unwrap()
}
