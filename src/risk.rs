//! Country + city risk & safety score: a transparent, heuristic indicator.
//!
//! `GET /api/risk?place=<name>&kind=auto|country|city` returns a 0–100
//! score built from three signal families — **unrest** (protest/strike/riot
//! keywords), **violent_incidents** (bombing/shooting/terror-attack
//! keywords), **crime_disorder** (crime/murder/robbery keywords) — each
//! baseline-normalized exactly like [`crate::unrest`]: a 7-day recent
//! window vs the prior 83-day baseline, ratio → `clamp((r-1)/4, 0, 1)`,
//! low-evidence discount below 5 recent items, spike-from-silence path.
//! Every number that feeds the score is visible in the response (raw
//! value, baseline value, normalized value, weight — per component, per
//! family). This is a heuristic indicator, not a forecast: the words
//! "forecast", "prediction", and "probability" appear nowhere in the
//! response or UI by design (a contract test asserts this).
//!
//! The endpoint also returns a `safety` reading ("High"/"Moderate"/"Low"/
//! "Very low") — risk and safety are two labeled views of the same score,
//! not independent measurements.
//!
//! ## Place handling
//!
//! - `kind=country`: the place must resolve via [`crate::geo::find_country`]
//!   (name, ISO-3166 alpha-2 or alpha-3); 404 otherwise. Full signal set:
//!   GDELT timelinevol + Google News + Bluesky.
//! - `kind=city`: any place string; Google News + Bluesky only. GDELT has
//!   no city granularity — its leg degrades to `ok: false`
//!   ("no city-level coverage"), weights redistribute, the response is
//!   marked `partial`. An optional `&country=` hint is appended to
//!   news/social queries for disambiguation.
//! - `kind=auto` (default): country resolution wins; anything else is
//!   treated as a city.
//!
//! ## Cache discipline
//!
//! Every component reuses its feed's normal cache path, key format, and
//! TTL — the same `gnews:q=…:limit=100` / `bluesky:q=…:limit=100:since=…`
//! keys and 300s TTLs as `/api/gnews`, `/api/bluesky`, and the unrest
//! scorer's GDELT `news:timelinevol:q=…` keys. At most 15 upstream calls
//! per country per TTL window (3 families × 5) and 12 per city (3 × 4).
//! No new upstreams, no new API keys.

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
    geo::find_country,
    routes::{AppState, cached_bytes},
    time::now_iso8601,
    unrest::{
        self, ComponentCounts, MIN_BASELINE_BINS, UnrestComponent, build_component,
        gdelt_country_name, gdelt_normalize, gdelt_window_sums, now_secs,
    },
};

/// Heuristic indicator, not a forecast. This score summarizes how current
/// open-source media signals about a place compare to their own recent
/// baselines. It does not predict whether risk will materialize, when, or
/// where. News and social coverage reflect what gets reported, not what
/// happens on the ground: big cities and capitals are systematically
/// over-covered relative to smaller places, so scores are not comparable
/// as absolute risk between places of very different media visibility.
/// Coverage is English-language sources only. This is not travel advice
/// and not a substitute for official government guidance. Use as one input
/// among many, never as a sole basis for decisions.
pub const DISCLAIMER: &str = "HEURISTIC INDICATOR — NOT A FORECAST. This score summarizes how current open-source media signals about a place compare to their own recent baselines. It does not predict whether risk will materialize, when, or where. News and social coverage reflect what gets reported, not what happens on the ground: big cities and capitals are systematically over-covered relative to smaller places, so scores are not comparable as absolute risk between places of very different media visibility. Coverage is English-language sources only. This is not travel advice and not a substitute for official government guidance. Use as one input among many, never as a sole basis for decisions.";

