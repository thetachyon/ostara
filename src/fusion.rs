//! Fusion layer: proprietary curated indicators that fuse multiple
//! feeds into labeled, scored signals with cited evidence.
//!
//! The value here is **curation and labeling, not the feeds**. The feeds
//! are public; the IP is the recipe: which inputs are fused per
//! indicator, their weights, the per-input normalization transforms, and
//! the label thresholds. The recipe lives in
//! `data/fusion_indicators.json` (embedded via `include_str!`); the
//! mechanics live below.
//!
//! ## Method
//!
//! Each indicator declares named inputs with a weight and a transform.
//! Every input is normalized to 0–100 by its transform (documented in
//! the registry meta and in [`apply_transform`]), then:
//!
//! ```text
//! score = Σ (wᵢ′ · vᵢ)   over live inputs
//! wᵢ′ = wᵢ / Σ wⱼ        (weights renormalized over live inputs only)
//! ```
//!
//! A degraded or missing input contributes nothing: its weight is
//! redistributed, never silently zeroed. Confidence is the fraction of
//! total weight that was live: ≥0.8 high, ≥0.5 medium, below low.
//!
//! Level thresholds: <25 calm, 25–50 watch, 50–75 elevated, ≥75 critical.
//! Each indicator carries a curated one-line `label` per level — the
//! analyst-facing read. The `methodology` field on every response states
//! the arithmetic in words (transparent mechanics, proprietary recipe).
//!
//! ## Input fetching
//!
//! All inputs are fetched **in-process** through the same functions the
//! HTTP handlers use, sharing their cache entries (the same cache keys
//! and TTLs as `/api/prices`, `/api/sanctions`, etc.), so a fusion
//! evaluation never doubles upstream traffic.
//!
//! ## Honest edges
//!
//! - Vessel density is conditional: a zero count with a healthy vessels
//!   feed is a coverage gap (the free AIS tier has sparse Gulf coverage —
//!   verified 2026-11-02), so the input is marked degraded and its weight
//!   redistributed — never scored as zero risk.
//! - The chokepoint status is a dated 2026-11-02 snapshot; it is weighted
//!   as context (0.10) and labeled as such in the evidence.
//! - When no input is live, the indicator returns `score: null` and
//!   `level: "unknown"` rather than a fabricated number.

use std::sync::OnceLock;

use axum::{
    Json, Router,
    extract::{Query, State},
};
use futures_util::future::join_all;
use serde::{Deserialize, Serialize};

use crate::{
    error::{ApiError, FeedError},
    feeds,
    routes::AppState,
    time::now_iso8601,
};

pub const FEED: &str = "fusion";
const FUSION_TTL: std::time::Duration = std::time::Duration::from_secs(600);

static REGISTRY_JSON: &str = include_str!("../data/fusion_indicators.json");

/// Curated per-level labels — the analyst-facing read. Written in the
/// registry, not the code: labeling is part of the curation.
#[derive(Debug, Clone, Deserialize)]
struct LevelLabels {
    calm: String,
    watch: String,
    elevated: String,
    critical: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct InputParams {
    #[serde(default)]
    symbol: Option<String>,
    #[serde(default)]
    days: Option<u32>,
    #[serde(default)]
    k: Option<f64>,
    #[serde(default)]
    bbox: Option<String>,
    #[serde(default)]
    window_days: Option<u32>,
    #[serde(default)]
    keep: Option<usize>,
}

#[derive(Debug, Clone, Deserialize)]
struct RegistryInput {
    key: String,
    feed: String,
    role: String,
    transform: String,
    weight: f64,
    #[serde(default)]
    params: InputParams,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RegistryIndicator {
    id: String,
    name: String,
    desk: String,
    description: String,
    methodology_note: String,
    inputs: Vec<RegistryInput>,
    labels: LevelLabels,
}

#[derive(Debug, Deserialize)]
struct RegistryFile {
    indicators: Vec<RegistryIndicator>,
    #[allow(dead_code)]
    meta: serde_json::Value,
}

fn registry() -> &'static RegistryFile {
    static DATA: OnceLock<RegistryFile> = OnceLock::new();
    DATA.get_or_init(|| {
        serde_json::from_str(REGISTRY_JSON).expect("data/fusion_indicators.json parses")
    })
}

/// The curated registry, for health/stats. A corrupt registry is a
/// deploy-time bug.
pub fn load_registry() -> Result<Vec<RegistryIndicator>, String> {
    serde_json::from_str::<RegistryFile>(REGISTRY_JSON)
        .map(|f| f.indicators)
        .map_err(|e| format!("fusion registry invalid: {e}"))
}

fn find_indicator(id: &str) -> Option<&'static RegistryIndicator> {
    registry().indicators.iter().find(|i| i.id == id)
}

// ---------------------------------------------------------------------------
// Transforms: raw input → 0–100. Pure, unit-tested.
// ---------------------------------------------------------------------------

