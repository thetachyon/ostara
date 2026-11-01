//! Phase-1 (keyed feeds) integration tests: route contracts through the real
//! axum router.
//!
//! Conventions (mirroring `tests/api_routes.rs`):
//! - Every keyed feed proves its unset-key behavior with
//!   `Config::test_default()` (no keys): 503 `{"error","detail"}` or a
//!   documented keyless fallback.
//! - A feed route's contract is 200 with the payload, 502 carrying
//!   `{"error","detail"}` (upstream unreachable), or 503 for a disabled
//!   keyed feed. Anything else is a broken contract.
//! - Live tests are included ONLY where the upstream was sandbox-verified
//!   via reqwest (not just curl): CoinGecko and NOAA SWPC. The abuse.ch
//!   feeds (URLhaus/ThreatFox) are NOT live-tested: the sandbox
//!   verification probe was declined at the user-confirmation layer on
//!   2026-11-01 and was not retried, so their liveness here is unknown.
//!   GDACS RSS is also NOT live-tested: curl returns HTTP 200, but reqwest
//!   fails with "error sending request" — the same proxy/client interaction
//!   documented for celestrak.org in `tests/feeds_live.rs`. They are covered
//!   by unit tests on documented shapes plus the 200-or-502 contract below.

use std::sync::Arc;
use std::time::Duration;

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use ostara::{cache::TtlCache, config::Config, feeds, routes};
use tower::ServiceExt; // .oneshot()

fn test_state() -> routes::AppState {
    let client = reqwest::Client::builder()
        .user_agent("ostara/0.1 phase1-tests")
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

/// The Phase-1 router under test: `feeds::routes()` with shared state.
fn app() -> Router {
    feeds::routes().with_state(test_state())
}

/// The canonical full app (for the edited /api/flights handler, which lives
/// in `routes.rs`).
fn full_app() -> Router {
    routes::app(test_state())
}

struct Got {
    status: StatusCode,
    content_type: Option<String>,
    body: serde_json::Value,
    raw: Vec<u8>,
}

async fn get(app: Router, uri: &str) -> Got {
    let req = Request::builder()
        .uri(uri)
        .body(Body::empty())
        .expect("request builds");
    let resp = app.oneshot(req).await.expect("oneshot serves");
    let status = resp.status();
    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let raw = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("body reads")
        .to_vec();
    let body = serde_json::from_slice(&raw).unwrap_or(serde_json::Value::Null);
    Got {
        status,
        content_type,
        body,
        raw,
    }
}

fn body_preview(g: &Got) -> String {
    String::from_utf8_lossy(&g.raw).chars().take(200).collect()
}

fn assert_json(g: &Got, route: &str) {
    assert!(
        g.content_type
            .as_deref()
            .unwrap_or("")
            .starts_with("application/json"),
        "{route}: expected application/json content-type, got {:?}",
        g.content_type
    );
    assert!(
        g.body.is_object(),
        "{route}: expected JSON object body, got: {}",
        body_preview(g)
    );
}

/// Error-body contract shared by 502 and 503: {"error","detail"} strings.
fn assert_error_body(g: &Got, route: &str) {
    assert_json(g, route);
    assert!(
        g.body.get("error").and_then(|v| v.as_str()).is_some(),
        "{route}: error body must carry string 'error', got: {}",
        body_preview(g)
    );
    assert!(
        g.body.get("detail").and_then(|v| v.as_str()).is_some(),
        "{route}: error body must carry string 'detail', got: {}",
        body_preview(g)
    );
}

/// A live-data route's contract: 200 with payload, or 502 {"error","detail"}.
fn assert_live_contract(g: &Got, route: &str) {
    assert!(
        g.status == StatusCode::OK || g.status == StatusCode::BAD_GATEWAY,
        "{route}: expected 200 or 502, got {}",
        g.status
    );
    if g.status == StatusCode::BAD_GATEWAY {
        assert_error_body(g, route);
    } else {
        assert_json(g, route);
    }
}

// ---------------------------------------------------------------------------
// Keyed feeds: unset-key behavior (the core Phase-1 contract)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn fires_unset_key_503() {
    let g = get(app(), "/api/fires").await;
    assert_eq!(
        g.status,
        StatusCode::SERVICE_UNAVAILABLE,
        "expected 503, got {}",
        g.status
    );
    assert_error_body(&g, "/api/fires");
    assert_eq!(g.body["error"], "fires feed disabled");
    assert!(
        g.body["detail"]
            .as_str()
            .is_some_and(|d| d.contains("FIRMS_API_KEY")),
        "detail must name the missing key, got: {}",
        body_preview(&g)
    );
}

#[tokio::test]
async fn vessels_unset_key_503() {
    let g = get(app(), "/api/vessels").await;
    assert_eq!(
        g.status,
        StatusCode::SERVICE_UNAVAILABLE,
        "expected 503, got {}",
        g.status
    );
    assert_error_body(&g, "/api/vessels");
    assert_eq!(g.body["error"], "vessels feed disabled");
    assert!(
        g.body["detail"]
            .as_str()
            .is_some_and(|d| d.contains("AIS_API_KEY")),
        "detail must name the missing key, got: {}",
        body_preview(&g)
    );
}

