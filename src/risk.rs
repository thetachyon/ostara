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
        self, MIN_BASELINE_BINS, UnrestComponent, gdelt_country_name, gdelt_normalize,
        gdelt_window_sums, now_secs,
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

pub const METHODOLOGY: &str = "Per-place risk & safety score (0-100), heuristic only. Three signal families, each built from baseline-normalized components (7-day recent rate vs baseline; ratio r maps to clamp((r-1)/4,0,1); low-evidence discount below 5 recent items): unrest (weight 0.40) — protest/strike/riot keywords; violent_incidents (weight 0.35) — bombing/shooting/terrorist-attack/explosion keywords; crime_disorder (weight 0.25) — crime/murder/robbery/looting keywords. Each family fans out over GDELT timelinevol (country path only; weight 0.50 within family), Google News date-bounded queries (0.35), and Bluesky windowed searches (0.15). ANTI-SATURATION: upstreams cap windows at 100 items, understating single-query 83-day baselines for high-coverage places. News/social baselines come from a weekly chunked refresh (6x14-day chunks summed) when fresh (<10d), else a legacy 83-day query; a saturated baseline nulls the leg with saturated_baseline instead of scoring (weights redistribute; score null/level unknown when nothing is usable). Per-leg diagnostics: saturated, baseline_source (chunked|legacy|unknown), cap_fill_hours/intensity. THIN-BASIS RULE: when the surviving signal rests on <50% of intended weight — within a family or across families — the score is kept (it is real) but flagged thin_basis:true, confidence forced to \"low\", and the level capped one step down (severe→high→moderate→low); the note names the live families. City path: GDELT has no city granularity — its leg reports ok:false and family weights redistribute. score_basis documents the time-ratio score; percentile/rank give the baseline-free peer rank vs the 30-place weekly watchlist (rank 1 = hottest; percentile = 100*(n-rank)/(n-1)). When a component or family is unavailable its weight redistributes proportionally and the score is marked partial. Levels: <25 low, <50 moderate, <75 high, >=75 severe. Safety is the inverse reading of the same score: High (<25), Moderate (<50), Low (<75), Very low (>=75).";

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
    /// "low" | "moderate" | "high" | "severe", or "unknown" when the family
    /// has no working components. Capped one step down when `thin_basis`.
    pub level: String,
    /// True when the family scored on <50% of its intended leg weight —
    /// the score is real but rests on a thin basis; treat the level as
    /// provisional.
    pub thin_basis: bool,
    /// Fraction of the family's intended leg weight that actually scored
    /// (0–1). The denominator the thin-basis rule is judged against.
    pub live_weight: f64,
    pub partial: bool,
    pub components: Vec<UnrestComponent>,
}

#[derive(Debug, Serialize)]
pub struct RiskResponse {
    pub place: RiskPlace,
    /// Time-ratio score 0–100 (7d recent vs baseline). Null when no
    /// component produced a usable score — an honest unknown.
    pub score: Option<f64>,
    /// "low" | "moderate" | "high" | "severe", or "unknown" when null.
    pub level: String,
    /// Inverse reading of the same score: "High"/"Moderate"/"Low"/"Very low"
    /// ("Unknown" when score is null).
    pub safety: String,
    /// What `score` measures (kept for backward compatibility).
    pub score_basis: String,
    /// Baseline-free peer rank vs the 30-place watchlist, when the place
    /// matches a watchlist entry and the weekly snapshot is fresh.
    pub percentile: Option<f64>,
    /// 1-based rank among watchlist peers (1 = hottest).
    pub rank: Option<usize>,
    /// Watchlist peer count.
    pub peer_count: Option<usize>,
    /// What `percentile`/`rank` measure, when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub percentile_basis: Option<String>,
    /// Why peer fields are null, when they are.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub peer_note: Option<String>,
    /// "high" | "medium" | "low". high = full signal; medium = partial
    /// (degraded feeds, weights redistributed); low = thin basis or no
    /// usable score.
    pub confidence: String,
    /// True when the composite scored on <50% of its intended family
    /// weight — the surviving signal is real, but the headline rests on a
    /// thin basis: `confidence` is forced to "low" and `level` is capped
    /// one step down. Treat the level as provisional.
    pub thin_basis: bool,
    pub partial: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    pub families: Vec<RiskFamily>,
    pub methodology: String,
    pub generated_at: String,
    pub disclaimer: String,
}