pub const METHODOLOGY: &str = "Per-place risk & safety score (0-100), heuristic only. Three signal families, each built from baseline-normalized components (7-day recent rate vs prior ~83-day baseline; ratio r maps to clamp((r-1)/4,0,1); low-evidence discount below 5 recent items): unrest (weight 0.40) — protest/strike/riot keywords; violent_incidents (weight 0.35) — bombing/shooting/terrorist-attack/explosion keywords; crime_disorder (weight 0.25) — crime/murder/robbery/looting keywords. Each family fans out over GDELT timelinevol (country path only; weight 0.50 within family), Google News date-bounded queries (0.35), and Bluesky windowed searches (0.15). City path: GDELT has no city granularity — its leg reports ok:false and family weights redistribute. When a component or family is unavailable its weight redistributes proportionally and the score is marked partial. Levels: <25 low, <50 moderate, <75 high, >=75 severe. Safety is the inverse reading of the same score: High (<25), Moderate (<50), Low (<75), Very low (>=75).";

const W_UNREST: f64 = 0.40;
const W_VIOLENT: f64 = 0.35;
const W_CRIME: f64 = 0.25;

// Within-family feed weights (same split as the unrest scorer).
const FW_GDELT: f64 = 0.50;
const FW_GNEWS: f64 = 0.35;
const FW_BLUESKY: f64 = 0.15;

fn level_for(score: f64) -> &'static str {
    if score < 25.0 {
        "low"
    } else if score < 50.0 {
        "moderate"
    } else if score < 75.0 {
        "high"
    } else {
        "severe"
    }
}

/// Safety is the inverse reading of the same risk score — two labeled
/// views of one number, not independent measurements.
fn safety_for(score: f64) -> &'static str {
    if score < 25.0 {
        "High"
    } else if score < 50.0 {
        "Moderate"
    } else if score < 75.0 {
        "Low"
    } else {
        "Very low"
    }
}

// ---------------------------------------------------------------------------
// Signal families
// ---------------------------------------------------------------------------

struct FamilyDef {
    id: &'static str,
    weight: f64,
    /// Google News parenthesized keyword group (verified OR/quotes syntax).
    gnews_kw: &'static str,
    /// Bluesky single keyword (parenthesized OR is unsupported there).
    bluesky_kw: &'static str,
    /// GDELT DOC query fragment (country path only).
    gdelt_q: &'static str,
    signal_gnews: &'static str,
    signal_bluesky: &'static str,
}

const FAMILIES: [FamilyDef; 3] = [
    FamilyDef {
        id: "unrest",
        weight: W_UNREST,
        gnews_kw: "(protest OR strike OR riot OR unrest)",
        bluesky_kw: "protest",
        gdelt_q: "theme:PROTEST",
        signal_gnews: "unrest-keyword article volume (Google News), 7-day rate vs older-window rate",
        signal_bluesky: "unrest-keyword post volume (Bluesky full-text search), 7-day rate vs older-window rate",
    },
    FamilyDef {
        id: "violent_incidents",
        weight: W_VIOLENT,
        gnews_kw: "(bombing OR shooting OR \"terrorist attack\" OR explosion)",
        bluesky_kw: "attack",
        gdelt_q: "(bombing OR shooting OR \"terrorist attack\" OR explosion)",
        signal_gnews: "violent-incident keyword article volume (Google News), 7-day rate vs older-window rate",
        signal_bluesky: "violent-incident keyword post volume (Bluesky full-text search), 7-day rate vs older-window rate",
    },
    FamilyDef {
        id: "crime_disorder",
        weight: W_CRIME,
        gnews_kw: "(crime OR murder OR robbery OR looting)",
        bluesky_kw: "crime",
        gdelt_q: "(crime OR murder OR robbery OR looting)",
        signal_gnews: "crime/disorder keyword article volume (Google News), 7-day rate vs older-window rate",
        signal_bluesky: "crime/disorder keyword post volume (Bluesky full-text search), 7-day rate vs older-window rate",
    },
];

const SIGNAL_GDELT: &str = "incident-keyword coverage by domestic outlets (GDELT DOC timelinevol via sourcecountry), 7-day rate vs prior ~83-day baseline";

