//! Sanctions monitor: keyword scanning of public sources for
//! sanctions / tariff / export-control language touching a curated
//! country watchlist.
//!
//! Reuses the threat-scan architecture (`src/threats.rs`): severity
//! tiers with word-boundary matching, per-item source attribution, the
//! KEYWORD SCAN disclaimer style, and the same source-cache discipline
//! (each source fetch reuses its feed's exact cache-key format and TTL,
//! so scans share cache entries with the normal feed endpoints).
//!
//! ## Endpoints
//!
//! - `GET /api/sanctions?country=<name|alias|iso|all>&severity=` —
//!   `country` defaults to `all` (the full 15-country watchlist,
//!   responses grouped per country); a single country does a deep scan.
//!   Sources: Google News RSS + Bluesky + the configured Telegram
//!   channels. Items are classified into severity tiers, filtered to
//!   `severity` (default `all`), sorted by severity then recency,
//!   capped (25 per country, 150 in all-mode). The assembled per-country
//!   scan is cached 900s under `sanctions:scan:…`; all-mode merges the
//!   cached per-country scans.
//!
//! ## Severity tiers (English-first — documented limitation)
//!
//! - **critical**: designation language — `designat*` + `sanction`,
//!   "added to the entity list", asset-freeze language, "new sanctions"
//!   + announced/imposed/unveiled.
//! - **high**: `tariff`, `export control*`, `secondary sanction*`.
//! - **medium**: sanctions debate (`considering/weighing/mulling
//!   sanctions`, `sanctions debate`), waiver talk, sanctions relief /
//!   easing, extension / renewal talk, plus a fallback: any item that
//!   matched the sanctions query but no higher tier is `medium`
//!   ("sanctions mention") — the tiers rank the actionable items up.
//!
//! Matching is case-insensitive over title + text/snippet. Single
//! generic words use word boundaries where a real false positive exists
//! (`waiver` requires `sanction` nearby anyway). Tiers are checked in
//! order and the first match wins; `matched_keywords` lists the terms
//! that fired in the winning tier.
//!
//! ## Deliberately excluded
//!
//! - **No official lists are queried.** OFAC SDN, the EU sanctions map,
//!   and national lists have no keyless machine-readable endpoint
//!   suitable here; inventing coverage is not acceptable. The disclaimer
//!   says on every response: verify against official lists before
//!   trading or taking compliance action.
//! - **Non-English sources**: keyword lists are English-first; every
//!   response states English-language coverage only.
//!
//! ## Cache discipline
//!
//! Source fetches reuse `gnews:q=…:limit=…`, `bluesky:q=…:limit=…`, and
//! `telegram:{channel}` keys. At most 2 upstream query calls per
//! country per TTL window (plus one per Telegram channel, shared across
//! countries). A failed source is recorded in `sources[]` as
//! `ok: false` with a human-readable detail — never silently dropped.

use std::sync::OnceLock;

use axum::{
    Json, Router,
    extract::{Query, State},
};
use futures_util::future::join_all;
use serde::{Deserialize, Serialize};

use crate::{
    error::ApiError,
    feeds,
    routes::{AppState, cached_bytes},
    time::now_iso8601,
};

/// Keyword scan, not verified designations. These are public news and
/// social-media mentions matching sanctions/tariff/export-control
/// keywords for the named country. They are NOT official designations:
/// always verify against OFAC's SDN list, the EU sanctions map, and
/// other official sources before trading or taking compliance action.
/// Keyword matches include news reporting about sanctions, policy
/// debate, speculation, and historical references — not just new
/// measures. Every item requires human review. Coverage is
/// English-language sources only.
pub const DISCLAIMER: &str = "KEYWORD SCAN — NOT VERIFIED DESIGNATIONS. These are public news and social-media mentions matching sanctions, tariff, or export-control keywords for the named country. They are not official designations: always verify against OFAC's SDN list, the EU sanctions map, and other official sources before trading or taking compliance action. Keyword matches include news reporting about sanctions, policy debate, speculation, waivers, and historical references — not just new measures. Every item requires human review. Coverage is English-language sources only.";

static WATCHLIST_JSON: &str = include_str!("../data/sanctions_watchlist.json");

const SCAN_TTL: std::time::Duration = std::time::Duration::from_secs(900);
const PER_COUNTRY_LIMIT: usize = 25;
const ALL_MODE_LIMIT: usize = 150;

