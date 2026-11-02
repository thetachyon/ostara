//! Bluesky social-media feed (keyless).
//!
//! Upstream: `https://api.bsky.app` — the public AppView API needs no auth
//! for `app.bsky.feed.searchPosts` (verified live 2026-11-01). This is the
//! keyless social-media leg of the EMBERS-inspired media expansion:
//! timestamps, author handles, and engagement counts are normalized so
//! Phase-2 unrest scoring can consume them directly.
//!
//! Responsible use: one upstream request per query per 300s TTL through the
//! shared cache; no auth, no private data — only public posts.

use std::time::Duration;

use axum::{
    extract::{Query, State},
    response::IntoResponse,
};
use serde::{Deserialize, Serialize};

use crate::{
    error::{ApiError, FeedError},
    feeds::{get_json, json_bytes},
    routes::AppState,
    time::now_iso8601,
};

pub const TTL: Duration = Duration::from_secs(300);
pub const FEED: &str = "bluesky";

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

// --- upstream shapes (only the fields we normalize) ------------------------

#[derive(Debug, Deserialize)]
struct SearchResponse {
    #[serde(default)]
    posts: Vec<PostView>,
}

#[derive(Debug, Deserialize)]
struct PostView {
    uri: String,
    #[allow(dead_code)]
    cid: String,
    author: Author,
    record: Record,
    #[serde(default)]
    like_count: Option<i64>,
    #[serde(default)]
    repost_count: Option<i64>,
    #[serde(default)]
    reply_count: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct Author {
    handle: String,
    #[serde(default)]
    display_name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Record {
    #[serde(default)]
    text: String,
    #[serde(rename = "createdAt", default)]
    created_at: String,
}

// --- normalized shapes -----------------------------------------------------

/// One normalized Bluesky post.
#[derive(Debug, Serialize, Deserialize)]
pub struct BlueskyPost {
    pub uri: String,
    pub url: String,
    pub author_handle: String,
    pub author_display: Option<String>,
    pub text: String,
    pub created_at: String,
    pub like_count: Option<i64>,
    pub repost_count: Option<i64>,
    pub reply_count: Option<i64>,
}

/// Normalized search snapshot served by `/api/bluesky`.
#[derive(Debug, Serialize, Deserialize)]
pub struct BlueskyResponse {
    pub query: String,
    pub count: usize,
    pub generated_at: String,
    pub posts: Vec<BlueskyPost>,
}

/// `at://did:plc:xyz/app.bsky.feed.post/3mzbxicsfkc2j` → post rkey + bsky.app URL.
fn post_url(uri: &str, handle: &str) -> String {
    match uri.rsplit('/').next() {
        Some(rkey) if !rkey.is_empty() => {
            format!("https://bsky.app/profile/{handle}/post/{rkey}")
        }
        _ => format!("https://bsky.app/profile/{handle}"),
    }
}

fn normalize(p: PostView) -> BlueskyPost {
    let url = post_url(&p.uri, &p.author.handle);
    BlueskyPost {
        uri: p.uri,
        url,
        author_handle: p.author.handle,
        author_display: p.author.display_name,
        text: p.record.text,
        created_at: p.record.created_at,
        like_count: p.like_count,
        repost_count: p.repost_count,
        reply_count: p.reply_count,
    }
}

pub async fn fetch(
    client: &reqwest::Client,
    query: &str,
    limit: u32,
) -> Result<BlueskyResponse, FeedError> {
    fetch_window(client, query, limit, None, None).await
}

/// Windowed search: `since`/`until` are RFC-3339 timestamps bounding post
/// creation time (honored by the AppView; verified live 2026-11-01).
/// `None` leaves that side unbounded. Enables proper temporal baselines —
/// e.g. a 7-day recent window vs the prior 83 days — for scoring.
pub async fn fetch_window(
    client: &reqwest::Client,
    query: &str,
    limit: u32,
    since: Option<&str>,
    until: Option<&str>,
) -> Result<BlueskyResponse, FeedError> {
    let limit = limit.clamp(1, 100);
    let mut url = format!(
        "https://api.bsky.app/xrpc/app.bsky.feed.searchPosts?q={}&limit={limit}",
        url_encode(query)
    );
    if let Some(s) = since {
        url.push_str(&format!("&since={}", url_encode(s)));
    }
    if let Some(u) = until {
        url.push_str(&format!("&until={}", url_encode(u)));
    }
    let resp: SearchResponse = get_json(client, FEED, &url).await?;
    let posts: Vec<BlueskyPost> = resp.posts.into_iter().map(normalize).collect();
    Ok(BlueskyResponse {
        query: query.to_string(),
        count: posts.len(),
        generated_at: now_iso8601(),
        posts,
    })
}

/// Keyless liveness probe: one tiny search.
pub async fn probe_detail(client: &reqwest::Client) -> Result<String, FeedError> {
    let r = fetch(client, "news", 1).await?;
    Ok(format!("{} posts (q=news, limit=1)", r.count))
}

// ---------------------------------------------------------------------------
// GET /api/bluesky?q=&limit=
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct BlueskyQuery {
    pub q: Option<String>,
    pub limit: Option<u32>,
}

pub async fn handler(
    State(state): State<AppState>,
    Query(q): Query<BlueskyQuery>,
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
    let key = format!("bluesky:q={query}:limit={limit}");
    if let Some(body) = state.cache.get(&key) {
        return Ok(json_bytes(body, true));
    }
    let client = state.client.clone();
    // Hand-rolled get→fetch→put so an upstream failure surfaces as 503
    // (honest "Bluesky unreachable") rather than the 502 that
    // FeedError→ApiError conversion would give.
    let resp = fetch(&client, &query, limit)
        .await
        .map_err(|e| ApiError::service_unavailable("bluesky upstream failed", e.detail))?;
    let body = serde_json::to_vec(&resp)
        .map_err(|e| ApiError::bad_gateway("serialize failed", e.to_string()))?;
    state.cache.put(key, body.clone(), TTL);
    Ok(json_bytes(body, false))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("../../benches/fixtures/bluesky_search.json");

    #[test]
    fn parses_fixture() {
        let resp: SearchResponse = serde_json::from_str(FIXTURE).expect("fixture parses");
        assert_eq!(resp.posts.len(), 10);
        let p = normalize(resp.posts.into_iter().next().unwrap());
        assert!(!p.uri.is_empty());
        assert!(!p.author_handle.is_empty());
        assert!(p.url.starts_with("https://bsky.app/profile/"));
        assert!(p.url.contains("/post/"));
        assert!(!p.created_at.is_empty());
    }

    #[test]
    fn post_url_shapes() {
        assert_eq!(
            post_url(
                "at://did:plc:abc/app.bsky.feed.post/3mzbxicsfkc2j",
                "alice.test"
            ),
            "https://bsky.app/profile/alice.test/post/3mzbxicsfkc2j"
        );
        // Empty rkey falls back to the profile URL.
        assert_eq!(
            post_url("", "alice.test"),
            "https://bsky.app/profile/alice.test"
        );
    }

    #[test]
    fn url_encode_cases() {
        assert_eq!(url_encode("protest paris"), "protest+paris");
        assert_eq!(url_encode("a/b"), "a%2Fb");
    }
}
