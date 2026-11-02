//! Unrest-signals score: a transparent, heuristic per-country indicator.
//!
//! `GET /api/unrest?country=<name|iso2|iso3>` returns a 0–100 score built
//! from open-source media signals, where **every number that feeds the
//! score is visible in the response** (raw value, baseline value,
//! normalized value, weight — per component). This is a heuristic
//! indicator, not a prediction: the words "prediction", "forecast", and
//! "probability" appear nowhere in the response or UI by design.
//!
//! ## Signals (all baseline-normalized — never raw counts)
//!
//! 1. **GDELT** (weight 0.50): `theme:PROTEST` articles from the country's
//!    own outlets (`sourcecountry:`), via the DOC 2.1 `timelinevol` mode
//!    over 90 days. 7-day per-bin rate vs the prior baseline per-bin rate.
//!    (The DOC API exposes themes, not raw CAMEO codes; `theme:PROTEST` is
//!    the documented proxy for CAMEO 14* protest coding.)
//! 2. **Google News** (weight 0.35): two queries —
//!    `<country> (protest OR strike OR riot OR unrest)` bounded to the last
//!    7 days (`after:`) and to the prior 83 days (`after:`/`before:`).
//!    Recent-window count vs baseline daily rate. (The `after:`/`before:`
//!    operators were verified live against the RSS endpoint 2026-11-01.)
//! 3. **Bluesky** (weight 0.15): two windowed `searchPosts` calls
//!    (`since:`/`until:` honored by the AppView; verified live 2026-11-01)
//!    over the same 7d / 83d windows. Country attribution is weak (global
//!    full-text search) — hence the low weight; documented, not hidden.
//!
//! Each window is capped at 100 items by the upstreams; a saturated window
//! is flagged in the component's `note` (it understates the baseline and
//! biases the ratio upward). Normalization: for a recent/baseline rate
//! ratio `r`, the component maps to `clamp((r-1)/4, 0, 1)` — at baseline →
//! 0, 5× baseline → 1 — with a low-evidence discount (`recent_count/5`,
//! capped at 1) so a handful of articles can't max out a component. When a
//! component is unavailable its weight redistributes proportionally among
//! the working ones and the response is marked `partial: true` — never
//! silently zero.
//!
//! ## Deliberately excluded
//!
//! - **Mastodon**: the feed is a 40-item client-side-filtered timeline
//!   snapshot — no server-side search, no rate or baseline is computable.
//! - **Conflicts (GDACS)**: tracks *natural disasters*, not unrest.
//!   Counting disaster alerts as unrest signals would be mislabeling.
//! - **RSS outlets**: global wires with no country field.
//! - **Telegram / markets / cyber / flights / quakes / cameras / vessels**:
//!   no country-attributable unrest semantics (or no country field at all).
//!
//! ## Cache discipline
//!
//! Every component reuses its feed's normal cache path, key format, and TTL
//! — GDELT through `feeds::news::TTL` (300s); Google News and Bluesky use
//! the same cache-key format as `/api/gnews` and `/api/bluesky` with
//! date-bounded queries. At most 5 upstream calls per country per TTL
//! window (1 GDELT + 2 Google News + 2 Bluesky), all cached. No new API
//! keys, no new upstreams.

use std::time::{SystemTime, UNIX_EPOCH};

use axum::{
    Json, Router,
    extract::{Query, State},
    routing::get,
};
use serde::{Deserialize, Serialize};

use crate::{
    error::ApiError,
    feeds,
    geo::{CountryIdentity, find_country},
    routes::{AppState, cached_bytes},
    time::now_iso8601,
};

/// Heuristic indicator, not a forecast. This score summarizes how current
/// open-source media signals compare to their own recent baselines. It does
/// not predict whether unrest will occur, when, or where. News and social
/// coverage reflect what gets reported, not what happens on the ground;
/// quiet periods can precede major events and loud periods can fizzle. Use
/// as one input among many, never as a sole basis for decisions.
pub const DISCLAIMER: &str = "HEURISTIC INDICATOR — NOT A FORECAST. This score summarizes how current open-source media signals compare to their own recent baselines. It does not predict whether unrest will occur, when, or where. News and social coverage reflect what gets reported, not what happens on the ground; quiet periods can precede major events and loud periods can fizzle. Use as one input among many, never as a sole basis for decisions.";

