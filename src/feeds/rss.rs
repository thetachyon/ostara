//! Traditional-media RSS aggregator (keyless).
//!
//! Curated outlet list, each RSS URL verified live 2026-11-01 (dead ones
//! dropped, per the brief):
//! - BBC World — `https://feeds.bbci.co.uk/news/world/rss.xml`
//! - Al Jazeera — `https://www.aljazeera.com/xml/rss/all.xml`
//! - Deutsche Welle — `https://rss.dw.com/rdf/rss-en-all`
//! - France 24 — `https://www.france24.com/en/rss`
//!
//! Deliberately excluded (verified dead 2026-11-01): Reuters (RSS endpoints
//! return 401), AP (403 / no public RSS offering). X/Twitter is paid-API
//! only and is excluded everywhere in this project.
//!
//! Parsed with `feed-rs` (handles RSS 2.0 / RDF / Atom uniformly).
//! Scoring-friendly: outlet, canonical link, and ISO-8601 published
//! timestamps are normalized for Phase-2 unrest scoring.

use std::time::Duration;

use axum::{
    extract::{Query, State},
    response::IntoResponse,
};
use serde::{Deserialize, Serialize};

use crate::{
    error::{ApiError, FeedError},
    feeds::json_bytes,
    routes::AppState,
    time::now_iso8601,
};

pub const TTL: Duration = Duration::from_secs(600);
pub const FEED: &str = "rss";

const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36";

/// Curated outlets: (id, display name, feed URL). Every URL verified live
/// 2026-11-01; the list is the allowlist for `?outlet=`.
pub const OUTLETS: &[(&str, &str, &str)] = &[
    (
        "bbc",
        "BBC World",
        "https://feeds.bbci.co.uk/news/world/rss.xml",
    ),
    (
        "aljazeera",
        "Al Jazeera",
        "https://www.aljazeera.com/xml/rss/all.xml",
    ),
    ("dw", "Deutsche Welle", "https://rss.dw.com/rdf/rss-en-all"),
    ("france24", "France 24", "https://www.france24.com/en/rss"),
];

/// One normalized news item (shared shape with the gnews feed).
#[derive(Debug, Serialize, Deserialize)]
pub struct RssItem {
    pub outlet: String,
    pub title: String,
    pub link: String,
    pub published: Option<String>,
    pub summary: Option<String>,
}

/// Normalized outlet snapshot served by `/api/rss`.
#[derive(Debug, Serialize, Deserialize)]
pub struct RssResponse {
    pub outlet: String,
    pub count: usize,
    pub generated_at: String,
    pub items: Vec<RssItem>,
}

fn outlet_by_id(id: &str) -> Option<(&'static str, &'static str, &'static str)> {
    OUTLETS.iter().find(|(oid, _, _)| *oid == id).copied()
}

