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

// ---------------------------------------------------------------------------
// /api/threats/*
// ---------------------------------------------------------------------------

#[tokio::test]
async fn threats_politicians_lists_watchlist() {
    let g = get("/api/threats/politicians").await;
    assert_eq!(g.status, StatusCode::OK, "got: {}", body_preview(&g));
    let count = g.body.get("count").and_then(|v| v.as_u64()).unwrap_or(0);
    assert!(count >= 60, "expected 60+ figures, got {count}");
    let figs = g
        .body
        .get("figures")
        .and_then(|v| v.as_array())
        .expect("figures array");
    assert_eq!(figs.len() as u64, count);
    let first = &figs[0];
    for k in ["name", "role", "country", "iso2", "iso3", "aliases"] {
        assert!(first.get(k).is_some(), "figure missing {k}");
    }
}

#[tokio::test]
async fn threats_politicians_country_filter() {
    let g = get("/api/threats/politicians?country=JP").await;
    assert_eq!(g.status, StatusCode::OK, "got: {}", body_preview(&g));
    let figs = g
        .body
        .get("figures")
        .and_then(|v| v.as_array())
        .expect("figures array");
    assert!(!figs.is_empty(), "expected Japanese figures");
    assert!(
        figs.iter()
            .all(|f| f.get("iso2").and_then(|v| v.as_str()) == Some("JP"))
    );
}

#[tokio::test]
async fn threats_scan_missing_name_is_400() {
    let g = get("/api/threats/scan").await;
    assert_eq!(
        g.status,
        StatusCode::BAD_REQUEST,
        "got: {}",
        body_preview(&g)
    );
}

#[tokio::test]
async fn threats_scan_unknown_figure_is_404() {
    let g = get("/api/threats/scan?name=Narnia").await;
    assert_eq!(g.status, StatusCode::NOT_FOUND, "got: {}", body_preview(&g));
}

#[tokio::test]
async fn threats_scan_bad_severity_is_400() {
    let g = get("/api/threats/scan?name=Macron&severity=bogus").await;
    assert_eq!(
        g.status,
        StatusCode::BAD_REQUEST,
        "got: {}",
        body_preview(&g)
    );
}

#[tokio::test]
async fn threats_scan_bad_direction_is_400() {
    let g = get("/api/threats/scan?name=Macron&direction=bogus").await;
    assert_eq!(
        g.status,
        StatusCode::BAD_REQUEST,
        "got: {}",
        body_preview(&g)
    );
}

#[tokio::test]
async fn threats_scan_direction_filter() {
    // Default: every returned item is direction=against. direction=all is
    // a superset. Upstreams may be down in the sandbox; the contract is
    // the filter behavior on whatever items exist.
    let dflt = get("/api/threats/scan?name=Trump").await;
    assert_eq!(dflt.status, StatusCode::OK, "got: {}", body_preview(&dflt));
    let items = dflt.body.get("items").and_then(|v| v.as_array()).unwrap();
    for it in items {
        assert_eq!(
            it.get("direction").and_then(|v| v.as_str()),
            Some("against"),
            "default filter keeps only against"
        );
    }
    let by = get("/api/threats/scan?name=Trump&direction=by").await;
    assert_eq!(by.status, StatusCode::OK, "got: {}", body_preview(&by));
    for it in by.body.get("items").and_then(|v| v.as_array()).unwrap() {
        assert_eq!(
            it.get("direction").and_then(|v| v.as_str()),
            Some("by"),
            "direction=by keeps only by"
        );
    }
    let all = get("/api/threats/scan?name=Trump&direction=all").await;
    assert_eq!(all.status, StatusCode::OK, "got: {}", body_preview(&all));
    let all_items = all.body.get("items").and_then(|v| v.as_array()).unwrap();
    assert!(
        all_items.len() >= items.len(),
        "direction=all is a superset of the default"
    );
    for it in all_items {
        let dir = it.get("direction").and_then(|v| v.as_str()).unwrap_or("");
        assert!(
            ["against", "by", "unclear"].contains(&dir),
            "bad direction {dir}"
        );
    }
    let disc = all
        .body
        .get("disclaimer")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert!(
        disc.contains("DIRECTIONALITY"),
        "disclaimer documents directionality"
    );
}