/// Level from score. Boundaries: 25 / 50 / 75.
pub fn level_for(score: f64) -> &'static str {
    if score < 25.0 {
        "calm"
    } else if score < 50.0 {
        "watch"
    } else if score < 75.0 {
        "elevated"
    } else {
        "critical"
    }
}

/// Downgrade indicator confidence to at most medium when any live input
/// rests on a thin-basis risk score. Pure for testing.
fn apply_thin_basis_confidence(confidence: &str, inputs: &[EvaluatedInput]) -> String {
    if confidence == "high" && inputs.iter().any(|i| i.live && i.thin_basis) {
        "medium".to_string()
    } else {
        confidence.to_string()
    }
}

/// Confidence from the fraction of total weight that was live.
pub fn confidence_for(live_weight_fraction: f64) -> &'static str {
    if live_weight_fraction >= 0.8 {
        "high"
    } else if live_weight_fraction >= 0.5 {
        "medium"
    } else {
        "low"
    }
}

/// |% change| * k, clamped to 100. k=12: a 5% move → 60.
fn transform_price_move(pct: f64, k: f64) -> f64 {
    (pct.abs() * k).min(100.0)
}

/// min(count * 25, 100). 4+ critical items saturate.
fn transform_sanctions_criticals(count: usize) -> f64 {
    ((count as f64) * 25.0).min(100.0)
}

/// min(high-importance events * 34, 100). 3+ events saturate.
fn transform_calendar_high(count: usize) -> f64 {
    ((count as f64) * 34.0).min(100.0)
}

/// VIX level → 0–100: (vix − 10) / 40 * 100, clamped. VIX 10→0, 30→50.
fn transform_vix_level(vix: f64) -> f64 {
    ((vix - 10.0) / 40.0 * 100.0).clamp(0.0, 100.0)
}

/// Dated chokepoint status string → 0–100. Keyword-ordered: the most
/// severe match wins.
fn transform_chokepoint_status(status: &str) -> f64 {
    let s = status.to_lowercase();
    if s.contains("block") || s.contains("clos") {
        90.0
    } else if s.contains("disrupt") || s.contains("partial") || s.contains("restrict") {
        65.0
    } else if s.contains("congest") {
        40.0
    } else {
        15.0
    }
}

/// Vessel density: min(count / 40 * 100, 100). Only called when count ≥ 1
/// — a zero count is a coverage gap, handled by the caller as degraded.
fn transform_vessel_density(count: usize) -> f64 {
    ((count as f64) / 40.0 * 100.0).min(100.0)
}

// ---------------------------------------------------------------------------
// Evidence + scoring
// ---------------------------------------------------------------------------