fn truncate(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

/// Parse one outlet's feed XML into normalized items. Pure: no networking.
pub fn parse_feed(xml: &str, outlet_name: &str) -> Result<Vec<RssItem>, FeedError> {
    let feed = feed_rs::parser::parse(xml.as_bytes())
        .map_err(|e| FeedError::new(FEED, format!("feed parse failed: {e}")))?;
    let items = feed
        .entries
        .into_iter()
        .map(|e| {
            let title = e
                .title
                .map(|t| t.content.trim().to_string())
                .filter(|t| !t.is_empty())
                .unwrap_or_else(|| "(untitled)".to_string());
            let link = e
                .links
                .iter()
                .find(|l| !l.href.is_empty())
                .map(|l| l.href.clone())
                .unwrap_or_default();
            let published = e.published.map(|d| d.to_rfc3339());
            let summary = e
                .summary
                .map(|t| truncate(&t.content, 400))
                .filter(|s| !s.is_empty());
            RssItem {
                outlet: outlet_name.to_string(),
                title,
                link,
                published,
                summary,
            }
        })
        .collect();
    Ok(items)
}

async fn fetch_outlet(client: &reqwest::Client, id: &str) -> Result<Vec<RssItem>, FeedError> {
    let (_, name, url) =
        outlet_by_id(id).ok_or_else(|| FeedError::new(FEED, format!("unknown outlet: {id}")))?;
    let xml = client
        .get(url)
        .header(reqwest::header::USER_AGENT, USER_AGENT)
        .send()
        .await
        .map_err(|e| FeedError::new(FEED, format!("request failed: {e}")))?
        .error_for_status()
        .map_err(|e| FeedError::new(FEED, format!("upstream status: {e}")))?
        .text()
        .await
        .map_err(|e| FeedError::new(FEED, format!("read body failed: {e}")))?;
    parse_feed(&xml, name)
}

/// Fetch one outlet (`Some(id)`) or all outlets merged newest-first (`None`).
pub async fn fetch(
    client: &reqwest::Client,
    outlet: Option<&str>,
    limit: u32,
) -> Result<RssResponse, FeedError> {
    let limit = limit.clamp(1, 100) as usize;
    let mut items = Vec::new();
    match outlet {
        Some(id) => {
            items.extend(fetch_outlet(client, id).await?);
        }
        None => {
            for (id, _, _) in OUTLETS {
                match fetch_outlet(client, id).await {
                    Ok(mut v) => items.append(&mut v),
                    Err(e) => tracing::warn!(outlet = id, "rss outlet failed: {}", e.detail),
                }
            }
            // Newest first; items without a timestamp sort last, stable.
            items.sort_by(|a, b| b.published.cmp(&a.published));
        }
    }
    items.truncate(limit);
    Ok(RssResponse {
        outlet: outlet.unwrap_or("all").to_string(),
        count: items.len(),
        generated_at: now_iso8601(),
        items,
    })
}

/// Keyless liveness probe: BBC World, one item.
pub async fn probe_detail(client: &reqwest::Client) -> Result<String, FeedError> {
    let r = fetch(client, Some("bbc"), 1).await?;
    Ok(format!("{} items (BBC World)", r.count))
}

// ---------------------------------------------------------------------------
// GET /api/rss?outlet=&limit=
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct RssQuery {
    pub outlet: Option<String>,
    pub limit: Option<u32>,
}

pub async fn handler(
    State(state): State<AppState>,
    Query(q): Query<RssQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let outlet = q
        .outlet
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty());
    if let Some(id) = outlet.as_deref()
        && id != "all"
        && outlet_by_id(id).is_none()
    {
        return Err(ApiError::bad_request(
            "unknown outlet",
            "outlet must be one of: all, bbc, aljazeera, dw, france24",
        ));
    }
    let effective = match outlet.as_deref() {
        Some("all") | None => None,
        Some(id) => Some(id),
    };
    let limit = q.limit.unwrap_or(25).clamp(1, 100);
    let key = format!("rss:outlet={}:limit={limit}", effective.unwrap_or("all"));
    if let Some(body) = state.cache.get(&key) {
        return Ok(json_bytes(body, true));
    }
    let client = state.client.clone();
    let owned_outlet = effective.map(str::to_string);
    // Hand-rolled get→fetch→put so an upstream failure surfaces as 503
    // rather than the 502 that FeedError→ApiError conversion would give.
    let resp = fetch(&client, owned_outlet.as_deref(), limit)
        .await
        .map_err(|e| ApiError::service_unavailable("rss upstream failed", e.detail))?;
    let body = serde_json::to_vec(&resp)
        .map_err(|e| ApiError::bad_gateway("serialize failed", e.to_string()))?;
    state.cache.put(key, body.clone(), TTL);
    Ok(json_bytes(body, false))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("../../benches/fixtures/rss_bbc.xml");

    #[test]
    fn parses_bbc_fixture() {
        let items = parse_feed(FIXTURE, "BBC World").expect("fixture parses");
        assert!(!items.is_empty());
        for i in &items {
            assert_eq!(i.outlet, "BBC World");
            assert!(!i.title.is_empty());
            assert!(i.link.starts_with("http"));
        }
    }

    #[test]
    fn outlet_allowlist() {
        assert!(outlet_by_id("bbc").is_some());
        assert!(outlet_by_id("reuters").is_none());
        assert_eq!(OUTLETS.len(), 4);
    }

    #[test]
    fn malformed_xml_is_error_not_panic() {
        assert!(parse_feed("<<< not xml >>>", "x").is_err());
        assert!(parse_feed("", "x").is_err());
    }

    #[test]
    fn truncate_respects_char_boundaries() {
        // "héllo " is 7 bytes; truncation never splits the 2-byte é.
        assert_eq!(truncate("héllo world foo", 7), "héllo …");
        assert_eq!(truncate("short", 100), "short");
    }
}