#[tokio::test]
async fn threats_scan_shape() {
    // Upstreams may be unreachable in the sandbox; the contract is the
    // shape: 200 with figure/items/sources/disclaimer, honest per-source ok.
    let g = get("/api/threats/scan?name=Macron").await;
    assert_eq!(g.status, StatusCode::OK, "got: {}", body_preview(&g));
    assert_eq!(
        g.body
            .get("figure")
            .and_then(|f| f.get("name"))
            .and_then(|v| v.as_str()),
        Some("Emmanuel Macron")
    );
    assert!(g.body.get("items").and_then(|v| v.as_array()).is_some());
    assert!(g.body.get("sources").and_then(|v| v.as_array()).is_some());
    let accounts = g.body.get("accounts").and_then(|v| v.as_array());
    assert!(accounts.is_some(), "accounts array present");
    for a in accounts.unwrap() {
        let plat = a.get("platform").and_then(|v| v.as_str()).unwrap_or("");
        assert!(
            ["bluesky", "telegram"].contains(&plat),
            "bad platform {plat}"
        );
        assert!(a.get("handle").and_then(|v| v.as_str()).is_some());
        assert!(a.get("profile_url").and_then(|v| v.as_str()).is_some());
        assert!(a.get("post_count").and_then(|v| v.as_u64()).is_some());
        assert!(a.get("severities").and_then(|v| v.as_object()).is_some());
        assert!(a.get("sample").and_then(|v| v.as_array()).is_some());
        let sev = a.get("max_severity").and_then(|v| v.as_str()).unwrap_or("");
        assert!(
            ["critical", "high", "medium"].contains(&sev),
            "bad max_severity {sev}"
        );
    }
    let disc = g
        .body
        .get("disclaimer")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert!(disc.contains("NOT VERIFIED THREATS"), "disclaimer present");
    assert!(
        disc.contains("human review"),
        "disclaimer mentions human review"
    );
    assert!(
        disc.contains("ACCOUNT ATTRIBUTION"),
        "disclaimer covers account attribution"
    );
    assert!(
        disc.contains("not a list of threat actors"),
        "disclaimer disavows threat-actor labeling"
    );
    for it in g.body.get("items").and_then(|v| v.as_array()).unwrap() {
        let sev = it.get("severity").and_then(|v| v.as_str()).unwrap_or("");
        assert!(
            ["critical", "high", "medium"].contains(&sev),
            "bad severity {sev}"
        );
    }
}

#[tokio::test]
async fn threats_scan_severity_filter() {
    let g = get("/api/threats/scan?name=Macron&severity=critical").await;
    assert_eq!(g.status, StatusCode::OK, "got: {}", body_preview(&g));
    for it in g.body.get("items").and_then(|v| v.as_array()).unwrap() {
        assert_eq!(
            it.get("severity").and_then(|v| v.as_str()),
            Some("critical")
        );
    }
}