// ---------------------------------------------------------------------------
// Watchlist data
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct Country {
    name: String,
    aliases: Vec<String>,
    iso2: String,
    iso3: String,
}

#[derive(Debug, Deserialize)]
struct Dataset {
    #[allow(dead_code)]
    meta: serde_json::Value,
    countries: Vec<Country>,
}

fn dataset() -> &'static Dataset {
    static DATA: OnceLock<Dataset> = OnceLock::new();
    DATA.get_or_init(|| {
        serde_json::from_str(WATCHLIST_JSON).expect("data/sanctions_watchlist.json parses")
    })
}

/// Find a country by name, alias, or ISO2/ISO3 (case-insensitive).
pub(crate) fn find_country(q: &str) -> Option<&'static Country> {
    let q = q.trim().to_lowercase();
    dataset().countries.iter().find(|c| {
        c.name.to_lowercase() == q
            || c.iso2.to_lowercase() == q
            || c.iso3.to_lowercase() == q
            || c.aliases.iter().any(|a| a.to_lowercase() == q)
    })
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

/// Word-boundary substring check on already-lowercased text.
fn contains_word(haystack: &str, needle: &str) -> bool {
    haystack
        .split(|c: char| !c.is_alphanumeric())
        .any(|w| w == needle)
}

/// Classify text into the highest matching sanctions tier. Returns the
/// tier and the keywords that fired, or `None` when nothing matches —
/// callers apply the "sanctions mention" medium fallback for items that
/// matched the sanctions query but no tier.
fn classify(text: &str) -> Option<(Severity, Vec<String>)> {
    let t = text.to_lowercase();
    // Critical: designation language.
    let mut crit = Vec::new();
    if t.contains("designat") && t.contains("sanction") {
        crit.push("designation + sanctions".to_string());
    }
    if t.contains("entity list") {
        crit.push("entity list".to_string());
    }
    if t.contains("asset freez")
        || t.contains("assets frozen")
        || t.contains("freeze assets")
        || t.contains("frozen asset")
    {
        crit.push("asset freeze".to_string());
    }
    if t.contains("new sanction")
        && (t.contains("announc") || t.contains("impos") || t.contains("unveil"))
    {
        crit.push("new sanctions announced".to_string());
    }
    if t.contains("sanction") && (t.contains("impos") || t.contains("slap")) {
        // "sanctions imposed on X" without the word "new".
        crit.push("sanctions imposed".to_string());
    }
    if !crit.is_empty() {
        return Some((Severity::Critical, crit));
    }
    // High: tariffs, export controls, secondary sanctions.
    let mut high = Vec::new();
    if t.contains("tariff") {
        high.push("tariff".to_string());
    }
    if t.contains("export control") {
        high.push("export controls".to_string());
    }
    if t.contains("secondary sanction") {
        high.push("secondary sanctions".to_string());
    }
    if !high.is_empty() {
        return Some((Severity::High, high));
    }
    // Medium: debate, waivers, relief/easing, extensions.
    let mut med = Vec::new();
    if (t.contains("consider") || t.contains("weighing") || t.contains("mull"))
        && t.contains("sanction")
        || t.contains("debate") && t.contains("sanction")
    {
        med.push("sanctions debate".to_string());
    }
    if contains_word(&t, "waiver") && t.contains("sanction") {
        med.push("sanctions waiver".to_string());
    }
    if t.contains("sanctions relief")
        || t.contains("easing sanction")
        || t.contains("ease sanction")
        || t.contains("lift sanction")
    {
        med.push("sanctions relief".to_string());
    }
    if t.contains("sanction")
        && (t.contains("extend") || t.contains("renew") || t.contains("extension"))
    {
        med.push("sanctions extension".to_string());
    }
    if !med.is_empty() {
        return Some((Severity::Medium, med));
    }
    None
}