pub const SCORE_BASIS: &str = "time-ratio: 7-day recent media volume vs the place's own baseline (weekly chunked 6x14-day refresh when fresh, else legacy 83-day query); saturated baselines null the leg instead of scoring";
pub const PERCENTILE_BASIS: &str = "peer-rank: this place's 7-day recent composite media volume ranked against the 30-place weekly risk watchlist — baseline-free, robust to upstream capping";

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
        saturated: false,
        baseline_source: "legacy".to_string(),
        cap_fill_hours: None,
        cap_fill_intensity: None,
        note,
        error: None,
    }
}

/// Google News leg for one family, via the shared keyword-leg builder
/// (chunked baselines, saturation nulling, cap-fill diagnostics).
async fn gnews_component(state: &AppState, subject: &str, fam: &FamilyDef) -> UnrestComponent {
    let base_q = format!("{} {}", subject, fam.gnews_kw);
    let now = now_secs_local();
    let (recent_since, _) = unrest::day_windows(now);
    let q = format!("{base_q} after:{}", unrest::ymd(recent_since));
    unrest::keyword_leg_component(
        state,
        unrest::LegSpec {
            leg: "gnews",
            family: fam.id,
            subject,
            base_query: &base_q,
            weight: FW_GNEWS,
            signal: fam.signal_gnews,
            query_repr: &q,
            raw_unit: "articles (last 7 days)",
        },
    )
    .await
}