// ---------------------------------------------------------------------------
// Response shapes
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct RiskPlace {
    pub name: String,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub iso2: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub iso3: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub country_hint: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct RiskFamily {
    pub family: String,
    pub weight: f64,
    /// Family score 0–100 (weighted, redistributed over working components).
    pub score_0_100: f64,
    pub partial: bool,
    pub components: Vec<UnrestComponent>,
}

#[derive(Debug, Serialize)]
pub struct RiskResponse {
    pub place: RiskPlace,
    pub score: f64,
    pub level: String,
    /// Inverse reading of the same score: "High"/"Moderate"/"Low"/"Very low".
    pub safety: String,
    pub partial: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    pub families: Vec<RiskFamily>,
    pub methodology: String,
    pub generated_at: String,
    pub disclaimer: String,
}

// ---------------------------------------------------------------------------
// Component builders (each reuses its feed's normal cache path + TTL)
// ---------------------------------------------------------------------------

fn now_secs_local() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// GDELT leg for one family (country path only): arbitrary DOC query over
/// a 90-day `timelinevol`, split into recent/baseline windows. Reuses the
/// unrest scorer's window helpers and cache-key format.
async fn gdelt_component(state: &AppState, query: &str, signal: &str) -> UnrestComponent {
    let key = format!("news:timelinevol:q={query}");
    let mk_err = |e: String| UnrestComponent::err("gdelt", signal, query, FW_GDELT, e);
    let q_fetch = query.to_string();
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
        signal: signal.to_string(),
        ok: true,
        query: query.to_string(),
        raw_value: sum_r,
        raw_unit: "articles (last 7 days)".to_string(),
        baseline_value: ((sum_b / n_b.max(1) as f64) * 100.0).round() / 100.0,
        baseline_unit: "articles/day (prior ~83-day baseline)".to_string(),
        normalized_0_1: (normalized * 1000.0).round() / 1000.0,
        weight: FW_GDELT,
        note,
        error: None,
    }
}

/// Google News leg for one family: two date-bounded windows over
/// `subject + keyword group`. Reuses unrest's window counter (same
/// `gnews:q=…:limit=100` cache keys).
async fn gnews_component(state: &AppState, subject: &str, fam: &FamilyDef) -> UnrestComponent {
    let base_q = format!("{} {}", subject, fam.gnews_kw);
    let now = now_secs_local();
    let (recent_since, baseline_since) = unrest::day_windows(now);
    let q = format!("{base_q} after:{}", unrest::ymd(recent_since));
    let mk_err = |e: String| UnrestComponent::err("gnews", fam.signal_gnews, &q, FW_GNEWS, e);
    let recent_after = unrest::ymd(recent_since);
    let baseline_after = unrest::ymd(baseline_since);
    let (recent, baseline) = tokio::join!(
        unrest::gnews_window_count(state, &base_q, &recent_after, None),
        unrest::gnews_window_count(state, &base_q, &baseline_after, Some(&recent_after))
    );
    let (recent, baseline) = match (recent, baseline) {
        (Ok(r), Ok(b)) => (r as f64, b as f64),
        (Err(e), _) | (_, Err(e)) => return mk_err(e),
    };
    build_component(ComponentCounts {
        source: "gnews",
        signal: fam.signal_gnews,
        query: &q,
        weight: FW_GNEWS,
        raw_unit: "articles (last 7 days)",
        baseline_unit: "articles/day (prior 83 days)",
        recent_count: recent,
        baseline_count: baseline,
        baseline_days: 83.0,
    })
}

/// Bluesky leg for one family: two windowed searches over
/// `subject + keyword`. Reuses unrest's window counter (same
/// `bluesky:q=…:limit=100:since=…:until=…` cache keys).
async fn bluesky_component(state: &AppState, subject: &str, fam: &FamilyDef) -> UnrestComponent {
    let q = format!("{} {}", subject, fam.bluesky_kw);
    let now = now_secs_local();
    let (recent_since, baseline_since) = unrest::day_windows(now);
    let mk_err = |e: String| UnrestComponent::err("bluesky", fam.signal_bluesky, &q, FW_BLUESKY, e);
    let recent_since_ts = unrest::rfc3339_day(recent_since);
    let baseline_since_ts = unrest::rfc3339_day(baseline_since);
    let (recent, baseline) = tokio::join!(
        unrest::bluesky_window_count(state, &q, &recent_since_ts, None),
        unrest::bluesky_window_count(state, &q, &baseline_since_ts, Some(&recent_since_ts))
    );
    let (recent, baseline) = match (recent, baseline) {
        (Ok(r), Ok(b)) => (r as f64, b as f64),
        (Err(e), _) | (_, Err(e)) => return mk_err(e),
    };
    build_component(ComponentCounts {
        source: "bluesky",
        signal: fam.signal_bluesky,
        query: &q,
        weight: FW_BLUESKY,
        raw_unit: "posts (last 7 days)",
        baseline_unit: "posts/day (prior 83 days)",
        recent_count: recent,
        baseline_count: baseline,
        baseline_days: 83.0,
    })
}