pub const METHODOLOGY: &str = "Per-country unrest-signals score (0-100), heuristic only. Components: GDELT protest-themed coverage by domestic outlets — 7-day rate vs prior ~83-day baseline from a 90-day volume timeline (weight 0.50); Google News unrest-keyword volume — two date-bounded queries (last 7 days vs prior 83 days; weight 0.35); Bluesky unrest-keyword posts — two windowed searches over the same 7d/83d windows (weight 0.15; weak country attribution, global full-text search). Each window is capped at 100 items by the upstreams; saturated windows are flagged and understate baselines. Each component maps its recent/baseline rate ratio r to clamp((r-1)/4,0,1) (at baseline = 0, 5x baseline = 1) with a low-evidence discount below 5 recent items. When a component is unavailable its weight redistributes proportionally among the working ones and the score is marked partial. Levels: <25 low, <50 elevated, <75 high, >=75 severe.";

const WEIGHT_GDELT: f64 = 0.50;
const WEIGHT_GNEWS: f64 = 0.35;
const WEIGHT_BLUESKY: f64 = 0.15;

const SIGNAL_GDELT: &str = "protest-themed articles from domestic outlets (GDELT theme:PROTEST via sourcecountry), 7-day rate vs prior ~83-day baseline";
const SIGNAL_GNEWS: &str = "unrest-keyword article volume (Google News), 7-day rate vs older-window rate within returned items";
const SIGNAL_BLUESKY: &str = "unrest-keyword post volume (Bluesky full-text search), 7-day rate vs older-window rate; weak country attribution";

/// Seconds in the "recent" window.
pub(crate) const RECENT_SECS: i64 = 7 * 86_400;
/// Minimum recent items before a component counts at full strength.
const EVIDENCE_FLOOR: f64 = 5.0;
/// Minimum baseline bins for the GDELT leg.
pub(crate) const MIN_BASELINE_BINS: usize = 7;

// ---------------------------------------------------------------------------
// Pure math (unit-tested below)
// ---------------------------------------------------------------------------

/// Map a recent/baseline rate ratio to 0..1: at baseline → 0, 5× → 1.
pub(crate) fn normalize_ratio(ratio: f64) -> f64 {
    ((ratio - 1.0) / 4.0).clamp(0.0, 1.0)
}

/// Low-evidence discount: scales 0→1 as the recent count goes 0→5.
pub(crate) fn evidence_factor(recent_count: f64) -> f64 {
    (recent_count / EVIDENCE_FLOOR).clamp(0.0, 1.0)
}

fn level_for(score: f64) -> &'static str {
    if score < 25.0 {
        "low"
    } else if score < 50.0 {
        "elevated"
    } else if score < 75.0 {
        "high"
    } else {
        "severe"
    }
}

/// Days since 1970-01-01 → (year, month, day). Inverse of `days_from_civil`
/// (same Hinnant algorithm family as `crate::time`).
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Epoch seconds → `YYYY-MM-DD` (UTC).
pub(crate) fn ymd(epoch_secs: i64) -> String {
    let (y, m, d) = civil_from_days(epoch_secs.div_euclid(86_400));
    format!("{y:04}-{m:02}-{d:02}")
}

/// Day-quantized window boundaries (UTC): `(recent_since, baseline_since)`.
/// Quantized to whole days so cache keys stay stable for 24h: the recent
/// window is the last 7 days, the baseline the 83 days before that.
pub(crate) fn day_windows(now: i64) -> (i64, i64) {
    let today = now - now.rem_euclid(86_400);
    (today - 7 * 86_400, today - 90 * 86_400)
}

/// Parameters for [`build_component`] — kept as a struct so the builder
/// stays within clippy's argument limit.
pub(crate) struct ComponentCounts<'a> {
    pub(crate) source: &'a str,
    pub(crate) signal: &'a str,
    pub(crate) query: &'a str,
    pub(crate) weight: f64,
    pub(crate) raw_unit: &'a str,
    pub(crate) baseline_unit: &'a str,
    pub(crate) recent_count: f64,
    pub(crate) baseline_count: f64,
    pub(crate) baseline_days: f64,
}

