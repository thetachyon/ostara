//! Threats-against-politicians scanning: defensive monitoring of public
//! sources for threat mentions targeting political figures (violent
//! rhetoric, death threats, assassination mentions).
//!
//! PROTECTIVE SCANNING ONLY — this is not a targeting system. Public
//! officials only (see `data/politicians.json` and its provenance notes).
//! Every hit is a keyword-flagged public mention that requires human
//! review; nothing here is presented as a verified threat.
//!
//! ## Endpoints
//!
//! - `GET /api/threats/politicians?country=` — the watchlist (78 figures:
//!   G20 heads of state/government, US president/VP/cabinet/congressional
//!   leadership, EU institutional leadership, other major world leaders).
//! - `GET /api/threats/scan?name=<name|alias>&severity=` — scans Google
//!   News, Bluesky, and the configured Telegram channels (TELEGRAM_CHANNELS)
//!   through their normal cache paths and TTLs (no new upstreams, no new
//!   keys), classifies each returned item into severity tiers, and returns
//!   items sorted by severity then recency, capped at 50 — plus an
//!   `accounts` array rolling social items up by (platform, author) with
//!   per-tier counts and sample posts. News outlets are excluded from
//!   `accounts`.
//!
//! ## Keyword tiers (English-first — documented limitation)
//!
//! - **critical**: `assassinat*` (assassinate/assassination/assassinated),
//!   `death threat`, or `kill` co-occurring with the figure's name/alias.
//! - **high**: `threaten` (covers threatened/threatening), `threat against`,
//!   `attack`.
//! - **medium**: `harm`, `violence against`, `plot against`.
//!
//! Matching is case-insensitive over title + text/snippet. Single generic
//! words (`kill`, `attack`, `harm`) require word boundaries to cut noise
//! (`pharmacy` must not match `harm`); stems and phrases match as
//! substrings. Tiers are checked in order and the first match wins;
//! `matched_keywords` lists the terms that fired in the winning tier.
//!
//! ## Queries (verified live 2026-11-01)
//!
//! - Google News RSS: `"<name>" (assassination OR assassinate OR "death
//!   threat" OR threaten OR threat OR attack OR kill)` — OR/quotes verified
//!   against the live RSS endpoint.
//! - Bluesky: `"<name>" threat` — parenthesized OR returned zero results
//!   live (unsupported syntax), so the query stays simple; tiering happens
//!   locally on returned text.
//!
//! ## Deliberately excluded
//!
//! - **GDELT**: no threat-relevant `theme:` value could be verified. The
//!   DOC API is unreachable from this environment and the GKG theme
//!   taxonomy lookup is likewise blocked; inventing a theme value is not
//!   acceptable. Revisit when live verification is possible.
//! - **Non-English sources**: keyword lists are English-first; every
//!   response states English-language coverage only.
//!
//! ## Cache discipline
//!
//! Each source fetch reuses its feed's exact cache-key format and TTL
//! (`gnews:q=…:limit=…`, `bluesky:q=…:limit=…`), so scans share cache
//! entries with the normal feed endpoints. The assembled scan response is
//! additionally cached under `threats:scan:…` for 300s. At most 2 upstream
//! calls per figure per TTL window. A failed source is recorded in
//! `sources[]` as `ok: false` with a human-readable detail — never silently
//! dropped, and the scan still returns 200 with whatever the other source
//! produced. An empty TELEGRAM_CHANNELS allowlist is reported honestly
//! (`telegram` source `ok: false`, "not configured").
//
//! ## Account attribution
//!
//! Social items carry `author`, `author_display`, `author_url`, and
//! `post_url`; the response's `accounts` array groups them by (platform,
//! author) with per-tier counts and up to 3 sample posts. These are public
//! accounts whose posts matched threat keywords — they include journalists
//! quoting threats, people condemning threats, and figurative language.
//! They are NEVER "threat actors" and no account is accused of making a
//! threat; the disclaimer says so on every response.

use std::sync::OnceLock;

use axum::{
    Json, Router,
    extract::{Query, State},
};
use serde::{Deserialize, Serialize};

use crate::{
    error::ApiError,
    feeds,
    routes::{AppState, cached_bytes},
    time::now_iso8601,
};

