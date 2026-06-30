//! End-to-end HTTP tests driving the router in-process via `tower::oneshot` (no port bind), the
//! same shape the rest of the estate uses. Covers the public healthz, the SSO dashboard + JSON
//! search, and the own-bearer ingest path (including its 401 on a bad token).

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use tower::ServiceExt;

use sift::build_dev_state;

fn body_to_string(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).to_string()
}

#[tokio::test]
async fn healthz_is_public_ok() {
    let app = sift::app(build_dev_state());
    let res = app
        .oneshot(Request::builder().uri("/healthz").body(Body::empty()).unwrap())
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
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 20).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["count"], 2);
    assert!(json["results"][0]["message"].as_str().unwrap().contains("timeout"));
}

#[tokio::test]
async fn dashboard_renders_html() {
    let app = sift::app(build_dev_state());
    let res = app
        .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 20).await.unwrap();
    let html = body_to_string(&bytes);
    assert!(html.contains("HOLDFAST"));
    assert!(html.contains("Top templates"));
}