/// Build a component from explicit recent/baseline counts over a known
/// baseline span. Pure.
pub(crate) fn build_component(p: ComponentCounts<'_>) -> UnrestComponent {
    let baseline_per_day = p.baseline_count / p.baseline_days.max(1.0);
    let normalized = normalize_component(p.recent_count, baseline_per_day);
    let mut notes: Vec<String> = Vec::new();
    if p.recent_count >= 100.0 || p.baseline_count >= 100.0 {
        notes.push(
            "a window hit the upstream 100-item cap — counts are lower bounds; a capped baseline biases the ratio upward"
                .to_string(),
        );
    }
    UnrestComponent {
        source: p.source.to_string(),
        signal: p.signal.to_string(),
        ok: true,
        query: p.query.to_string(),
        raw_value: p.recent_count,
        raw_unit: p.raw_unit.to_string(),
        baseline_value: (baseline_per_day * 100.0).round() / 100.0,
        baseline_unit: p.baseline_unit.to_string(),
        normalized_0_1: (normalized * 1000.0).round() / 1000.0,
        weight: p.weight,
        note: if notes.is_empty() {
            None
        } else {
            Some(notes.join(" "))
        },
        error: None,
    }
}

/// Normalize one component from a recent count and a baseline per-day rate.
pub(crate) fn normalize_component(recent_count: f64, baseline_per_day: f64) -> f64 {
    if baseline_per_day <= 0.0 {
        // Baseline silence with recent activity: spike from zero. Scales
        // with evidence rather than claiming an infinite ratio.
        return (recent_count / 25.0).clamp(0.0, 1.0);
    }
    let ratio = (recent_count / 7.0) / baseline_per_day;
    normalize_ratio(ratio) * evidence_factor(recent_count)
}

/// Split GDELT timeline bins into recent/baseline sums. Pure; shared
/// with the risk scorer.
pub(crate) fn gdelt_window_sums(
    bins: &[feeds::news::TimelineBin],
    now: i64,
) -> (f64, usize, f64, usize) {
    let mut sum_r = 0.0f64;
    let mut n_r = 0usize;
    let mut sum_b = 0.0f64;
    let mut n_b = 0usize;
    for b in bins {
        if b.epoch_secs >= now - RECENT_SECS {
            sum_r += b.value;
            n_r += 1;
        } else {
            sum_b += b.value;
            n_b += 1;
        }
    }
    (sum_r, n_r, sum_b, n_b)
}

/// Normalize GDELT window sums; returns `(normalized_0_1, note)`. Pure;
/// shared with the risk scorer.
pub(crate) fn gdelt_normalize(
    sum_r: f64,
    n_r: usize,
    sum_b: f64,
    n_b: usize,
) -> (f64, Option<String>) {
    let baseline_per_bin = sum_b / n_b.max(1) as f64;
    let normalized = if baseline_per_bin <= 0.0 {
        if sum_r <= 0.0 {
            0.0
        } else {
            (sum_r / 25.0).clamp(0.0, 1.0)
        }
    } else {
        let ratio = (sum_r / n_r.max(1) as f64) / baseline_per_bin;
        normalize_ratio(ratio) * evidence_factor(sum_r)
    };
    let note = if baseline_per_bin <= 0.0 && sum_r > 0.0 {
        Some(
            "baseline window was silent; scored as a spike from zero, scaled by evidence"
                .to_string(),
        )
    } else {
        None
    };
    (normalized, note)
}

// ---------------------------------------------------------------------------
// Response shapes
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Clone)]
pub struct UnrestCountry {
    pub name: String,
    pub iso2: String,
    pub iso3: String,
}