/// Keyword scan, not verified threats. These are public news and
/// social-media mentions matching threat-related keywords for the named
/// public figure. Every item requires human review: keyword matches include
/// news reporting about threats, condemnations of threats, historical
/// references, and figurative language — not just genuine threats. Nothing
/// here is a verified threat against anyone. ACCOUNT ATTRIBUTION: the
/// accounts listed are public social-media accounts whose posts matched the
/// threat keywords for this figure — that includes journalists quoting
/// threats, people condemning them, and figurative or political-rhetoric
/// uses. This is not a list of threat actors, and no account listed is
/// accused of making a threat. Coverage is English-language sources only.
/// This is protective monitoring of public discourse, not a targeting
/// system.
pub const DISCLAIMER: &str = "KEYWORD SCAN — NOT VERIFIED THREATS. These are public news and social-media mentions matching threat-related keywords for the named public figure. Every item requires human review: keyword matches include news reporting about threats, condemnations of threats, historical references, and figurative language — not just genuine threats. Nothing here is a verified threat against anyone. ACCOUNT ATTRIBUTION: the accounts listed are public social-media accounts whose posts matched the threat keywords for this figure — that includes journalists quoting threats, people condemning them, and figurative or political-rhetoric uses. This is not a list of threat actors, and no account listed is accused of making a threat. Coverage is English-language sources only. This is protective monitoring of public discourse, not a targeting system.";

static POLITICIANS_JSON: &str = include_str!("../data/politicians.json");

const SCAN_TTL: std::time::Duration = std::time::Duration::from_secs(300);
const SCAN_LIMIT: u32 = 50;

// ---------------------------------------------------------------------------
// Watchlist data
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct Figure {
    name: String,
    aliases: Vec<String>,
    country: String,
    iso2: String,
    iso3: String,
    role: String,
}

#[derive(Debug, Deserialize)]
struct Dataset {
    #[allow(dead_code)]
    meta: serde_json::Value,
    figures: Vec<Figure>,
}

fn dataset() -> &'static Dataset {
    static DATA: OnceLock<Dataset> = OnceLock::new();
    DATA.get_or_init(|| {
        serde_json::from_str(POLITICIANS_JSON).expect("data/politicians.json parses")
    })
}

/// Find a figure by name or alias (case-insensitive, trimmed).
pub(crate) fn find_figure(q: &str) -> Option<&'static Figure> {
    let q = q.trim().to_lowercase();
    dataset()
        .figures
        .iter()
        .find(|f| f.name.to_lowercase() == q || f.aliases.iter().any(|a| a.to_lowercase() == q))
}

// ---------------------------------------------------------------------------
// Severity tiers (pure, unit-tested)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Severity {
    Critical,
    High,
    Medium,
}

impl Severity {
    fn as_str(self) -> &'static str {
        match self {
            Severity::Critical => "critical",
            Severity::High => "high",
            Severity::Medium => "medium",
        }
    }
}

/// Word-boundary substring check on already-lowercased text, so `harm`
/// does not match `pharmacy` and `kill` does not match `killer`.
fn contains_word(haystack: &str, needle: &str) -> bool {
    haystack
        .split(|c: char| !c.is_alphanumeric())
        .any(|w| w == needle)
}

/// Classify text into the highest matching severity tier.
/// `name_terms` are the lowercased figure name + aliases (for the
/// `kill` + name co-occurrence rule). Returns the tier and the keywords
/// that fired within it, or `None` when nothing matches.
fn classify(text: &str, name_terms: &[String]) -> Option<(Severity, Vec<String>)> {
    let t = text.to_lowercase();
    // Critical: assassination stem, death threat, or kill + the figure named.
    let mut crit = Vec::new();
    if t.contains("assassinat") {
        crit.push("assassinat*".to_string());
    }
    if t.contains("death threat") {
        crit.push("death threat".to_string());
    }
    if contains_word(&t, "kill") && name_terms.iter().any(|n| t.contains(n.as_str())) {
        crit.push("kill + name".to_string());
    }
    if !crit.is_empty() {
        return Some((Severity::Critical, crit));
    }
    // High: threaten (covers threatened/threatening), threat against, attack.
    let mut high = Vec::new();
    if t.contains("threaten") {
        high.push("threaten".to_string());
    }
    if t.contains("threat against") {
        high.push("threat against".to_string());
    }
    if contains_word(&t, "attack") {
        high.push("attack".to_string());
    }
    if !high.is_empty() {
        return Some((Severity::High, high));
    }
    // Medium: harm, violence against, plot against.
    let mut med = Vec::new();
    if contains_word(&t, "harm") {
        med.push("harm".to_string());
    }
    if t.contains("violence against") {
        med.push("violence against".to_string());
    }
    if t.contains("plot against") {
        med.push("plot against".to_string());
    }
    if !med.is_empty() {
        return Some((Severity::Medium, med));
    }
    None
}

