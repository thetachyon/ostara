//! Mastodon social-media feed (keyless).
//!
//! Upstream: the public local timeline of a large instance. Verified live
//! 2026-11-01: `https://fosstodon.org/api/v1/timelines/public?local=true`
//! needs no auth. (mastodon.social / mastodon.online / mstdn.social all
//! require auth for the public timeline; `/api/v2/search` returns empty
//! results unauthenticated on every instance tested — so there is no
//! server-side search available keyless.)
//!
//! Honest design consequence: `?q=` filters the fetched timeline snapshot
//! client-side (case-insensitive substring on plain text), it is NOT a
//! server-side search. Documented here and in the API response.
//!
//! Scoring-friendly: timestamps, account, and engagement counts
//! (reblogs/favourites) are normalized for Phase-2 unrest scoring.

use std::time::Duration;

use axum::{
    extract::{Query, State},
    response::IntoResponse,
};
use scraper::{ElementRef, Html};
use serde::{Deserialize, Serialize};

use crate::{
    error::{ApiError, FeedError},
    feeds::{get_json, json_bytes},
    routes::AppState,
    time::now_iso8601,
};

pub const TTL: Duration = Duration::from_secs(300);
pub const FEED: &str = "mastodon";

/// Public instance whose local timeline is keyless (verified 2026-11-01).
const INSTANCE: &str = "https://fosstodon.org";

/// How many timeline items to pull before client-side filtering.
const FETCH_LIMIT: u32 = 40;

