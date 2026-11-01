//! Integration tests for the Phase 3 region dossier (`ostara::dossier`).
//!
//! Three layers:
//! 1. **Haversine unit tests** — pure function, known city pairs.
//! 2. **Query validation contracts** — 400s for missing/out-of-range
//!    coords, radius clamping, default radius.
//! 3. **Shape contracts** — the dossier always answers 200 (per-feed
//!    degradation, never a whole-request 502), with every section present.
//!
//! Live upstream behavior in this sandbox is flaky: shape tests assert the
//! JSON *structure* only — never upstream data content, never `ok: true`.

use std::sync::Arc;
use std::time::Duration;

use axum::{
    Router,
    body::Body,
    http::{HeaderMap, Request, StatusCode},
};
use ostara::{cache::TtlCache, config::Config, dossier, routes};
use tower::ServiceExt; // .oneshot()

fn test_client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent("ostara/0.1 phase3-dossier-tests")
        .timeout(Duration::from_secs(20))
        .connect_timeout(Duration::from_secs(5))
        .pool_max_idle_per_host(4)
        .build()
        .expect("test client builds")
}

fn state_with(config: Config) -> routes::AppState {
    let config = Arc::new(config);
    routes::AppState {
        client: test_client(),
        cache: Arc::new(TtlCache::new()),
        vessels: ostara::feeds::vessels::VesselStream::new(config.ais_api_key.clone()),
        config,
    }
}

/// Canonical app (the dossier router is merged into `routes::app` by the
/// parent, alongside the feed and recon routers).
fn app() -> Router {
    let state = state_with(Config::test_default());
    routes::app(state)
}

struct Got {
    status: StatusCode,
    headers: HeaderMap,
    body: serde_json::Value,
}

async fn get(app: Router, uri: &str) -> Got {
    let req = Request::builder()
        .uri(uri)
        .body(Body::empty())
        .expect("request builds");
    let resp = app.oneshot(req).await.expect("oneshot responds");
    let status = resp.status();
    let headers = resp.headers().clone();
    let raw = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("body reads");
    let body = serde_json::from_slice(&raw).unwrap_or(serde_json::Value::Null);
    Got {
        status,
        headers,
        body,
    }
}

// ---------------------------------------------------------------------------
// 1. Haversine
// ---------------------------------------------------------------------------

#[test]
fn haversine_nyc_london() {
    // New York (40.7128, -74.0060) to London (51.5074, -0.1278) ≈ 5570 km.
    let d = dossier::haversine_km(40.7128, -74.0060, 51.5074, -0.1278);
    assert!(
        (d - 5570.0).abs() <= 50.0,
        "NYC-London = {d:.1} km, expected ~5570 ± 50"
    );
}

#[test]
fn haversine_same_point_is_zero() {
    let d = dossier::haversine_km(40.7, -74.0, 40.7, -74.0);
    assert!(d.abs() < 1e-9, "same point = {d}");
}

#[test]
fn haversine_sf_la() {
    // San Francisco to Los Angeles ≈ 559 km.
    let d = dossier::haversine_km(37.7749, -122.4194, 34.0522, -118.2437);
    assert!(
        (d - 559.0).abs() <= 25.0,
        "SF-LA = {d:.1} km, expected ~559 ± 25"
    );
}

#[test]
fn haversine_is_symmetric() {
    let a = dossier::haversine_km(10.0, 20.0, -30.0, 40.0);
    let b = dossier::haversine_km(-30.0, 40.0, 10.0, 20.0);
    assert!((a - b).abs() < 1e-9, "haversine must be symmetric");
}

// ---------------------------------------------------------------------------
// 2. Query validation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn lat_out_of_range_is_400() {
    let g = get(app(), "/api/dossier?lat=91&lon=0").await;
    assert_eq!(g.status, StatusCode::BAD_REQUEST);
    assert!(g.body.get("error").is_some(), "expected error shape");
}

