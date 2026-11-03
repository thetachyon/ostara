//! Dark-web intel: onion search via Ahmia plus ransomware leak-site
//! monitoring via ransomware.live — all from clearnet, no Tor, no keys.
//!
//! ## What this is and is not
//!
//! OSTARA does not run Tor and never fetches `.onion` sites directly.
//! This module queries dark-web-*adjacent* sources that are reachable
//! over clearnet HTTPS:
//!
//! - **Ahmia** (ahmia.fi): a Tor hidden-service search engine with a
//!   clearnet web interface. `GET /api/darkweb/search?q=` scrapes its
//!   HTML results for onion URLs and titles. Results are unvetted
//!   onion-site listings.
//! - **ransomware.live** (v2 API, anonymous free tier, 1 req/min per
//!   endpoint): aggregates ransomware groups' leak-site posts
//!   (victim claims). `GET /api/darkweb/ransomware/victims` and
//!   `/api/darkweb/ransomware/groups`.
//!
//! ## Honest limitations (stated on every response)
//!
//! - Ransomware victim listings are **operator claims, not verified
//!   breaches**. Leak-site posts are written by the attackers and may
//!   be exaggerated, misattributed, or fabricated. Never treat a
//!   listing as proof of compromise.
//! - OSTARA never fetches, hosts, or displays stolen data content.
//! - Both upstreams may block datacenter IPs (bot protection). A
//!   blocked or failed source is reported as `ok: false` with a
//!   human-readable detail — never silently dropped, never faked.
//! - Coverage is whatever these two aggregators mirror; many leak
//!   sites are onion-only and Tor is out of scope.
//!
//! ## Endpoints
//!
//! - `GET /api/darkweb` — overview: per-source status, recent victim
//!   count, group count, the disclaimer, and the registry summary.
//! - `GET /api/darkweb/search?q=<query>&limit=` — Ahmia onion search.
//!   `q` required (min 2 chars), `limit` default 20, max 50.
//! - `GET /api/darkweb/ransomware/victims?country=<ISO2>&q=<text>&limit=`
//!   — victim claims. `country` uses ransomware.live's countryvictims
//!   endpoint; `q` uses searchvictims; neither uses the recent feed.
//!   `limit` default 25, max 100. At most one of `country`/`q`.
//! - `GET /api/darkweb/ransomware/groups` — group profiles.
//! - `GET /api/darkweb/sources` — the curated registry
//!   (`data/darkweb_sources.json`).
//!
//! ## Cache discipline
//!
//! ransomware.live's free tier allows 1 req/min per endpoint, so its
//! responses are cached 900s. Ahmia responses are cached 600s. Cache
//! keys are namespaced `darkweb:…`.

use std::time::Duration;

use axum::{
    Json, Router,
    extract::{Query, State},
    routing::get,
};
use serde::{Deserialize, Serialize};

use crate::{
    error::{ApiError, FeedError},
    routes::{AppState, cached_bytes},
    time::now_iso8601,
};

/// Stated on every dark-web response. Operator claims are not verified
/// breaches; OSTARA has no Tor access and never touches stolen data.
pub const DISCLAIMER: &str = "DARK-WEB INTEL, NOT VERIFIED BREACHES. Ransomware victim listings are OPERATOR CLAIMS posted by the attackers on their leak sites — they may be exaggerated, misattributed, or fabricated, and are never proof of compromise. Verify independently before acting. Ahmia results are unvetted Tor hidden-service listings. OSTARA does not run Tor, never fetches .onion sites directly, and never fetches, hosts, or displays stolen data content.";

static SOURCES_JSON: &str = include_str!("../data/darkweb_sources.json");

const FEED: &str = "darkweb";
const RW_BASE: &str = "https://api.ransomware.live";
const AHMIA_SEARCH: &str = "https://ahmia.fi/search/";
/// ransomware.live free tier: 1 req/min per endpoint — cache well above it.
const RW_TTL: Duration = Duration::from_secs(900);
pub(crate) const AHMIA_TTL: Duration = Duration::from_secs(600);

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Response shapes
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct SourceStatus {
    name: String,
    ok: bool,
    detail: String,
}

