//! Phase 5 integration tests: static web UI serving + SSE intel stream.
//!
//! The SSE test asserts headers/shape only — it never consumes the infinite
//! stream (that would hang).

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use ostara::config::Config;
use ostara::routes::{self, AppState};
use std::sync::Arc;

fn app() -> Router {
    let state = AppState {
        client: reqwest::Client::builder()
            .user_agent("ostara-test")
            .build()
            .unwrap(),
        cache: Arc::new(ostara::cache::TtlCache::new()),
        config: Arc::new(Config::test_default()),
    };
    routes::app(state)
}

async fn get(app: Router, uri: &str) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
    let res = tower::ServiceExt::oneshot(
        app,
        Request::builder().uri(uri).body(Body::empty()).unwrap(),
    )
    .await
    .unwrap();
    let status = res.status();
    let headers = res.headers().clone();
    let body = axum::body::to_bytes(res.into_body(), 1024 * 1024)
        .await
        .unwrap()
        .to_vec();
    (status, headers, body)
}

#[tokio::test]
async fn index_serves_html() {
    let (status, headers, body) = get(app(), "/").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        headers["content-type"]
            .to_str()
            .unwrap()
            .contains("text/html")
    );
    let html = String::from_utf8(body).unwrap();
    assert!(html.contains("maplibre"), "index must load MapLibre");
    assert!(html.contains("/app.js"), "index must load the app script");
}

#[tokio::test]
async fn app_js_serves() {
    let (status, _, body) = get(app(), "/app.js").await;
    assert_eq!(status, StatusCode::OK);
    let js = String::from_utf8(body).unwrap();
    assert!(
        js.contains("/api/flights"),
        "app.js must fetch the flights endpoint"
    );
    assert!(js.contains("EventSource"), "app.js must use the SSE stream");
}

#[tokio::test]
async fn style_css_serves() {
    let (status, _, _) = get(app(), "/style.css").await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn intel_stream_is_sse() {
    // Shape contract only: status + content-type. The body is an infinite
    // stream and is deliberately NOT consumed (that would hang).
    let res = tower::ServiceExt::oneshot(
        app(),
        Request::builder()
            .uri("/api/intel/stream")
            .body(Body::empty())
            .unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert!(
        res.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/event-stream"),
        "must be an SSE response"
    );
}

#[tokio::test]
async fn api_routes_still_win_over_static_fallback() {
    // Known API routes are unaffected by the static fallback...
    let (status, _, _) = get(app(), "/healthz").await;
    assert_eq!(status, StatusCode::OK);
    // ...and unknown paths 404 (via the static fallback) rather than
    // colliding with the API.
    let (status, _, _) = get(app(), "/api/nope").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