// ---------------------------------------------------------------------------
// Response shapes
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct SanctionItem {
    source: String,
    country: String,
    title: String,
    text_snippet: String,
    url: String,
    published: Option<String>,
    severity: String,
    matched_keywords: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct CountrySummary {
    country: String,
    iso2: String,
    iso3: String,
    count: usize,
    critical: usize,
    high: usize,
    medium: usize,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SourceStatus {
    source: String,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct SanctionsResponse {
    mode: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    country: Option<String>,
    pub count: usize,
    /// Per-country rollup (single entry in country mode).
    countries: Vec<CountrySummary>,
    items: Vec<SanctionItem>,
    sources: Vec<SourceStatus>,
    scanned_at: String,
    disclaimer: String,
}

// ---------------------------------------------------------------------------
// Scan assembly
// ---------------------------------------------------------------------------

fn rank(severity: &str) -> u8 {
    match severity {
        "critical" => 0,
        "high" => 1,
        _ => 2,
    }
}

/// Sort by severity (critical first), then recency (ISO-8601 strings
/// sort lexicographically; items without dates go last).
fn sort_items(items: &mut [SanctionItem]) {
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

fn snippet(s: &str, max: usize) -> String {
    // Google News descriptions embed raw HTML; strip tags.
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
        // Truncate on a char boundary — byte slicing can split a
        // multi-byte codepoint (e.g. Arabic-script snippets).
        let mut end = max.min(s.len());
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", s[..end].trim_end())
    }
}

/// Classify a candidate text for a country. Items matching no tier get
/// the medium "sanctions mention" fallback — they matched the sanctions
/// query, so they belong in the scan, ranked below actionable tiers.
fn classify_item(country: &str, title: &str, text: &str) -> Option<(Severity, Vec<String>)> {
    let _ = country;
    let joined = format!("{title} {text}");
    match classify(&joined) {
        Some(hit) => Some(hit),
        None if joined.to_lowercase().contains("sanction") => {
            Some((Severity::Medium, vec!["sanctions mention".to_string()]))
        }
        None => None,
    }
}

async fn scan_gnews(state: &AppState, c: &Country) -> (Vec<SanctionItem>, SourceStatus) {
    // OR/quotes verified against the live gnews RSS endpoint (see threats.rs).
    let query = format!(
        "\"{}\" (sanction OR sanctions OR tariff OR tariffs OR \"export controls\" OR \"entity list\")",
        c.name
    );
    let limit = PER_COUNTRY_LIMIT as u32;
    let key = format!("gnews:q={query}:limit={limit}");
    let client = state.client.clone();
    let res: Result<feeds::gnews::GnewsResponse, ApiError> = async {
        let (body, _) = cached_bytes(state, &key, feeds::gnews::TTL, || {
            feeds::gnews::fetch(&client, &query, limit)
        })
        .await?;
        serde_json::from_slice(&body)
            .map_err(|e| ApiError::bad_gateway("gnews decode failed", e.to_string()))
    }
    .await;
    match res {
        Ok(r) => {
            let mut items = Vec::new();
            for a in r.items {
                let text = a.summary.as_deref().unwrap_or("");
                if let Some((sev, kws)) = classify_item(&c.name, &a.title, text) {
                    items.push(SanctionItem {
                        source: "gnews".into(),
                        country: c.name.clone(),
                        title: a.title.clone(),
                        text_snippet: snippet(text, 280),
                        url: a.link.clone(),
                        published: a.published.clone(),
                        severity: sev.as_str().into(),
                        matched_keywords: kws,
                    });
                }
            }
            (
                items,
                SourceStatus {
                    source: "gnews".into(),
                    ok: true,
                    detail: None,
                },
            )
        }
        Err(e) => (
            Vec::new(),
            SourceStatus {
                source: "gnews".into(),
                ok: false,
                detail: Some(e.user_message()),
            },
        ),
    }
}

async fn scan_bluesky(state: &AppState, c: &Country) -> (Vec<SanctionItem>, SourceStatus) {
    // Parenthesized OR is unsupported by the AppView search (verified
    // live); keep the query simple — tiering happens locally.
    let query = format!("\"{}\" sanctions", c.name);
    let limit = PER_COUNTRY_LIMIT as u32;
    let key = format!("bluesky:q={query}:limit={limit}");
    let client = state.client.clone();
    let res: Result<feeds::bluesky::BlueskyResponse, ApiError> = async {
        let (body, _) = cached_bytes(state, &key, feeds::bluesky::TTL, || {
            feeds::bluesky::fetch(&client, &query, limit)
        })
        .await?;
        serde_json::from_slice(&body)
            .map_err(|e| ApiError::bad_gateway("bluesky decode failed", e.to_string()))
    }
    .await;
    match res {
        Ok(r) => {
            let mut items = Vec::new();
            for p in r.posts {
                if let Some((sev, kws)) = classify_item(&c.name, "", &p.text) {
                    items.push(SanctionItem {
                        source: "bluesky".into(),
                        country: c.name.clone(),
                        title: format!("Post by @{}", p.author_handle),
                        text_snippet: snippet(&p.text, 280),
                        url: p.url.clone(),
                        published: Some(p.created_at.clone()),
                        severity: sev.as_str().into(),
                        matched_keywords: kws,
                    });
                }
            }
            (
                items,
                SourceStatus {
                    source: "bluesky".into(),
                    ok: true,
                    detail: None,
                },
            )
        }
        Err(e) => (
            Vec::new(),
            SourceStatus {
                source: "bluesky".into(),
                ok: false,
                detail: Some(e.user_message()),
            },
        ),
    }
}

/// Scan the configured Telegram channels for country mentions, then
/// classify. Channel snapshots are cached under the same
/// `telegram:{channel}` key the /api/telegram handler uses.
async fn scan_telegram(state: &AppState, c: &Country) -> (Vec<SanctionItem>, SourceStatus) {
    let channels: Vec<String> = state.config.telegram_channels.clone();
    if channels.is_empty() {
        return (
            Vec::new(),
            SourceStatus {
                source: "telegram".into(),
                ok: false,
                detail: Some(
                    "not configured — set TELEGRAM_CHANNELS to public channel names to enable Telegram monitoring"
                        .into(),
                ),
            },
        );
    }
    let mut terms: Vec<String> = c.aliases.iter().map(|a| a.to_lowercase()).collect();
    terms.push(c.name.to_lowercase());
    let mut items = Vec::new();
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
                    let mt = m.text.to_lowercase();
                    if !terms.iter().any(|t| mt.contains(t.as_str())) {
                        continue;
                    }
                    if let Some((sev, kws)) = classify_item(&c.name, "", &m.text) {
                        items.push(SanctionItem {
                            source: "telegram".into(),
                            country: c.name.clone(),
                            title: format!("Post in @{channel}"),
                            text_snippet: snippet(&m.text, 280),
                            url: m.url.clone(),
                            published: Some(m.date.clone()),
                            severity: sev.as_str().into(),
                            matched_keywords: kws,
                        });
                    }
                }
            }
            Err(e) => failures.push(format!("{channel}: {}", e.user_message())),
        }
    }
    (
        items,
        SourceStatus {
            source: "telegram".into(),
            ok: any_ok,
            detail: if failures.is_empty() {
                None
            } else {
                Some(failures.join("; "))
            },
        },
    )
}

