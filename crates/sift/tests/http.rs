//! End-to-end HTTP tests driving the router in-process via `tower::oneshot` (no port bind), the
//! same shape the rest of the estate uses. Covers the public healthz, the SSO dashboard + JSON
//! search, and the own-bearer ingest path (including its 401 on a bad token).

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use tower::ServiceExt;

use sift::build_dev_state;
use sift::store::LogEntry;

fn body_to_string(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).to_string()
}

fn log(id: &str, ts: i64, host: &str, app: &str, severity: &str, message: &str) -> LogEntry {
    LogEntry {
        id: id.to_string(),
        ts,
        host: host.to_string(),
        app: app.to_string(),
        severity: severity.to_string(),
        message: message.to_string(),
        template_id: "t_test".to_string(),
    }
}

#[tokio::test]
async fn stylesheet_is_public_and_immutable() {
    let res = sift::app(build_dev_state())
        .oneshot(
            Request::builder()
                .uri("/assets/sift-20260908.css")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(
        res.headers().get(header::CACHE_CONTROL).unwrap(),
        "public, max-age=31536000, immutable"
    );
    assert_eq!(
        res.headers().get(header::X_CONTENT_TYPE_OPTIONS).unwrap(),
        "nosniff"
    );
}

#[tokio::test]
async fn healthz_is_public_ok() {
    let app = sift::app(build_dev_state());
    let res = app
        .oneshot(
            Request::builder()
                .uri("/healthz")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(res.into_body(), 1024).await.unwrap();
    assert_eq!(body_to_string(&bytes), "ok");
}

#[tokio::test]
async fn ingest_requires_bearer() {
    let app = sift::app(build_dev_state());
    // No Authorization header -> 401.
    let res = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/ingest")
                .body(Body::from(r#"{"message":"hello"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn ingest_then_search_roundtrip() {
    let state = build_dev_state();
    let token = state.config.ingest_token.clone();

    // Ingest a small batch under the dev bearer token.
    let app = sift::app(state.clone());
    let res = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/ingest")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"[{"app":"web","level":"error","message":"db timeout after 30s"},
                        {"app":"web","level":"info","message":"db timeout after 12s"}]"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    // The two near-identical lines cluster to one template.
    assert_eq!(state.store.count_logs().await.unwrap(), 2);
    assert_eq!(state.store.count_templates().await.unwrap(), 1);

    // Search via the JSON API (SSO surface — dev state trusts no header, but the route is open in
    // tests since there's no gateway; the handler does not itself require identity for GET).
    let app = sift::app(state.clone());
    let res = app
        .oneshot(
            Request::builder()
                .uri("/api/search?q=timeout&app=web")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["count"], 2);
    assert!(json["results"][0]["message"]
        .as_str()
        .unwrap()
        .contains("timeout"));
}

#[tokio::test]
async fn api_search_supports_source_filter_and_keyset_next_link() {
    let state = build_dev_state();
    state
        .store
        .insert_log(&log("a", 100, "edge-b", "api", "info", "other host"))
        .await
        .unwrap();
    state
        .store
        .insert_log(&log("b", 200, "edge-a", "api", "info", "second edge event"))
        .await
        .unwrap();
    state
        .store
        .insert_log(&log("c", 300, "edge-a", "api", "err", "first edge event"))
        .await
        .unwrap();

    let app = sift::app(state.clone());
    let res = app
        .oneshot(
            Request::builder()
                .uri("/api/search?source=edge-a&limit=1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["count"], 1);
    assert_eq!(json["results"][0]["id"], "c");
    assert_eq!(json["next_cursor"]["before_ts"], 300);
    assert_eq!(json["next_cursor"]["before_id"], "c");

    let next = json["next"].as_str().unwrap();
    let app = sift::app(state.clone());
    let res = app
        .oneshot(Request::builder().uri(next).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["count"], 1);
    assert_eq!(json["results"][0]["id"], "b");
    assert!(json["next_cursor"].is_null());
}

#[tokio::test]
async fn dashboard_renders_html() {
    let app = sift::app(build_dev_state());
    let res = app
        .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
        .await
        .unwrap();
    let html = body_to_string(&bytes);
    assert!(html.contains("Steadholme"));
    assert!(html.contains("Top templates"));
    assert!(html.contains("/assets/sift-20260908.css"));
    assert!(!html.contains("<style>"));
}