// ---------------------------------------------------------------------------
// Response shapes
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct FigureSummary {
    name: String,
    role: String,
    country: String,
    iso2: String,
    iso3: String,
}

#[derive(Debug, Serialize)]
pub struct ThreatItem {
    source: String,
    title: String,
    text_snippet: String,
    url: String,
    published: Option<String>,
    severity: String,
    matched_keywords: Vec<String>,
    /// Social attribution: Bluesky handle (bare, e.g. "user.bsky.social")
    /// or Telegram channel name. `None` for news outlets.
    #[serde(skip_serializing_if = "Option::is_none")]
    author: Option<String>,
    /// Bluesky display name, when the upstream provides one.
    #[serde(skip_serializing_if = "Option::is_none")]
    author_display: Option<String>,
    /// https://bsky.app/profile/<handle> or https://t.me/s/<channel>.
    #[serde(skip_serializing_if = "Option::is_none")]
    author_url: Option<String>,
    /// Direct link to the post (bsky.app post URL / t.me message URL).
    #[serde(skip_serializing_if = "Option::is_none")]
    post_url: Option<String>,
}

/// One sample post inside an account rollup.
#[derive(Debug, Serialize)]
pub struct AccountSample {
    title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    post_url: Option<String>,
    severity: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    published: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct SeverityCounts {
    critical: usize,
    high: usize,
    medium: usize,
}

/// One public social account whose posts matched the scan's threat
/// keywords. Named deliberately: this is an account with flagged posts,
/// NOT a threat actor — the disclaimer states this on every response.
#[derive(Debug, Serialize)]
pub struct AccountSummary {
    platform: String,
    handle: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    display_name: Option<String>,
    profile_url: String,
    post_count: usize,
    max_severity: String,
    severities: SeverityCounts,
    /// Up to 3 sample posts, sorted by severity then recency.
    sample: Vec<AccountSample>,
    #[serde(skip_serializing_if = "Option::is_none")]
    first_seen: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_seen: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct SourceStatus {
    source: String,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ScanResponse {
    figure: FigureSummary,
    count: usize,
    items: Vec<ThreatItem>,
    /// Public social accounts with flagged posts, rolled up from `items`.
    /// News outlets are excluded. Never label these "threat actors".
    accounts: Vec<AccountSummary>,
    sources: Vec<SourceStatus>,
    scanned_at: String,
    disclaimer: String,
}

// ---------------------------------------------------------------------------
// Scan assembly
// ---------------------------------------------------------------------------

fn name_terms(fig: &Figure) -> Vec<String> {
    let mut terms: Vec<String> = fig.aliases.iter().map(|a| a.to_lowercase()).collect();
    terms.push(fig.name.to_lowercase());
    terms
}

fn rank(severity: &str) -> u8 {
    match severity {
        "critical" => 0,
        "high" => 1,
        _ => 2,
    }
}

/// Sort by severity (critical first), then recency (ISO-8601 strings sort
/// lexicographically; items without dates go last).
fn sort_items(items: &mut [ThreatItem]) {
    items.sort_by(|a, b| {
        rank(a.severity.as_str())
            .cmp(&rank(b.severity.as_str()))
            .then_with(|| match (&b.published, &a.published) {
                (Some(pb), Some(pa)) => pb.cmp(pa),
                (Some(_), None) => std::cmp::Ordering::Greater,
                (None, Some(_)) => std::cmp::Ordering::Less,
                (None, None) => std::cmp::Ordering::Equal,
            })
    });
}

/// Roll social items up by (platform, author) into account summaries.
/// News-outlet items (`author: None`) are excluded. Sorted by max
/// severity (critical first), then post count descending, then handle.
fn group_accounts(items: &[ThreatItem]) -> Vec<AccountSummary> {
    use std::collections::BTreeMap;
    let mut groups: BTreeMap<(String, String), Vec<&ThreatItem>> = BTreeMap::new();
    for it in items {
        if let Some(author) = it.author.as_deref() {
            groups
                .entry((it.source.clone(), author.to_string()))
                .or_default()
                .push(it);
        }
    }
    let mut out: Vec<AccountSummary> = groups
        .into_iter()
        .map(|((platform, handle), posts)| {
            let mut counts = SeverityCounts {
                critical: 0,
                high: 0,
                medium: 0,
            };
            let mut best_rank = 2u8;
            let mut display_name: Option<String> = None;
            let mut profile_url: Option<String> = None;
            let mut first_seen: Option<String> = None;
            let mut last_seen: Option<String> = None;
            for p in &posts {
                let r = rank(p.severity.as_str());
                if r < best_rank {
                    best_rank = r;
                }
                match r {
                    0 => counts.critical += 1,
                    1 => counts.high += 1,
                    _ => counts.medium += 1,
                }
                if display_name.is_none() {
                    display_name.clone_from(&p.author_display);
                }
                if profile_url.is_none() {
                    profile_url.clone_from(&p.author_url);
                }
                if let Some(pubd) = p.published.as_deref() {
                    if first_seen.as_deref().is_none_or(|f| pubd < f) {
                        first_seen = Some(pubd.to_string());
                    }
                    if last_seen.as_deref().is_none_or(|l| pubd > l) {
                        last_seen = Some(pubd.to_string());
                    }
                }
            }
            // Sample: best severity first, then most recent, capped at 3.
            let mut sample_src: Vec<&ThreatItem> = posts.clone();
            sample_src.sort_by(|a, b| {
                rank(a.severity.as_str())
                    .cmp(&rank(b.severity.as_str()))
                    .then_with(|| {
                        b.published
                            .as_deref()
                            .unwrap_or("")
                            .cmp(a.published.as_deref().unwrap_or(""))
                    })
            });
            let sample = sample_src
                .into_iter()
                .take(3)
                .map(|p| AccountSample {
                    title: p.title.clone(),
                    post_url: p.post_url.clone(),
                    severity: p.severity.clone(),
                    published: p.published.clone(),
                })
                .collect();
            let max_severity = match best_rank {
                0 => "critical",
                1 => "high",
                _ => "medium",
            }
            .to_string();
            AccountSummary {
                profile_url: profile_url.unwrap_or_default(),
                platform,
                handle,
                display_name,
                post_count: posts.len(),
                max_severity,
                severities: counts,
                sample,
                first_seen,
                last_seen,
            }
        })
        .collect();
    out.sort_by(|a, b| {
        rank(a.max_severity.as_str())
            .cmp(&rank(b.max_severity.as_str()))
            .then_with(|| b.post_count.cmp(&a.post_count))
            .then_with(|| a.handle.cmp(&b.handle))
    });
    out
}

fn snippet(s: &str, max: usize) -> String {
    // Google News descriptions embed raw HTML (e.g. <a href=...> links);
    // strip tags so the UI doesn't render literal markup as escaped text.
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
    let s = out.trim().replace(|c: char| c.is_whitespace(), " ");
    if s.len() <= max {
        s
    } else {
        format!("{}…", s[..max].trim_end())
    }
}

async fn scan_gnews(
    state: &AppState,
    fig: &Figure,
    terms: &[String],
    sources: &mut Vec<SourceStatus>,
    out: &mut Vec<ThreatItem>,
) {
    let query = format!(
        "\"{}\" (assassination OR assassinate OR \"death threat\" OR threaten OR threat OR attack OR kill)",
        fig.name
    );
    let key = format!("gnews:q={query}:limit={SCAN_LIMIT}");
    let client = state.client.clone();
    let res: Result<feeds::gnews::GnewsResponse, ApiError> = async {
        let (body, _) = cached_bytes(state, &key, feeds::gnews::TTL, || {
            feeds::gnews::fetch(&client, &query, SCAN_LIMIT)
        })
        .await?;
        serde_json::from_slice(&body)
            .map_err(|e| ApiError::bad_gateway("gnews decode failed", e.to_string()))
    }
    .await;
    match res {
        Ok(r) => {
            sources.push(SourceStatus {
                source: "gnews".into(),
                ok: true,
                detail: None,
            });
            for a in r.items {
                let text = format!("{} {}", a.title, a.summary.as_deref().unwrap_or(""));
                if let Some((sev, kws)) = classify(&text, terms) {
                    out.push(ThreatItem {
                        source: "gnews".into(),
                        title: a.title.clone(),
                        text_snippet: snippet(a.summary.as_deref().unwrap_or(""), 280),
                        url: a.link.clone(),
                        published: a.published.clone(),
                        severity: sev.as_str().into(),
                        matched_keywords: kws,
                        // News outlets are not social accounts — no attribution.
                        author: None,
                        author_display: None,
                        author_url: None,
                        post_url: None,
                    });
                }
            }
        }
        Err(e) => sources.push(SourceStatus {
            source: "gnews".into(),
            ok: false,
            detail: Some(e.user_message()),
        }),
    }
}

async fn scan_bluesky(
    state: &AppState,
    fig: &Figure,
    terms: &[String],
    sources: &mut Vec<SourceStatus>,
    out: &mut Vec<ThreatItem>,
) {
    // Parenthesized OR is unsupported by the AppView search (verified live);
    // keep the query simple — tiering happens locally on returned text.
    let query = format!("\"{}\" threat", fig.name);
    let key = format!("bluesky:q={query}:limit={SCAN_LIMIT}");
    let client = state.client.clone();
    let res: Result<feeds::bluesky::BlueskyResponse, ApiError> = async {
        let (body, _) = cached_bytes(state, &key, feeds::bluesky::TTL, || {
            feeds::bluesky::fetch(&client, &query, SCAN_LIMIT)
        })
        .await?;
        serde_json::from_slice(&body)
            .map_err(|e| ApiError::bad_gateway("bluesky decode failed", e.to_string()))
    }
    .await;
    match res {
        Ok(r) => {
            sources.push(SourceStatus {
                source: "bluesky".into(),
                ok: true,
                detail: None,
            });
            for p in r.posts {
                if let Some((sev, kws)) = classify(&p.text, terms) {
                    out.push(ThreatItem {
                        source: "bluesky".into(),
                        title: format!(
                            "Post by @{}{}",
                            p.author_handle,
                            p.author_display
                                .as_deref()
                                .map(|d| format!(" ({d})"))
                                .unwrap_or_default()
                        ),
                        text_snippet: snippet(&p.text, 280),
                        url: p.url.clone(),
                        published: Some(p.created_at.clone()),
                        severity: sev.as_str().into(),
                        matched_keywords: kws,
                        author: Some(p.author_handle.clone()),
                        author_display: p.author_display.clone(),
                        author_url: Some(format!("https://bsky.app/profile/{}", p.author_handle)),
                        post_url: Some(p.url.clone()),
                    });
                }
            }
        }
        Err(e) => sources.push(SourceStatus {
            source: "bluesky".into(),
            ok: false,
            detail: Some(e.user_message()),
        }),
    }
}

/// Scan the configured Telegram channels (TELEGRAM_CHANNELS) for
/// figure mentions, then classify. Each channel's snapshot is cached
/// under the same `telegram:{channel}` key the /api/telegram handler
/// uses, so scans share cache entries with the normal endpoint.
async fn scan_telegram(
    state: &AppState,
    terms: &[String],
    sources: &mut Vec<SourceStatus>,
    out: &mut Vec<ThreatItem>,
) {
    let channels: Vec<String> = state.config.telegram_channels.clone();
    if channels.is_empty() {
        sources.push(SourceStatus {
            source: "telegram".into(),
            ok: false,
            detail: Some(
                "not configured — set TELEGRAM_CHANNELS to public channel names to enable Telegram monitoring"
                    .into(),
            ),
        });
        return;
    }
    let mut any_ok = false;
    let mut failures: Vec<String> = Vec::new();
    for channel in &channels {
        let key = format!("telegram:{channel}");
        let client = state.client.clone();
        let ch = channel.clone();
        let res: Result<feeds::telegram::TelegramResponse, ApiError> = async {
            let (body, _) = cached_bytes(state, &key, feeds::telegram::TTL, || {
                feeds::telegram::fetch(&client, &ch)
            })
            .await?;
            serde_json::from_slice(&body)
                .map_err(|e| ApiError::bad_gateway("telegram decode failed", e.to_string()))
        }
        .await;
        match res {
            Ok(r) => {
                any_ok = true;
                for m in r.messages {
                    // Channel-wide scan: only messages that actually name
                    // the figure are candidates.
                    let mt = m.text.to_lowercase();
                    if !terms.iter().any(|t| mt.contains(t.as_str())) {
                        continue;
                    }
                    if let Some((sev, kws)) = classify(&m.text, terms) {
                        out.push(ThreatItem {
                            source: "telegram".into(),
                            title: format!("Post in @{channel}"),
                            text_snippet: snippet(&m.text, 280),
                            url: m.url.clone(),
                            published: Some(m.date.clone()),
                            severity: sev.as_str().into(),
                            matched_keywords: kws,
                            author: Some(channel.clone()),
                            author_display: None,
                            author_url: Some(format!("https://t.me/s/{channel}")),
                            post_url: Some(m.url.clone()),
                        });
                    }
                }
            }
            Err(e) => failures.push(format!("{channel}: {}", e.user_message())),
        }
    }
    sources.push(SourceStatus {
        source: "telegram".into(),
        ok: any_ok,
        detail: if failures.is_empty() {
            None
        } else {
            Some(failures.join("; "))
        },
    });
}

// ---------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------

/// `GET /api/threats/politicians?country=` — the watchlist.
pub async fn politicians(
    Query(q): Query<PoliticiansQuery>,
) -> Result<Json<PoliticiansResponse>, ApiError> {
    let figs: Vec<Figure> = match q
        .country
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        None => dataset().figures.clone(),
        Some(c) => {
            let needle = c.to_lowercase();
            dataset()
                .figures
                .iter()
                .filter(|f| {
                    f.country.to_lowercase() == needle
                        || f.iso2.to_lowercase() == needle
                        || f.iso3.to_lowercase() == needle
                })
                .cloned()
                .collect()
        }
    };
    Ok(Json(PoliticiansResponse {
        count: figs.len(),
        figures: figs,
    }))
}

#[derive(Debug, Deserialize)]
pub struct PoliticiansQuery {
    pub country: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct PoliticiansResponse {
    pub count: usize,
    pub figures: Vec<Figure>,
}

#[derive(Debug, Deserialize)]
pub struct ScanQuery {
    pub name: Option<String>,
    pub severity: Option<String>,
}

/// `GET /api/threats/scan?name=<name|alias>&severity=all|critical|high|medium`.
pub async fn scan(
    State(state): State<AppState>,
    Query(q): Query<ScanQuery>,
) -> Result<impl axum::response::IntoResponse, ApiError> {
    let name = q
        .name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            ApiError::bad_request(
                "missing name",
                "query param name is required, e.g. ?name=Macron",
            )
        })?;
    let severity_filter = q.severity.as_deref().unwrap_or("all").to_lowercase();
    if !["all", "critical", "high", "medium"].contains(&severity_filter.as_str()) {
        return Err(ApiError::bad_request(
            "bad severity",
            "severity must be one of: all, critical, high, medium",
        ));
    }
    let fig = find_figure(name).ok_or_else(|| {
        ApiError::not_found(
            "unknown figure",
            format!(
                "no watchlist entry for {name:?}; see /api/threats/politicians for the covered list"
            ),
        )
    })?;