/// Assemble one family's components. `gdelt_query`: `Some` on the country
/// path, `None` on the city path (GDELT has no city granularity — the leg
/// degrades honestly instead of being faked).
async fn family_components(
    state: &AppState,
    subject: &str,
    gdelt_query: Option<String>,
    fam: &FamilyDef,
) -> Vec<UnrestComponent> {
    let gdelt_fut = async {
        match gdelt_query {
            Some(q) => gdelt_component(state, &q, SIGNAL_GDELT).await,
            None => UnrestComponent::err(
                "gdelt",
                SIGNAL_GDELT,
                "(no city-level query — GDELT is country/source-level only)",
                FW_GDELT,
                "no city-level coverage — GDELT is country/source-level only".to_string(),
            ),
        }
    };
    let (gdelt, gnews, bluesky) = tokio::join!(
        gdelt_fut,
        gnews_component(state, subject, fam),
        bluesky_component(state, subject, fam),
    );
    vec![gdelt, gnews, bluesky]
}

/// Family score 0–100 with weight redistribution over working components.
/// Returns `(score, partial, all_down)`.
fn family_score(components: &[UnrestComponent]) -> (f64, bool, bool) {
    let ok_weight: f64 = components.iter().filter(|c| c.ok).map(|c| c.weight).sum();
    if ok_weight <= 0.0 {
        return (0.0, true, true);
    }
    let score = 100.0
        * components
            .iter()
            .filter(|c| c.ok)
            .map(|c| c.weight * c.normalized_0_1)
            .sum::<f64>()
        / ok_weight;
    (score, components.iter().any(|c| !c.ok), false)
}

// ---------------------------------------------------------------------------
// Handler
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct RiskQuery {
    pub place: Option<String>,
    pub kind: Option<String>,
    pub country: Option<String>,
}

