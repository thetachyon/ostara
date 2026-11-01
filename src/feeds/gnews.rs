//! Google News RSS feed (keyless) — traditional-media leg.
//!
//! Upstream: `https://news.google.com/rss/search?q=<query>&hl=en-US&gl=US&ceid=US:en`
//! — no auth, but it requires a browser-like User-Agent (verified live
//! 2026-11-01; a bare request returns an empty body). Parsed with `feed-rs`
//! into the same normalized `{outlet, title, link, published, summary}`
//! shape as the `rss` feed.
//!
//! Notes: `q` doubles as the country-queryable path (e.g. `?q=protest+France`)
//! for Phase-2 unrest scoring. Item links are Google News redirect URLs
//! (`news.google.com/rss/articles/…`) — passed through as-is, never rewritten.

use std::time::Duration;

use axum::{
    extract::{Query, State},
    response::IntoResponse,
};
use serde::{Deserialize, Serialize};

use crate::{
    error::{ApiError, FeedError},
    feeds::{json_bytes, rss::RssItem},
    routes::AppState,
    time::now_iso8601,
};

pub const TTL: Duration = Duration::from_secs(300);
pub const FEED: &str = "gnews";

const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36";

/// Minimal percent-encoding for query params (same shape as news.rs's).
fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Normalized search snapshot served by `/api/gnews`.
#[derive(Debug, Serialize, Deserialize)]
pub struct GnewsResponse {
    pub query: String,
    pub count: usize,
    pub generated_at: String,
    pub items: Vec<RssItem>,
}

pub async fn fetch(
    client: &reqwest::Client,
    query: &str,
    limit: u32,
) -> Result<GnewsResponse, FeedError> {
    let limit = limit.clamp(1, 100) as usize;
    let url = format!(
        "https://news.google.com/rss/search?q={}&hl=en-US&gl=US&ceid=US:en",
        url_encode(query)
    );
    let xml = client
        .get(&url)
        .header(reqwest::header::USER_AGENT, USER_AGENT)
        .send()
        .await
        .map_err(|e| FeedError::new(FEED, format!("request failed: {e}")))?
        .error_for_status()
        .map_err(|e| FeedError::new(FEED, format!("upstream status: {e}")))?
        .text()
        .await
        .map_err(|e| FeedError::new(FEED, format!("read body failed: {e}")))?;
    if xml.trim().is_empty() {
        return Err(FeedError::new(
            FEED,
            "empty response — Google News likely blocked the request",
        ));
    }
    let mut items = crate::feeds::rss::parse_feed(&xml, "Google News")?;
    items.truncate(limit);
    Ok(GnewsResponse {
        query: query.to_string(),
        count: items.len(),
        generated_at: now_iso8601(),
        items,
    })
}

/// Keyless liveness probe: one tiny search.
pub async fn probe_detail(client: &reqwest::Client) -> Result<String, FeedError> {
    let r = fetch(client, "news", 1).await?;
    Ok(format!("{} items (q=news, limit=1)", r.count))
}

// ---------------------------------------------------------------------------
// GET /api/gnews?q=&limit=
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct GnewsQuery {
    pub q: Option<String>,
    pub limit: Option<u32>,
}

pub async fn handler(
    State(state): State<AppState>,
    Query(q): Query<GnewsQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let query = q.q.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    let query = match query {
        Some(q) => q,
        None => {
            return Err(ApiError::bad_request(
                "missing q",
                "query param q is required, e.g. ?q=protest",
            ));
        }
    };
    let limit = q.limit.unwrap_or(25).clamp(1, 100);
    let key = format!("gnews:q={query}:limit={limit}");
    if let Some(body) = state.cache.get(&key) {
        return Ok(json_bytes(body, true));
    }
    let client = state.client.clone();
    // Hand-rolled get→fetch→put so an upstream failure surfaces as 503
    // rather than the 502 that FeedError→ApiError conversion would give.
    let resp = fetch(&client, &query, limit)
        .await
        .map_err(|e| ApiError::service_unavailable("gnews upstream failed", e.detail))?;
    let body = serde_json::to_vec(&resp)
        .map_err(|e| ApiError::bad_gateway("serialize failed", e.to_string()))?;
    state.cache.put(key, body.clone(), TTL);
    Ok(json_bytes(body, false))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("../../benches/fixtures/gnews_search.xml");

    #[test]
    fn parses_fixture() {
        let items = crate::feeds::rss::parse_feed(FIXTURE, "Google News").expect("fixture parses");
        assert!(!items.is_empty());
        for i in &items {
            assert_eq!(i.outlet, "Google News");
            assert!(!i.title.is_empty());
            assert!(i.link.contains("news.google.com"));
        }
    }

    #[test]
    fn url_encode_cases() {
        assert_eq!(url_encode("protest France"), "protest+France");
        assert_eq!(url_encode("a&b"), "a%26b");
    }
}
