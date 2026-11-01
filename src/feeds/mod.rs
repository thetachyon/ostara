//! Feed modules: fetch from keyless public upstreams, normalize to a common
//! struct, serialize to JSON.
//!
//! OSIRIS's philosophy is "no keys required" — every feed here uses a
//! keyless public API. Each module exposes:
//! - `TTL`: per-feed cache TTL (`std::time::Duration`)
//! - `fetch(...)`: network fetch + parse + normalize (pure `parse_*`
//!   functions are separated out so benchmarks and tests exercise them
//!   without networking)
//! - a `*Response` struct that is the exact JSON shape served.

pub mod flights;
pub mod news;
pub mod quakes;
pub mod satellites;
pub mod weather;

use crate::error::FeedError;

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
