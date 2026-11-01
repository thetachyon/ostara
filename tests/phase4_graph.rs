//! Integration tests: the Phase-4 entity graph routes.
//!
//! These assert the HTTP *contract* — status codes, content-type, and the
//! exact JSON shapes — not upstream liveness. `routes::app` cannot be merged
//! post-hoc here (`with_state` already consumed its state type), so the test
//! app does exactly what the canonical wiring does: merge the stateful
//! `graph::routes()` router, then `with_state`. `Config::test_default()` is
//! used throughout, so keyed feeds take their degraded paths.
//!
//! Live-network behavior in this sandbox is flaky: tests that touch
//! upstream feeds assert shape contracts (200-or-404), never upstream data
//! content. The nuclear registry is static embedded data, so
//! facility/country assertions are deterministic.

use std::sync::Arc;
use std::time::Duration;

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use ostara::{cache::TtlCache, config::Config, graph, routes};
use tower::ServiceExt; // .oneshot()

fn test_state() -> routes::AppState {
    let client = reqwest::Client::builder()
        .user_agent("ostara/0.1 integration-tests")
        .timeout(Duration::from_secs(10))
        .connect_timeout(Duration::from_secs(3))
        .pool_max_idle_per_host(4)
        .build()
        .expect("test client builds");
    routes::AppState {
        client,
        cache: Arc::new(TtlCache::new()),
        // Deterministic: no keys, so keyed feeds take their degraded paths.
        config: Arc::new(Config::test_default()),
    }
}

/// Graph router merged with state exactly as `routes::app` does it.
fn app() -> Router {
    Router::new()
        .merge(graph::routes())
        .with_state(test_state())
}

struct Got {
    status: StatusCode,
    content_type: Option<String>,
    body: serde_json::Value,
}

async fn get(uri: &str) -> Got {
    let req = Request::builder()
        .uri(uri)
        .body(Body::empty())
        .expect("request builds");
    let resp = app().oneshot(req).await.expect("oneshot serves");
    let status = resp.status();
    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let raw = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("body reads");
    let body = serde_json::from_slice(&raw).unwrap_or(serde_json::Value::Null);
    Got {
        status,
        content_type,
        body,
    }
}

fn assert_json(g: &Got, route: &str) {
    assert!(
        g.content_type
            .as_deref()
            .is_some_and(|ct| ct.starts_with("application/json")),
        "{route}: expected application/json, got {:?}",
        g.content_type
    );
}

fn error_shape<'a>(g: &'a Got, route: &str) -> (&'a str, &'a str) {
    assert_json(g, route);
    let error = g.body.get("error").and_then(|v| v.as_str()).unwrap_or("");
    let detail = g.body.get("detail").and_then(|v| v.as_str()).unwrap_or("");
    assert!(!error.is_empty(), "{route}: missing error field");
    assert!(!detail.is_empty(), "{route}: missing detail field");
    (error, detail)
}

// ---------------------------------------------------------------------------
// /api/graph?entity=
// ---------------------------------------------------------------------------

#[tokio::test]
async fn graph_requires_entity_param() {
    let g = get("/api/graph").await;
    assert_eq!(g.status, StatusCode::BAD_REQUEST);
    let (error, _) = error_shape(&g, "/api/graph");
    assert_eq!(error, "missing entity");
}

#[tokio::test]
async fn graph_rejects_blank_entity() {
    let g = get("/api/graph?entity=%20%20").await;
    assert_eq!(g.status, StatusCode::BAD_REQUEST);
    let (error, _) = error_shape(&g, "/api/graph?entity=%20%20");
    assert_eq!(error, "missing entity");
}

#[tokio::test]
async fn graph_unknown_entity_is_404() {
    // Full live graph build runs first (network may be flaky, but a failing
    // feed contributes no nodes and never fails the build).
    let g = get("/api/graph?entity=definitely-not-an-entity-xyz").await;
    assert_eq!(g.status, StatusCode::NOT_FOUND);
    let (error, _) = error_shape(&g, "/api/graph?entity=...");
    assert_eq!(error, "entity not found");
}

