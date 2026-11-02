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
//!   News and Bluesky through their normal cache paths and TTLs (no new
//!   upstreams, no new keys), classifies each returned item into severity
//!   tiers, and returns items sorted by severity then recency, capped at 50.
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
//! produced.

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
/// here is a verified threat against anyone. Coverage is English-language
/// sources only. This is protective monitoring of public discourse, not a
/// targeting system.
pub const DISCLAIMER: &str = "KEYWORD SCAN — NOT VERIFIED THREATS. These are public news and social-media mentions matching threat-related keywords for the named public figure. Every item requires human review: keyword matches include news reporting about threats, condemnations of threats, historical references, and figurative language — not just genuine threats. Nothing here is a verified threat against anyone. Coverage is English-language sources only. This is protective monitoring of public discourse, not a targeting system.";

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

fn rank(item: &ThreatItem) -> u8 {
    match item.severity.as_str() {
        "critical" => 0,
        "high" => 1,
        _ => 2,
    }
}

/// Sort by severity (critical first), then recency (ISO-8601 strings sort
/// lexicographically; items without dates go last).
fn sort_items(items: &mut [ThreatItem]) {
    items.sort_by(|a, b| {
        rank(a)
            .cmp(&rank(b))
            .then_with(|| match (&b.published, &a.published) {
                (Some(pb), Some(pa)) => pb.cmp(pa),
                (Some(_), None) => std::cmp::Ordering::Greater,
                (None, Some(_)) => std::cmp::Ordering::Less,
                (None, None) => std::cmp::Ordering::Equal,
            })
    });
}

fn snippet(s: &str, max: usize) -> String {
    let s = s.trim().replace(|c: char| c.is_whitespace(), " ");
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
    let mut sources = Vec::with_capacity(2);
    let mut items: Vec<ThreatItem> = Vec::new();
    scan_gnews(&state, fig, &terms, &mut sources, &mut items).await;
    scan_bluesky(&state, fig, &terms, &mut sources, &mut items).await;

    if severity_filter != "all" {
        items.retain(|i| i.severity == severity_filter);
    }
    sort_items(&mut items);
    items.truncate(SCAN_LIMIT as usize);

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
        let mut items = vec![
            ThreatItem {
                source: "gnews".into(),
                title: "m".into(),
                text_snippet: "".into(),
                url: "".into(),
                published: Some("2026-11-01T10:00:00Z".into()),
                severity: "medium".into(),
                matched_keywords: vec![],
            },
            ThreatItem {
                source: "gnews".into(),
                title: "c-old".into(),
                text_snippet: "".into(),
                url: "".into(),
                published: Some("2026-10-01T10:00:00Z".into()),
                severity: "critical".into(),
                matched_keywords: vec![],
            },
            ThreatItem {
                source: "bluesky".into(),
                title: "c-new".into(),
                text_snippet: "".into(),
                url: "".into(),
                published: Some("2026-11-01T12:00:00Z".into()),
                severity: "critical".into(),
                matched_keywords: vec![],
            },
            ThreatItem {
                source: "bluesky".into(),
                title: "c-nodate".into(),
                text_snippet: "".into(),
                url: "".into(),
                published: None,
                severity: "critical".into(),
                matched_keywords: vec![],
            },
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
    }
}