#[derive(Debug, Serialize)]
struct Overview {
    scanned_at: String,
    sources: Vec<SourceStatus>,
    ransomware_groups: Option<usize>,
    ransomware_victims_total: Option<usize>,
    note: String,
    disclaimer: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct OnionResult {
    pub(crate) title: String,
    pub(crate) url: String,
    pub(crate) snippet: String,
}

#[derive(Debug, Serialize)]
struct SearchResponse {
    query: String,
    count: usize,
    results: Vec<OnionResult>,
    source_ok: bool,
    source_detail: String,
    scanned_at: String,
    disclaimer: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct VictimClaim {
    victim: String,
    group: String,
    country: String,
    discovered: String,
    description: String,
    claim_url: String,
}

#[derive(Debug, Serialize)]
struct VictimsResponse {
    mode: String,
    count: usize,
    claims: Vec<VictimClaim>,
    source_ok: bool,
    source_detail: String,
    scanned_at: String,
    disclaimer: String,
}

#[derive(Debug, Serialize)]
struct GroupsResponse {
    count: usize,
    groups: Vec<serde_json::Value>,
    source_ok: bool,
    source_detail: String,
    scanned_at: String,
    disclaimer: String,
}

// ---------------------------------------------------------------------------
// Upstream clients
// ---------------------------------------------------------------------------

/// GET JSON from ransomware.live; a non-JSON or error response becomes a
/// FeedError with a human-readable detail (Cloudflare blocks surface as
/// decode failures — reported honestly, never silently dropped).
async fn rw_get_json<T>(client: &reqwest::Client, path: &str) -> Result<T, FeedError>
where
    T: serde::de::DeserializeOwned,
{
    let url = format!("{RW_BASE}{path}");
    let resp = client
        .get(&url)
        .header("User-Agent", "ostara/0.1 (dark-web intel; 1 req/min)")
        .send()
        .await
        .map_err(|e| FeedError::new(FEED, format!("ransomware.live request failed: {e}")))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(FeedError::new(
            FEED,
            format!("ransomware.live upstream status {status} for {path}"),
        ));
    }
    let text = resp
        .text()
        .await
        .map_err(|e| FeedError::new(FEED, format!("ransomware.live read failed: {e}")))?;
    serde_json::from_str::<T>(&text).map_err(|_| {
        FeedError::new(
            FEED,
            format!(
                "ransomware.live returned non-JSON for {path} (likely bot protection); not parsed"
            ),
        )
    })
}

/// Scrape Ahmia's HTML search results for onion URLs + titles.
/// Ahmia has no documented JSON API; this parses the result anchors.
fn parse_ahmia(html: &str, limit: usize) -> Vec<OnionResult> {
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    // Result anchors look like: <a href="http://xxxx.onion/...">title</a>
    // with an optional snippet nearby. Keep the parser conservative:
    // only http(s)://*.onion hrefs, title = anchor text stripped of tags.
    let mut rest = html;
    while let Some(a_start) = rest.find("<a ") {
        let tag = &rest[a_start..];
        let Some(tag_end) = tag.find('>') else { break };
        let tag_head = &tag[..tag_end];
        let href = match tag_head.find("href=\"") {
            Some(i) => {
                let after = &tag_head[i + 6..];
                match after.find('"') {
                    Some(j) => &after[..j],
                    None => {
                        rest = &tag[tag_end + 1..];
                        continue;
                    }
                }
            }
            None => {
                rest = &tag[tag_end + 1..];
                continue;
            }
        };
        let body_start = tag_end + 1;
        let body_end = tag[body_start..].find("</a>").map(|i| body_start + i);
        let Some(be) = body_end else {
            rest = &tag[body_start..];
            continue;
        };
        let raw_title = &tag[body_start..be];
        rest = &tag[be + 4..];
        if !(href.contains(".onion")) || !seen.insert(href.to_string()) {
            continue;
        }
        let title = strip_tags(raw_title);
        if title.is_empty() {
            continue;
        }
        out.push(OnionResult {
            title: truncate(&title, 160),
            url: href.to_string(),
            snippet: String::new(),
        });
        if out.len() >= limit {
            break;
        }
    }
    out
}

fn strip_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

pub(crate) async fn ahmia_search(
    client: &reqwest::Client,
    q: &str,
    limit: usize,
) -> Result<Vec<OnionResult>, FeedError> {
    let url = format!("{AHMIA_SEARCH}?q={}", urlencoding(q));
    let resp = client
        .get(&url)
        .header(
            "User-Agent",
            "Mozilla/5.0 (X11; Linux x86_64; rv:128.0) Gecko/20100101 Firefox/128.0",
        )
        .send()
        .await
        .map_err(|e| FeedError::new(FEED, format!("ahmia request failed: {e}")))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(FeedError::new(
            FEED,
            format!("ahmia upstream status {status}"),
        ));
    }
    let html = resp
        .text()
        .await
        .map_err(|e| FeedError::new(FEED, format!("ahmia read failed: {e}")))?;
    if html.len() < 500 {
        return Err(FeedError::new(
            FEED,
            "ahmia returned an empty/blocked page (likely bot protection)".to_string(),
        ));
    }
    Ok(parse_ahmia(&html, limit))
}