#[tokio::test]
async fn chokepoints_endpoint_shape() {
    let g = get("/api/chokepoints").await;
    assert_eq!(g.status, StatusCode::OK, "got: {}", body_preview(&g));
    assert_eq!(
        g.body.get("count").and_then(|v| v.as_u64()),
        Some(10),
        "got: {}",
        body_preview(&g)
    );
    let caveat = g
        .body
        .get("caveat")
        .and_then(|v| v.as_str())
        .expect("caveat key");
    assert!(caveat.contains("2026-11-02"), "got: {}", body_preview(&g));
    assert!(
        caveat.contains("not a live feed"),
        "got: {}",
        body_preview(&g)
    );
    let entries = g
        .body
        .get("entries")
        .and_then(|v| v.as_array())
        .expect("entries array");
    assert_eq!(entries.len(), 10);
    for e in entries {
        assert!(e.get("id").and_then(|v| v.as_str()).is_some());
        assert!(e.get("name").and_then(|v| v.as_str()).is_some());
        let cat = e.get("category").and_then(|v| v.as_str()).unwrap_or("");
        assert!(["chokepoint", "congestion_port"].contains(&cat));
        assert!(e.get("latitude").and_then(|v| v.as_f64()).is_some());
        assert!(e.get("longitude").and_then(|v| v.as_f64()).is_some());
        assert!(e.get("status").and_then(|v| v.as_str()).is_some());
        assert_eq!(
            e.get("status_date").and_then(|v| v.as_str()),
            Some("2026-11-02")
        );
    }
}

#[tokio::test]
async fn chokepoints_category_filter() {
    let g = get("/api/chokepoints?category=chokepoint").await;
    assert_eq!(g.status, StatusCode::OK, "got: {}", body_preview(&g));
    assert_eq!(
        g.body.get("count").and_then(|v| v.as_u64()),
        Some(6),
        "got: {}",
        body_preview(&g)
    );
    let g = get("/api/chokepoints?category=congestion_port").await;
    assert_eq!(g.status, StatusCode::OK, "got: {}", body_preview(&g));
    assert_eq!(
        g.body.get("count").and_then(|v| v.as_u64()),
        Some(4),
        "got: {}",
        body_preview(&g)
    );
}

// ---------------------------------------------------------------------------
// /api/risk contract
// ---------------------------------------------------------------------------

#[tokio::test]
async fn risk_missing_place_is_400() {
    let g = get("/api/risk").await;
    assert_eq!(
        g.status,
        StatusCode::BAD_REQUEST,
        "got: {}",
        body_preview(&g)
    );
}

#[tokio::test]
async fn risk_bad_kind_is_400() {
    let g = get("/api/risk?place=France&kind=planet").await;
    assert_eq!(
        g.status,
        StatusCode::BAD_REQUEST,
        "got: {}",
        body_preview(&g)
    );
}

#[tokio::test]
async fn risk_unknown_country_is_404() {
    let g = get("/api/risk?place=Narnia&kind=country").await;
    assert_eq!(g.status, StatusCode::NOT_FOUND, "got: {}", body_preview(&g));
}