    // The assembled scan is cached too (sources are cached under their own
    // feed key formats, so this is a cheap second layer).
    let skey = format!(
        "threats:scan:name={}:severity={severity_filter}",
        fig.name.to_lowercase()
    );
    if let Some(body) = state.cache.get(&skey) {
        return Ok(crate::feeds::json_bytes(body, true));
    }

    let terms = name_terms(fig);
    let mut sources = Vec::with_capacity(3);
    let mut items: Vec<ThreatItem> = Vec::new();
    scan_gnews(&state, fig, &terms, &mut sources, &mut items).await;
    scan_bluesky(&state, fig, &terms, &mut sources, &mut items).await;
    scan_telegram(&state, &terms, &mut sources, &mut items).await;

    if severity_filter != "all" {
        items.retain(|i| i.severity == severity_filter);
    }
    sort_items(&mut items);
    items.truncate(SCAN_LIMIT as usize);
    let accounts = group_accounts(&items);

    let resp = ScanResponse {
        figure: FigureSummary {
            name: fig.name.clone(),
            role: fig.role.clone(),
            country: fig.country.clone(),
            iso2: fig.iso2.clone(),
            iso3: fig.iso3.clone(),
        },
        count: items.len(),
        items,
        accounts,
        sources,
        scanned_at: now_iso8601(),
        disclaimer: DISCLAIMER.into(),
    };
    let body = serde_json::to_vec(&resp)
        .map_err(|e| ApiError::bad_gateway("serialize failed", e.to_string()))?;
    state.cache.put(skey, body.clone(), SCAN_TTL);
    Ok(crate::feeds::json_bytes(body, false))
}

pub fn routes() -> Router<crate::routes::AppState> {
    use axum::routing::get;
    Router::new()
        .route("/api/threats/politicians", get(politicians))
        .route("/api/threats/scan", get(scan))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dataset_parses_with_expected_size() {
        let d = dataset();
        assert!(d.figures.len() >= 60, "expected 60+ figures");
        for f in &d.figures {
            assert!(!f.name.is_empty() && !f.role.is_empty());
            assert!(!f.iso2.is_empty() && !f.iso3.is_empty());
        }
    }

    #[test]
    fn lookup_by_name_and_alias_case_insensitive() {
        assert_eq!(find_figure("Emmanuel Macron").unwrap().iso2, "FR");
        assert_eq!(find_figure("macron").unwrap().name, "Emmanuel Macron");
        assert_eq!(find_figure("  MBS ").unwrap().name, "Mohammed bin Salman");
        assert!(find_figure("Narnia").is_none());
    }

    fn terms() -> Vec<String> {
        vec!["emmanuel macron".into(), "macron".into()]
    }

    #[test]
    fn classify_critical_tier() {
        let t = terms();
        let (s, k) = classify("Assassination plot against Emmanuel Macron foiled", &t).unwrap();
        assert_eq!(s, Severity::Critical);
        assert!(k.iter().any(|k| k.contains("assassinat")));
        let (s, _) = classify("Macron receives death threat by mail", &t).unwrap();
        assert_eq!(s, Severity::Critical);
        let (s, k) = classify("Man arrested over plan to kill Macron", &t).unwrap();
        assert_eq!(s, Severity::Critical);
        assert!(k.iter().any(|k| k.contains("kill")));
    }

    #[test]
    fn classify_high_tier() {
        let t = terms();
        let (s, k) = classify("Activist threatens to threaten Macron at rally", &t).unwrap();
        assert_eq!(s, Severity::High);
        assert!(k.contains(&"threaten".to_string()));
        let (s, _) = classify("Threat against Macron condemned by allies", &t).unwrap();
        assert_eq!(s, Severity::High);
        let (s, _) = classify("Macron survives attack attempt, unharmed", &t).unwrap();
        assert_eq!(s, Severity::High);
    }

    #[test]
    fn classify_medium_tier() {
        let t = terms();
        let (s, _) = classify("Plot to harm Macron uncovered", &t).unwrap();
        // "harm" is medium, but check tier order: nothing higher matches.
        assert_eq!(s, Severity::Medium);
        let (s, _) = classify("Calls for violence against Macron condemned", &t).unwrap();
        assert_eq!(s, Severity::Medium);
    }

    #[test]
    fn classify_rejects_noise() {
        let t = terms();
        // Bare "threat" with no tier keyword: not a personal threat.
        assert!(classify("Threats to democracy, says Macron", &t).is_none());
        // Word boundaries: pharmacy must not match harm; killer must not
        // match the kill+name rule on its own.
        assert!(classify("Macron visits pharmacy in Lyon", &t).is_none());
        // kill without the figure named is not critical.
        assert_eq!(
            classify("Activists vow to kill the bill in parliament", &t).map(|(s, _)| s),
            None
        );
    }

    #[test]
    fn tier_order_wins_over_lower_matches() {
        let t = terms();
        // Contains both "attack" (high) and "assassination" (critical).
        let (s, _) = classify("Assassination attack plot against Macron", &t).unwrap();
        assert_eq!(s, Severity::Critical);
    }

    #[test]
    fn sort_orders_severity_then_recency() {
        fn item(title: &str, severity: &str, published: Option<&str>) -> ThreatItem {
            ThreatItem {
                source: "gnews".into(),
                title: title.into(),
                text_snippet: "".into(),
                url: "".into(),
                published: published.map(str::to_string),
                severity: severity.into(),
                matched_keywords: vec![],
                author: None,
                author_display: None,
                author_url: None,
                post_url: None,
            }
        }
        let mut items = vec![
            item("m", "medium", Some("2026-11-01T10:00:00Z")),
            item("c-old", "critical", Some("2026-10-01T10:00:00Z")),
            item("c-new", "critical", Some("2026-11-01T12:00:00Z")),
            item("c-nodate", "critical", None),
        ];
        sort_items(&mut items);
        let titles: Vec<&str> = items.iter().map(|i| i.title.as_str()).collect();
        assert_eq!(titles, vec!["c-new", "c-old", "c-nodate", "m"]);
    }

    #[test]
    fn disclaimer_says_what_it_is() {
        assert!(DISCLAIMER.contains("NOT VERIFIED THREATS"));
        assert!(DISCLAIMER.contains("human review"));
        assert!(DISCLAIMER.contains("English-language"));
        assert!(DISCLAIMER.contains("not a targeting system"));
        assert!(DISCLAIMER.contains("ACCOUNT ATTRIBUTION"));
        assert!(DISCLAIMER.contains("not a list of threat actors"));
        assert!(DISCLAIMER.contains("no account listed is accused of making a threat"));
    }

    #[test]
    fn snippet_strips_html_tags() {
        let s = snippet(
            r#"<a href="https://news.google.com/rss/xyz" target="_blank">Far-right militants</a> go on trial"#,
            280,
        );
        assert_eq!(s, "Far-right militants go on trial");
    }

    fn social_item(
        source: &str,
        author: Option<&str>,
        severity: &str,
        published: Option<&str>,
        display: Option<&str>,
    ) -> ThreatItem {
        ThreatItem {
            source: source.into(),
            title: format!("post {severity}"),
            text_snippet: "".into(),
            url: "https://example.com/post".into(),
            published: published.map(str::to_string),
            severity: severity.into(),
            matched_keywords: vec![],
            author: author.map(str::to_string),
            author_display: display.map(str::to_string),
            author_url: author.map(|a| format!("https://example.com/{a}")),
            post_url: Some("https://example.com/post".into()),
        }
    }

    #[test]
    fn group_accounts_groups_and_rolls_up() {
        let items = vec![
            social_item(
                "bluesky",
                Some("alice.bsky.social"),
                "high",
                Some("2026-11-02T10:00:00Z"),
                Some("Alice"),
            ),
            social_item(
                "bluesky",
                Some("alice.bsky.social"),
                "critical",
                Some("2026-11-02T12:00:00Z"),
                None,
            ),
            social_item(
                "bluesky",
                Some("alice.bsky.social"),
                "medium",
                Some("2026-11-01T10:00:00Z"),
                None,
            ),
            social_item(
                "telegram",
                Some("newschannel"),
                "medium",
                Some("2026-11-02T09:00:00Z"),
                None,
            ),
            // News outlets have no author: excluded.
            social_item(
                "gnews",
                None,
                "critical",
                Some("2026-11-02T08:00:00Z"),
                None,
            ),
        ];
        let accs = group_accounts(&items);
        assert_eq!(accs.len(), 2, "gnews item excluded, two social accounts");
        let alice = &accs[0];
        assert_eq!(alice.platform, "bluesky");
        assert_eq!(alice.handle, "alice.bsky.social");
        assert_eq!(alice.display_name.as_deref(), Some("Alice"));
        assert_eq!(alice.post_count, 3);
        assert_eq!(alice.max_severity, "critical");
        assert_eq!(alice.severities.critical, 1);
        assert_eq!(alice.severities.high, 1);
        assert_eq!(alice.severities.medium, 1);
        // Sample: severity then recency, capped at 3.
        assert_eq!(alice.sample.len(), 3);
        assert_eq!(alice.sample[0].severity, "critical");
        assert_eq!(alice.first_seen.as_deref(), Some("2026-11-01T10:00:00Z"));
        assert_eq!(alice.last_seen.as_deref(), Some("2026-11-02T12:00:00Z"));
        // Critical account sorts before the medium-only one.
        assert_eq!(accs[1].handle, "newschannel");
        assert_eq!(accs[1].max_severity, "medium");
    }

    #[test]
    fn group_accounts_separates_platforms_and_sorts_by_count() {
        let items = vec![
            social_item(
                "bluesky",
                Some("same"),
                "high",
                Some("2026-11-02T10:00:00Z"),
                None,
            ),
            social_item(
                "bluesky",
                Some("same"),
                "high",
                Some("2026-11-02T11:00:00Z"),
                None,
            ),
            social_item(
                "telegram",
                Some("same"),
                "high",
                Some("2026-11-02T10:00:00Z"),
                None,
            ),
            social_item(
                "bluesky",
                Some("zz-top"),
                "high",
                Some("2026-11-02T10:00:00Z"),
                None,
            ),
        ];
        let accs = group_accounts(&items);
        assert_eq!(accs.len(), 3, "same handle on two platforms = two accounts");
        // Same max severity: higher post count first, then handle alpha.
        assert_eq!(accs[0].platform, "bluesky");
        assert_eq!(accs[0].handle, "same");
        assert_eq!(accs[0].post_count, 2);
        assert_eq!(accs[1].handle, "same");
        assert_eq!(accs[1].platform, "telegram");
        assert_eq!(accs[2].handle, "zz-top");
    }

    #[test]
    fn group_accounts_sample_capped_at_three() {
        let items: Vec<ThreatItem> = (0..6)
            .map(|i| {
                social_item(
                    "bluesky",
                    Some("prolific.bsky.social"),
                    "medium",
                    Some(&format!("2026-11-02T{i:02}:00:00Z")),
                    None,
                )
            })
            .collect();
        let accs = group_accounts(&items);
        assert_eq!(accs.len(), 1);
        assert_eq!(accs[0].post_count, 6);
        assert_eq!(accs[0].sample.len(), 3, "sample capped at 3");
        // Most recent first within the same tier.
        assert_eq!(
            accs[0].sample[0].published.as_deref(),
            Some("2026-11-02T05:00:00Z")
        );
    }

    #[test]
    fn group_accounts_empty_for_news_only() {
        let items = vec![social_item("gnews", None, "critical", None, None)];
        assert!(group_accounts(&items).is_empty());
        assert!(group_accounts(&[]).is_empty());
    }
}