/// One country's full scan: the three sources run concurrently, items
/// are severity-filtered, sorted, and capped. The assembled scan is
/// cached 900s so all-mode merges share it.
async fn scan_country(
    state: &AppState,
    c: &Country,
    severity_filter: &str,
) -> (Vec<SanctionItem>, Vec<SourceStatus>) {
    let skey = format!(
        "sanctions:scan:country={}:severity={severity_filter}",
        c.name.to_lowercase()
    );
    if let Some(body) = state.cache.get(&skey)
        && let Ok(cached) = serde_json::from_slice::<CachedScan>(&body)
    {
        return (cached.items, cached.sources);
    }
    let ((mut gi, gs), (mut bi, bs), (mut ti, ts)) = tokio::join!(
        scan_gnews(state, c),
        scan_bluesky(state, c),
        scan_telegram(state, c)
    );
    let mut items = Vec::new();
    items.append(&mut gi);
    items.append(&mut bi);
    items.append(&mut ti);
    let sources = vec![gs, bs, ts];
    if severity_filter != "all" {
        items.retain(|i| i.severity == severity_filter);
    }
    sort_items(&mut items);
    items.truncate(PER_COUNTRY_LIMIT);
    let cached = CachedScan {
        items: items.clone(),
        sources: sources
            .iter()
            .map(|s| SourceStatus {
                source: s.source.clone(),
                ok: s.ok,
                detail: s.detail.clone(),
            })
            .collect(),
    };
    if let Ok(body) = serde_json::to_vec(&cached) {
        state.cache.put(skey, body, SCAN_TTL);
    }
    (items, sources)
}

#[derive(Debug, Serialize, Deserialize)]
struct CachedScan {
    items: Vec<SanctionItem>,
    sources: Vec<SourceStatus>,
}