#[tokio::test]
async fn risk_shape_for_france() {
    // Shape contract only: in the sandbox upstreams are flaky, so
    // components may legitimately be ok:false — the endpoint must still
    // return 200 with the full transparent shape, never a 502.
    let g = get("/api/risk?place=FRA").await;
    assert_eq!(g.status, StatusCode::OK, "got: {}", body_preview(&g));
    let b = &g.body;
    for key in [
        "place",
        "score",
        "level",
        "safety",
        "partial",
        "families",
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
    let place = b.get("place").expect("place");
    assert_eq!(place.get("kind").and_then(|v| v.as_str()), Some("country"));
    assert_eq!(place.get("iso3").and_then(|v| v.as_str()), Some("FRA"));
    let level = b.get("level").and_then(|v| v.as_str()).unwrap_or("");
    assert!(
        ["low", "moderate", "high", "severe"].contains(&level),
        "bad level {level}"
    );
    let safety = b.get("safety").and_then(|v| v.as_str()).unwrap_or("");
    assert!(
        ["High", "Moderate", "Low", "Very low"].contains(&safety),
        "bad safety {safety}"
    );
    let score = b.get("score").and_then(|v| v.as_f64()).unwrap_or(-1.0);
    assert!(
        (0.0..=100.0).contains(&score),
        "score out of range: {score}"
    );
    let fams = b
        .get("families")
        .and_then(|v| v.as_array())
        .expect("families array");
    assert_eq!(fams.len(), 3, "three signal families");
    let mut fam_ids: Vec<&str> = fams
        .iter()
        .filter_map(|f| f.get("family").and_then(|v| v.as_str()))
        .collect();
    fam_ids.sort_unstable();
    assert_eq!(fam_ids, ["crime_disorder", "unrest", "violent_incidents"]);
    for f in fams {
        for key in ["family", "weight", "score_0_100", "partial", "components"] {
            assert!(f.get(key).is_some(), "family missing {key}: {f}");
        }
        let comps = f
            .get("components")
            .and_then(|v| v.as_array())
            .expect("components");
        assert_eq!(comps.len(), 3, "three feed components per family");
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
        }
    }
    // The honesty words must be present in the disclaimer.
    let disclaimer = b.get("disclaimer").and_then(|v| v.as_str()).unwrap_or("");
    assert!(disclaimer.contains("NOT A FORECAST"));
    assert!(disclaimer.contains("over-covered"));
    assert!(disclaimer.contains("not travel advice"));
    // The disclaimer denies forecasting ("not a forecast") — strip it, then
    // the remaining response must never claim to predict or forecast.
    let blob = serde_json::to_string(b).unwrap().to_lowercase();
    let blob = blob.replacen(&disclaimer.to_lowercase(), "", 1);
    assert!(
        !blob.contains("prediction"),
        "response must never say prediction"
    );
    assert!(
        !blob.contains("forecast"),
        "response must never say forecast"
    );
    assert!(
        !blob.contains("probability"),
        "response must never say probability"
    );
}

#[tokio::test]
async fn risk_city_path_marks_gdelt_unavailable_and_partial() {
    // Deterministic regardless of upstream health: the city path always
    // degrades the GDELT leg (no city-level coverage).
    let g = get("/api/risk?place=Paris&kind=city&country=France").await;
    assert_eq!(g.status, StatusCode::OK, "got: {}", body_preview(&g));
    let b = &g.body;
    let place = b.get("place").expect("place");
    assert_eq!(place.get("kind").and_then(|v| v.as_str()), Some("city"));
    assert_eq!(place.get("name").and_then(|v| v.as_str()), Some("Paris"));
    assert_eq!(
        place.get("country_hint").and_then(|v| v.as_str()),
        Some("France")
    );
    assert_eq!(b.get("partial").and_then(|v| v.as_bool()), Some(true));
    let fams = b
        .get("families")
        .and_then(|v| v.as_array())
        .expect("families");
    for f in fams {
        let gdelt = f
            .get("components")
            .and_then(|v| v.as_array())
            .expect("components")
            .iter()
            .find(|c| c.get("source").and_then(|v| v.as_str()) == Some("gdelt"))
            .expect("gdelt component present");
        assert_eq!(gdelt.get("ok").and_then(|v| v.as_bool()), Some(false));
        let err = gdelt.get("error").and_then(|v| v.as_str()).unwrap_or("");
        assert!(err.contains("no city-level coverage"), "gdelt error: {err}");
    }
}

#[tokio::test]
async fn risk_kind_auto_detects() {
    // France resolves to the country path; Paris falls through to city.
    let g = get("/api/risk?place=France").await;
    assert_eq!(g.status, StatusCode::OK, "got: {}", body_preview(&g));
    assert_eq!(
        g.body
            .get("place")
            .and_then(|p| p.get("kind"))
            .and_then(|v| v.as_str()),
        Some("country")
    );
    let g = get("/api/risk?place=Paris").await;
    assert_eq!(g.status, StatusCode::OK, "got: {}", body_preview(&g));
    assert_eq!(
        g.body
            .get("place")
            .and_then(|p| p.get("kind"))
            .and_then(|v| v.as_str()),
        Some("city")
    );
}