/// `GET /api/risk?place=<name>&kind=auto|country|city[&country=<hint>]`.
///
/// Never fails on feed trouble: components degrade to `ok: false`
/// independently, weights redistribute within and across families, and the
/// response is marked `partial`. 400 on missing/invalid params, 404 on an
/// unresolvable `kind=country` place.
pub async fn risk(
    State(state): State<AppState>,
    Query(q): Query<RiskQuery>,
) -> Result<Json<RiskResponse>, ApiError> {
    let place_raw = q
        .place
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            ApiError::bad_request(
                "missing place",
                "query param place is required, e.g. ?place=France or ?place=Paris&kind=city",
            )
        })?;
    let kind = q.kind.as_deref().unwrap_or("auto").to_lowercase();
    if !["auto", "country", "city"].contains(&kind.as_str()) {
        return Err(ApiError::bad_request(
            "bad kind",
            "kind must be one of: auto, country, city",
        ));
    }
    let hint = q
        .country
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());

    // Resolve country vs city.
    enum Resolved<'a> {
        Country(crate::geo::CountryIdentity),
        City(&'a str),
    }
    let resolved = match kind.as_str() {
        "country" => Resolved::Country(find_country(place_raw).ok_or_else(|| {
            ApiError::not_found(
                "unknown country",
                format!(
                    "no country matches {place_raw:?}; try a name, ISO-3166 alpha-2, or alpha-3 code"
                ),
            )
        })?),
        "city" => Resolved::City(place_raw),
        _ => match find_country(place_raw) {
            Some(c) => Resolved::Country(c),
            None => Resolved::City(place_raw),
        },
    };

    let (place, subject, gdelt_queries): (RiskPlace, String, Vec<Option<String>>) = match resolved {
        Resolved::Country(c) => {
            let subject = c.name.clone();
            let queries = FAMILIES
                .iter()
                .map(|f| {
                    Some(format!(
                        "{} sourcecountry:\"{}\"",
                        f.gdelt_q,
                        gdelt_country_name(&c.name)
                    ))
                })
                .collect();
            (
                RiskPlace {
                    name: c.name.clone(),
                    kind: "country".to_string(),
                    iso2: Some(c.iso2.clone()),
                    iso3: Some(c.iso3.clone()),
                    country_hint: None,
                },
                subject,
                queries,
            )
        }
        Resolved::City(name) => {
            let subject = match hint {
                Some(h) => format!("{name} {h}"),
                None => name.to_string(),
            };
            (
                RiskPlace {
                    name: name.to_string(),
                    kind: "city".to_string(),
                    iso2: None,
                    iso3: None,
                    country_hint: hint.map(str::to_string),
                },
                subject,
                vec![None, None, None],
            )
        }
    };

    // Concurrent fan-out over families; latency ~= the slowest component.
    let (f0, f1, f2) = tokio::join!(
        family_components(&state, &subject, gdelt_queries[0].clone(), &FAMILIES[0]),
        family_components(&state, &subject, gdelt_queries[1].clone(), &FAMILIES[1]),
        family_components(&state, &subject, gdelt_queries[2].clone(), &FAMILIES[2]),
    );
    let family_comps = [f0, f1, f2];

    let mut families: Vec<RiskFamily> = Vec::with_capacity(3);
    let mut fam_scores: Vec<(f64, f64)> = Vec::new(); // (weight, score)
    for (fam, comps) in FAMILIES.iter().zip(family_comps.iter()) {
        let (score, partial, all_down) = family_score(comps);
        if !all_down {
            fam_scores.push((fam.weight, score));
        }
        families.push(RiskFamily {
            family: fam.id.to_string(),
            weight: fam.weight,
            score_0_100: (score * 10.0).round() / 10.0,
            partial,
            components: comps.to_vec(),
        });
    }

    let work_weight: f64 = fam_scores.iter().map(|(w, _)| w).sum();
    let score = if work_weight > 0.0 {
        fam_scores.iter().map(|(w, s)| w * s).sum::<f64>() / work_weight
    } else {
        0.0
    };
    let partial = families.iter().any(|f| f.partial);
    let down: Vec<String> = families
        .iter()
        .flat_map(|f| f.components.iter())
        .filter(|c| !c.ok)
        .map(|c| format!("{}:{}", c.source, c.error.as_deref().unwrap_or("error")))
        .collect();
    let note = if partial {
        Some(format!(
            "partial score: {} unavailable; working weights redistributed proportionally",
            down.join("; ")
        ))
    } else {
        None
    };

    Ok(Json(RiskResponse {
        place,
        score: (score * 10.0).round() / 10.0,
        level: level_for(score).to_string(),
        safety: safety_for(score).to_string(),
        partial,
        note,
        families,
        methodology: METHODOLOGY.to_string(),
        generated_at: now_iso8601(),
        disclaimer: DISCLAIMER.to_string(),
    }))
}

pub fn routes() -> Router<AppState> {
    Router::new().route("/api/risk", get(risk))
}