#[derive(Debug, Serialize, Clone)]
pub struct UnrestComponent {
    pub source: String,
    pub signal: String,
    pub ok: bool,
    pub query: String,
    pub raw_value: f64,
    pub raw_unit: String,
    pub baseline_value: f64,
    pub baseline_unit: String,
    pub normalized_0_1: f64,
    pub weight: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl UnrestComponent {
    pub(crate) fn err(source: &str, signal: &str, query: &str, weight: f64, error: String) -> Self {
        Self {
            source: source.to_string(),
            signal: signal.to_string(),
            ok: false,
            query: query.to_string(),
            raw_value: 0.0,
            raw_unit: String::new(),
            baseline_value: 0.0,
            baseline_unit: String::new(),
            normalized_0_1: 0.0,
            weight,
            note: None,
            error: Some(error),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct UnrestResponse {
    pub country: UnrestCountry,
    pub score: f64,
    pub level: String,
    /// True when any component was unavailable (weights redistributed).
    pub partial: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    pub components: Vec<UnrestComponent>,
    pub methodology: String,
    pub generated_at: String,
    pub disclaimer: String,
}

// ---------------------------------------------------------------------------
// Component builders (each reuses its feed's normal cache path + TTL)
// ---------------------------------------------------------------------------

pub(crate) fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// GDELT's `sourcecountry:` names diverge from Natural Earth in a few
/// verified cases; multi-word names are always quoted (as in
/// country_inspect.rs).
pub(crate) fn gdelt_country_name(name: &str) -> String {
    match name {
        "United States of America" => "United States".to_string(),
        _ => name.to_string(),
    }
}

async fn gdelt_component(state: &AppState, country: &CountryIdentity) -> UnrestComponent {
    let q = format!(
        "theme:PROTEST sourcecountry:\"{}\"",
        gdelt_country_name(&country.name)
    );
    let key = format!("news:timelinevol:q={q}");
    let mk_err = |e: String| UnrestComponent::err("gdelt", SIGNAL_GDELT, &q, WEIGHT_GDELT, e);
    let q_fetch = q.clone();
    let client = state.client.clone();
    let bins: Vec<feeds::news::TimelineBin> =
        match cached_bytes(state, &key, feeds::news::TTL, || async move {
            feeds::news::fetch_timeline_volume(&client, &q_fetch, "90days").await
        })
        .await
        {
            Ok((body, _)) => match serde_json::from_slice(&body) {
                Ok(b) => b,
                Err(e) => return mk_err(format!("decode failed: {e}")),
            },
            Err(e) => return mk_err(e.user_message()),
        };
    let now = now_secs();
    let (sum_r, n_r, sum_b, n_b) = gdelt_window_sums(&bins, now);
    if n_b < MIN_BASELINE_BINS {
        return mk_err(format!(
            "insufficient baseline history: only {n_b} baseline bins (need {MIN_BASELINE_BINS})"
        ));
    }
    let (normalized, note) = gdelt_normalize(sum_r, n_r, sum_b, n_b);
    UnrestComponent {
        source: "gdelt".to_string(),
        signal: SIGNAL_GDELT.to_string(),
        ok: true,
        query: q,
        raw_value: sum_r,
        raw_unit: "articles (last 7 days)".to_string(),
        baseline_value: ((sum_b / n_b.max(1) as f64) * 100.0).round() / 100.0,
        baseline_unit: "articles/day (prior ~83-day baseline)".to_string(),
        normalized_0_1: (normalized * 1000.0).round() / 1000.0,
        weight: WEIGHT_GDELT,
        note,
        error: None,
    }
}

/// Fetch one date-bounded window for Google News and return its item count.
/// Shared with the risk scorer (same cache-key format/TTL).
/// The `after:`/`before:` operators are honored by the RSS endpoint
/// (verified live 2026-11-01); the full query string is part of the cache
/// key, so each window is cached independently on the feed's normal TTL.
pub(crate) async fn gnews_window_count(
    state: &AppState,
    base_query: &str,
    after: &str,
    before: Option<&str>,
) -> Result<usize, String> {
    let q = match before {
        Some(b) => format!("{base_query} after:{after} before:{b}"),
        None => format!("{base_query} after:{after}"),
    };
    let key = format!("gnews:q={q}:limit=100");
    let client = state.client.clone();
    let (body, _) = cached_bytes(state, &key, feeds::gnews::TTL, || async move {
        feeds::gnews::fetch(&client, &q, 100).await
    })
    .await
    .map_err(|e| e.user_message())?;
    let resp: feeds::gnews::GnewsResponse =
        serde_json::from_slice(&body).map_err(|e| format!("decode failed: {e}"))?;
    Ok(resp.count)
}

async fn gnews_component(state: &AppState, country: &CountryIdentity) -> UnrestComponent {
    let base_q = format!("{} (protest OR strike OR riot OR unrest)", country.name);
    let now = now_secs();
    let (recent_since, baseline_since) = day_windows(now);
    // Representative query shown in the response (the recent window's).
    let q = format!("{base_q} after:{}", ymd(recent_since));
    let mk_err = |e: String| UnrestComponent::err("gnews", SIGNAL_GNEWS, &q, WEIGHT_GNEWS, e);
    let recent_after = ymd(recent_since);
    let baseline_after = ymd(baseline_since);
    let (recent, baseline) = tokio::join!(
        gnews_window_count(state, &base_q, &recent_after, None),
        gnews_window_count(state, &base_q, &baseline_after, Some(&recent_after))
    );
    let (recent, baseline) = match (recent, baseline) {
        (Ok(r), Ok(b)) => (r as f64, b as f64),
        (Err(e), _) | (_, Err(e)) => return mk_err(e),
    };
    let mut comp = build_component(ComponentCounts {
        source: "gnews",
        signal: SIGNAL_GNEWS,
        query: &q,
        weight: WEIGHT_GNEWS,
        raw_unit: "articles (last 7 days)",
        baseline_unit: "articles/day (prior 83 days)",
        recent_count: recent,
        baseline_count: baseline,
        baseline_days: 83.0,
    });
    if recent == 0.0 && baseline == 0.0 {
        comp.note = Some(
            "no matching articles in either window — treated as no detected signal".to_string(),
        );
    }
    comp
}

/// Fetch one time-bounded window from Bluesky and return its post count.
/// Shared with the risk scorer (same cache-key format/TTL).
pub(crate) async fn bluesky_window_count(
    state: &AppState,
    query: &str,
    since: &str,
    until: Option<&str>,
) -> Result<usize, String> {
    let key = format!(
        "bluesky:q={query}:limit=100:since={since}:until={}",
        until.unwrap_or("")
    );
    let client = state.client.clone();
    let query = query.to_string();
    let since = since.to_string();
    let until = until.map(str::to_string);
    let (body, _) = cached_bytes(state, &key, feeds::bluesky::TTL, || async move {
        feeds::bluesky::fetch_window(&client, &query, 100, Some(&since), until.as_deref()).await
    })
    .await
    .map_err(|e| e.user_message())?;
    let resp: feeds::bluesky::BlueskyResponse =
        serde_json::from_slice(&body).map_err(|e| format!("decode failed: {e}"))?;
    Ok(resp.count)
}

async fn bluesky_component(state: &AppState, country: &CountryIdentity) -> UnrestComponent {
    let q = format!("{} protest", country.name);
    let now = now_secs();
    let (recent_since, baseline_since) = day_windows(now);
    let mk_err = |e: String| UnrestComponent::err("bluesky", SIGNAL_BLUESKY, &q, WEIGHT_BLUESKY, e);
    let recent_since_ts = rfc3339_day(recent_since);
    let baseline_since_ts = rfc3339_day(baseline_since);
    let (recent, baseline) = tokio::join!(
        bluesky_window_count(state, &q, &recent_since_ts, None),
        bluesky_window_count(state, &q, &baseline_since_ts, Some(&recent_since_ts))
    );
    let (recent, baseline) = match (recent, baseline) {
        (Ok(r), Ok(b)) => (r as f64, b as f64),
        (Err(e), _) | (_, Err(e)) => return mk_err(e),
    };
    let mut comp = build_component(ComponentCounts {
        source: "bluesky",
        signal: SIGNAL_BLUESKY,
        query: &q,
        weight: WEIGHT_BLUESKY,
        raw_unit: "posts (last 7 days)",
        baseline_unit: "posts/day (prior 83 days)",
        recent_count: recent,
        baseline_count: baseline,
        baseline_days: 83.0,
    });
    if recent == 0.0 && baseline == 0.0 {
        comp.note =
            Some("no matching posts in either window — treated as no detected signal".to_string());
    }
    comp
}

/// RFC-3339 midnight UTC for a day boundary, e.g. `2026-10-25T00:00:00.000Z`.
pub(crate) fn rfc3339_day(epoch_secs: i64) -> String {
    format!("{}T00:00:00.000Z", ymd(epoch_secs))
}

// ---------------------------------------------------------------------------
// Handler
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct UnrestQuery {
    pub country: Option<String>,
}

/// `GET /api/unrest?country=<name|iso2|iso3>`.
///
/// The endpoint itself never fails on feed trouble: each component degrades
/// to `ok: false` independently, weights redistribute, and the response is
/// marked `partial`. 400 on missing `country`, 404 on unknown.
pub async fn unrest(
    State(state): State<AppState>,
    Query(q): Query<UnrestQuery>,
) -> Result<Json<UnrestResponse>, ApiError> {
    let needle = q.country.ok_or_else(|| {
        ApiError::bad_request(
            "missing country",
            "query param country is required, e.g. ?country=France or ?country=FR or ?country=FRA",
        )
    })?;
    let geo_country = find_country(&needle).ok_or_else(|| {
        ApiError::not_found(
            "unknown country",
            format!("no country matches {needle:?}; try a name, ISO-3166 alpha-2, or alpha-3 code"),
        )
    })?;
    let country = UnrestCountry {
        name: geo_country.name.clone(),
        iso2: geo_country.iso2.clone(),
        iso3: geo_country.iso3.clone(),
    };

    // Concurrent fan-out: latency ~= the slowest component, not the sum.
    let (gdelt, gnews, bluesky) = tokio::join!(
        gdelt_component(&state, &geo_country),
        gnews_component(&state, &geo_country),
        bluesky_component(&state, &geo_country),
    );
    let components = vec![gdelt, gnews, bluesky];

    let ok_weight: f64 = components.iter().filter(|c| c.ok).map(|c| c.weight).sum();
    let score = if ok_weight > 0.0 {
        100.0
            * components
                .iter()
                .filter(|c| c.ok)
                .map(|c| c.weight * c.normalized_0_1)
                .sum::<f64>()
            / ok_weight
    } else {
        0.0
    };
    let partial = components.iter().any(|c| !c.ok);
    let down: Vec<&str> = components
        .iter()
        .filter(|c| !c.ok)
        .map(|c| c.source.as_str())
        .collect();
    let note = if partial {
        Some(format!(
            "partial score: {} unavailable ({}); weights of working components redistributed proportionally",
            down.join(", "),
            components
                .iter()
                .filter(|c| !c.ok)
                .map(|c| c.error.as_deref().unwrap_or("unknown error"))
                .collect::<Vec<_>>()
                .join("; ")
        ))
    } else {
        None
    };

    Ok(Json(UnrestResponse {
        country,
        score: (score * 10.0).round() / 10.0,
        level: level_for(score).to_string(),
        partial,
        note,
        components,
        methodology: METHODOLOGY.to_string(),
        generated_at: now_iso8601(),
        disclaimer: DISCLAIMER.to_string(),
    }))
}

pub fn routes() -> Router<AppState> {
    Router::new().route("/api/unrest", get(unrest))
}

// ---------------------------------------------------------------------------
// Unit tests (pure math + fixtures; no live-score test — real unrest must
// never gate the suite)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_ratio_shape() {
        assert_eq!(normalize_ratio(1.0), 0.0); // at baseline -> 0
        assert_eq!(normalize_ratio(0.5), 0.0); // below baseline clamps
        assert!((normalize_ratio(3.0) - 0.5).abs() < 1e-9);
        assert_eq!(normalize_ratio(5.0), 1.0);
        assert_eq!(normalize_ratio(50.0), 1.0); // saturates
    }

    #[test]
    fn evidence_factor_ramps() {
        assert_eq!(evidence_factor(0.0), 0.0);
        assert!((evidence_factor(2.5) - 0.5).abs() < 1e-9);
        assert_eq!(evidence_factor(5.0), 1.0);
        assert_eq!(evidence_factor(100.0), 1.0);
    }

    #[test]
    fn level_thresholds() {
        assert_eq!(level_for(0.0), "low");
        assert_eq!(level_for(24.9), "low");
        assert_eq!(level_for(25.0), "elevated");
        assert_eq!(level_for(49.9), "elevated");
        assert_eq!(level_for(50.0), "high");
        assert_eq!(level_for(74.9), "high");
        assert_eq!(level_for(75.0), "severe");
        assert_eq!(level_for(100.0), "severe");
    }

    #[test]
    fn ymd_round_trip() {
        // Epochs cross-checked with `date -u +%s`; ymd verified against
        // time.rs's forward conversion for the same values.
        for (epoch, want) in [
            (1793491200, "2026-11-01"), // 2026-11-01T00:00:00Z
            (1767268800, "2026-01-01"), // 2026-01-01T12:00:00Z
            (1767225599, "2025-12-31"), // 2025-12-31T23:59:59Z
            (0, "1970-01-01"),
            (-86_400, "1969-12-31"),
        ] {
            assert_eq!(ymd(epoch), want, "epoch {epoch}");
        }
        assert_eq!(rfc3339_day(1791936000), "2026-10-14T00:00:00.000Z");
    }

    #[test]
    fn day_windows_are_day_quantized_and_sized() {
        // 2026-11-01T15:30:00Z (1793547000) -> recent window starts
        // 2026-10-25 00:00Z, baseline 2026-08-03 00:00Z; both boundaries
        // land on whole days.
        let now = 1793547000i64;
        let (recent_since, baseline_since) = day_windows(now);
        assert_eq!(ymd(recent_since), "2026-10-25");
        assert_eq!(ymd(baseline_since), "2026-08-03");
        assert_eq!(recent_since % 86_400, 0);
        assert_eq!(baseline_since % 86_400, 0);
        assert_eq!((recent_since - baseline_since) / 86_400, 83);
    }

    #[test]
    fn build_component_math() {
        // 14 recent items vs 83 baseline items over 83 days -> ratio
        // (14/7)/(83/83) = 2 -> norm (2-1)/4 = 0.25, evidence 1.
        let c = build_component(ComponentCounts {
            source: "gnews",
            signal: "sig",
            query: "q",
            weight: 0.35,
            raw_unit: "articles (last 7 days)",
            baseline_unit: "articles/day (prior 83 days)",
            recent_count: 14.0,
            baseline_count: 83.0,
            baseline_days: 83.0,
        });
        assert!(c.ok);
        assert_eq!(c.raw_value, 14.0);
        assert_eq!(c.baseline_value, 1.0);
        assert!(
            (c.normalized_0_1 - 0.25).abs() < 1e-9,
            "norm {}",
            c.normalized_0_1
        );
        assert!(c.note.is_none());
    }

    #[test]
    fn build_component_flags_saturated_windows() {
        let c = build_component(ComponentCounts {
            source: "bluesky",
            signal: "sig",
            query: "q",
            weight: 0.15,
            raw_unit: "posts (last 7 days)",
            baseline_unit: "posts/day (prior 83 days)",
            recent_count: 100.0,
            baseline_count: 100.0,
            baseline_days: 83.0,
        });
        assert!(c.ok);
        let note = c.note.expect("saturated windows must be flagged");
        assert!(note.contains("100-item cap"), "{note}");
    }

    #[test]
    fn build_component_zero_signal() {
        let c = build_component(ComponentCounts {
            source: "gnews",
            signal: "sig",
            query: "q",
            weight: 0.35,
            raw_unit: "articles (last 7 days)",
            baseline_unit: "articles/day (prior 83 days)",
            recent_count: 0.0,
            baseline_count: 0.0,
            baseline_days: 83.0,
        });
        assert!(c.ok);
        assert_eq!(c.normalized_0_1, 0.0);
    }

    #[test]
    fn normalize_component_spike_from_silence() {
        // Baseline silent, 10 recent items: scaled by evidence, not infinite.
        assert!((normalize_component(10.0, 0.0) - 0.4).abs() < 1e-9);
        assert_eq!(normalize_component(0.0, 0.0), 0.0);
        // 2x baseline with plenty of evidence -> 0.25.
        assert!((normalize_component(14.0, 1.0) - 0.25).abs() < 1e-9);
        // 2x baseline with 2 recent items -> discounted by 0.4.
        assert!((normalize_component(2.0, 1.0 / 7.0) - 0.25 * 0.4).abs() < 1e-9);
    }

    #[test]
    fn score_redistributes_when_component_down() {
        // Two components ok (0.5 @ w=0.5, 1.0 @ w=0.35), one down (w=0.15).
        let w_ok = 0.5 + 0.35;
        let score: f64 = 100.0 * (0.5 * 0.5 + 0.35 * 1.0) / w_ok;
        assert!((score - 70.588).abs() < 0.01, "score {score}");
        assert_eq!(level_for(score), "high");
    }

    #[test]
    fn gdelt_fixture_bins_cover_90d() {
        let bins = feeds::news::parse_timeline_volume(include_str!(
            "../benches/fixtures/gdelt_timelinevol.json"
        ))
        .unwrap();
        let now = 1793577600i64; // 2026-11-02T00:00:00Z
        let n_base = bins
            .iter()
            .filter(|b| b.epoch_secs < now - RECENT_SECS)
            .count();
        assert!(n_base >= MIN_BASELINE_BINS, "n_base {n_base}");
        // The fixture's engineered spike (days 80-84) lands in the recent window.
        let recent_sum: f64 = bins
            .iter()
            .filter(|b| b.epoch_secs >= now - RECENT_SECS)
            .map(|b| b.value)
            .sum();
        assert!(recent_sum > 50.0, "recent_sum {recent_sum}");
    }

    #[test]
    fn gdelt_country_name_mapping() {
        assert_eq!(
            gdelt_country_name("United States of America"),
            "United States"
        );
        assert_eq!(gdelt_country_name("France"), "France");
    }
}