fn summarize(c: &Country, items: &[SanctionItem]) -> CountrySummary {
    let (mut critical, mut high, mut medium) = (0, 0, 0);
    for i in items {
        match i.severity.as_str() {
            "critical" => critical += 1,
            "high" => high += 1,
            _ => medium += 1,
        }
    }
    CountrySummary {
        country: c.name.clone(),
        iso2: c.iso2.clone(),
        iso3: c.iso3.clone(),
        count: items.len(),
        critical,
        high,
        medium,
    }
}

// ---------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct SanctionsQuery {
    pub country: Option<String>,
    pub severity: Option<String>,
}

/// `GET /api/sanctions?country=<name|alias|iso|all>&severity=all|critical|high|medium`.
/// `country` defaults to `all` (the full watchlist, grouped per
/// country); a single country name does a deep scan.
pub async fn scan(
    State(state): State<AppState>,
    Query(q): Query<SanctionsQuery>,
) -> Result<Json<SanctionsResponse>, ApiError> {
    let severity_filter = q.severity.as_deref().unwrap_or("all").to_lowercase();
    if !["all", "critical", "high", "medium"].contains(&severity_filter.as_str()) {
        return Err(ApiError::bad_request(
            "bad severity",
            "severity must be one of: all, critical, high, medium",
        ));
    }
    let country_q = q
        .country
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let all_mode = country_q
        .map(|s| s.eq_ignore_ascii_case("all"))
        .unwrap_or(true);

    let mut all_items: Vec<SanctionItem> = Vec::new();
    let mut countries: Vec<CountrySummary> = Vec::new();
    let mut sources: Vec<SourceStatus> = Vec::new();
    let mut mode_country: Option<String> = None;

    if all_mode {
        // Countries scan concurrently; each country's assembled scan is
        // independently cached, so repeat hits are cheap.
        let per: Vec<(Vec<SanctionItem>, Vec<SourceStatus>)> =
            join_all(dataset().countries.iter().map(|c| {
                let state = &state;
                let sev = severity_filter.as_str();
                async move { scan_country(state, c, sev).await }
            }))
            .await;
        let mut seen_src: std::collections::HashSet<String> = std::collections::HashSet::new();
        for (c, (items, srcs)) in dataset().countries.iter().zip(per) {
            countries.push(summarize(c, &items));
            all_items.extend(items);
            for s in srcs {
                // gnews/bluesky statuses are per-country query runs;
                // keep one entry per source with a rollup of ok-ness.
                if seen_src.insert(s.source.clone()) {
                    sources.push(s);
                } else if !s.ok
                    && let Some(existing) = sources.iter_mut().find(|e| e.source == s.source)
                {
                    existing.ok = false;
                    existing.detail = s.detail;
                }
            }
        }
        sort_items(&mut all_items);
        all_items.truncate(ALL_MODE_LIMIT);
    } else {
        let c = find_country(country_q.unwrap()).ok_or_else(|| {
            ApiError::not_found(
                "unknown country",
                format!(
                    "no sanctions-watchlist entry for {:?}; use country=all for the full list",
                    country_q.unwrap()
                ),
            )
        })?;
        let (items, srcs) = scan_country(&state, c, &severity_filter).await;
        countries.push(summarize(c, &items));
        all_items = items;
        sources = srcs;
        mode_country = Some(c.name.clone());
    }

    Ok(Json(SanctionsResponse {
        mode: if all_mode {
            "all".into()
        } else {
            "country".into()
        },
        country: mode_country,
        count: all_items.len(),
        countries,
        items: all_items,
        sources,
        scanned_at: now_iso8601(),
        disclaimer: DISCLAIMER.into(),
    }))
}