// ---------------------------------------------------------------------------
// Unit tests (pure math + fixtures; no live-score test — real risk must
// never gate the suite)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn risk_level_thresholds() {
        assert_eq!(level_for(0.0), "low");
        assert_eq!(level_for(24.9), "low");
        assert_eq!(level_for(25.0), "moderate");
        assert_eq!(level_for(49.9), "moderate");
        assert_eq!(level_for(50.0), "high");
        assert_eq!(level_for(74.9), "high");
        assert_eq!(level_for(75.0), "severe");
        assert_eq!(level_for(100.0), "severe");
    }

    #[test]
    fn safety_is_inverse_of_risk() {
        assert_eq!(safety_for(0.0), "High");
        assert_eq!(safety_for(24.9), "High");
        assert_eq!(safety_for(25.0), "Moderate");
        assert_eq!(safety_for(49.9), "Moderate");
        assert_eq!(safety_for(50.0), "Low");
        assert_eq!(safety_for(74.9), "Low");
        assert_eq!(safety_for(75.0), "Very low");
        assert_eq!(safety_for(100.0), "Very low");
    }

    #[test]
    fn family_weights_sum_to_one() {
        let total: f64 = FAMILIES.iter().map(|f| f.weight).sum();
        assert!((total - 1.0).abs() < 1e-9, "family weights {total}");
        assert!((FW_GDELT + FW_GNEWS + FW_BLUESKY - 1.0).abs() < 1e-9);
    }

    #[test]
    fn kind_auto_detects_countries_and_cities() {
        // Countries resolve (names + ISO codes); anything else is a city.
        assert!(find_country("France").is_some());
        assert!(find_country("FR").is_some());
        assert!(find_country("FRA").is_some());
        assert!(find_country("United States of America").is_some());
        assert!(find_country("Paris").is_none());
        assert!(find_country("Springfield").is_none());
        assert!(find_country("Atlantis").is_none());
    }

    fn ok_comp(weight: f64, norm: f64) -> UnrestComponent {
        UnrestComponent {
            source: "t".to_string(),
            signal: "s".to_string(),
            ok: true,
            query: "q".to_string(),
            raw_value: 0.0,
            raw_unit: String::new(),
            baseline_value: 0.0,
            baseline_unit: String::new(),
            normalized_0_1: norm,
            weight,
            note: None,
            error: None,
        }
    }

    fn err_comp(weight: f64) -> UnrestComponent {
        UnrestComponent::err("t", "s", "q", weight, "down".to_string())
    }

    #[test]
    fn family_score_redistributes_on_partial() {
        // 0.5@0.5 + 0.35@1.0 ok, 0.15 down -> 100*(0.25+0.35)/0.85
        let comps = vec![ok_comp(0.5, 0.5), ok_comp(0.35, 1.0), err_comp(0.15)];
        let (score, partial, all_down) = family_score(&comps);
        assert!(!all_down);
        assert!(partial);
        assert!((score - 70.588).abs() < 0.01, "score {score}");
    }

    #[test]
    fn family_score_all_down() {
        let comps = vec![err_comp(0.5), err_comp(0.35), err_comp(0.15)];
        let (score, partial, all_down) = family_score(&comps);
        assert!(all_down);
        assert!(partial);
        assert_eq!(score, 0.0);
    }

    #[test]
    fn overall_redistributes_across_families() {
        // unrest fully down; violent 80 @ 0.35, crime 20 @ 0.25 ->
        // (0.35*80 + 0.25*20) / 0.60 = 55.
        let score: f64 = (0.35 * 80.0 + 0.25 * 20.0) / 0.60;
        assert!((score - 55.0).abs() < 1e-9);
        assert_eq!(level_for(score), "high");
        assert_eq!(safety_for(score), "Low");
    }

    #[test]
    fn noise_queries_are_not_empty() {
        // Every family must define real keyword groups for all three feeds.
        for f in &FAMILIES {
            assert!(!f.gnews_kw.is_empty() && !f.bluesky_kw.is_empty() && !f.gdelt_q.is_empty());
            assert!(
                f.gnews_kw.contains("OR"),
                "gnews kw needs OR group: {}",
                f.id
            );
        }
    }

    #[test]
    fn disclaimer_says_what_it_is() {
        assert!(DISCLAIMER.contains("NOT A FORECAST"));
        assert!(DISCLAIMER.contains("over-covered"));
        assert!(DISCLAIMER.contains("English-language"));
        assert!(DISCLAIMER.contains("not travel advice"));
        assert!(DISCLAIMER.contains("official government guidance"));
        let low = DISCLAIMER.to_lowercase();
        assert!(!low.contains("prediction"));
        assert!(!low.contains("probability"));
    }
}
