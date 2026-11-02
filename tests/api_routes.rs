//! Integration tests: every route served through the real axum router.
//!
//! These assert the HTTP *contract* a UI depends on — status codes,
//! content-type, and the exact JSON shapes — not upstream liveness. A feed
//! route may legitimately return 200 (upstream healthy) or 502 with
//! `{"error","detail"}` (upstream unreachable); both are correct behavior
//! and both are asserted here. Live upstream health is covered separately
//! by `tests/feeds_live.rs`; deterministic ingest/normalize coverage lives
//! in the unit tests under `src/`.

use std::sync::Arc;
use std::time::Duration;

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use ostara::{cache::TtlCache, config::Config, routes};
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
        vessels: ostara::feeds::vessels::VesselStream::new(None),
    }
}

/// The exact app the binary serves: `routes::app` is the canonical router.
fn app() -> Router {
    routes::app(test_state())
}

struct Got {
    status: StatusCode,
    content_type: Option<String>,
    body: serde_json::Value,
    raw: Vec<u8>,
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

/// A feed route's contract: 200 with the payload, or 502 carrying
/// {"error","detail"}. Anything else is a broken contract.
fn assert_feed_contract(g: &Got, route: &str) {
    assert_json(g, route);
    assert!(
        g.status == StatusCode::OK || g.status == StatusCode::BAD_GATEWAY,
        "{route}: expected 200 or 502, got {}",
        g.status
    );
    if g.status == StatusCode::BAD_GATEWAY {
        assert!(
            g.body.get("error").and_then(|v| v.as_str()).is_some(),
            "{route}: 502 body must carry string 'error', got: {}",
            body_preview(g)
        );
        assert!(
            g.body.get("detail").and_then(|v| v.as_str()).is_some(),
            "{route}: 502 body must carry string 'detail', got: {}",
            body_preview(g)
        );
    }
}

#[tokio::test]
async fn healthz_ok() {
    let g = get("/healthz").await;
    assert_eq!(g.status, StatusCode::OK, "expected 200, got {}", g.status);
    assert_json(&g, "/healthz");
    assert_eq!(g.body["status"], "ok");
}

#[tokio::test]
async fn unknown_route_404() {
    let g = get("/api/nope").await;
    assert_eq!(g.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn flights_contract_and_ui_shape() {
    let g = get("/api/flights").await;
    assert_feed_contract(&g, "/api/flights");
    if g.status == StatusCode::OK {
        assert!(
            g.body.get("count").and_then(|v| v.as_u64()).is_some(),
            "flights: missing numeric 'count'"
        );
        let flights = g.body["flights"]
            .as_array()
            .expect("flights: 'flights' must be an array");
        if let Some(f) = flights.first() {
            for field in ["hex", "callsign", "latitude", "longitude"] {
                assert!(
                    f.get(field).is_some(),
                    "flights: first aircraft missing UI field '{field}'"
                );
            }
        }
    }
}

#[tokio::test]
async fn quakes_contract_and_ui_shape() {
    let g = get("/api/quakes").await;
    assert_feed_contract(&g, "/api/quakes");
    if g.status == StatusCode::OK {
        let quakes = g.body["quakes"]
            .as_array()
            .expect("quakes: 'quakes' must be an array");
        if let Some(q) = quakes.first() {
            for field in ["magnitude", "place", "latitude", "longitude", "time"] {
                assert!(
                    q.get(field).is_some(),
                    "quakes: first quake missing UI field '{field}'"
                );
            }
        }
    }
}

#[tokio::test]
async fn weather_contract_and_ui_shape() {
    let g = get("/api/weather?lat=40.71&lon=-74.00").await;
    assert_feed_contract(&g, "/api/weather");
    if g.status == StatusCode::OK {
        for field in ["latitude", "longitude", "temperature_c", "time"] {
            assert!(
                g.body.get(field).is_some(),
                "weather: missing UI field '{field}'"
            );
        }
    }
}

#[tokio::test]
async fn weather_requires_coordinates() {
    let g = get("/api/weather").await;
    assert_eq!(g.status, StatusCode::BAD_REQUEST);
    assert_json(&g, "/api/weather");
    assert!(g.body.get("error").is_some());
    assert!(g.body.get("detail").is_some());
}

#[tokio::test]
async fn satellites_contract_and_ui_shape() {
    let g = get("/api/satellites").await;
    assert_feed_contract(&g, "/api/satellites");
    if g.status == StatusCode::OK {
        let sats = g.body["satellites"]
            .as_array()
            .expect("satellites: 'satellites' must be an array");
        if let Some(s) = sats.first() {
            for field in ["name", "norad_id", "line1", "line2"] {
                assert!(
                    s.get(field).is_some(),
                    "satellites: first object missing UI field '{field}'"
                );
            }
        }
    }
}

#[tokio::test]
async fn news_contract_and_ui_shape() {
    let g = get("/api/news?q=earthquake").await;
    assert_feed_contract(&g, "/api/news");
    if g.status == StatusCode::OK {
        let articles = g.body["articles"]
            .as_array()
            .expect("news: 'articles' must be an array");
        if let Some(a) = articles.first() {
            for field in ["title", "url", "domain", "seen_at"] {
                assert!(
                    a.get(field).is_some(),
                    "news: first article missing UI field '{field}'"
                );
            }
        }
    }
}

#[tokio::test]
async fn news_requires_query() {
    let g = get("/api/news").await;
    assert_eq!(g.status, StatusCode::BAD_REQUEST);
    assert_json(&g, "/api/news");
    assert!(g.body.get("error").is_some());
    assert!(g.body.get("detail").is_some());
}

#[tokio::test]
async fn cameras_contract_and_ui_shape() {
    // The registry is embedded: this route never 502s on upstream failure.
    let g = get("/api/cameras").await;
    assert_eq!(g.status, StatusCode::OK, "expected 200, got {}", g.status);
    assert_json(&g, "/api/cameras");
    let cameras = g.body["cameras"]
        .as_array()
        .expect("cameras: 'cameras' must be an array");
    assert!(!cameras.is_empty(), "cameras: registry should not be empty");
    let first = &cameras[0];
    for field in [
        "id",
        "name",
        "lat",
        "lon",
        "category",
        "image_url",
        "page_url",
        "location",
    ] {
        assert!(
            first.get(field).is_some(),
            "cameras: first camera missing UI field '{field}'"
        );
    }
    assert!(
        first["image_url"]
            .as_str()
            .is_some_and(|u| u.starts_with("https://") || u.starts_with("http://")),
        "cameras: image_url must be http(s)"
    );
}

#[tokio::test]
async fn cameras_category_filter() {
    let g = get("/api/cameras?category=traffic").await;
    assert_eq!(g.status, StatusCode::OK);
    let cameras = g.body["cameras"].as_array().expect("array");
    assert!(!cameras.is_empty());
    for c in cameras {
        assert_eq!(
            c["category"].as_str(),
            Some("traffic"),
            "category filter leaked a non-traffic camera"
        );
    }
}

#[tokio::test]
async fn cameras_bbox_filter() {
    // California bbox: keeps Bay Area cams, drops Finland/Nebraska.
    let g = get("/api/cameras?bbox=-125,32,-114,42").await;
    assert_eq!(g.status, StatusCode::OK);
    let cameras = g.body["cameras"].as_array().expect("array");
    assert!(
        !cameras.is_empty(),
        "expected cameras inside California bbox"
    );
    for c in cameras {
        let (lon, lat) = (
            c["lon"].as_f64().expect("lon numeric"),
            c["lat"].as_f64().expect("lat numeric"),
        );
        assert!(
            (-125.0..=-114.0).contains(&lon) && (32.0..=42.0).contains(&lat),
            "camera outside requested bbox"
        );
    }
}

#[tokio::test]
async fn cameras_bad_bbox_400() {
    let g = get("/api/cameras?bbox=garbage").await;
    assert_eq!(g.status, StatusCode::BAD_REQUEST);
    assert_json(&g, "/api/cameras");
    assert!(g.body.get("error").is_some());
    assert!(g.body.get("detail").is_some());
}

#[tokio::test]
async fn cameras_limit_clamped() {
    let g = get("/api/cameras?limit=3").await;
    assert_eq!(g.status, StatusCode::OK);
    assert_eq!(g.body["cameras"].as_array().expect("array").len(), 3);
    assert_eq!(g.body["count"].as_u64(), Some(3));
}

#[tokio::test]
async fn stats_always_200_with_six_feeds() {
    // /api/stats degrades per-feed instead of 502ing: always 200.
    let g = get("/api/stats").await;
    assert_eq!(g.status, StatusCode::OK, "expected 200, got {}", g.status);
    assert_json(&g, "/api/stats");
    let feeds = g.body["feeds"]
        .as_object()
        .expect("stats: 'feeds' must be an object");
    for name in [
        "flights",
        "quakes",
        "weather",
        "satellites",
        "news",
        "cameras",
    ] {
        let f = feeds
            .get(name)
            .unwrap_or_else(|| panic!("stats: missing feed '{name}'"));
        assert!(
            f.get("ok").and_then(|v| v.as_bool()).is_some(),
            "stats: feed '{name}' missing boolean 'ok'"
        );
    }
    assert!(
        g.body.get("degraded").and_then(|v| v.as_bool()).is_some(),
        "stats: missing boolean 'degraded'"
    );
}

#[tokio::test]
async fn feeds_health_always_200_with_six_reports() {
    // /api/health/feeds never fails: one dead feed reports "down", the
    // endpoint itself stays 200.
    let g = get("/api/health/feeds").await;
    assert_eq!(g.status, StatusCode::OK, "expected 200, got {}", g.status);
    assert_json(&g, "/api/health/feeds");
    let feeds = g.body["feeds"]
        .as_object()
        .expect("feeds_health: 'feeds' must be an object");
    for name in [
        "flights",
        "quakes",
        "weather",
        "satellites",
        "news",
        "cameras",
    ] {
        let r = feeds
            .get(name)
            .unwrap_or_else(|| panic!("feeds_health: missing report '{name}'"));
        let status = r["status"]
            .as_str()
            .unwrap_or_else(|| panic!("feeds_health: '{name}' status must be a string"));
        assert!(
            ["ok", "degraded", "down"].contains(&status),
            "feeds_health: '{name}' has unexpected status '{status}'"
        );
        assert!(
            r["latency_ms"].as_u64().is_some(),
            "feeds_health: '{name}' latency_ms must be a number"
        );
        assert!(
            r["detail"].as_str().is_some(),
            "feeds_health: '{name}' detail must be a string"
        );
    }
    assert!(
        g.body
            .get("generated_at")
            .and_then(|v| v.as_str())
            .is_some(),
        "feeds_health: missing 'generated_at'"
    );
}

#[tokio::test]
async fn threatfox_unset_key_is_503_with_error_detail() {
    // The default test state has no THREATFOX_API_KEY: the route must
    // degrade by design, never panic.
    let g = get("/api/threatfox").await;
    assert_json(&g, "/api/threatfox");
    assert_eq!(g.status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(
        g.body
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .contains("disabled"),
        "/api/threatfox: 503 error must say disabled, got: {}",
        body_preview(&g)
    );
    assert!(
        g.body.get("detail").and_then(|v| v.as_str()).is_some(),
        "/api/threatfox: 503 body must carry string 'detail', got: {}",
        body_preview(&g)
    );
}

#[tokio::test]
async fn threatfox_query_params_accepted_when_disabled() {
    // Query validation happens after the key gate: with no key the answer
    // is 503 regardless of params (no 400 leaks).
    let g = get("/api/threatfox?days=3&limit=10&malware=emotet").await;
    assert_eq!(g.status, StatusCode::SERVICE_UNAVAILABLE);
}

// ---------------------------------------------------------------------------
// Country OSINT directory (static curated data)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn country_osint_by_name_iso2_iso3() {
    for q in [
        "Japan",
        "jp",
        "JPN",
        "united%20kingdom",
        "United%20States",
        "de",
    ] {
        let g = get(&format!("/api/country-osint?country={}", q)).await;
        assert_eq!(g.status, StatusCode::OK, "country={q} should be 200");
        assert!(
            g.body.get("name").and_then(|v| v.as_str()).is_some(),
            "pack must have a name, got: {}",
            body_preview(&g)
        );
        assert!(
            g.body
                .get("categories")
                .and_then(|v| v.as_object())
                .is_some(),
            "pack must have categories, got: {}",
            body_preview(&g)
        );
    }
}

#[tokio::test]
async fn country_osint_missing_param_is_400() {
    let g = get("/api/country-osint").await;
    assert_eq!(g.status, StatusCode::BAD_REQUEST);
    assert!(
        g.body.get("error").and_then(|v| v.as_str()).is_some(),
        "400 body must carry 'error', got: {}",
        body_preview(&g)
    );
}

#[tokio::test]
async fn country_osint_unknown_country_is_404() {
    let g = get("/api/country-osint?country=Narnia").await;
    assert_eq!(g.status, StatusCode::NOT_FOUND);
    assert!(
        g.body
            .get("detail")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .contains("/api/country-osint/countries"),
        "404 detail must point at the country list, got: {}",
        body_preview(&g)
    );
}

#[tokio::test]
async fn country_osint_countries_list() {
    let g = get("/api/country-osint/countries").await;
    assert_eq!(g.status, StatusCode::OK);
    let arr = g.body.as_array().expect("countries list is an array");
    assert!(arr.len() >= 20, "expected 20+ countries, got {}", arr.len());
    for c in arr {
        for f in ["name", "iso2", "iso3", "coverage", "resource_count"] {
            assert!(c.get(f).is_some(), "country entry missing {f}: {c}");
        }
        let cov = c.get("coverage").and_then(|v| v.as_str()).unwrap_or("");
        assert!(
            ["full", "partial", "links"].contains(&cov),
            "bad coverage {cov:?}"
        );
    }
}

#[tokio::test]
async fn country_osint_global() {
    let g = get("/api/country-osint/global").await;
    assert_eq!(g.status, StatusCode::OK);
    assert!(
        g.body.get("count").and_then(|v| v.as_u64()).unwrap_or(0) >= 5,
        "global list should have several resources, got: {}",
        body_preview(&g)
    );
    assert!(
        g.body.get("resources").and_then(|v| v.as_array()).is_some(),
        "global must have resources array, got: {}",
        body_preview(&g)
    );
}

// ---------------------------------------------------------------------------
// Country inspect: click-to-identify (bbox resolver + OSINT + feed sections)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn country_inspect_paris_shape() {
    let g = get("/api/country/inspect?lat=48.85&lon=2.35").await;
    assert_eq!(g.status, StatusCode::OK, "got: {}", body_preview(&g));
    let country = g.body.get("country").expect("country key");
    assert_eq!(
        country.get("iso3").and_then(|v| v.as_str()),
        Some("FRA"),
        "got: {}",
        body_preview(&g)
    );
    assert_eq!(
        g.body.get("resolution").and_then(|v| v.as_str()),
        Some("bbox"),
        "got: {}",
        body_preview(&g)
    );
    // OSINT pack rides along when the directory covers the country.
    let osint = g.body.get("osint").expect("osint key");
    assert!(
        osint.get("name").and_then(|v| v.as_str()).is_some(),
        "osint pack must have a name, got: {}",
        body_preview(&g)
    );
    // Feed sections are always present; ok may be false when upstream is
    // unreachable from the sandbox — the contract is shape, not liveness.
    for feed in ["news", "conflicts", "nuclear"] {
        let sec = g
            .body
            .get("feeds")
            .and_then(|f| f.get(feed))
            .unwrap_or_else(|| panic!("feeds.{feed} missing: {}", body_preview(&g)));
        assert!(sec.get("ok").and_then(|v| v.as_bool()).is_some());
        assert!(sec.get("count").and_then(|v| v.as_u64()).is_some());
        assert!(sec.get("items").and_then(|v| v.as_array()).is_some());
    }
}

#[tokio::test]
async fn country_inspect_international_waters() {
    let g = get("/api/country/inspect?lat=-45&lon=-30").await;
    assert_eq!(g.status, StatusCode::OK, "got: {}", body_preview(&g));
    assert!(
        g.body.get("country").and_then(|v| v.as_null()).is_some(),
        "ocean point must resolve to null country, got: {}",
        body_preview(&g)
    );
    assert_eq!(
        g.body.get("resolution").and_then(|v| v.as_str()),
        Some("international-waters"),
        "got: {}",
        body_preview(&g)
    );
}

#[tokio::test]
async fn country_inspect_bad_coords_are_400() {
    for uri in [
        "/api/country/inspect",
        "/api/country/inspect?lat=48.85",
        "/api/country/inspect?lat=91&lon=2",
        "/api/country/inspect?lat=48&lon=181",
    ] {
        let g = get(uri).await;
        assert_eq!(
            g.status,
            StatusCode::BAD_REQUEST,
            "{uri} should be 400, got: {}",
            body_preview(&g)
        );
    }
}

#[tokio::test]
async fn telegram_missing_channel_is_400() {
    // Empty allowlist in test config: no ?channel= -> 400, no network touched.
    let g = get("/api/telegram").await;
    assert_eq!(g.status, StatusCode::BAD_REQUEST);
    assert!(
        g.body.get("error").and_then(|v| v.as_str()).is_some(),
        "400 body must carry 'error', got: {}",
        body_preview(&g)
    );
}

#[tokio::test]
async fn telegram_invalid_channel_name_is_400() {
    for uri in [
        "/api/telegram?channel=ab",          // too short
        "/api/telegram?channel=no%20spaces", // illegal chars
        "/api/telegram?channel=../x",        // path traversal attempt
    ] {
        let g = get(uri).await;
        assert_eq!(
            g.status,
            StatusCode::BAD_REQUEST,
            "{uri} should be 400, got: {}",
            body_preview(&g)
        );
    }
}

#[tokio::test]
async fn telegram_at_prefix_accepted_shape() {
    // Contract only: @-stripping happens before validation; a valid name
    // proceeds to fetch (which may 503 in sandbox — not asserted here).
    let g = get("/api/telegram?channel=%40telegram").await;
    assert!(
        g.status == StatusCode::OK || g.status == StatusCode::SERVICE_UNAVAILABLE,
        "expected 200 or honest 503, got: {}",
        body_preview(&g)
    );
    if g.status == StatusCode::OK {
        assert_eq!(
            g.body.get("channel").and_then(|v| v.as_str()),
            Some("telegram")
        );
        assert!(g.body.get("messages").and_then(|v| v.as_array()).is_some());
    }
}

// ---------------------------------------------------------------------------
// WhatsApp channels directory (static curated data, NOT a live feed)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn whatsapp_channels_full_list_has_entries_and_note() {
    let g = get("/api/whatsapp-channels").await;
    assert_eq!(g.status, StatusCode::OK, "got: {}", body_preview(&g));
    let channels = g
        .body
        .get("channels")
        .and_then(|v| v.as_array())
        .expect("channels array");
    assert!(
        channels.len() >= 10,
        "expected 10+ curated channels, got {}",
        channels.len()
    );
    assert_eq!(
        g.body.get("count").and_then(|v| v.as_u64()),
        Some(channels.len() as u64)
    );
    // Honest labeling: every response carries the directory disclaimer.
    let note = g
        .body
        .get("directory_note")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert!(
        note.contains("not live"),
        "directory_note must disclaim liveness, got: {note}"
    );
    for c in channels {
        let url = c.get("invite_url").and_then(|v| v.as_str()).unwrap_or("");
        assert!(
            url.starts_with("https://whatsapp.com/channel/"),
            "bad invite URL: {url}"
        );
    }
}

#[tokio::test]
async fn whatsapp_channels_country_filter() {
    let g = get("/api/whatsapp-channels?country=IN").await;
    assert_eq!(g.status, StatusCode::OK, "got: {}", body_preview(&g));
    let channels = g.body.get("channels").and_then(|v| v.as_array()).unwrap();
    assert!(!channels.is_empty(), "expected India channels");
    for c in channels {
        assert_eq!(
            c.get("iso2").and_then(|v| v.as_str()),
            Some("IN"),
            "country filter leaked: {}",
            body_preview(&g)
        );
    }
    // Unknown country => 200 with an empty list (not an error).
    let g2 = get("/api/whatsapp-channels?country=Narnia").await;
    assert_eq!(g2.status, StatusCode::OK);
    assert_eq!(
        g2.body.get("count").and_then(|v| v.as_u64()),
        Some(0),
        "got: {}",
        body_preview(&g2)
    );
}

#[tokio::test]
async fn whatsapp_channels_category_filter() {
    let g = get("/api/whatsapp-channels?category=news").await;
    assert_eq!(g.status, StatusCode::OK, "got: {}", body_preview(&g));
    let channels = g.body.get("channels").and_then(|v| v.as_array()).unwrap();
    assert!(!channels.is_empty(), "expected news channels");
    for c in channels {
        assert_eq!(c.get("category").and_then(|v| v.as_str()), Some("news"));
    }
}

#[tokio::test]
async fn country_inspect_includes_whatsapp_section() {
    // Central India (Nagpur area): the inspector must carry the
    // whatsapp_channels section with India's entries.
    let g = get("/api/country/inspect?lat=21.0&lon=78.0").await;
    assert_eq!(g.status, StatusCode::OK, "got: {}", body_preview(&g));
    let feeds = g.body.get("feeds").expect("feeds object");
    let wa = feeds
        .get("whatsapp_channels")
        .expect("whatsapp_channels section");
    assert_eq!(
        wa.get("ok").and_then(|v| v.as_bool()),
        Some(true),
        "got: {}",
        body_preview(&g)
    );
    let items = wa.get("items").and_then(|v| v.as_array()).unwrap();
    assert!(
        !items.is_empty(),
        "expected WhatsApp channels for India, got: {}",
        body_preview(&g)
    );
}

/// /api/vessels with an enabled (but empty-snapshot) stream: 200 with the
/// documented shape, no network involved.
#[tokio::test]
async fn vessels_enabled_empty_snapshot_200() {
    let state = routes::AppState {
        client: reqwest::Client::new(),
        cache: Arc::new(TtlCache::new()),
        config: Arc::new(Config::test_default()),
        vessels: ostara::feeds::vessels::VesselStream::enabled_for_test(),
    };
    let app = routes::app(state);
    let req = Request::builder()
        .uri("/api/vessels?limit=10")
        .body(Body::empty())
        .expect("request builds");
    let resp = app.oneshot(req).await.expect("oneshot serves");
    assert_eq!(resp.status(), StatusCode::OK);
    let raw = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("body reads");
    let body: serde_json::Value = serde_json::from_slice(&raw).expect("json");
    assert_eq!(body["count"], 0);
    assert_eq!(body["source"], "aisstream");
    assert!(body["vessels"].as_array().unwrap().is_empty());
}

/// /api/vessels rejects a malformed bbox with 400 (enabled stream, so the
/// disabled-503 path does not mask validation).
#[tokio::test]
async fn vessels_invalid_bbox_400() {
    let state = routes::AppState {
        client: reqwest::Client::new(),
        cache: Arc::new(TtlCache::new()),
        config: Arc::new(Config::test_default()),
        vessels: ostara::feeds::vessels::VesselStream::enabled_for_test(),
    };
    let app = routes::app(state);
    let req = Request::builder()
        .uri("/api/vessels?bbox=not-a-bbox")
        .body(Body::empty())
        .expect("request builds");
    let resp = app.oneshot(req).await.expect("oneshot serves");
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

// ---------------------------------------------------------------------------
// EMBERS media feeds: Bluesky / Mastodon (social), RSS / Google News (traditional)
// ---------------------------------------------------------------------------

/// Contract helper: a feed route returns 200 with its shape, or an honest
/// 503/400 — never a 500, never HTML.
async fn assert_media_feed_contract(uri: &str, shape_keys: &[&str]) {
    let g = get(uri).await;
    assert!(
        g.status == StatusCode::OK || g.status == StatusCode::SERVICE_UNAVAILABLE,
        "{uri}: expected 200 or honest 503, got: {}",
        body_preview(&g)
    );
    if g.status == StatusCode::OK {
        for k in shape_keys {
            assert!(
                g.body.get(*k).is_some(),
                "{uri}: 200 body missing '{k}', got: {}",
                body_preview(&g)
            );
        }
        assert!(
            g.body.get("count").and_then(|v| v.as_u64()).is_some(),
            "{uri}: 200 body missing numeric 'count', got: {}",
            body_preview(&g)
        );
    } else {
        assert!(
            g.body.get("error").and_then(|v| v.as_str()).is_some(),
            "{uri}: 503 body must carry 'error', got: {}",
            body_preview(&g)
        );
    }
}

#[tokio::test]
async fn bluesky_missing_q_is_400() {
    let g = get("/api/bluesky").await;
    assert_eq!(g.status, StatusCode::BAD_REQUEST);
    assert!(g.body.get("error").and_then(|v| v.as_str()).is_some());
}

#[tokio::test]
async fn bluesky_contract() {
    assert_media_feed_contract("/api/bluesky?q=test&limit=1", &["query", "posts"]).await;
}

#[tokio::test]
async fn mastodon_contract() {
    // q is optional: bare route returns the latest timeline snapshot.
    assert_media_feed_contract("/api/mastodon?limit=1", &["instance", "posts"]).await;
    assert_media_feed_contract("/api/mastodon?q=test&limit=1", &["query", "posts"]).await;
}

#[tokio::test]
async fn rss_contract() {
    assert_media_feed_contract("/api/rss?outlet=bbc&limit=1", &["outlet", "items"]).await;
}

#[tokio::test]
async fn rss_unknown_outlet_is_400() {
    let g = get("/api/rss?outlet=reuters").await;
    assert_eq!(g.status, StatusCode::BAD_REQUEST);
    assert!(g.body.get("error").and_then(|v| v.as_str()).is_some());
}

#[tokio::test]
async fn gnews_missing_q_is_400() {
    let g = get("/api/gnews").await;
    assert_eq!(g.status, StatusCode::BAD_REQUEST);
    assert!(g.body.get("error").and_then(|v| v.as_str()).is_some());
}

#[tokio::test]
async fn gnews_contract() {
    assert_media_feed_contract("/api/gnews?q=test&limit=1", &["query", "items"]).await;
}

#[tokio::test]
async fn unrest_missing_country_is_400() {
    let g = get("/api/unrest").await;
    assert_eq!(g.status, StatusCode::BAD_REQUEST);
    assert!(g.body.get("error").and_then(|v| v.as_str()).is_some());
}

#[tokio::test]
async fn unrest_unknown_country_is_404() {
    let g = get("/api/unrest?country=XX").await;
    assert_eq!(g.status, StatusCode::NOT_FOUND, "got: {}", body_preview(&g));
}

#[tokio::test]
async fn unrest_shape_for_france() {
    // Shape contract only: in the sandbox GDELT is unreachable, so
    // components may legitimately be ok:false — the endpoint must still
    // return 200 with the full transparent shape, never a 502.
    let g = get("/api/unrest?country=FRA").await;
    assert_eq!(g.status, StatusCode::OK, "got: {}", body_preview(&g));
    let b = &g.body;
    for key in [
        "country",
        "score",
        "level",
        "partial",
        "components",
        "methodology",
        "generated_at",
        "disclaimer",
    ] {
        assert!(
            b.get(key).is_some(),
            "missing key {key}, got: {}",
            body_preview(&g)
        );
    }
    assert_eq!(
        b.get("country")
            .and_then(|c| c.get("iso3"))
            .and_then(|v| v.as_str()),
        Some("FRA")
    );
    let level = b.get("level").and_then(|v| v.as_str()).unwrap_or("");
    assert!(
        ["low", "elevated", "high", "severe"].contains(&level),
        "bad level {level}"
    );
    let score = b.get("score").and_then(|v| v.as_f64()).unwrap_or(-1.0);
    assert!(
        (0.0..=100.0).contains(&score),
        "score out of range: {score}"
    );
    let comps = b
        .get("components")
        .and_then(|v| v.as_array())
        .expect("components array");
    assert_eq!(comps.len(), 3, "three signal components");
    for c in comps {
        for key in [
            "source",
            "signal",
            "ok",
            "query",
            "raw_value",
            "baseline_value",
            "normalized_0_1",
            "weight",
        ] {
            assert!(c.get(key).is_some(), "component missing {key}: {c}");
        }
        // The honesty words must be present; prediction words must not.
        let disclaimer = b.get("disclaimer").and_then(|v| v.as_str()).unwrap_or("");
        assert!(disclaimer.contains("NOT A FORECAST") || disclaimer.contains("not a forecast"));
    }
    let blob = serde_json::to_string(b).unwrap().to_lowercase();
    // The disclaimer denies forecasting ("not a forecast") — strip it, then
    // the remaining response must never claim to predict or forecast.
    let disclaimer = b
        .get("disclaimer")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_lowercase();
    let blob = blob.replacen(&disclaimer, "", 1);
    assert!(
        !blob.contains("prediction"),
        "response must never say prediction"
    );
    assert!(
        !blob.contains("forecast"),
        "response must never say forecast"
    );
}

#[tokio::test]
async fn unrest_accepts_name_and_iso2() {
    for q in ["/api/unrest?country=France", "/api/unrest?country=FR"] {
        let g = get(q).await;
        assert_eq!(
            g.status,
            StatusCode::OK,
            "got: {} for {q}",
            body_preview(&g)
        );
        assert_eq!(
            g.body
                .get("country")
                .and_then(|c| c.get("iso3"))
                .and_then(|v| v.as_str()),
            Some("FRA")
        );
    }
}
