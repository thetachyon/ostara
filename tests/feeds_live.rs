//! Live upstream tests: one per input source. Each hits the real upstream
//! over the network and asserts a 2xx response with the expected shape.
//!
//! KNOWN ENVIRONMENT QUIRKS (documented, not "fixed"): this sandbox's
//! egress proxy interacts badly with some upstreams, and the behavior
//! shifts over time:
//! - 2026-11-01 (earlier): reqwest TLS to `api.adsb.lol` and
//!   `api.gdeltproject.org` stalled at connect while `curl` succeeded.
//! - 2026-11-01 (later): `api.adsb.lol` works via both curl and reqwest;
//!   `api.gdeltproject.org` is unreachable via both (host-level, not a
//!   code bug); `celestrak.org` returns HTTP 200 via curl but reqwest
//!   fails with "error sending request" — a proxy/client interaction.
//!
//! These tests FAIL honestly in such environments: they are liveness
//! probes, and a failed probe is information. Do not #[ignore] them and
//! do not weaken the assertions to make them pass here.
//!
//! These tests need network and are slow/flaky by nature — that is the
//! point. `cargo test` runs them alongside everything else.

use std::future::Future;
use std::time::Duration;

use ostara::{error::FeedError, feeds};

fn live_client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent("ostara/0.1 live-tests")
        .timeout(Duration::from_secs(6))
        .connect_timeout(Duration::from_secs(6))
        .build()
        .expect("live client builds")
}

/// Run a feed fetch with a hard 6s budget. On any failure, panic with a
/// diagnostic naming the feed, the upstream host, and the failure mode.
/// Honest failure, never silent.
async fn probe<T, F, Fut>(feed: &str, upstream: &str, f: F) -> T
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<T, FeedError>>,
{
    match tokio::time::timeout(Duration::from_secs(6), f()).await {
        Err(_) => panic!(
            "LIVE-FAIL {feed} ({upstream}): timed out after 6s — upstream unreachable or connection stalled"
        ),
        Ok(Err(e)) => panic!("LIVE-FAIL {feed} ({upstream}): fetch failed: {}", e.detail),
        Ok(Ok(v)) => v,
    }
}

#[tokio::test]
async fn flights_live() {
    let client = live_client();
    let r = probe("flights", "api.adsb.lol", || {
        feeds::flights::fetch(&client, "mil", 5)
    })
    .await;
    assert_eq!(r.scope, "mil", "LIVE-FAIL flights: unexpected scope");
    assert!(
        !r.generated_at.is_empty(),
        "LIVE-FAIL flights: empty generated_at"
    );
    // Shape the UI renders: every aircraft carries the map fields.
    for f in &r.flights {
        assert!(!f.hex.is_empty(), "LIVE-FAIL flights: aircraft missing hex");
    }
}

#[tokio::test]
async fn quakes_live() {
    let client = live_client();
    let r = probe("quakes", "earthquake.usgs.gov", || {
        feeds::quakes::fetch(&client)
    })
    .await;
    assert!(
        !r.quakes.is_empty(),
        "LIVE-FAIL quakes: M4.5+ day feed returned zero quakes"
    );
    let q = &r.quakes[0];
    assert!(
        q.magnitude.is_some(),
        "LIVE-FAIL quakes: first quake missing magnitude"
    );
    assert!(
        q.place.is_some(),
        "LIVE-FAIL quakes: first quake missing place"
    );
}

#[tokio::test]
async fn weather_live() {
    let client = live_client();
    let r = probe("weather", "api.open-meteo.com", || {
        feeds::weather::fetch(&client, 40.71, -74.0)
    })
    .await;
    assert!(
        (r.latitude - 40.71).abs() < 0.5,
        "LIVE-FAIL weather: latitude {} not near requested 40.71",
        r.latitude
    );
    assert!(
        r.temperature_c.is_some(),
        "LIVE-FAIL weather: missing temperature_c"
    );
}

#[tokio::test]
async fn satellites_live() {
    let client = live_client();
    let r = probe("satellites", "celestrak.org", || {
        feeds::satellites::fetch(&client, 5)
    })
    .await;
    assert!(
        !r.satellites.is_empty(),
        "LIVE-FAIL satellites: stations group returned zero objects"
    );
    assert!(
        r.satellites[0].norad_id > 0,
        "LIVE-FAIL satellites: first object has invalid norad_id"
    );
}

#[tokio::test]
async fn news_live() {
    let client = live_client();
    let r = probe("news", "api.gdeltproject.org", || {
        feeds::news::fetch(&client, "earthquake", 5)
    })
    .await;
    assert_eq!(r.query, "earthquake", "LIVE-FAIL news: query not echoed");
    assert!(
        !r.generated_at.is_empty(),
        "LIVE-FAIL news: empty generated_at"
    );
    for a in &r.articles {
        assert!(
            !a.url.is_empty() && !a.title.is_empty(),
            "LIVE-FAIL news: article missing url/title"
        );
    }
}

#[tokio::test]
async fn cameras_live_spot_check() {
    // Spot-checks a fixed camera per category: the registry is static, so
    // liveness means the operators' image URLs still serve image bytes.
    // Same honesty rules as the other live tests.
    let client = live_client();
    let spots = [
        (
            "traffic",
            "https://dot511.nebraska.gov/images/vid-001077037-04.jpg",
        ),
        ("weather", "https://weathercam.digitraffic.fi/C0150301.jpg"),
        (
            "park",
            "https://www.nps.gov/featurecontent/ard/webcams/images/grcalarge.jpg",
        ),
    ];
    for (category, url) in spots {
        let outcome = tokio::time::timeout(Duration::from_secs(6), client.get(url).send()).await;
        let resp = match outcome {
            Err(_) => panic!(
                "LIVE-FAIL cameras ({category} spot-check): timed out after 6s fetching {url}"
            ),
            Ok(Err(e)) => {
                panic!("LIVE-FAIL cameras ({category} spot-check): request failed for {url}: {e}")
            }
            Ok(Ok(r)) => r,
        };
        assert!(
            resp.status().is_success(),
            "LIVE-FAIL cameras ({category} spot-check): {url} returned {}",
            resp.status()
        );
        let ct = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        assert!(
            ct.starts_with("image/"),
            "LIVE-FAIL cameras ({category} spot-check): {url} content-type is '{ct}', not image/*"
        );
    }
}