pub fn routes() -> Router<crate::routes::AppState> {
    use axum::routing::get;
    Router::new().route("/api/sanctions", get(scan))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn watchlist_parses_with_fifteen_countries() {
        let d = dataset();
        assert_eq!(d.countries.len(), 15);
        for c in &d.countries {
            assert!(!c.name.is_empty() && !c.iso2.is_empty() && !c.iso3.is_empty());
        }
    }

    #[test]
    fn lookup_by_name_alias_and_iso() {
        assert_eq!(find_country("North Korea").unwrap().iso2, "KP");
        assert_eq!(find_country("dprk").unwrap().name, "North Korea");
        assert_eq!(find_country("RU").unwrap().name, "Russia");
        assert_eq!(find_country("rus").unwrap().name, "Russia");
        assert_eq!(find_country("  Burma ").unwrap().name, "Myanmar");
        assert!(find_country("Narnia").is_none());
    }

    #[test]
    fn classify_critical_tier() {
        let (s, k) =
            classify("US designates 12 firms in new sanctions round against Iran").unwrap();
        assert_eq!(s, Severity::Critical);
        assert!(k.iter().any(|k| k.contains("designation")));
        let (s, k) =
            classify("Three oligarchs added to the entity list over export violations").unwrap();
        assert_eq!(s, Severity::Critical);
        assert!(k.contains(&"entity list".to_string()));
        let (s, _) = classify("Treasury orders asset freeze on Russian bank accounts").unwrap();
        assert_eq!(s, Severity::Critical);
        let (s, _) =
            classify("White House announces new sanctions targeting Belarus officials").unwrap();
        assert_eq!(s, Severity::Critical);
        let (s, _) = classify("EU imposes sanctions on Myanmar junta-linked firms").unwrap();
        assert_eq!(s, Severity::Critical);
    }

    #[test]
    fn classify_high_tier() {
        let (s, k) = classify("Trump threatens 100% tariffs on Chinese goods").unwrap();
        assert_eq!(s, Severity::High);
        assert!(k.contains(&"tariff".to_string()));
        let (s, _) = classify("New export controls on AI chips to China unveiled").unwrap();
        assert_eq!(s, Severity::High);
        let (s, _) =
            classify("US warns of secondary sanctions on banks dealing with Russia").unwrap();
        assert_eq!(s, Severity::High);
    }

    #[test]
    fn classify_medium_tier() {
        let (s, _) = classify("Senators debate whether sanctions on Iran are working").unwrap();
        assert_eq!(s, Severity::Medium);
        let (s, _) = classify("EU extends sanctions on Russia for another six months").unwrap();
        assert_eq!(s, Severity::Medium);
        let (s, _) = classify("US grants sanctions waiver for humanitarian aid to Syria").unwrap();
        assert_eq!(s, Severity::Medium);
        let (s, _) = classify("Calls grow for easing sanctions on Venezuela oil").unwrap();
        assert_eq!(s, Severity::Medium);
    }

    #[test]
    fn classify_rejects_noise() {
        // No sanctions language at all.
        assert!(classify("Russia wins hockey match in overtime").is_none());
        // "sanctioned" as an official event endorsement is not sanctions.
        assert!(classify("The event was sanctioned by the athletics body").is_none());
    }

    #[test]
    fn fallback_medium_for_query_matches() {
        // Matched the sanctions query ("sanctions") but no specific tier
        // keyword → medium "sanctions mention".
        let (s, k) = classify_item(
            "Iran",
            "Fresh sanctions on Iran draw mixed reaction",
            "analysts react",
        )
        .unwrap();
        assert_eq!(s, Severity::Medium);
        assert!(k.contains(&"sanctions mention".to_string()));
        // No "sanction" anywhere → dropped.
        assert!(classify_item("Iran", "Tehran traffic worsens", "commuters complain").is_none());
    }

    #[test]
    fn tier_order_wins_over_lower_matches() {
        let (s, _) =
            classify("New tariffs accompany designation of firms under Russia sanctions").unwrap();
        assert_eq!(s, Severity::Critical);
    }

    #[test]
    fn disclaimer_documents_verification_requirement() {
        assert!(DISCLAIMER.contains("NOT VERIFIED DESIGNATIONS"));
        assert!(DISCLAIMER.contains("OFAC"));
        assert!(DISCLAIMER.contains("before trading"));
    }

    #[test]
    fn sort_orders_severity_then_recency() {
        fn item(title: &str, severity: &str, published: Option<&str>) -> SanctionItem {
            SanctionItem {
                source: "gnews".into(),
                country: "Iran".into(),
                title: title.into(),
                text_snippet: "".into(),
                url: "".into(),
                published: published.map(str::to_string),
                severity: severity.into(),
                matched_keywords: vec![],
            }
        }
        let mut items = vec![
            item("m1", "medium", Some("2026-11-02T10:00:00Z")),
            item("c1", "critical", Some("2026-11-01T10:00:00Z")),
            item("h1", "high", None),
            item("c2", "critical", Some("2026-11-02T09:00:00Z")),
        ];
        sort_items(&mut items);
        let titles: Vec<&str> = items.iter().map(|i| i.title.as_str()).collect();
        assert_eq!(titles, vec!["c2", "c1", "h1", "m1"]);
    }
}