/// Bluesky leg for one family, via the shared keyword-leg builder.
async fn bluesky_component(state: &AppState, subject: &str, fam: &FamilyDef) -> UnrestComponent {
    let base_q = format!("{} {}", subject, fam.bluesky_kw);
    unrest::keyword_leg_component(
        state,
        unrest::LegSpec {
            leg: "bluesky",
            family: fam.id,
            subject,
            base_query: &base_q,
            weight: FW_BLUESKY,
            signal: fam.signal_bluesky,
            query_repr: &base_q,
            raw_unit: "posts (last 7 days)",
        },
    )
    .await
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
struct FamilyScore {
    score: f64,
    partial: bool,
    all_down: bool,
    /// Fraction of the family's intended leg weight that actually scored
    /// (0–1). The anti-saturation package nulls legs with saturated
    /// baselines; when most legs are nulled this fraction goes thin.
    live_weight: f64,
    /// True when the family scored on <50% of its intended leg weight.
    thin_basis: bool,
}

fn family_score(components: &[UnrestComponent]) -> FamilyScore {
    let total_weight: f64 = components.iter().map(|c| c.weight).sum();
    let ok_weight: f64 = components.iter().filter(|c| c.ok).map(|c| c.weight).sum();
    let live_weight = if total_weight > 0.0 {
        ok_weight / total_weight
    } else {
        0.0
    };
    if ok_weight <= 0.0 {
        return FamilyScore {
            score: 0.0,
            partial: true,
            all_down: true,
            live_weight: 0.0,
            thin_basis: false,
        };
    }
    let score = 100.0
        * components
            .iter()
            .filter(|c| c.ok)
            .map(|c| c.weight * c.normalized_0_1)
            .sum::<f64>()
        / ok_weight;
    FamilyScore {
        score,
        partial: components.iter().any(|c| !c.ok),
        all_down: false,
        live_weight,
        thin_basis: live_weight < 0.5,
    }
}

/// Cap a level one step down the ladder (severe→high→moderate→low).
/// "low" and "unknown" have nothing below them and stay put.
fn cap_level_one_step(level: &str) -> String {
    match level {
        "severe" => "high".to_string(),
        "high" => "moderate".to_string(),
        "moderate" => "low".to_string(),
        _ => level.to_string(),
    }
}

/// Composite-level thin-basis qualification, pure for testing.
/// `live_weight_frac` = fraction of total intended family weight that
/// actually scored. When it drops below 0.5 the surviving signal is kept
/// but qualified: thin_basis flag, confidence forced low, headline level
/// capped one step down.
struct CompositeQualifiers {
    thin_basis: bool,
    level: String,
    confidence: String,
}

fn qualify_composite(
    score: Option<f64>,
    live_weight_frac: f64,
    partial: bool,
) -> CompositeQualifiers {
    let thin_basis = score.is_some() && live_weight_frac < 0.5;
    let level = match score {
        Some(s) => {
            let lvl = level_for(s);
            if thin_basis {
                cap_level_one_step(lvl)
            } else {
                lvl.to_string()
            }
        }
        None => "unknown".to_string(),
    };
    let confidence = if thin_basis || score.is_none() {
        "low"
    } else if partial {
        "medium"
    } else {
        "high"
    }
    .to_string();
    CompositeQualifiers {
        thin_basis,
        level,
        confidence,
    }
}

/// The thin-basis note sentence, pure for testing.
fn thin_basis_note(
    live_weight_frac: f64,
    live_families: &[String],
    thin_families: &[String],
) -> String {
    let pct = (live_weight_frac * 100.0).round() as i64;
    let mut note = format!(
        "thin basis: score rests on {pct}% of intended signal ({}) — treat the level as provisional.",
        live_families.join(", ")
    );
    if !thin_families.is_empty() {
        note.push_str(&format!(
            " thin-basis families: {}.",
            thin_families.join(", ")
        ));
    }
    note
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

/// Resolved place: the API response place block, the subject string used
/// in news/social queries (and chunked-baseline keys), and per-family
/// GDELT queries (`None` on the city path).
struct ResolvedPlace {
    place: RiskPlace,
    subject: String,
    gdelt_queries: Vec<Option<String>>,
}

fn resolve_place(
    place_raw: &str,
    kind: &str,
    hint: Option<&str>,
) -> Result<ResolvedPlace, ApiError> {
    enum Resolved<'a> {
        Country(crate::geo::CountryIdentity),
        City(&'a str),
    }
    let resolved = match kind {
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
    Ok(match resolved {
        Resolved::Country(c) => {
            let subject = c.name.clone();
            let gdelt_queries = FAMILIES
                .iter()
                .map(|f| {
                    Some(format!(
                        "{} sourcecountry:\"{}\"",
                        f.gdelt_q,
                        gdelt_country_name(&c.name)
                    ))
                })
                .collect();
            ResolvedPlace {
                place: RiskPlace {
                    name: c.name.clone(),
                    kind: "country".to_string(),
                    iso2: Some(c.iso2.clone()),
                    iso3: Some(c.iso3.clone()),
                    country_hint: None,
                },
                subject,
                gdelt_queries,
            }
        }
        Resolved::City(name) => {
            let subject = match hint {
                Some(h) => format!("{name} {h}"),
                None => name.to_string(),
            };
            ResolvedPlace {
                place: RiskPlace {
                    name: name.to_string(),
                    kind: "city".to_string(),
                    iso2: None,
                    iso3: None,
                    country_hint: hint.map(str::to_string),
                },
                subject,
                gdelt_queries: vec![None, None, None],
            }
        }
    })
}

/// Validate the shared `place`/`kind`/`country` params. Returns
/// `(place_raw, kind, hint)`.
fn parse_place_params(q: &RiskQuery) -> Result<(&str, String, Option<&str>), ApiError> {
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
    Ok((place_raw, kind, hint))
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
    let (place_raw, kind, hint) = parse_place_params(&q)?;
    let ResolvedPlace {
        place,
        subject,
        gdelt_queries,
    } = resolve_place(place_raw, &kind, hint)?;

    // Concurrent fan-out over families; latency ~= the slowest component.
    let (f0, f1, f2) = tokio::join!(
        family_components(&state, &subject, gdelt_queries[0].clone(), &FAMILIES[0]),
        family_components(&state, &subject, gdelt_queries[1].clone(), &FAMILIES[1]),
        family_components(&state, &subject, gdelt_queries[2].clone(), &FAMILIES[2]),
    );
    let family_comps = [f0, f1, f2];

    let mut families: Vec<RiskFamily> = Vec::with_capacity(3);
    let mut fam_scores: Vec<(f64, f64)> = Vec::new(); // (weight, score)
    let mut live_family_names: Vec<String> = Vec::new();
    let mut thin_family_names: Vec<String> = Vec::new();
    for (fam, comps) in FAMILIES.iter().zip(family_comps.iter()) {
        let fs = family_score(comps);
        if !fs.all_down {
            fam_scores.push((fam.weight, fs.score));
            live_family_names.push(fam.id.to_string());
        }
        if fs.thin_basis {
            thin_family_names.push(fam.id.to_string());
        }
        let family_level = if fs.all_down {
            "unknown".to_string()
        } else if fs.thin_basis {
            cap_level_one_step(level_for(fs.score))
        } else {
            level_for(fs.score).to_string()
        };
        families.push(RiskFamily {
            family: fam.id.to_string(),
            weight: fam.weight,
            score_0_100: (fs.score * 10.0).round() / 10.0,
            level: family_level,
            thin_basis: fs.thin_basis,
            live_weight: (fs.live_weight * 1000.0).round() / 1000.0,
            partial: fs.partial,
            components: comps.to_vec(),
        });
    }

    let total_family_weight: f64 = FAMILIES.iter().map(|f| f.weight).sum();
    let work_weight: f64 = fam_scores.iter().map(|(w, _)| w).sum();
    let live_weight_frac = if total_family_weight > 0.0 {
        work_weight / total_family_weight
    } else {
        0.0
    };
    let score = if work_weight > 0.0 {
        Some(
            (fam_scores.iter().map(|(w, s)| w * s).sum::<f64>() / work_weight * 10.0).round()
                / 10.0,
        )
    } else {
        None
    };
    // THIN-BASIS RULE: the surviving signal is real, so keep the
    // redistributed score — but when it rests on <50% of the intended
    // family weight, qualify it: flag it, force confidence low, and cap
    // the headline level one step down. No score at all (all families
    // down) is already "unknown" and needs no flag.
    let partial = families.iter().any(|f| f.partial);
    let q = qualify_composite(score, live_weight_frac, partial);
    let thin_basis = q.thin_basis;
    let (level, safety) = match score {
        Some(s) => (q.level, safety_for(s).to_string()),
        None => (q.level, "Unknown".to_string()),
    };
    let confidence = q.confidence;

    // Baseline-free peer rank vs the weekly watchlist snapshot.
    let (percentile, rank, peer_count, percentile_basis, peer_note) =
        match crate::feeds::risk_map::peer_status(&place.name) {
            crate::feeds::risk_map::PeerStatus::Ranked {
                percentile,
                rank,
                peer_count,
            } => (
                Some(percentile),
                Some(rank),
                Some(peer_count),
                Some(PERCENTILE_BASIS.to_string()),
                None,
            ),
            crate::feeds::risk_map::PeerStatus::NoSnapshot => (
                None,
                None,
                None,
                None,
                Some("no watchlist snapshot yet".to_string()),
            ),
            crate::feeds::risk_map::PeerStatus::Stale => (
                None,
                None,
                None,
                None,
                Some("watchlist snapshot is stale (>9 days)".to_string()),
            ),
            crate::feeds::risk_map::PeerStatus::NotListed => (
                None,
                None,
                None,
                None,
                Some("place is not in the 30-place risk watchlist".to_string()),
            ),
            crate::feeds::risk_map::PeerStatus::NoPeerData => (
                None,
                None,
                None,
                None,
                Some(
                    "watchlist snapshot predates peer ranks — the next weekly scoring run adds them"
                        .to_string(),
                ),
            ),
        };

    let down: Vec<String> = families
        .iter()
        .flat_map(|f| f.components.iter())
        .filter(|c| !c.ok)
        .map(|c| format!("{}:{}", c.source, c.error.as_deref().unwrap_or("error")))
        .collect();
    let mut note_parts: Vec<String> = Vec::new();
    if partial {
        note_parts.push(format!(
            "partial score: {} unavailable; working weights redistributed proportionally.",
            down.join("; ")
        ));
    }
    if thin_basis {
        note_parts.push(thin_basis_note(
            live_weight_frac,
            &live_family_names,
            &thin_family_names,
        ));
    } else if !thin_family_names.is_empty() {
        // Composite rests on >=50% of intended weight, but some of the
        // contributing families are thin themselves — say so.
        note_parts.push(format!(
            "thin-basis families (levels capped one step down): {}.",
            thin_family_names.join(", ")
        ));
    }
    let note = if note_parts.is_empty() {
        None
    } else {
        Some(note_parts.join(" "))
    };

    Ok(Json(RiskResponse {
        place,
        score,
        level,
        safety,
        score_basis: SCORE_BASIS.to_string(),
        percentile,
        rank,
        peer_count,
        percentile_basis,
        peer_note,
        confidence,
        thin_basis,
        partial,
        note,
        families,
        methodology: METHODOLOGY.to_string(),
        generated_at: now_iso8601(),
        disclaimer: DISCLAIMER.to_string(),
    }))
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/risk", get(risk))
        .route("/api/risk/baseline-plan", get(baseline_plan))
        .route("/api/risk/window-count", get(window_count))
}

// ---------------------------------------------------------------------------
// Chunked-baseline infrastructure: plan + window counting
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct BaselineChunk {
    /// `YYYY-MM-DD` (inclusive start).
    pub after: String,
    /// `YYYY-MM-DD` (exclusive end).
    pub before: String,
}

#[derive(Debug, Serialize)]
pub struct BaselineQueryDef {
    pub family: String,
    pub leg: String,
    pub base_query: String,
}

#[derive(Debug, Serialize)]
pub struct BaselinePlanResponse {
    pub place: RiskPlace,
    pub subject: String,
    /// 6 consecutive 14-day chunks covering the last 84 days, oldest first.
    pub chunks: Vec<BaselineChunk>,
    /// One entry per (family, leg) — gnews + bluesky only (GDELT's
    /// timelinevol has its own binning and is unreachable from some
    /// sandboxes; its leg keeps the legacy path).
    pub queries: Vec<BaselineQueryDef>,
    pub generated_at: String,
}

/// `GET /api/risk/baseline-plan?place=<name>&kind=auto|country|city[&country=<hint>]`.
///
/// Returns the exact chunk windows and base queries the weekly
/// `scripts/risk_baseline_refresh.sh` uses, so query construction lives in
/// one place (here) and the script is pure orchestration. Same params and
/// resolution as `/api/risk`.
pub async fn baseline_plan(
    State(_state): State<AppState>,
    Query(q): Query<RiskQuery>,
) -> Result<Json<BaselinePlanResponse>, ApiError> {
    let (place_raw, kind, hint) = parse_place_params(&q)?;
    let ResolvedPlace { place, subject, .. } = resolve_place(place_raw, &kind, hint)?;

    let now = now_secs_local();
    let today = now - now.rem_euclid(86_400);
    let chunks: Vec<BaselineChunk> = (0..unrest::BASELINE_CHUNKS)
        .map(|i| {
            let start = today - 84 * 86_400 + i as i64 * unrest::BASELINE_CHUNK_DAYS * 86_400;
            let end = start + unrest::BASELINE_CHUNK_DAYS * 86_400;
            BaselineChunk {
                after: unrest::ymd(start),
                before: unrest::ymd(end),
            }
        })
        .collect();
    let queries: Vec<BaselineQueryDef> = FAMILIES
        .iter()
        .flat_map(|f| {
            [
                BaselineQueryDef {
                    family: f.id.to_string(),
                    leg: "gnews".to_string(),
                    base_query: format!("{} {}", subject, f.gnews_kw),
                },
                BaselineQueryDef {
                    family: f.id.to_string(),
                    leg: "bluesky".to_string(),
                    base_query: format!("{} {}", subject, f.bluesky_kw),
                },
            ]
        })
        .collect();

    Ok(Json(BaselinePlanResponse {
        place,
        subject,
        chunks,
        queries,
        generated_at: now_iso8601(),
    }))
}

#[derive(Debug, Deserialize)]
pub struct WindowCountQuery {
    pub leg: Option<String>,
    pub base_query: Option<String>,
    pub after: Option<String>,
    pub before: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct WindowCountResponse {
    pub leg: String,
    pub count: usize,
    /// True when the query hit the upstream item cap (count is a lower bound).
    pub saturated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cap_fill_hours: Option<f64>,
}

/// `GET /api/risk/window-count?leg=gnews|bluesky&base_query=<q>&after=<YYYY-MM-DD>&before=<YYYY-MM-DD>`.
///
/// Counts one upstream window through the feed's normal cache path — the
/// primitive the weekly baseline refresh calls per chunk. `after`/`before`
/// are day dates; the window is `[after, before)`.
pub async fn window_count(
    State(state): State<AppState>,
    Query(q): Query<WindowCountQuery>,
) -> Result<Json<WindowCountResponse>, ApiError> {
    let leg = q
        .leg
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ApiError::bad_request("missing leg", "leg must be gnews or bluesky"))?;
    if leg != "gnews" && leg != "bluesky" {
        return Err(ApiError::bad_request(
            "bad leg",
            "leg must be gnews or bluesky",
        ));
    }
    let base_query = q
        .base_query
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ApiError::bad_request("missing base_query", "base_query is required"))?;
    let after = q
        .after
        .as_deref()
        .map(str::trim)
        .filter(|s| valid_ymd(s))
        .ok_or_else(|| ApiError::bad_request("bad after", "after must be a YYYY-MM-DD date"))?;
    let before = q
        .before
        .as_deref()
        .map(str::trim)
        .filter(|s| valid_ymd(s))
        .ok_or_else(|| ApiError::bad_request("bad before", "before must be a YYYY-MM-DD date"))?;
    if before <= after {
        return Err(ApiError::bad_request(
            "bad window",
            "before must be after after",
        ));
    }

    let stats = if leg == "gnews" {
        unrest::gnews_window_stats(&state, base_query, after, Some(before))
            .await
            .map_err(|e| ApiError::bad_gateway("window count failed", e))?
    } else {
        unrest::bluesky_window_stats(
            &state,
            base_query,
            &unrest::rfc3339_day(ymd_to_epoch(after).ok_or_else(|| {
                ApiError::bad_request("bad after", "after must be a YYYY-MM-DD date")
            })?),
            Some(&unrest::rfc3339_day(ymd_to_epoch(before).ok_or_else(
                || ApiError::bad_request("bad before", "before must be a YYYY-MM-DD date"),
            )?)),
        )
        .await
        .map_err(|e| ApiError::bad_gateway("window count failed", e))?
    };
    let now = now_secs_local();
    Ok(Json(WindowCountResponse {
        leg: leg.to_string(),
        count: stats.count,
        saturated: stats.saturated,
        cap_fill_hours: unrest::cap_fill_hours(stats.oldest_epoch, now),
    }))
}

/// Strict `YYYY-MM-DD` shape check (also rejects impossible months/days
/// via the round-trip).
fn valid_ymd(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() != 10 || b[4] != b'-' || b[7] != b'-' {
        return false;
    }
    ymd_to_epoch(s).is_some()
}

/// `YYYY-MM-DD` → epoch seconds at 00:00 UTC. `None` on malformed input.
fn ymd_to_epoch(s: &str) -> Option<i64> {
    let epoch = crate::time::parse_rfc3339(&format!("{s}T00:00:00Z"))?;
    // Round-trip guard: rejects e.g. month 13 or day 32.
    if unrest::ymd(epoch) == s {
        Some(epoch)
    } else {
        None
    }
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
            saturated: false,
            baseline_source: "legacy".to_string(),
            cap_fill_hours: None,
            cap_fill_intensity: None,
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
        let fs = family_score(&comps);
        assert!(!fs.all_down);
        assert!(fs.partial);
        assert!((fs.score - 70.588).abs() < 0.01, "score {}", fs.score);
        // 0.85 of leg weight live — not thin.
        assert!((fs.live_weight - 0.85).abs() < 1e-9);
        assert!(!fs.thin_basis);
    }

    #[test]
    fn family_score_all_down() {
        let comps = vec![err_comp(0.5), err_comp(0.35), err_comp(0.15)];
        let fs = family_score(&comps);
        assert!(fs.all_down);
        assert!(fs.partial);
        assert_eq!(fs.score, 0.0);
        assert_eq!(fs.live_weight, 0.0);
        assert!(!fs.thin_basis); // no score at all — unknown, not "thin"
    }

    #[test]
    fn thin_basis_boundary_at_half() {
        // Exactly 0.50 live weight is NOT thin (>= 0.5 keeps current behavior).
        let comps = vec![ok_comp(0.5, 0.8), err_comp(0.5)];
        let fs = family_score(&comps);
        assert!((fs.live_weight - 0.5).abs() < 1e-9);
        assert!(!fs.thin_basis, "0.50 must not be thin");
        // A hair below is thin.
        let comps = vec![ok_comp(0.35, 0.8), ok_comp(0.14, 0.8), err_comp(0.51)];
        let fs = family_score(&comps);
        assert!(fs.live_weight < 0.5);
        assert!(fs.thin_basis, "0.49 must be thin");
    }

    #[test]
    fn thin_basis_family_keeps_score_but_flags() {
        // Only the 0.15-weight leg survives at normalized 1.0.
        let comps = vec![err_comp(0.5), err_comp(0.35), ok_comp(0.15, 1.0)];
        let fs = family_score(&comps);
        assert!(!fs.all_down);
        assert!((fs.score - 100.0).abs() < 1e-9, "score kept: {}", fs.score);
        assert!(fs.thin_basis);
        assert!((fs.live_weight - 0.15).abs() < 1e-9);
    }

    #[test]
    fn level_cap_steps_down_once() {
        assert_eq!(cap_level_one_step("severe"), "high");
        assert_eq!(cap_level_one_step("high"), "moderate");
        assert_eq!(cap_level_one_step("moderate"), "low");
        assert_eq!(cap_level_one_step("low"), "low"); // floor
        assert_eq!(cap_level_one_step("unknown"), "unknown");
    }

    #[test]
    fn qualify_composite_china_case() {
        // China: unrest + violent_incidents nulled, only crime_disorder
        // (0.25) live at 100. Live weight 0.25 < 0.5 → thin basis:
        // score kept, level capped severe→high, confidence low.
        let q = qualify_composite(Some(100.0), 0.25, true);
        assert!(q.thin_basis);
        assert_eq!(q.level, "high");
        assert_eq!(q.confidence, "low");
    }

    #[test]
    fn qualify_composite_healthy_cases() {
        // Full signal, no partial: high confidence, no cap.
        let q = qualify_composite(Some(80.0), 1.0, false);
        assert!(!q.thin_basis);
        assert_eq!(q.level, "severe");
        assert_eq!(q.confidence, "high");
        // Partial but >= 0.5 live: medium, no cap.
        let q = qualify_composite(Some(80.0), 0.6, true);
        assert!(!q.thin_basis);
        assert_eq!(q.level, "severe");
        assert_eq!(q.confidence, "medium");
        // Exactly 0.5: not thin.
        let q = qualify_composite(Some(30.0), 0.5, true);
        assert!(!q.thin_basis);
        assert_eq!(q.level, "moderate");
        // No score at all: unknown + low, no thin flag.
        let q = qualify_composite(None, 0.0, true);
        assert!(!q.thin_basis);
        assert_eq!(q.level, "unknown");
        assert_eq!(q.confidence, "low");
    }

    #[test]
    fn qualify_composite_thin_caps_every_level() {
        // 0.4 live weight → thin; each score's level steps down once.
        let cases = [
            (100.0, "high"),
            (80.0, "high"),
            (74.9, "moderate"),
            (60.0, "moderate"),
            (49.9, "low"),
            (30.0, "low"),
            (24.9, "low"),
            (0.0, "low"),
        ];
        for (score, capped) in cases {
            let q = qualify_composite(Some(score), 0.4, true);
            assert!(q.thin_basis);
            assert_eq!(q.level, capped, "score {score}");
            assert_eq!(q.confidence, "low");
        }
    }

    #[test]
    fn thin_basis_note_text() {
        let note = thin_basis_note(
            0.25,
            &["crime_disorder".to_string()],
            &["crime_disorder".to_string()],
        );
        assert!(
            note.contains("thin basis: score rests on 25% of intended signal (crime_disorder)"),
            "note: {note}"
        );
        assert!(
            note.contains("treat the level as provisional"),
            "note: {note}"
        );
        assert!(
            note.contains("thin-basis families: crime_disorder"),
            "note: {note}"
        );
        // No forbidden forecast language in the note.
        let low = note.to_lowercase();
        assert!(!low.contains("forecast"));
        assert!(!low.contains("prediction"));
        assert!(!low.contains("probability"));
    }

    #[test]
    fn thin_basis_note_without_thin_families() {
        // Composite thin via all-down families (no family scored thin
        // itself): the family list clause is omitted.
        let note = thin_basis_note(0.25, &["crime_disorder".to_string()], &[]);
        assert!(note.contains("25% of intended signal (crime_disorder)"));
        assert!(!note.contains("thin-basis families"));
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