/// One evaluated input: the normalized value, whether it was live, and
/// the evidence to cite.
#[derive(Debug, Clone)]
struct EvaluatedInput {
    key: String,
    feed: String,
    role: String,
    weight: f64,
    live: bool,
    /// 0–100 when live.
    value: Option<f64>,
    /// Human-readable evidence value, e.g. "85.9" or "4 critical items".
    evidence_value: String,
    /// Extra detail for the evidence list.
    detail: String,
    /// True when this input's upstream risk score was flagged thin_basis
    /// (rested on <50% of intended signal). Downgrades the indicator's
    /// confidence to at most medium.
    thin_basis: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct EvidenceItem {
    pub input: String,
    pub source: String,
    pub role: String,
    pub live: bool,
    pub weight: f64,
    /// Renormalized weight actually used (0 when degraded).
    pub weight_used: f64,
    pub value: String,
    pub detail: String,
}

/// Score with weight redistribution over live inputs.
/// Returns (score, live_weight_fraction).
fn score_inputs(inputs: &[EvaluatedInput]) -> (Option<f64>, f64) {
    let total: f64 = inputs.iter().map(|i| i.weight).sum();
    let live_total: f64 = inputs.iter().filter(|i| i.live).map(|i| i.weight).sum();
    if total <= 0.0 || live_total <= 0.0 {
        return (None, 0.0);
    }
    let score: f64 = inputs
        .iter()
        .filter(|i| i.live)
        .map(|i| i.weight / live_total * i.value.unwrap_or(0.0))
        .sum();
    (Some(score.clamp(0.0, 100.0)), live_total / total)
}

// ---------------------------------------------------------------------------
// Input fetchers (in-process, shared cache discipline)
// ---------------------------------------------------------------------------

/// Risk score via the /api/risk handler, in-process. Risk degrades
/// internally and never fails hard, so a successful call is a live input.
/// Returns (score, level, partial, thin_basis).
async fn fetch_risk(
    state: &AppState,
    place: &str,
    kind: Option<&str>,
) -> Option<(f64, String, bool, bool)> {
    let q = crate::risk::RiskQuery {
        place: Some(place.to_string()),
        kind: kind.map(str::to_string),
        country: None,
    };
    let resp = crate::risk::risk(State(state.clone()), Query(q))
        .await
        .ok()?;
    let r = resp.0;
    // A null risk score is an honest unknown — the fusion input degrades
    // rather than carrying a fake number forward.
    Some((r.score?, r.level.clone(), r.partial, r.thin_basis))
}

/// Critical-only sanctions count for a country, in-process. Uses the
/// critical-severity scan (own cache entry, 900s).
async fn fetch_sanctions_criticals(state: &AppState, country: &str) -> Option<usize> {
    let q = crate::sanctions::SanctionsQuery {
        country: Some(country.to_string()),
        severity: Some("critical".to_string()),
    };
    let resp = crate::sanctions::scan(State(state.clone()), Query(q))
        .await
        .ok()?;
    // The scan was requested with severity=critical, so every returned
    // item is critical — the count is the designation-momentum signal.
    Some(resp.0.count)
}

/// Price % move over the last `days` closes + the latest price, via the
/// shared prices cache entry for the single symbol.
async fn fetch_price_move(state: &AppState, symbol: &str, days: u32) -> Option<(f64, f64)> {
    let symbols = vec![symbol.to_string()];
    let key = feeds::prices::cache_key(&symbols);
    let (body, _) = feeds::cached_bytes(state, &key, feeds::prices::TTL, || {
        let client = state.client.clone();
        let symbols = symbols.clone();
        async move { feeds::prices::fetch_quotes(&client, &symbols).await }
    })
    .await
    .ok()?;
    let resp: feeds::prices::PricesResponse = serde_json::from_slice(&body).ok()?;
    let q = resp.quotes.first()?;
    let spark = &q.spark;
    if spark.len() < 2 {
        return None;
    }
    let n = (days as usize + 1).min(spark.len());
    let first = spark[spark.len() - n];
    let last = spark[spark.len() - 1];
    if first == 0.0 {
        return None;
    }
    Some(((last - first) / first * 100.0, q.price))
}

/// Latest price for a symbol (for VIX level), via the shared cache.
async fn fetch_price(state: &AppState, symbol: &str) -> Option<f64> {
    let symbols = vec![symbol.to_string()];
    let key = feeds::prices::cache_key(&symbols);
    let (body, _) = feeds::cached_bytes(state, &key, feeds::prices::TTL, || {
        let client = state.client.clone();
        let symbols = symbols.clone();
        async move { feeds::prices::fetch_quotes(&client, &symbols).await }
    })
    .await
    .ok()?;
    let resp: feeds::prices::PricesResponse = serde_json::from_slice(&body).ok()?;
    Some(resp.quotes.first()?.price)
}

/// Vessel count in a bbox. None when the vessels feed is disabled; the
/// caller treats a healthy-feed zero count as a coverage gap.
async fn fetch_vessel_count(state: &AppState, bbox: &str) -> Option<usize> {
    let stream = state.vessels.clone();
    if !stream.is_enabled() {
        return None;
    }
    let parsed = crate::routes::parse_bbox(bbox).ok()?;
    let vessels = feeds::vessels::filter_and_limit(stream.snapshot(), Some(parsed), 1000);
    Some(vessels.len())
}

/// Days since epoch → YYYY-MM-DD (Howard Hinnant's algorithm; the
/// codebase carries its own copies rather than a chrono dependency).
fn days_to_ymd(z: i64) -> String {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    format!("{:04}-{:02}-{:02}", if m <= 2 { y + 1 } else { y }, m, d)
}

fn today_days() -> i64 {
    let days = feeds::econ_calendar::parse_ymd(&feeds::econ_calendar::today_ymd());
    days.unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Per-indicator evaluation
// ---------------------------------------------------------------------------

async fn eval_static_input(state: &AppState, inp: &RegistryInput) -> EvaluatedInput {
    let base = EvaluatedInput {
        key: inp.key.clone(),
        feed: inp.feed.clone(),
        role: inp.role.clone(),
        weight: inp.weight,
        live: false,
        value: None,
        evidence_value: "unavailable".to_string(),
        detail: String::new(),
        thin_basis: false,
    };
    let live = |value: f64, evidence_value: String, detail: String| EvaluatedInput {
        live: true,
        value: Some(value),
        evidence_value,
        detail,
        ..base.clone()
    };
    match inp.transform.as_str() {
        "risk_passthrough" => {
            // Feed strings look like "/api/risk?place=Iran" or
            // "/api/risk?place=Taiwan&kind=country".
            let place = inp
                .feed
                .split("place=")
                .nth(1)
                .unwrap_or("")
                .split('&')
                .next()
                .unwrap_or("");
            let kind = inp
                .feed
                .split("kind=")
                .nth(1)
                .map(|s| s.split('&').next().unwrap_or("auto").to_string());
            // URL-decode the place (registry uses plain names; be lenient).
            let place = percent_decode(place);
            match fetch_risk(state, &place, kind.as_deref()).await {
                Some((score, level, partial, thin)) => {
                    let mut e = live(
                        score,
                        format!("{score:.1} ({level})"),
                        format!(
                            "composite risk score for {place}{}{}",
                            if partial {
                                " · partial (some feeds down)"
                            } else {
                                ""
                            },
                            if thin {
                                " · THIN BASIS (<50% of intended signal) — treat as provisional"
                            } else {
                                ""
                            }
                        ),
                    );
                    e.thin_basis = thin;
                    e
                }
                None => base,
            }
        }
        "sanctions_criticals" => {
            let country = inp
                .feed
                .split("country=")
                .nth(1)
                .unwrap_or("")
                .split('&')
                .next()
                .unwrap_or("");
            let country = percent_decode(country);
            match fetch_sanctions_criticals(state, &country).await {
                Some(n) => live(
                    transform_sanctions_criticals(n),
                    format!("{n} critical item{s}", s = if n == 1 { "" } else { "s" }),
                    format!(
                        "keyword-flagged designation-language mentions for {country} (critical tier)"
                    ),
                ),
                None => base,
            }
        }
        "price_move_pct" => {
            let symbol = inp.params.symbol.as_deref().unwrap_or("");
            let days = inp.params.days.unwrap_or(5);
            let k = inp.params.k.unwrap_or(12.0);
            match fetch_price_move(state, symbol, days).await {
                Some((pct, price)) => live(
                    transform_price_move(pct, k),
                    format!("{pct:+.2}% over {days}d ({symbol} @ {price:.2})"),
                    format!("absolute {days}-day move × {k}, clamped to 100"),
                ),
                None => base,
            }
        }
        "vix_level" => {
            let symbol = inp.params.symbol.as_deref().unwrap_or("^VIX");
            match fetch_price(state, symbol).await {
                Some(vix) => live(
                    transform_vix_level(vix),
                    format!("VIX {vix:.1}"),
                    "spot level mapped (VIX−10)/40 → 0–100".to_string(),
                ),
                None => base,
            }
        }
        "calendar_high_7d" => match calendar_high_count(7) {
            Ok(n) => live(
                transform_calendar_high(n),
                format!(
                    "{n} high-importance event{s} in 7d",
                    s = if n == 1 { "" } else { "s" }
                ),
                "curated econ-calendar registry (static, always live)".to_string(),
            ),
            Err(_) => base,
        },
        "chokepoint_status" => match chokepoint_status("hormuz") {
            Some(status) => live(
                transform_chokepoint_status(&status),
                status.clone(),
                "DATED SNAPSHOT (2026-11-02) — context only, verify before operational use"
                    .to_string(),
            ),
            None => base,
        },
        "vessel_density" => {
            let bbox = inp.params.bbox.as_deref().unwrap_or("55,24,58,27");
            match fetch_vessel_count(state, bbox).await {
                // Conditional input: a zero count with a healthy feed is
                // a coverage gap (free AIS tier has sparse Gulf
                // coverage), NOT zero risk — mark degraded so the weight
                // redistributes.
                Some(0) => EvaluatedInput {
                    detail: "0 vessels visible in bbox — free AIS tier has sparse Gulf coverage; treated as a coverage gap, weight redistributed".to_string(),
                    ..base
                },
                Some(n) => live(
                    transform_vessel_density(n),
                    format!("{n} vessel{s} in strait bbox", s = if n == 1 { "" } else { "s" }),
                    "live AIS snapshot density (exposure proxy)".to_string(),
                ),
                None => EvaluatedInput {
                    detail: "vessels feed disabled (AIS_API_KEY unset)".to_string(),
                    ..base
                },
            }
        }
        _ => base,
    }
}

/// High-importance calendar events in the next `days` days. The registry
/// is static, so this is infallible in practice — but the Result keeps
/// the honest edge if the registry ever fails to parse.
fn calendar_high_count(days: u32) -> Result<usize, String> {
    let reg = feeds::econ_calendar::load_registry()?;
    let today = today_days();
    let to = days_to_ymd(today + days as i64);
    let events = feeds::econ_calendar::window_events(&reg, today, None, Some(&to), None)
        .map_err(|e| e.user_message())?;
    Ok(events
        .iter()
        .filter(|e| e.event.importance.eq_ignore_ascii_case("high"))
        .count())
}

fn chokepoint_status(id: &str) -> Option<String> {
    let reg = feeds::chokepoints::load_registry().ok()?;
    reg.into_iter()
        .find(|e| e.id.eq_ignore_ascii_case(id))
        .map(|e| e.status)
}

/// Minimal percent-decoding for registry query fragments. Byte-wise,
/// then UTF-8 lossy — so multi-byte sequences (e.g. %C3%B4 → ô) decode
/// correctly instead of producing mojibake.
fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 3 <= b.len() => {
                match std::str::from_utf8(&b[i + 1..i + 3])
                    .ok()
                    .and_then(|hex| u8::from_str_radix(hex, 16).ok())
                {
                    Some(byte) => {
                        out.push(byte);
                        i += 3;
                    }
                    None => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The dynamic election indicator: elections in the next 30 days, top-3
/// countries by risk score, weights 0.5/0.3/0.2 renormalized.
async fn eval_election_indicator(state: &AppState, def: &RegistryIndicator) -> Vec<EvaluatedInput> {
    let window_days = def
        .inputs
        .first()
        .and_then(|i| i.params.window_days)
        .unwrap_or(30);
    let keep = def.inputs.first().and_then(|i| i.params.keep).unwrap_or(3);
    let weights = [0.5, 0.3, 0.2];

    let elections: Vec<(String, String)> = match feeds::econ_calendar::load_registry() {
        Ok(reg) => {
            let today = today_days();
            let to = days_to_ymd(today + window_days as i64);
            match feeds::econ_calendar::window_events(
                &reg,
                today,
                None,
                Some(&to),
                Some("elections"),
            ) {
                Ok(evs) => evs
                    .into_iter()
                    .map(|e| (e.event.country_org.clone(), e.event.name.clone()))
                    .collect(),
                Err(_) => Vec::new(),
            }
        }
        Err(_) => Vec::new(),
    };

    if elections.is_empty() {
        // No elections in window: a real, high-confidence calm — not a
        // missing input. Represented as one synthetic live input at 0.
        return vec![EvaluatedInput {
            key: "no_elections_in_window".to_string(),
            feed: "/api/calendar?kind=elections".to_string(),
            role: format!("elections in next {window_days}d"),
            weight: 1.0,
            live: true,
            value: Some(0.0),
            evidence_value: "none scheduled".to_string(),
            detail: "curated calendar registry — complete by construction".to_string(),
            thin_basis: false,
        }];
    }

    // Score each election country concurrently; keep the top `keep`.
    let scored: Vec<(String, String, f64, String, bool)> =
        join_all(elections.iter().map(|(c, name)| {
            let state = state.clone();
            let c = c.clone();
            let name = name.clone();
            async move {
                fetch_risk(&state, &c, None)
                    .await
                    .map(|(score, level, _, thin)| (c, name, score, level, thin))
            }
        }))
        .await
        .into_iter()
        .flatten()
        .collect();

    let mut ranked = scored;
    ranked.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));
    ranked.truncate(keep);

    let live_w: f64 = weights.iter().take(ranked.len()).sum();
    ranked
        .into_iter()
        .enumerate()
        .map(
            |(i, (country, event_name, score, level, thin))| EvaluatedInput {
                key: format!("election_risk:{country}"),
                feed: format!("/api/risk?place={country}"),
                role: format!("election-country risk ({event_name})"),
                weight: weights.get(i).copied().unwrap_or(0.0) / live_w.max(f64::MIN_POSITIVE),
                live: true,
                value: Some(score),
                evidence_value: format!("{country}: {score:.1} ({level})"),
                detail: format!(
                    "risk score for election country {country}{}",
                    if thin {
                        " · THIN BASIS (<50% of intended signal) — treat as provisional"
                    } else {
                        ""
                    }
                ),
                thin_basis: thin,
            },
        )
        .collect()
}

// ---------------------------------------------------------------------------
// Response shapes
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize)]
pub struct FusionIndicator {
    pub id: String,
    pub name: String,
    pub desk: String,
    pub description: String,
    /// 0–100, or null when no input was live.
    pub score: Option<f64>,
    /// calm | watch | elevated | critical | unknown (unknown = no live inputs).
    pub level: String,
    /// Curated one-line analyst read for the level.
    pub label: String,
    /// high | medium | low — fraction of input weight that was live.
    pub confidence: String,
    /// e.g. "5/6".
    pub live_inputs: String,
    pub evidence: Vec<EvidenceItem>,
    pub methodology: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct FusionResponse {
    pub generated_at: String,
    pub count: usize,
    pub indicators: Vec<FusionIndicator>,
    pub disclaimer: String,
}

pub const DISCLAIMER: &str = "FUSION INDICATORS — curated heuristic signals, not forecasts and not trading advice. Each score fuses public feeds through proprietary weights, per-input normalization transforms, and label thresholds; weights redistribute over live inputs when a feed is degraded, and missing inputs lower confidence rather than fabricate values. The chokepoint status is a dated 2026-11-02 snapshot (context only). Verify all underlying facts before trading.";

fn label_for(def: &RegistryIndicator, level: &str) -> String {
    match level {
        "calm" => def.labels.calm.clone(),
        "watch" => def.labels.watch.clone(),
        "elevated" => def.labels.elevated.clone(),
        "critical" => def.labels.critical.clone(),
        _ => "Insufficient live inputs to score this indicator.".to_string(),
    }
}

async fn evaluate(state: &AppState, def: &RegistryIndicator) -> FusionIndicator {
    let inputs: Vec<EvaluatedInput> = if def.id == "election-unrest-risk" {
        eval_election_indicator(state, def).await
    } else {
        join_all(def.inputs.iter().map(|i| eval_static_input(state, i))).await
    };

    let (score, live_frac) = score_inputs(&inputs);
    let live_w: f64 = inputs.iter().filter(|i| i.live).map(|i| i.weight).sum();
    let (level, label) = match score {
        Some(s) => {
            let l = level_for(s);
            (l.to_string(), label_for(def, l))
        }
        None => ("unknown".to_string(), label_for(def, "unknown")),
    };
    let live_n = inputs.iter().filter(|i| i.live).count();
    // A thin-basis risk input caps the indicator's confidence at medium:
    // the score is real, but part of its foundation is provisional.
    let confidence = apply_thin_basis_confidence(confidence_for(live_frac), &inputs);

    // Methodology in words: the arithmetic, with renormalized weights.
    let terms: Vec<String> = inputs
        .iter()
        .filter(|i| i.live)
        .map(|i| {
            let w_used = if live_w > 0.0 { i.weight / live_w } else { 0.0 };
            format!("{:.2}×{}({:.1})", w_used, i.key, i.value.unwrap_or(0.0))
        })
        .collect();
    let degraded: Vec<String> = inputs
        .iter()
        .filter(|i| !i.live)
        .map(|i| i.key.clone())
        .collect();
    let methodology = match score {
        Some(s) => format!(
            "score = {} = {:.1} → {}. {}{}",
            terms.join(" + "),
            s,
            level,
            def.methodology_note,
            if degraded.is_empty() {
                String::new()
            } else {
                format!(
                    " Degraded inputs (weight redistributed): {}.",
                    degraded.join(", ")
                )
            }
        ),
        None => format!("no live inputs — score withheld. {}", def.methodology_note),
    };

    let evidence = inputs
        .iter()
        .map(|i| EvidenceItem {
            input: i.key.clone(),
            source: i.feed.clone(),
            role: i.role.clone(),
            live: i.live,
            weight: (i.weight * 100.0).round() / 100.0,
            weight_used: if i.live && live_w > 0.0 {
                (i.weight / live_w * 100.0).round() / 100.0
            } else {
                0.0
            },
            value: i.evidence_value.clone(),
            detail: i.detail.clone(),
        })
        .collect();

    FusionIndicator {
        id: def.id.clone(),
        name: def.name.clone(),
        desk: def.desk.clone(),
        description: def.description.clone(),
        score: score.map(|s| (s * 100.0).round() / 100.0),
        level,
        label,
        confidence,
        live_inputs: format!("{live_n}/{}", inputs.len()),
        evidence,
        methodology,
    }
}

// ---------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct FusionQuery {
    pub indicator: Option<String>,
}