#[tokio::test]
async fn telegram_unset_creds_503() {
    let g = get(app(), "/api/telegram?channel=example").await;
    assert_eq!(
        g.status,
        StatusCode::SERVICE_UNAVAILABLE,
        "expected 503, got {}",
        g.status
    );
    assert_error_body(&g, "/api/telegram");
    assert_eq!(g.body["error"], "telegram feed disabled");
    assert!(
        g.body["detail"]
            .as_str()
            .is_some_and(|d| d.contains("TELEGRAM_API_ID") && d.contains("my.telegram.org")),
        "detail must explain setup, got: {}",
        body_preview(&g)
    );
}

#[tokio::test]
async fn telegram_missing_channel_400() {
    // No ?channel= and no TELEGRAM_CHANNELS configured → 400, not 503:
    // parameter validation runs before the key check.
    let g = get(app(), "/api/telegram").await;
    assert_eq!(
        g.status,
        StatusCode::BAD_REQUEST,
        "expected 400, got {}",
        g.status
    );
    assert_error_body(&g, "/api/telegram");
}

#[tokio::test]
async fn n2yo_unset_key_falls_back_to_celestrak() {
    // Keyless fallback (not 503): CelesTrak TLE served as the catalog.
    let g = get(app(), "/api/satellites/catalog").await;
    assert_live_contract(&g, "/api/satellites/catalog");
    if g.status == StatusCode::OK {
        assert_eq!(g.body["source"], "celestrak");
        assert!(
            g.body.get("count").and_then(|v| v.as_u64()).is_some(),
            "catalog: missing numeric 'count'"
        );
        let sats = g.body["satellites"]
            .as_array()
            .expect("catalog: 'satellites' must be an array");
        if let Some(s) = sats.first() {
            for field in ["name", "norad_id"] {
                assert!(
                    s.get(field).is_some(),
                    "catalog: first entry missing '{field}'"
                );
            }
            assert!(
                s.get("latitude").is_none(),
                "celestrak fallback must not fabricate positions"
            );
        }
    }
}

#[tokio::test]
async fn n2yo_bad_coordinates_400() {
    let g = get(app(), "/api/satellites/catalog?lat=999").await;
    assert_eq!(
        g.status,
        StatusCode::BAD_REQUEST,
        "expected 400, got {}",
        g.status
    );
    assert_error_body(&g, "/api/satellites/catalog");
}

// ---------------------------------------------------------------------------
// Edited /api/flights handler (routes.rs): no OpenSky creds → adsb.lol path
// ---------------------------------------------------------------------------

#[tokio::test]
async fn flights_without_opensky_creds_still_serves() {
    let g = get(full_app(), "/api/flights").await;
    assert_live_contract(&g, "/api/flights");
    if g.status == StatusCode::OK {
        // adsb.lol path echoes the requested scope; the OpenSky path would
        // report "opensky".
        assert_eq!(g.body["scope"], "mil");
        assert!(
            g.body.get("count").and_then(|v| v.as_u64()).is_some(),
            "flights: missing numeric 'count'"
        );
    }
}

// ---------------------------------------------------------------------------
// Keyless live feeds: contract tests (200-or-502)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn cyber_contract() {
    // Verified 2026-11-01: the abuse.ch APIs (URLhaus AND ThreatFox) require
    // an Auth-Key — keyless requests get HTTP 401. With no key configured
    // the feed degrades by design (503), never 200-or-502.
    let g = get(app(), "/api/cyber/threats").await;
    assert_eq!(
        g.status,
        StatusCode::SERVICE_UNAVAILABLE,
        "/api/cyber/threats: expected 503 without THREATFOX_API_KEY, got {}",
        g.status
    );
    assert_error_body(&g, "/api/cyber/threats");
}

#[tokio::test]
async fn markets_contract() {
    let g = get(app(), "/api/markets").await;
    assert_live_contract(&g, "/api/markets");
    if g.status == StatusCode::OK {
        assert!(
            g.body.get("count").and_then(|v| v.as_u64()).is_some(),
            "markets: missing numeric 'count'"
        );
        let mkts = g.body["markets"]
            .as_array()
            .expect("markets: 'markets' must be an array");
        if let Some(m) = mkts.first() {
            for field in [
                "id",
                "symbol",
                "name",
                "price_usd",
                "market_cap_usd",
                "change_24h_pct",
            ] {
                assert!(
                    m.get(field).is_some(),
                    "markets: first market missing '{field}'"
                );
            }
        }
    }
}