#[tokio::test]
async fn lon_out_of_range_is_400() {
    let g = get(app(), "/api/dossier?lat=0&lon=181").await;
    assert_eq!(g.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn missing_lat_is_400() {
    let g = get(app(), "/api/dossier?lon=0").await;
    assert_eq!(g.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn missing_lon_is_400() {
    let g = get(app(), "/api/dossier?lat=0").await;
    assert_eq!(g.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn non_finite_radius_is_400() {
    let g = get(app(), "/api/dossier?lat=0&lon=0&radius_km=nan").await;
    assert_eq!(g.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn radius_clamps_to_5000() {
    let g = get(app(), "/api/dossier?lat=40.7&lon=-74.0&radius_km=99999").await;
    assert_eq!(g.status, StatusCode::OK);
    assert_eq!(
        g.body.get("radius_km").and_then(|v| v.as_f64()),
        Some(5000.0),
        "radius must clamp to 5000"
    );
}

#[tokio::test]
async fn radius_clamps_to_1_at_bottom() {
    let g = get(app(), "/api/dossier?lat=40.7&lon=-74.0&radius_km=0.2").await;
    assert_eq!(g.status, StatusCode::OK);
    assert_eq!(
        g.body.get("radius_km").and_then(|v| v.as_f64()),
        Some(1.0),
        "radius must clamp to 1"
    );
}

#[tokio::test]
async fn radius_defaults_to_100() {
    let g = get(app(), "/api/dossier?lat=40.7&lon=-74.0").await;
    assert_eq!(g.status, StatusCode::OK);
    assert_eq!(
        g.body.get("radius_km").and_then(|v| v.as_f64()),
        Some(100.0),
        "radius must default to 100"
    );
}

// ---------------------------------------------------------------------------
// 3. Shape contracts (network-agnostic)
// ---------------------------------------------------------------------------

/// Every geo section must carry ok/count/items; count never exceeds the
/// documented 50-item cap.
fn assert_geo_section_shape(sections: &serde_json::Value, name: &str) {
    let s = sections
        .get(name)
        .unwrap_or_else(|| panic!("missing section {name}"));
    assert!(s.get("ok").and_then(|v| v.as_bool()).is_some());
    let count = s
        .get("count")
        .and_then(|v| v.as_u64())
        .unwrap_or_else(|| panic!("{name}: missing count"));
    let items = s
        .get("items")
        .and_then(|v| v.as_array())
        .unwrap_or_else(|| panic!("{name}: missing items"));
    assert!(count <= 50, "{name}: count {count} exceeds 50-item cap");
    assert!(
        items.len() as u64 <= 50,
        "{name}: items len {} exceeds 50-item cap",
        items.len()
    );
}

#[tokio::test]
async fn dossier_returns_200_with_all_sections() {
    let g = get(app(), "/api/dossier?lat=40.7&lon=-74.0&radius_km=50").await;
    // Per-feed degradation: the dossier itself never 502s.
    assert_eq!(g.status, StatusCode::OK);

    for key in ["lat", "lon", "radius_km", "generated_at", "sections"] {
        assert!(g.body.get(key).is_some(), "missing top-level key {key}");
    }
    let sections = g.body.get("sections").unwrap();
    for name in ["flights", "quakes", "fires", "nuclear", "cameras"] {
        assert_geo_section_shape(sections, name);
    }

    // Conflicts: raw count, explicitly not geocoded.
    let conflicts = sections
        .get("conflicts")
        .expect("missing conflicts section");
    assert_eq!(
        conflicts.get("geocoded"),
        Some(&serde_json::Value::Bool(false))
    );
    assert!(conflicts.get("count").and_then(|v| v.as_u64()).is_some());
    assert!(
        conflicts
            .get("note")
            .and_then(|v| v.as_str())
            .is_some_and(|n| !n.is_empty()),
        "conflicts section must carry its scope note"
    );

    // Fires is keyed and the test config has no FIRMS_API_KEY: the section
    // must degrade in-place, not fail the dossier.
    let fires = sections.get("fires").unwrap();
    assert_eq!(fires.get("ok"), Some(&serde_json::Value::Bool(false)));
    assert!(
        fires
            .get("error")
            .and_then(|v| v.as_str())
            .is_some_and(|e| e.contains("FIRMS_API_KEY")),
        "fires must report the missing key, got: {fires}"
    );
}

#[tokio::test]
async fn in_radius_items_are_actually_in_radius() {
    // NYC, 50 km: whatever the network returns, any geocoded item served
    // must lie within the radius. (Items may be empty when upstreams are
    // unreachable — the assertion only constrains present items.)
    let g = get(app(), "/api/dossier?lat=40.7&lon=-74.0&radius_km=50").await;
    assert_eq!(g.status, StatusCode::OK);
    let sections = g.body.get("sections").unwrap();

    let check = |section: &str, lat_keys: &[&str], lon_keys: &[&str]| {
        let items = sections
            .get(section)
            .and_then(|s| s.get("items"))
            .and_then(|i| i.as_array())
            .unwrap();
        for item in items {
            let lat = lat_keys
                .iter()
                .find_map(|k| item.get(k).and_then(|v| v.as_f64()));
            let lon = lon_keys
                .iter()
                .find_map(|k| item.get(k).and_then(|v| v.as_f64()));
            if let (Some(la), Some(lo)) = (lat, lon) {
                let d = dossier::haversine_km(40.7, -74.0, la, lo);
                assert!(
                    d <= 50.0 + 1e-6,
                    "{section}: item at ({la}, {lo}) is {d:.1} km away, beyond 50 km"
                );
            }
        }
    };
    check("flights", &["latitude"], &["longitude"]);
    check("quakes", &["latitude"], &["longitude"]);
    check("fires", &["lat"], &["lon"]);
    check("nuclear", &["lat"], &["lon"]);
    check("cameras", &["lat"], &["lon"]);
}

#[tokio::test]
async fn dossier_caches_by_rounded_key() {
    // Fresh state so the cache starts empty.
    let state = state_with(Config::test_default());
    let router = || {
        let s = state.clone();
        routes::app(s)
    };

    let first = get(router(), "/api/dossier?lat=40.71&lon=-74.00&radius_km=50").await;
    assert_eq!(first.status, StatusCode::OK);
    assert_eq!(
        first.headers.get("x-cache").and_then(|v| v.to_str().ok()),
        Some("MISS")
    );
    let second = get(router(), "/api/dossier?lat=40.71&lon=-74.00&radius_km=50").await;
    assert_eq!(second.status, StatusCode::OK);
    assert_eq!(
        second.headers.get("x-cache").and_then(|v| v.to_str().ok()),
        Some("HIT")
    );
    // Same rounded bucket (2-decimal coords, 10 km radius) → same key → HIT.
    let third = get(router(), "/api/dossier?lat=40.714&lon=-74.004&radius_km=52").await;
    assert_eq!(
        third.headers.get("x-cache").and_then(|v| v.to_str().ok()),
        Some("HIT"),
        "nearby coords in the same rounded bucket must hit"
    );
}