/// Minimal percent-encoding for a query string value.
fn urlencoding(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// ransomware.live response normalization
// ---------------------------------------------------------------------------

/// v2 victim objects carry several possible field spellings; normalize the
/// ones the OSINToolKit documents: post_title/victim, group_name,
/// country, discovered, description, claim_url/website.
fn normalize_victim(v: &serde_json::Value) -> VictimClaim {
    let s = |k: &str| {
        v.get(k)
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .trim()
            .to_string()
    };
    let victim = if s("post_title").is_empty() {
        s("victim")
    } else {
        s("post_title")
    };
    let claim_url = if s("claim_url").is_empty() {
        s("website")
    } else {
        s("claim_url")
    };
    VictimClaim {
        victim,
        group: s("group_name"),
        country: s("country"),
        discovered: s("discovered"),
        description: truncate(&s("description"), 280),
        claim_url,
    }
}

fn as_array(v: serde_json::Value) -> Vec<serde_json::Value> {
    match v {
        serde_json::Value::Array(a) => a,
        _ => Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

async fn overview(State(state): State<AppState>) -> Result<Json<Overview>, ApiError> {
    // Probe both upstreams (cached); a failed probe is a status, not an error.
    let info_key = "darkweb:rw:info";
    let info: Option<serde_json::Value> =
        match cached_bytes::<serde_json::Value, _>(&state, info_key, RW_TTL, || {
            let client = state.client.clone();
            async move { rw_get_json::<serde_json::Value>(&client, "/v2/info").await }
        })
        .await
        {
            Ok((body, _)) => serde_json::from_slice(&body).ok(),
            Err(_) => state
                .cache
                .get(info_key)
                .and_then(|b| serde_json::from_slice(&b).ok()),
        };
    let (groups_n, victims_n, rw_ok, rw_detail) = match &info {
        Some(v) => (
            v.get("groups").and_then(|x| x.as_u64()).map(|x| x as usize),
            v.get("victims")
                .and_then(|x| x.as_u64())
                .map(|x| x as usize),
            true,
            "ok".to_string(),
        ),
        None => (
            None,
            None,
            false,
            "unreachable or blocked (see /api/darkweb/sources)".to_string(),
        ),
    };

    // Ahmia probe: one cheap search; we only care whether it answers.
    let ahmia_key = "darkweb:ahmia:probe";
    let ahmia_ok = cached_bytes::<Vec<OnionResult>, _>(&state, ahmia_key, AHMIA_TTL, || {
        let client = state.client.clone();
        async move { ahmia_search(&client, "ransomware", 1).await }
    })
    .await
    .is_ok();

    Ok(Json(Overview {
        scanned_at: now_iso8601(),
        sources: vec![
            SourceStatus {
                name: "ransomware.live".into(),
                ok: rw_ok,
                detail: rw_detail,
            },
            SourceStatus {
                name: "ahmia".into(),
                ok: ahmia_ok,
                detail: if ahmia_ok {
                    "ok".into()
                } else {
                    "unreachable or blocked (see /api/darkweb/sources)".into()
                },
            },
        ],
        ransomware_groups: groups_n,
        ransomware_victims_total: victims_n,
        note: "Victim counts are operator claims aggregated by ransomware.live, not verified breaches.".into(),
        disclaimer: DISCLAIMER.into(),
    }))
}

#[derive(Debug, Deserialize)]
struct SearchQuery {
    q: String,
    #[serde(default)]
    limit: Option<usize>,
}

async fn search(
    State(state): State<AppState>,
    Query(p): Query<SearchQuery>,
) -> Result<Json<SearchResponse>, ApiError> {
    let q = p.q.trim().to_string();
    if q.chars().count() < 2 {
        return Err(ApiError::bad_request(
            "invalid query",
            "q must be at least 2 characters",
        ));
    }
    if q.chars().count() > 120 {
        return Err(ApiError::bad_request(
            "invalid query",
            "q must be at most 120 characters",
        ));
    }
    let limit = p.limit.unwrap_or(20).clamp(1, 50);
    let key = format!("darkweb:ahmia:q={q}:limit={limit}");

    let fetch = || {
        let client = state.client.clone();
        let q2 = q.clone();
        async move { ahmia_search(&client, &q2, limit).await }
    };
    let (results, source_ok, source_detail) =
        match cached_bytes::<Vec<OnionResult>, _>(&state, &key, AHMIA_TTL, fetch).await {
            Ok((body, _)) => (
                serde_json::from_slice::<Vec<OnionResult>>(&body).unwrap_or_default(),
                true,
                "ok".to_string(),
            ),
            Err(e) => (Vec::new(), false, e.user_message()),
        };

    Ok(Json(SearchResponse {
        query: q,
        count: results.len(),
        results,
        source_ok,
        source_detail,
        scanned_at: now_iso8601(),
        disclaimer: DISCLAIMER.into(),
    }))
}

#[derive(Debug, Deserialize)]
struct VictimsQuery {
    #[serde(default)]
    country: Option<String>,
    #[serde(default)]
    q: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

async fn victims(
    State(state): State<AppState>,
    Query(p): Query<VictimsQuery>,
) -> Result<Json<VictimsResponse>, ApiError> {
    let country = p
        .country
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let q = p.q.as_deref().map(str::trim).filter(|s| !s.is_empty());
    if country.is_some() && q.is_some() {
        return Err(ApiError::bad_request(
            "invalid query",
            "use only one of country or q",
        ));
    }
    if let Some(c) = country
        && (c.chars().count() != 2 || !c.chars().all(|x| x.is_ascii_alphabetic()))
    {
        return Err(ApiError::bad_request(
            "invalid country",
            "country must be an ISO2 code (e.g. US)",
        ));
    }
    if let Some(qq) = q
        && (qq.chars().count() < 2 || qq.chars().count() > 120)
    {
        return Err(ApiError::bad_request(
            "invalid query",
            "q must be 2–120 characters",
        ));
    }
    let limit = p.limit.unwrap_or(25).clamp(1, 100);

    let (mode, path) = match (country, q) {
        (Some(c), _) => (
            format!("country:{}", c.to_uppercase()),
            format!("/v2/countryvictims/{}", c.to_uppercase()),
        ),
        (_, Some(qq)) => (
            format!("search:{qq}"),
            format!("/v2/searchvictims/{}", urlencoding(qq)),
        ),
        _ => ("recent".to_string(), "/v2/victims/recent".to_string()),
    };
    let key = format!("darkweb:rw:victims:{mode}:limit={limit}");

    let fetch = || {
        let client = state.client.clone();
        let path2 = path.clone();
        async move {
            let v: serde_json::Value = rw_get_json(&client, &path2).await?;
            Ok::<Vec<VictimClaim>, FeedError>(
                as_array(v)
                    .into_iter()
                    .map(|x| normalize_victim(&x))
                    .filter(|c| !c.victim.is_empty())
                    .take(limit)
                    .collect(),
            )
        }
    };
    let (claims, source_ok, source_detail) =
        match cached_bytes::<Vec<VictimClaim>, _>(&state, &key, RW_TTL, fetch).await {
            Ok((body, _)) => (
                serde_json::from_slice::<Vec<VictimClaim>>(&body).unwrap_or_default(),
                true,
                "ok".to_string(),
            ),
            Err(e) => (Vec::new(), false, e.user_message()),
        };

    Ok(Json(VictimsResponse {
        mode,
        count: claims.len(),
        claims,
        source_ok,
        source_detail,
        scanned_at: now_iso8601(),
        disclaimer: DISCLAIMER.into(),
    }))
}

async fn groups(State(state): State<AppState>) -> Result<Json<GroupsResponse>, ApiError> {
    let key = "darkweb:rw:groups";
    let fetch = || {
        let client = state.client.clone();
        async move {
            let v: serde_json::Value = rw_get_json(&client, "/v2/groups").await?;
            Ok::<Vec<serde_json::Value>, FeedError>(as_array(v))
        }
    };
    let (group_list, source_ok, source_detail) =
        match cached_bytes::<Vec<serde_json::Value>, _>(&state, key, RW_TTL, fetch).await {
            Ok((body, _)) => (
                serde_json::from_slice::<Vec<serde_json::Value>>(&body).unwrap_or_default(),
                true,
                "ok".to_string(),
            ),
            Err(e) => (Vec::new(), false, e.user_message()),
        };
    Ok(Json(GroupsResponse {
        count: group_list.len(),
        groups: group_list,
        source_ok,
        source_detail,
        scanned_at: now_iso8601(),
        disclaimer: DISCLAIMER.into(),
    }))
}

async fn sources() -> Json<serde_json::Value> {
    let v: serde_json::Value =
        serde_json::from_str(SOURCES_JSON).expect("data/darkweb_sources.json parses");
    Json(v)
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/darkweb", get(overview))
        .route("/api/darkweb/search", get(search))
        .route("/api/darkweb/ransomware/victims", get(victims))
        .route("/api/darkweb/ransomware/groups", get(groups))
        .route("/api/darkweb/sources", get(sources))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_parses_with_four_sources() {
        let v: serde_json::Value =
            serde_json::from_str(SOURCES_JSON).expect("data/darkweb_sources.json parses");
        assert_eq!(v["sources"].as_array().map(|a| a.len()), Some(4));
    }

    #[test]
    fn disclaimer_states_operator_claims_and_no_tor() {
        assert!(DISCLAIMER.contains("OPERATOR CLAIMS"));
        assert!(DISCLAIMER.contains("does not run Tor"));
    }

    #[test]
    fn parse_ahmia_extracts_onion_links_only() {
        let html = r#"
            <div class="result"><a href="http://abc123.onion/news">Leak Blog</a></div>
            <div class="result"><a href="https://example.com/clearnet">Clearnet</a></div>
            <div class="result"><a href="http://xyz789.onion/">Second <b>Onion</b></a></div>
            <div class="result"><a href="http://abc123.onion/news">Leak Blog</a></div>
        "#;
        let out = parse_ahmia(html, 10);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].title, "Leak Blog");
        assert!(out[0].url.contains(".onion"));
        assert_eq!(out[1].title, "Second Onion");
    }

    #[test]
    fn parse_ahmia_ignores_empty_titles_and_respects_limit() {
        let html = r#"<a href="http://a1.onion/">One</a><a href="http://a2.onion/"></a><a href="http://a3.onion/">Three</a>"#;
        let out = parse_ahmia(html, 1);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].title, "One");
    }

    #[test]
    fn normalize_victim_handles_field_spellings() {
        let v = serde_json::json!({
            "post_title": "ACME Corp",
            "group_name": "lockbit",
            "country": "US",
            "discovered": "2026-10-01",
            "description": "desc",
            "claim_url": "http://x.onion/1"
        });
        let c = normalize_victim(&v);
        assert_eq!(c.victim, "ACME Corp");
        assert_eq!(c.group, "lockbit");
        assert_eq!(c.claim_url, "http://x.onion/1");
        // fallback spellings
        let v2 = serde_json::json!({"victim": "Beta LLC", "website": "http://y.onion/2"});
        let c2 = normalize_victim(&v2);
        assert_eq!(c2.victim, "Beta LLC");
        assert_eq!(c2.claim_url, "http://y.onion/2");
    }

    #[test]
    fn urlencoding_encodes_reserved_chars() {
        assert_eq!(urlencoding("a b/c?"), "a%20b%2Fc%3F");
        assert_eq!(urlencoding("abc-_.~"), "abc-_.~");
    }

    #[test]
    fn truncate_respects_char_boundaries() {
        let s = "héllo wörld, this is long";
        let t = truncate(s, 10);
        assert!(t.len() <= 13); // 10 + ellipsis
        assert!(t.ends_with('…'));
    }
}