/// Write a minimal risk snapshot into a temp dir and point
/// RISK_SNAPSHOT_DIR at it. Returns the dir path and the previous env value.
fn risk_map_fixture(files: &[(&str, &str)]) -> (std::path::PathBuf, Option<String>) {
    let dir = std::env::temp_dir().join(format!(
        "riskmap-api-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    for (name, body) in files {
        std::fs::write(dir.join(name), body).unwrap();
    }
    let prev = std::env::var("RISK_SNAPSHOT_DIR").ok();
    // SAFETY: single-threaded test use; no other test touches this var.
    unsafe { std::env::set_var("RISK_SNAPSHOT_DIR", &dir) };
    (dir, prev)
}

fn risk_map_restore(dir: &std::path::PathBuf, prev: Option<String>) {
    // SAFETY: restores the pre-test value; see above.
    unsafe {
        match prev {
            Some(v) => std::env::set_var("RISK_SNAPSHOT_DIR", v),
            None => std::env::remove_var("RISK_SNAPSHOT_DIR"),
        }
    }
    std::fs::remove_dir_all(dir).ok();
}

fn snap_doc(at: &str, entries: &[(&str, f64)]) -> String {
    let places: Vec<serde_json::Value> = entries
        .iter()
        .map(|(n, s)| {
            serde_json::json!({"name": n, "kind": "country", "lon": 0.0, "lat": 0.0,
                "score": s, "level": "high", "safety": "Low", "partial": true,
                "ok_components": 6,
                "families": [{"family": "unrest", "weight": 0.4, "score_0_100": *s}]})
        })
        .collect();
    serde_json::json!({"generated_at": at, "place_count": places.len(),
        "places": places, "caveat": "test"})
    .to_string()
}

#[tokio::test]
async fn risk_map_no_snapshot_is_503() {
    let (dir, prev) = risk_map_fixture(&[]);
    let g = get("/api/risk-map").await;
    assert_eq!(
        g.status,
        StatusCode::SERVICE_UNAVAILABLE,
        "got: {}",
        body_preview(&g)
    );
    let detail = g.body.get("detail").and_then(|v| v.as_str()).unwrap_or("");
    assert!(detail.contains("has not run"), "got: {}", body_preview(&g));
    risk_map_restore(&dir, prev);
}

#[tokio::test]
async fn risk_map_shape_and_delta() {
    let s1 = snap_doc("2026-10-26T06:00:00Z", &[("France", 80.0), ("Paris", 70.0)]);
    let s2 = snap_doc("2026-11-02T06:00:00Z", &[("France", 86.8), ("Kyiv", 74.5)]);
    let (dir, prev) = risk_map_fixture(&[
        ("snapshot-20261026-060000.json", &s1),
        ("snapshot-20261102-060000.json", &s2),
    ]);
    let g = get("/api/risk-map").await;
    assert_eq!(g.status, StatusCode::OK, "got: {}", body_preview(&g));
    assert_eq!(
        g.body.get("count").and_then(|v| v.as_u64()),
        Some(2),
        "got: {}",
        body_preview(&g)
    );
    assert_eq!(
        g.body.get("generated_at").and_then(|v| v.as_str()),
        Some("2026-11-02T06:00:00Z")
    );
    assert!(
        g.body.get("caveat").and_then(|v| v.as_str()).is_some(),
        "caveat present"
    );
    let places = g.body.get("places").and_then(|v| v.as_array()).unwrap();
    let fr = places.iter().find(|p| p["name"] == "France").unwrap();
    assert_eq!(fr["score"].as_f64(), Some(86.8));
    assert!((fr["delta"].as_f64().unwrap() - 6.8).abs() < 0.01);
    assert!(fr["families"].as_array().unwrap().len() == 1);
    let ky = places.iter().find(|p| p["name"] == "Kyiv").unwrap();
    assert!(ky["delta"].is_null(), "new place: null delta");
    risk_map_restore(&dir, prev);
}

// ---------------------------------------------------------------------------
// /api/prices contract
// ---------------------------------------------------------------------------

#[tokio::test]
async fn prices_default_shape_or_degraded() {
    let g = get("/api/prices").await;
    match g.status {
        StatusCode::OK => {
            let quotes = g
                .body
                .get("quotes")
                .and_then(|v| v.as_array())
                .expect("quotes array");
            assert!(
                quotes.len() >= 20,
                "default list should return 20+ quotes, got: {}",
                body_preview(&g)
            );
            for q in quotes {
                assert!(q.get("symbol").and_then(|v| v.as_str()).is_some());
                assert!(q.get("price").and_then(|v| v.as_f64()).is_some());
                assert!(q.get("spark").and_then(|v| v.as_array()).is_some());
            }
            assert!(g.body.get("missing").and_then(|v| v.as_array()).is_some());
            let source = g.body.get("source").and_then(|v| v.as_str()).unwrap_or("");
            assert!(source.contains("delayed"), "got: {}", body_preview(&g));
        }
        StatusCode::BAD_GATEWAY => {
            assert!(g.body.get("error").is_some(), "got: {}", body_preview(&g));
            assert!(g.body.get("detail").is_some(), "got: {}", body_preview(&g));
        }
        s => panic!("unexpected status {s}, got: {}", body_preview(&g)),
    }
}

#[tokio::test]
async fn prices_custom_symbols_and_missing() {
    let g = get("/api/prices?symbols=AAPL,ZZZZQQQNOTREAL").await;
    match g.status {
        StatusCode::OK => {
            let quotes = g.body.get("quotes").and_then(|v| v.as_array()).unwrap();
            assert!(
                quotes
                    .iter()
                    .any(|q| q.get("symbol").and_then(|v| v.as_str()) == Some("AAPL")),
                "got: {}",
                body_preview(&g)
            );
            let missing = g.body.get("missing").and_then(|v| v.as_array()).unwrap();
            assert!(
                missing
                    .iter()
                    .any(|m| m.get("symbol").and_then(|v| v.as_str()) == Some("ZZZZQQQNOTREAL")),
                "bad symbol must be reported in missing[], got: {}",
                body_preview(&g)
            );
        }
        StatusCode::BAD_GATEWAY => {
            assert!(g.body.get("error").is_some(), "got: {}", body_preview(&g));
        }
        s => panic!("unexpected status {s}, got: {}", body_preview(&g)),
    }
}

#[tokio::test]
async fn prices_rejects_too_many_symbols() {
    let many = (0..41)
        .map(|i| format!("S{i}"))
        .collect::<Vec<_>>()
        .join(",");
    let g = get(&format!("/api/prices?symbols={many}")).await;
    assert_eq!(
        g.status,
        StatusCode::BAD_REQUEST,
        "got: {}",
        body_preview(&g)
    );
}
#[tokio::test]
async fn prices_blank_symbols_uses_default_list() {
    // Pure whitespace trims to empty -> default watchlist (matches the UI's
    // "blank = watchlist" placeholder).
    let g = get("/api/prices?symbols=%20").await;
    match g.status {
        StatusCode::OK => {
            let quotes = g.body.get("quotes").and_then(|v| v.as_array()).unwrap();
            assert!(
                quotes.len() >= 20,
                "blank should serve the default list, got: {}",
                body_preview(&g)
            );
        }
        StatusCode::BAD_GATEWAY => {
            assert!(g.body.get("error").is_some(), "got: {}", body_preview(&g));
        }
        s => panic!("unexpected status {s}, got: {}", body_preview(&g)),
    }
}

#[tokio::test]
async fn sanctions_bad_severity_is_400() {
    let g = get("/api/sanctions?country=Iran&severity=bogus").await;
    assert_eq!(
        g.status,
        StatusCode::BAD_REQUEST,
        "got: {}",
        body_preview(&g)
    );
}

#[tokio::test]
async fn sanctions_unknown_country_is_404() {
    let g = get("/api/sanctions?country=Narnia").await;
    assert_eq!(g.status, StatusCode::NOT_FOUND, "got: {}", body_preview(&g));
}

#[tokio::test]
async fn sanctions_country_shape_and_severity_filter() {
    // Live scan; upstreams may be down in the sandbox — the contract is
    // the shape and the filter behavior on whatever items exist.
    let g = get("/api/sanctions?country=Cuba").await;
    assert_eq!(g.status, StatusCode::OK, "got: {}", body_preview(&g));
    assert_json(&g, "/api/sanctions");
    assert_eq!(g.body["mode"], "country");
    assert_eq!(g.body["country"], "Cuba");
    let countries = g.body.get("countries").and_then(|v| v.as_array()).unwrap();
    assert_eq!(countries.len(), 1);
    assert_eq!(
        countries[0].get("country").and_then(|v| v.as_str()),
        Some("Cuba")
    );
    let disc = g.body.get("disclaimer").and_then(|v| v.as_str()).unwrap();
    assert!(
        disc.contains("NOT VERIFIED DESIGNATIONS"),
        "disclaimer states non-verified status"
    );
    assert!(disc.contains("OFAC"), "disclaimer names OFAC");
    let items = g.body.get("items").and_then(|v| v.as_array()).unwrap();
    for it in items {
        let sev = it.get("severity").and_then(|v| v.as_str()).unwrap_or("");
        assert!(
            ["critical", "high", "medium"].contains(&sev),
            "bad severity {sev}"
        );
        assert_eq!(
            it.get("country").and_then(|v| v.as_str()),
            Some("Cuba"),
            "country-mode items carry the country"
        );
        assert!(
            it.get("matched_keywords")
                .and_then(|v| v.as_array())
                .is_some(),
            "items carry matched_keywords"
        );
    }
    // severity=critical is a subset of severity=all.
    let crit = get("/api/sanctions?country=Cuba&severity=critical").await;
    assert_eq!(crit.status, StatusCode::OK, "got: {}", body_preview(&crit));
    let crit_items = crit.body.get("items").and_then(|v| v.as_array()).unwrap();
    assert!(
        crit_items.len() <= items.len(),
        "severity=critical is a subset of all"
    );
    for it in crit_items {
        assert_eq!(
            it.get("severity").and_then(|v| v.as_str()),
            Some("critical"),
            "filter keeps only critical"
        );
    }
}

#[tokio::test]
async fn storms_shape() {
    // Feed contract: 200 with the payload, or 502 with {error, detail}.
    let g = get("/api/storms").await;
    assert_feed_contract(&g, "/api/storms");
    if g.status == StatusCode::OK {
        let none_active = g.body.get("none_active").and_then(|v| v.as_bool()).unwrap();
        let active = g.body.get("active").and_then(|v| v.as_array()).unwrap();
        let count = g.body.get("count").and_then(|v| v.as_u64()).unwrap() as usize;
        assert_eq!(count, active.len(), "count matches active array");
        assert_eq!(none_active, active.is_empty(), "none_active is consistent");
        for s in active {
            for f in ["name", "basin", "category", "atcf_id"] {
                assert!(
                    s.get(f).and_then(|v| v.as_str()).is_some(),
                    "storm carries {f}"
                );
            }
            assert!(s.get("lat").and_then(|v| v.as_f64()).is_some());
            assert!(s.get("lon").and_then(|v| v.as_f64()).is_some());
            let cat = s.get("category").and_then(|v| v.as_str()).unwrap_or("");
            assert!(
                [
                    "TD", "TS", "Cat1", "Cat2", "Cat3", "Cat4", "Cat5", "PT", "HU"
                ]
                .contains(&cat),
                "bad category {cat}"
            );
        }
    }
}