/// Deterministic: the nuclear registry is static embedded data, so this
/// holds with or without network access.
#[tokio::test]
async fn graph_resolves_nuclear_facility() {
    let g = get("/api/graph?entity=Astravets").await;
    assert_eq!(g.status, StatusCode::OK);
    assert_json(&g, "/api/graph?entity=Astravets");
    assert_eq!(
        g.body.get("entity").and_then(|v| v.as_str()),
        Some("Astravets")
    );
    assert_eq!(
        g.body.get("kind").and_then(|v| v.as_str()),
        Some("facility")
    );
    let neighbors = g
        .body
        .get("neighbors")
        .and_then(|v| v.as_array())
        .expect("neighbors array");
    assert!(
        neighbors.iter().any(|n| {
            n.get("entity").and_then(|v| v.as_str()) == Some("Belarus")
                && n.get("kind").and_then(|v| v.as_str()) == Some("country")
                && n.get("relation").and_then(|v| v.as_str()) == Some("located-in")
                && n.get("source_feed").and_then(|v| v.as_str()) == Some("nuclear")
        }),
        "expected Belarus/country/located-in neighbor, got {neighbors:?}"
    );
}

#[tokio::test]
async fn graph_lookup_is_case_insensitive_over_http() {
    let g = get("/api/graph?entity=astravets").await;
    assert_eq!(g.status, StatusCode::OK);
    // The canonical value is returned, not the query's casing.
    assert_eq!(
        g.body.get("entity").and_then(|v| v.as_str()),
        Some("Astravets")
    );
}

/// Shape contract only: live satellite data may or may not be reachable.
#[tokio::test]
async fn graph_satellite_shape_contract() {
    let g = get("/api/graph?entity=25544").await;
    assert!(
        g.status == StatusCode::OK || g.status == StatusCode::NOT_FOUND,
        "unexpected status {}",
        g.status
    );
    assert_json(&g, "/api/graph?entity=25544");
    if g.status == StatusCode::OK {
        assert_eq!(g.body.get("entity").and_then(|v| v.as_str()), Some("25544"));
        assert_eq!(
            g.body.get("kind").and_then(|v| v.as_str()),
            Some("norad_id")
        );
        assert!(g.body.get("neighbors").and_then(|v| v.as_array()).is_some());
    } else {
        let (error, _) = error_shape(&g, "/api/graph?entity=25544");
        assert_eq!(error, "entity not found");
    }
}

// ---------------------------------------------------------------------------
// /api/graph/expand?entity=&depth=
// ---------------------------------------------------------------------------

#[tokio::test]
async fn expand_requires_entity_param() {
    let g = get("/api/graph/expand").await;
    assert_eq!(g.status, StatusCode::BAD_REQUEST);
    let (error, _) = error_shape(&g, "/api/graph/expand");
    assert_eq!(error, "missing entity");
}

#[tokio::test]
async fn expand_unknown_entity_is_404() {
    let g = get("/api/graph/expand?entity=definitely-not-an-entity-xyz&depth=2").await;
    assert_eq!(g.status, StatusCode::NOT_FOUND);
    let (error, _) = error_shape(&g, "/api/graph/expand");
    assert_eq!(error, "entity not found");
}

/// Deterministic via the static nuclear registry.
#[tokio::test]
async fn expand_facility_default_depth() {
    let g = get("/api/graph/expand?entity=Astravets").await;
    assert_eq!(g.status, StatusCode::OK);
    assert_json(&g, "/api/graph/expand?entity=Astravets");
    assert_eq!(
        g.body.get("entity").and_then(|v| v.as_str()),
        Some("Astravets")
    );
    assert_eq!(g.body.get("depth").and_then(|v| v.as_u64()), Some(1));
    let nodes = g
        .body
        .get("nodes")
        .and_then(|v| v.as_array())
        .expect("nodes array");
    let vals: Vec<&str> = nodes
        .iter()
        .filter_map(|n| n.get("entity").and_then(|v| v.as_str()))
        .collect();
    assert!(vals.contains(&"Astravets"));
    assert!(vals.contains(&"Belarus"));
    let edges = g
        .body
        .get("edges")
        .and_then(|v| v.as_array())
        .expect("edges array");
    assert!(
        !edges.is_empty(),
        "expected at least the facility-country edge"
    );
    for e in edges {
        assert!(e.get("from").is_some() && e.get("to").is_some());
        assert!(e.get("relation").and_then(|v| v.as_str()).is_some());
        assert!(e.get("source_feed").and_then(|v| v.as_str()).is_some());
    }
}

#[tokio::test]
async fn expand_depth_is_clamped() {
    let g = get("/api/graph/expand?entity=Astravets&depth=99").await;
    assert_eq!(g.status, StatusCode::OK);
    assert_eq!(g.body.get("depth").and_then(|v| v.as_u64()), Some(3));

    let g = get("/api/graph/expand?entity=Astravets&depth=0").await;
    assert_eq!(g.status, StatusCode::OK);
    assert_eq!(g.body.get("depth").and_then(|v| v.as_u64()), Some(1));
}