#[derive(Debug, Deserialize)]
struct Status {
    id: String,
    created_at: String,
    #[serde(default)]
    content: String,
    account: Account,
    #[serde(default)]
    reblogs_count: Option<i64>,
    #[serde(default)]
    favourites_count: Option<i64>,
    #[serde(default)]
    url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Account {
    #[serde(default)]
    acct: String,
    #[serde(default)]
    display_name: Option<String>,
}

/// One normalized Mastodon status.
#[derive(Debug, Serialize, Deserialize)]
pub struct MastodonPost {
    pub id: String,
    pub account: String,
    pub display_name: Option<String>,
    pub text: String,
    pub created_at: String,
    pub reblogs: Option<i64>,
    pub favourites: Option<i64>,
    pub url: Option<String>,
}

/// Normalized timeline snapshot served by `/api/mastodon`.
#[derive(Debug, Serialize, Deserialize)]
pub struct MastodonResponse {
    pub instance: String,
    pub query: Option<String>,
    pub count: usize,
    pub fetched: usize,
    pub generated_at: String,
    pub note: String,
    pub posts: Vec<MastodonPost>,
}

/// Strip HTML to plain text (status `content` is an HTML fragment).
/// Same walk pattern as telegram.rs `message_text`: `<br>`/`<p>` become
/// newlines, other tags are dropped.
fn html_to_text(html: &str) -> String {
    use scraper::node::Node;

    fn walk(el: ElementRef, out: &mut String) {
        for node in el.children() {
            match node.value() {
                Node::Text(t) => out.push_str(t),
                Node::Element(e) if e.name() == "br" => out.push('\n'),
                Node::Element(e) if e.name() == "p" => {
                    out.push('\n');
                    if let Some(child) = ElementRef::wrap(node) {
                        walk(child, out);
                    }
                }
                Node::Element(_) => {
                    if let Some(child) = ElementRef::wrap(node) {
                        walk(child, out);
                    }
                }
                _ => {}
            }
        }
    }

    if html.is_empty() {
        return String::new();
    }
    let frag = Html::parse_fragment(html);
    let mut out = String::new();
    walk(frag.root_element(), &mut out);
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn normalize(s: Status) -> MastodonPost {
    MastodonPost {
        id: s.id,
        account: s.account.acct,
        display_name: s.account.display_name.filter(|d| !d.is_empty()),
        text: html_to_text(&s.content),
        created_at: s.created_at,
        reblogs: s.reblogs_count,
        favourites: s.favourites_count,
        url: s.url,
    }
}

pub async fn fetch(
    client: &reqwest::Client,
    query: Option<&str>,
    limit: u32,
) -> Result<MastodonResponse, FeedError> {
    let limit = limit.clamp(1, FETCH_LIMIT);
    let url = format!("{INSTANCE}/api/v1/timelines/public?local=true&limit={FETCH_LIMIT}");
    let statuses: Vec<Status> = get_json(client, FEED, &url).await?;
    let fetched = statuses.len();
    let needle = query
        .map(str::trim)
        .filter(|q| !q.is_empty())
        .map(|q| q.to_lowercase());
    let posts: Vec<MastodonPost> = statuses
        .into_iter()
        .map(normalize)
        .filter(|p| match &needle {
            Some(n) => p.text.to_lowercase().contains(n),
            None => true,
        })
        .take(limit as usize)
        .collect();
    // Newest first (timeline arrives newest-first; keep that order).
    Ok(MastodonResponse {
        instance: INSTANCE.to_string(),
        query: query
            .map(str::trim)
            .filter(|q| !q.is_empty())
            .map(str::to_string),
        count: posts.len(),
        fetched,
        generated_at: now_iso8601(),
        note: "q filters the public-timeline snapshot client-side; no server-side search is available keyless".to_string(),
        posts,
    })
}

/// Keyless liveness probe: one tiny timeline pull.
pub async fn probe_detail(client: &reqwest::Client) -> Result<String, FeedError> {
    let r = fetch(client, None, 1).await?;
    Ok(format!(
        "{} statuses ({} public timeline)",
        r.fetched, INSTANCE
    ))
}

// ---------------------------------------------------------------------------
// GET /api/mastodon?q=&limit=
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct MastodonQuery {
    pub q: Option<String>,
    pub limit: Option<u32>,
}

pub async fn handler(
    State(state): State<AppState>,
    Query(q): Query<MastodonQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let query = q.q.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    let limit = q.limit.unwrap_or(20).clamp(1, FETCH_LIMIT);
    let key = format!(
        "mastodon:q={}:limit={limit}",
        query.as_deref().unwrap_or("")
    );
    if let Some(body) = state.cache.get(&key) {
        return Ok(json_bytes(body, true));
    }
    let client = state.client.clone();
    // Hand-rolled get→fetch→put so an upstream failure surfaces as 503
    // rather than the 502 that FeedError→ApiError conversion would give.
    let resp = fetch(&client, query.as_deref(), limit)
        .await
        .map_err(|e| ApiError::service_unavailable("mastodon upstream failed", e.detail))?;
    let body = serde_json::to_vec(&resp)
        .map_err(|e| ApiError::bad_gateway("serialize failed", e.to_string()))?;
    state.cache.put(key, body.clone(), TTL);
    Ok(json_bytes(body, false))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("../../benches/fixtures/mastodon_timeline.json");

    #[test]
    fn parses_fixture() {
        let statuses: Vec<Status> = serde_json::from_str(FIXTURE).expect("fixture parses");
        assert_eq!(statuses.len(), 10);
        let p = normalize(statuses.into_iter().next().unwrap());
        assert!(!p.id.is_empty());
        assert!(!p.account.is_empty());
        assert!(!p.created_at.is_empty());
        // No HTML tags leak into plain text.
        assert!(!p.text.contains('<'));
    }

    #[test]
    fn html_to_text_strips_tags() {
        let t = html_to_text("<p>Hello <a href=\"https://x\">world</a></p><p>second</p>");
        assert_eq!(t, "Hello world second");
        assert_eq!(html_to_text(""), "");
    }

    #[test]
    fn query_filter_is_case_insensitive() {
        let mk = |text: &str| MastodonPost {
            id: "1".into(),
            account: "a".into(),
            display_name: None,
            text: text.into(),
            created_at: "".into(),
            reblogs: None,
            favourites: None,
            url: None,
        };
        let posts = [mk("Protest in Paris"), mk("hello world")];
        let needle = "protest".to_lowercase();
        let kept: Vec<_> = posts
            .iter()
            .filter(|p| p.text.to_lowercase().contains(&needle))
            .collect();
        assert_eq!(kept.len(), 1);
    }
}