async fn evaluate_all(state: &AppState) -> Result<FusionResponse, ApiError> {
    let indicators = join_all(registry().indicators.iter().map(|d| evaluate(state, d))).await;
    Ok(FusionResponse {
        generated_at: now_iso8601(),
        count: indicators.len(),
        indicators,
        disclaimer: DISCLAIMER.to_string(),
    })
}

async fn evaluate_one(state: &AppState, id: &str) -> Result<FusionResponse, ApiError> {
    let def = find_indicator(id).ok_or_else(|| {
        ApiError::not_found(
            "unknown indicator",
            format!(
                "no fusion indicator {:?}; known ids: {}",
                id,
                registry()
                    .indicators
                    .iter()
                    .map(|d| d.id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        )
    })?;
    let indicator = evaluate(state, def).await;
    Ok(FusionResponse {
        generated_at: now_iso8601(),
        count: 1,
        indicators: vec![indicator],
        disclaimer: DISCLAIMER.to_string(),
    })
}

/// `GET /api/fusion` — all indicators; `?indicator=<id>` for one.
/// Cached 600s (inputs are cached upstream anyway).
pub async fn fusion(
    State(state): State<AppState>,
    Query(q): Query<FusionQuery>,
) -> Result<Json<FusionResponse>, ApiError> {
    let id = q
        .indicator
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_lowercase());
    // Unknown ids 404 here — inside the cache closure the ApiError would
    // be flattened into a FeedError (502).
    if let Some(ref id) = id
        && find_indicator(id).is_none()
    {
        return Err(ApiError::not_found(
            "unknown indicator",
            format!(
                "no fusion indicator {id:?}; known ids: {}",
                registry()
                    .indicators
                    .iter()
                    .map(|d| d.id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        ));
    }
    let key = format!("fusion:{}", id.as_deref().unwrap_or("all"));
    let (body, _) = feeds::cached_bytes(&state, &key, FUSION_TTL, || {
        let state = state.clone();
        let id = id.clone();
        async move {
            // Ids are pre-validated above, so these cannot 404; the
            // map_err is only a type conversion for the cache closure.
            let to_feed_err = |e: ApiError| FeedError::new(FEED, e.user_message());
            let resp = match id.as_deref() {
                Some(id) => evaluate_one(&state, id).await.map_err(to_feed_err)?,
                None => evaluate_all(&state).await.map_err(to_feed_err)?,
            };
            serde_json::to_value(&resp)
                .map_err(|e| FeedError::new(FEED, format!("serialize failed: {e}")))
        }
    })
    .await?;
    let resp: FusionResponse = serde_json::from_slice(&body)
        .map_err(|e| ApiError::internal("fusion decode failed", e.to_string()))?;
    Ok(Json(resp))
}

/// Short detail string for the health endpoint: the registry is the
/// product here — it must parse and list the 5 indicators.
pub async fn probe_detail() -> Result<String, FeedError> {
    let inds = load_registry().map_err(|e| FeedError::new(FEED, e))?;
    Ok(format!("{} curated indicators", inds.len()))
}

pub fn routes() -> Router<AppState> {
    use axum::routing::get;
    Router::new().route("/api/fusion", get(fusion))
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- transforms -------------------------------------------------------

    #[test]
    fn price_move_transform_scales_and_clamps() {
        assert!((transform_price_move(5.0, 12.0) - 60.0).abs() < 1e-9);
        assert!((transform_price_move(-2.5, 12.0) - 30.0).abs() < 1e-9);
        assert!((transform_price_move(20.0, 12.0) - 100.0).abs() < 1e-9);
        assert!((transform_price_move(0.0, 12.0)).abs() < 1e-9);
    }

    #[test]
    fn sanctions_criticals_saturate_at_four() {
        assert!((transform_sanctions_criticals(0) - 0.0).abs() < 1e-9);
        assert!((transform_sanctions_criticals(2) - 50.0).abs() < 1e-9);
        assert!((transform_sanctions_criticals(4) - 100.0).abs() < 1e-9);
        assert!((transform_sanctions_criticals(25) - 100.0).abs() < 1e-9);
    }

    #[test]
    fn calendar_high_saturates_at_three() {
        assert!((transform_calendar_high(1) - 34.0).abs() < 1e-9);
        assert!((transform_calendar_high(3) - 100.0).abs() < 1e-9);
        assert!((transform_calendar_high(10) - 100.0).abs() < 1e-9);
    }

    #[test]
    fn vix_level_maps_reference_points() {
        assert!((transform_vix_level(10.0) - 0.0).abs() < 1e-9);
        assert!((transform_vix_level(30.0) - 50.0).abs() < 1e-9);
        assert!((transform_vix_level(50.0) - 100.0).abs() < 1e-9);
        assert!((transform_vix_level(5.0) - 0.0).abs() < 1e-9);
        assert!((transform_vix_level(90.0) - 100.0).abs() < 1e-9);
    }

    #[test]
    fn chokepoint_status_severity_ordering() {
        assert!((transform_chokepoint_status("Strait blocked by naval mines") - 90.0).abs() < 1e-9);
        assert!((transform_chokepoint_status("Disrupted since Feb 2026") - 65.0).abs() < 1e-9);
        assert!((transform_chokepoint_status("Partial restrictions") - 65.0).abs() < 1e-9);
        assert!((transform_chokepoint_status("Congested, delays") - 40.0).abs() < 1e-9);
        assert!((transform_chokepoint_status("Open, normal traffic") - 15.0).abs() < 1e-9);
    }

    #[test]
    fn vessel_density_scales() {
        assert!((transform_vessel_density(1) - 2.5).abs() < 1e-9);
        assert!((transform_vessel_density(40) - 100.0).abs() < 1e-9);
        assert!((transform_vessel_density(200) - 100.0).abs() < 1e-9);
    }

    // -- levels + confidence ----------------------------------------------

    #[test]
    fn level_thresholds_at_boundaries() {
        assert_eq!(level_for(0.0), "calm");
        assert_eq!(level_for(24.999), "calm");
        assert_eq!(level_for(25.0), "watch");
        assert_eq!(level_for(49.999), "watch");
        assert_eq!(level_for(50.0), "elevated");
        assert_eq!(level_for(74.999), "elevated");
        assert_eq!(level_for(75.0), "critical");
        assert_eq!(level_for(100.0), "critical");
    }

    #[test]
    fn confidence_thresholds() {
        assert_eq!(confidence_for(1.0), "high");
        assert_eq!(confidence_for(0.8), "high");
        assert_eq!(confidence_for(0.79), "medium");
        assert_eq!(confidence_for(0.5), "medium");
        assert_eq!(confidence_for(0.49), "low");
        assert_eq!(confidence_for(0.0), "low");
    }

    #[test]
    fn thin_basis_risk_input_caps_confidence_at_medium() {
        // high + thin-basis live input → medium.
        let inputs = vec![ev("a", 0.5, true, 80.0), ev_thin("risk_thin", 0.5, 100.0)];
        assert_eq!(apply_thin_basis_confidence("high", &inputs), "medium");
        // medium stays medium (at most medium, never raised).
        assert_eq!(apply_thin_basis_confidence("medium", &inputs), "medium");
        // low stays low.
        assert_eq!(apply_thin_basis_confidence("low", &inputs), "low");
        // thin input that is NOT live does not downgrade.
        let mut dead = ev("risk_dead", 0.5, false, 0.0);
        dead.thin_basis = true;
        let inputs = vec![ev("a", 0.5, true, 80.0), dead];
        assert_eq!(apply_thin_basis_confidence("high", &inputs), "high");
        // no thin inputs → unchanged.
        let inputs = vec![ev("a", 0.5, true, 80.0), ev("b", 0.5, true, 60.0)];
        assert_eq!(apply_thin_basis_confidence("high", &inputs), "high");
    }

    // -- weight redistribution --------------------------------------------

    fn ev(key: &str, weight: f64, live: bool, value: f64) -> EvaluatedInput {
        EvaluatedInput {
            key: key.into(),
            feed: "test".into(),
            role: "test".into(),
            weight,
            live,
            value: live.then_some(value),
            evidence_value: format!("{value}"),
            detail: String::new(),
            thin_basis: false,
        }
    }

    fn ev_thin(key: &str, weight: f64, value: f64) -> EvaluatedInput {
        EvaluatedInput {
            thin_basis: true,
            detail: "THIN BASIS".to_string(),
            ..ev(key, weight, true, value)
        }
    }

    #[test]
    fn all_live_weights_unchanged() {
        let inputs = vec![ev("a", 0.6, true, 80.0), ev("b", 0.4, true, 50.0)];
        let (score, frac) = score_inputs(&inputs);
        assert!((score.unwrap() - 68.0).abs() < 1e-9);
        assert!((frac - 1.0).abs() < 1e-9);
    }

    #[test]
    fn degraded_input_weight_redistributes_not_zeroes() {
        // b degraded: a's weight renormalizes 0.6 → 1.0.
        let inputs = vec![ev("a", 0.6, true, 80.0), ev("b", 0.4, false, 0.0)];
        let (score, frac) = score_inputs(&inputs);
        // Must be 80.0 (a at full weight), NOT 48.0 (a at 0.6 with b as 0).
        assert!((score.unwrap() - 80.0).abs() < 1e-9);
        assert!((frac - 0.6).abs() < 1e-9);
    }

    #[test]
    fn no_live_inputs_yields_none() {
        let inputs = vec![ev("a", 0.6, false, 0.0), ev("b", 0.4, false, 0.0)];
        let (score, frac) = score_inputs(&inputs);
        assert!(score.is_none());
        assert!((frac - 0.0).abs() < 1e-9);
    }

    // -- registry integrity ------------------------------------------------

    #[test]
    fn registry_has_five_indicators_with_valid_weights() {
        let inds = load_registry().unwrap();
        assert_eq!(inds.len(), 5);
        let ids: Vec<&str> = inds.iter().map(|i| i.id.as_str()).collect();
        for want in [
            "hormuz-energy-risk",
            "russia-supply-risk",
            "china-strait-tension",
            "us-macro-risk",
            "election-unrest-risk",
        ] {
            assert!(ids.contains(&want), "missing indicator {want}");
        }
        for ind in &inds {
            assert!(!ind.name.is_empty() && !ind.description.is_empty());
            assert!(!ind.labels.calm.is_empty() && !ind.labels.critical.is_empty());
            let w: f64 = ind.inputs.iter().map(|i| i.weight).sum();
            // Static indicators must sum to 1.0; the dynamic election
            // indicator declares a single 1.0 input (renormalized at eval).
            assert!(
                (w - 1.0).abs() < 1e-9,
                "weights for {} sum to {w}, want 1.0",
                ind.id
            );
            for inp in &ind.inputs {
                assert!(
                    inp.weight > 0.0 && inp.weight <= 1.0,
                    "bad weight {}",
                    inp.key
                );
                assert!(!inp.key.is_empty() && !inp.role.is_empty());
            }
        }
    }

    #[test]
    fn unknown_indicator_lookup_is_none() {
        assert!(find_indicator("nope").is_none());
        assert!(find_indicator("hormuz-energy-risk").is_some());
    }

    #[test]
    fn label_for_unknown_level_is_honest() {
        let ind = find_indicator("hormuz-energy-risk").unwrap();
        let l = label_for(ind, "unknown");
        assert!(l.contains("Insufficient live inputs"));
    }

    #[test]
    fn percent_decode_handles_registry_fragments() {
        assert_eq!(percent_decode("Taiwan"), "Taiwan");
        assert_eq!(percent_decode("C%C3%B4te"), "Côte");
    }

    #[test]
    fn days_to_ymd_roundtrip() {
        assert_eq!(days_to_ymd(0), "1970-01-01");
        assert_eq!(days_to_ymd(20422), "2025-11-30");
    }

    #[test]
    fn disclaimer_states_not_advice() {
        assert!(DISCLAIMER.contains("not trading advice"));
        assert!(DISCLAIMER.contains("dated 2026-11-02"));
    }
}
