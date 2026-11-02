//! Feed modules: fetch from public upstreams, normalize to a common
//! struct, serialize to JSON.
//!
//! Feeds prefer keyless public APIs; where a source needs credentials the
//! module reads them from [`crate::config::Config`] and degrades gracefully
//! when unset (keyless fallback or 503 with {"error","detail"}), mirroring
//! OSIRIS's own conventions. Each module exposes:
//! - `TTL`: per-feed cache TTL (`std::time::Duration`)
//! - `fetch(...)`: network fetch + parse + normalize (pure `parse_*`
//!   functions are separated out so benchmarks and tests exercise them
//!   without networking)
//! - a `*Response` struct that is the exact JSON shape served.

pub mod bluesky;
pub mod cameras;
pub mod chokepoints;
pub mod conflicts;
pub mod cyber;
pub mod earnings;
pub mod econ_calendar;
pub mod fires;
pub mod flights;
pub mod gnews;
pub mod helio;
pub mod markets;
pub mod mastodon;
pub mod n2yo;
pub mod news;
pub mod nuclear;
pub mod prices;
pub mod quakes;
pub mod risk_map;
pub mod rss;
pub mod satellites;
pub mod telegram;
pub mod threatfox;
pub mod vessels;
pub mod weather;

use std::time::Duration;

use axum::{
    http::{HeaderName, StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};

use crate::error::{ApiError, FeedError};

/// GET `url` and deserialize the JSON body, mapping transport errors into
/// `FeedError` with the feed name attached.
pub async fn get_json<T>(
    client: &reqwest::Client,
    feed: &'static str,
    url: &str,
) -> Result<T, FeedError>
where
    T: serde::de::DeserializeOwned,
{
    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| FeedError::new(feed, format!("request failed: {e}")))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(FeedError::new(feed, format!("upstream status {status}")));
    }
    resp.json::<T>()
        .await
        .map_err(|e| FeedError::new(feed, format!("decode failed: {e}")))
}

/// GET `url` and return the raw text body (for non-JSON upstreams like TLE).
pub async fn get_text(
    client: &reqwest::Client,
    feed: &'static str,
    url: &str,
) -> Result<String, FeedError> {
    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| FeedError::new(feed, format!("request failed: {e}")))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(FeedError::new(feed, format!("upstream status {status}")));
    }
    resp.text()
        .await
        .map_err(|e| FeedError::new(feed, format!("read body failed: {e}")))
}

/// POST a JSON `body` and return the raw text response.
pub async fn post_text(
    client: &reqwest::Client,
    feed: &'static str,
    url: &str,
    body: &serde_json::Value,
) -> Result<String, FeedError> {
    let resp = client
        .post(url)
        .json(body)
        .send()
        .await
        .map_err(|e| FeedError::new(feed, format!("request failed: {e}")))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(FeedError::new(feed, format!("upstream status {status}")));
    }
    resp.text()
        .await
        .map_err(|e| FeedError::new(feed, format!("read body failed: {e}")))
}

// ---------------------------------------------------------------------------
// Shared handler plumbing for the Phase-1 (keyed) feeds.
//
// `routes.rs` keeps its own private copies for the original six feeds (that
// file is owned by the parent orchestrator); the new feed modules use these
// public twins so their `handler`s stay one-liners.
// ---------------------------------------------------------------------------

/// Fetch the normalized response (or reuse the cached bytes), serialize
/// once, store, and return the bytes plus a cache-hit flag.
pub async fn cached_bytes<T, Fut>(
    state: &crate::routes::AppState,
    key: &str,
    ttl: Duration,
    fetch: impl FnOnce() -> Fut,
) -> Result<(Vec<u8>, bool), ApiError>
where
    T: serde::Serialize,
    Fut: std::future::Future<Output = Result<T, FeedError>>,
{
    if let Some(body) = state.cache.get(key) {
        return Ok((body, true));
    }
    let value = fetch().await?;
    let body = serde_json::to_vec(&value)
        .map_err(|e| ApiError::bad_gateway("serialize failed", e.to_string()))?;
    state.cache.put(key.to_string(), body.clone(), ttl);
    Ok((body, false))
}

/// Wrap cached bytes as a JSON response with the `x-cache` hit/miss header.
pub fn json_bytes(body: Vec<u8>, hit: bool) -> Response {
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "application/json"),
            (
                HeaderName::from_static("x-cache"),
                if hit { "HIT" } else { "MISS" },
            ),
        ],
        body,
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// Phase-1 route table: ONLY the new feeds. The original six routes stay in
// `routes.rs::app`; the parent orchestrator merges this router (or mounts
// the handlers individually) when wiring the server.
// ---------------------------------------------------------------------------

/// Router aggregating the Phase-1 feed routes. State type matches
/// `routes::app` so the two routers compose.
pub fn routes() -> axum::Router<crate::routes::AppState> {
    axum::Router::new()
        .route("/api/fires", get(fires::handler))
        .route("/api/satellites/catalog", get(n2yo::handler))
        .route("/api/vessels", get(vessels::handler))
        .route("/api/cyber/threats", get(cyber::handler))
        .route("/api/threatfox", get(threatfox::handler))
        .route("/api/telegram", get(telegram::handler))
        .route("/api/markets", get(markets::handler))
        .route("/api/prices", get(prices::handler))
        .route("/api/nuclear", get(nuclear::handler))
        .route("/api/chokepoints", get(chokepoints::handler))
        .route("/api/risk-map", get(risk_map::handler))
        .route("/api/conflicts", get(conflicts::handler))
        .route("/api/space-weather", get(helio::handler))
        .route("/api/bluesky", get(bluesky::handler))
        .route("/api/mastodon", get(mastodon::handler))
        .route("/api/rss", get(rss::handler))
        .route("/api/gnews", get(gnews::handler))
        .route("/api/calendar", get(econ_calendar::handler))
        .route("/api/earnings", get(earnings::handler))
}