#[tokio::test]
async fn nuclear_registry_always_200() {
    // Embedded static registry: never 502s.
    let g = get(app(), "/api/nuclear").await;
    assert_eq!(g.status, StatusCode::OK, "expected 200, got {}", g.status);
    assert_json(&g, "/api/nuclear");
    let count = g.body["count"].as_u64().expect("nuclear: numeric 'count'");
    assert!(count >= 50, "nuclear: expected 50+ facilities, got {count}");
    let facs = g.body["facilities"]
        .as_array()
        .expect("nuclear: 'facilities' must be an array");
    assert_eq!(facs.len() as u64, count);
    let first = &facs[0];
    for field in ["id", "name", "lat", "lon", "country", "status", "source"] {
        assert!(
            first.get(field).is_some(),
            "nuclear: first facility missing '{field}'"
        );
    }
}

#[tokio::test]
async fn nuclear_country_filter() {
    let g = get(app(), "/api/nuclear?country=japan").await;
    assert_eq!(g.status, StatusCode::OK, "expected 200, got {}", g.status);
    let facs = g.body["facilities"]
        .as_array()
        .expect("nuclear: 'facilities' must be an array");
    assert!(!facs.is_empty(), "expected Japanese facilities");
    for f in facs {
        assert_eq!(f["country"], "Japan", "country filter leaked");
    }
    let g2 = get(app(), "/api/nuclear?country=Atlantis").await;
    assert_eq!(g2.status, StatusCode::OK);
    assert_eq!(g2.body["count"], 0);
}

#[tokio::test]
async fn conflicts_contract() {
    let g = get(app(), "/api/conflicts").await;
    assert_live_contract(&g, "/api/conflicts");
    if g.status == StatusCode::OK {
        assert!(
            g.body.get("count").and_then(|v| v.as_u64()).is_some(),
            "conflicts: missing numeric 'count'"
        );
        assert!(
            g.body.get("note").and_then(|v| v.as_str()).is_some(),
            "conflicts: missing scope 'note'"
        );
        let events = g.body["events"]
            .as_array()
            .expect("conflicts: 'events' must be an array");
        if let Some(e) = events.first() {
            for field in ["title", "country", "date", "url", "source"] {
                assert!(
                    e.get(field).is_some(),
                    "conflicts: first event missing '{field}'"
                );
            }
            assert_eq!(e["source"], "gdacs");
        }
    }
}

#[tokio::test]
async fn helio_contract() {
    let g = get(app(), "/api/space-weather").await;
    assert_live_contract(&g, "/api/space-weather");
    if g.status == StatusCode::OK {
        for field in ["generated_at", "xray_flux_latest", "kp_latest", "note"] {
            assert!(
                g.body.get(field).is_some(),
                "helio: missing field '{field}'"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Live tests: real upstreams, honest failure. Only hosts sandbox-verified
// via reqwest (2026-11-01). abuse.ch is deliberately NOT live-tested (see
// module docs in feeds/cyber.rs).
// ---------------------------------------------------------------------------

fn live_client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent("ostara/0.1 phase1-live-tests")
        .timeout(Duration::from_secs(6))
        .connect_timeout(Duration::from_secs(6))
        .build()
        .expect("live client builds")
}

#[tokio::test]
async fn markets_live() {
    let client = live_client();
    let r = match tokio::time::timeout(Duration::from_secs(6), feeds::markets::fetch(&client)).await
    {
        Err(_) => panic!("LIVE-FAIL markets (api.coingecko.com): timed out after 6s"),
        Ok(Err(e)) => panic!("LIVE-FAIL markets (api.coingecko.com): {}", e.detail),
        Ok(Ok(r)) => r,
    };
    assert!(
        !r.markets.is_empty(),
        "LIVE-FAIL markets: zero markets returned"
    );
    let m = &r.markets[0];
    assert!(
        m.price_usd > 0.0,
        "LIVE-FAIL markets: first market has non-positive price"
    );
    assert!(
        !m.id.is_empty(),
        "LIVE-FAIL markets: first market missing id"
    );
}

#[tokio::test]
async fn helio_live() {
    let client = live_client();
    let r = match tokio::time::timeout(Duration::from_secs(6), feeds::helio::fetch(&client)).await {
        Err(_) => panic!("LIVE-FAIL helio (services.swpc.noaa.gov): timed out after 6s"),
        Ok(Err(e)) => panic!("LIVE-FAIL helio (services.swpc.noaa.gov): {}", e.detail),
        Ok(Ok(r)) => r,
    };
    assert!(
        r.xray_flux_latest.is_some() || r.kp_latest.is_some(),
        "LIVE-FAIL helio: both series empty"
    );
    if let Some(flux) = r.xray_flux_latest {
        assert!(
            (1e-9..1e-2).contains(&flux),
            "LIVE-FAIL helio: xray flux {flux} outside plausible W/m² range"
        );
    }
    if let Some(kp) = r.kp_latest {
        assert!(
            (0.0..=9.0).contains(&kp),
            "LIVE-FAIL helio: kp {kp} outside 0-9 scale"
        );
    }
}

// NOTE: no conflicts_live test. GDACS RSS verifies over curl (HTTP 200)
// but reqwest fails in this sandbox with "error sending request" — the
// same proxy/client interaction class documented for celestrak.org in
// tests/feeds_live.rs. The 200-or-502 contract test above covers the route.
