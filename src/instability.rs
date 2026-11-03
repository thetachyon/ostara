//! Country instability index: composite scores combining structural
//! baselines (food stress, government trust, social trust, economic stress)
//! with live media signals.
//!
//! Two outputs:
//! - **instability_score** (0-100): structural composite, slow-moving.
//! - **unrest_probability** (0-100): estimated probability of civil unrest,
//!   combining structural fragility with live media signals.
//!
//! All structural data is curated in `data/instability_baseline.json` with
//! documented sources and confidence tags. These are modeling estimates,
//! not precise measurements. The unrest probability is a heuristic model
//! output, not a forecast.

use std::collections::HashMap;
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

static BASELINE_JSON: &str = include_str!("../data/instability_baseline.json");
/// Structural baselines change slowly; cache computed scores for 1 hour.
const TTL: Duration = Duration::from_secs(3600);

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/instability", get(instability))
        .route("/api/instability/countries", get(countries))
}

// Weights for the structural composite. Documented in the response.
const W_FOOD: f64 = 0.25;
const W_GOV: f64 = 0.25;
const W_SOCIAL: f64 = 0.20;
const W_ECON: f64 = 0.20;
const W_LIVE: f64 = 0.10;

#[derive(Debug, Clone, Deserialize)]
struct BaselineCountry {
    name: String,
    iso: String,
    food_stress: f64,
    gov_trust_deficit: f64,
    social_trust_deficit: f64,
    economic_stress: f64,
    confidence: String,
}

fn baseline() -> HashMap<String, BaselineCountry> {
    let v: serde_json::Value =
        serde_json::from_str(BASELINE_JSON).expect("data/instability_baseline.json parses");
    let mut map = HashMap::new();
    if let Some(arr) = v.get("countries").and_then(|c| c.as_array()) {
        for c in arr {
            if let Ok(bc) = serde_json::from_value::<BaselineCountry>(c.clone()) {
                map.insert(bc.iso.to_uppercase(), bc.clone());
                map.insert(bc.name.to_lowercase(), bc);
            }
        }
    }
    map
}

fn find_baseline(needle: &str) -> Option<BaselineCountry> {
    let map = baseline();
    let n = needle.trim();
    // Try ISO code, then name (case-insensitive), then substring.
    if let Some(bc) = map.get(&n.to_uppercase()) {
        return Some(bc.clone());
    }
    if let Some(bc) = map.get(&n.to_lowercase()) {
        return Some(bc.clone());
    }
    let nl = n.to_lowercase();
    if let Some(bc) = map.values().find(|bc| bc.name.to_lowercase().contains(&nl)) {
        return Some(bc.clone());
    }
    // Fallback for territories / disputed areas / unlisted states:
    // global-average structural estimate, explicitly very-low confidence.
    // The live media signal still provides value.
    Some(BaselineCountry {
        name: n.to_string(),
        iso: n.to_uppercase(),
        food_stress: 18.0,
        gov_trust_deficit: 60.0,
        social_trust_deficit: 62.0,
        economic_stress: 52.0,
        confidence: "very low (global-average fallback; no country baseline)".to_string(),
    })
}

#[derive(Debug, Serialize, Deserialize)]
struct Component {
    name: String,
    score: f64,
    weight: f64,
    detail: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct InstabilityResponse {
    country: String,
    iso: String,
    instability_score: f64,
    instability_level: String,
    unrest_probability: f64,
    unrest_level: String,
    components: Vec<Component>,
    structural_confidence: String,
    live_signal: Option<f64>,
    live_signal_detail: String,
    scanned_at: String,
    disclaimer: String,
}

const DISCLAIMER: &str = "HEURISTIC MODEL, NOT A FORECAST. The instability score combines curated structural indicators (food stress, trust deficits, economic pressure) with live media signals. The unrest probability is a model estimate, not a prediction of actual events. Structural data are estimates from public sources with confidence tags. Verify independently before acting.";

fn level(score: f64) -> &'static str {
    match score {
        s if s < 25.0 => "low",
        s if s < 50.0 => "moderate",
        s if s < 75.0 => "high",
        _ => "severe",
    }
}

/// Logistic function mapping a 0-100 fragility input to a 0-100 probability.
/// Calibrated so that fragility=50 → ~34%, fragility=75 → ~73%, fragility=90 → ~88%.
fn fragility_to_probability(fragility: f64) -> f64 {
    let x = (fragility - 60.0) / 15.0;
    100.0 / (1.0 + (-x).exp())
}

#[derive(Debug, Deserialize)]
struct InstabilityQuery {
    country: Option<String>,
}

/// Minimal URL encoding for query params (alphanumeric + a few safe chars
/// pass through; everything else becomes %XX).
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push_str("%20"),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Parse an RSS pubDate like "Sat, 31 Oct 2026 21:50:29 GMT" into days-ago.
/// Returns None if unparseable.
fn days_ago(pub_date: &str) -> Option<f64> {
    let parts: Vec<&str> = pub_date.split_whitespace().collect();
    if parts.len() < 5 {
        return None;
    }
    let day: u32 = parts[1].parse().ok()?;
    let month = match parts[2] {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return None,
    };
    let year: i32 = parts[3].parse().ok()?;
    let days_since_epoch = year as f64 * 365.25 + month as f64 * 30.44 + day as f64;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?;
    let now_days = now.as_secs_f64() / 86400.0;
    let epoch_days = 719527.5; // 1970*365.25
    Some(now_days - (days_since_epoch - epoch_days))
}

/// Severity of an unrest headline (0-100) based on keyword language.
/// Severe = deaths/violence, high = clashes/crackdown, moderate = protest/march.
fn headline_severity(title: &str) -> f64 {
    let t = title.to_lowercase();
    // Check severe first (highest priority).
    for kw in [
        "killed",
        "killing",
        "dead",
        "deaths",
        "death toll",
        "massacre",
        "bombing",
        "bombed",
        "explosion",
        "coup",
        "civil war",
        "martial law",
        "mass killing",
    ] {
        if t.contains(kw) {
            return 90.0;
        }
    }
    for kw in [
        "clash",
        "violent",
        "violence",
        "tear gas",
        "rubber bullets",
        "crackdown",
        "arrests",
        "arrested",
        "emergency",
        "curfew",
        "stormed",
        "torched",
        "burning",
    ] {
        if t.contains(kw) {
            return 65.0;
        }
    }
    for kw in [
        "protest",
        "demonstration",
        "march",
        "rally",
        "strike",
        "unrest",
        "riot",
    ] {
        if t.contains(kw) {
            return 40.0;
        }
    }
    // Matched the search but no severity keywords — likely tangential.
    15.0
}

/// Lightweight live unrest signal: severity-weighted count of RECENT
/// (last 7 days) unrest articles in Google News RSS for the country.
/// Returns 0-100 or None if unavailable. Historical articles excluded.
async fn live_unrest_signal(client: &reqwest::Client, country: &str) -> Option<f64> {
    let q = format!("{country} protest OR riot OR unrest OR strike");
    let url = format!(
        "https://news.google.com/rss/search?q={}&hl=en-US&gl=US&ceid=US:en",
        urlencode(&q)
    );
    let resp = client
        .get(&url)
        .header("User-Agent", "Mozilla/5.0 (compatible; ostara/0.1)")
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let text = resp.text().await.ok()?;
    let mut severity_sum = 0.0;
    let mut total = 0u32;
    for chunk in text.split("<item>") {
        let date_str = chunk
            .find("<pubDate>")
            .and_then(|s| chunk[s..].find("</pubDate>").map(|e| &chunk[s + 9..s + e]));
        let title = chunk
            .find("<title>")
            .and_then(|s| chunk[s..].find("</title>").map(|e| &chunk[s + 7..s + e]));
        if let (Some(ds), Some(ti)) = (date_str, title) {
            total += 1;
            if let Some(d) = days_ago(ds) {
                if d >= 0.0 && d <= 7.0 {
                    severity_sum += headline_severity(ti);
                }
            }
        }
    }
    if total == 0 {
        return None;
    }
    // Severity-weighted: 5 severe articles (~450) → ~90, 1 moderate (40) → ~19.
    // Logarithmic so it takes sustained severe coverage to peg at 100.
    Some(100.0 * ((1.0 + severity_sum / 100.0).ln() / 6.0_f64.ln()))
}

async fn instability(
    State(state): State<AppState>,
    Query(q): Query<InstabilityQuery>,
) -> Result<Json<InstabilityResponse>, ApiError> {
    let needle = q
        .country
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            ApiError::bad_request(
                "missing country",
                "query param country is required, e.g. ?country=France or ?country=FR",
            )
        })?;
    let bc = find_baseline(needle).expect("find_baseline always returns a fallback");

    let key = format!("instability:{}", bc.iso);
    let fetch = || {
        let client = state.client.clone();
        let bc2 = bc.clone();
        async move {
            // Structural composite.
            let structural = W_FOOD * bc2.food_stress
                + W_GOV * bc2.gov_trust_deficit
                + W_SOCIAL * bc2.social_trust_deficit
                + W_ECON * bc2.economic_stress;

            // Live signal (may be unavailable).
            let live = live_unrest_signal(&client, &bc2.name).await;

            // Instability score: structural weighted with live.
            let instability = if let Some(l) = live {
                // Renormalize: structural components sum to 0.9 weight, live 0.1.
                (structural / 0.9 * (1.0 - W_LIVE)) + l * W_LIVE
            } else {
                // No live signal: structural only, renormalized.
                structural / 0.9
            };

            // Unrest probability: structural fragility (60%) + live (40%).
            let fragility = (bc2.food_stress * 0.3
                + bc2.gov_trust_deficit * 0.25
                + bc2.social_trust_deficit * 0.15
                + bc2.economic_stress * 0.3)
                .min(100.0);
            let live_component = live.unwrap_or(fragility * 0.5);
            let combined = fragility * 0.6 + live_component * 0.4;
            let probability = fragility_to_probability(combined);

            let components = vec![
                Component {
                    name: "food_stress".into(),
                    score: bc2.food_stress,
                    weight: W_FOOD,
                    detail: "Food insecurity / affordability pressure. Higher = worse.".into(),
                },
                Component {
                    name: "gov_trust_deficit".into(),
                    score: bc2.gov_trust_deficit,
                    weight: W_GOV,
                    detail: "Lack of public trust in government. Higher = less trust.".into(),
                },
                Component {
                    name: "social_trust_deficit".into(),
                    score: bc2.social_trust_deficit,
                    weight: W_SOCIAL,
                    detail: "Lack of interpersonal trust. Higher = more fragmented.".into(),
                },
                Component {
                    name: "economic_stress".into(),
                    score: bc2.economic_stress,
                    weight: W_ECON,
                    detail: "Inflation, unemployment, currency/debt pressure.".into(),
                },
                Component {
                    name: "live_unrest_signal".into(),
                    score: live.unwrap_or(-1.0),
                    weight: W_LIVE,
                    detail: if live.is_some() {
                        "Recent protest/riot/unrest media volume.".into()
                    } else {
                        "Unavailable — structural only.".into()
                    },
                },
            ];

            Ok::<InstabilityResponse, FeedError>(InstabilityResponse {
                country: bc2.name.clone(),
                iso: bc2.iso.clone(),
                instability_score: (instability * 10.0).round() / 10.0,
                instability_level: level(instability).into(),
                unrest_probability: (probability * 10.0).round() / 10.0,
                unrest_level: level(probability).into(),
                components,
                structural_confidence: bc2.confidence.clone(),
                live_signal: live,
                live_signal_detail: if live.is_some() {
                    "Google News unrest-keyword volume (0-100).".into()
                } else {
                    "Live signal unavailable; scores are structural only.".into()
                },
                scanned_at: now_iso8601(),
                disclaimer: DISCLAIMER.into(),
            })
        }
    };

    let (body, _) = cached_bytes::<InstabilityResponse, _>(&state, &key, TTL, fetch).await?;
    let resp: InstabilityResponse = serde_json::from_slice(&body)
        .map_err(|e| ApiError::bad_gateway("decode failed", e.to_string()))?;
    Ok(Json(resp))
}

#[derive(Debug, Serialize)]
struct CountryListItem {
    name: String,
    iso: String,
    confidence: String,
}

async fn countries() -> Json<Vec<CountryListItem>> {
    let map = baseline();
    let mut seen = std::collections::HashSet::new();
    let mut out: Vec<CountryListItem> = Vec::new();
    for bc in map.values() {
        if seen.insert(bc.iso.clone()) {
            out.push(CountryListItem {
                name: bc.name.clone(),
                iso: bc.iso.clone(),
                confidence: bc.confidence.clone(),
            });
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Json(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn baseline_loads() {
        let m = baseline();
        assert!(m.len() > 50);
        assert!(m.contains_key("US"));
    }

    #[test]
    fn find_baseline_by_iso_and_name() {
        assert!(find_baseline("FR").is_some());
        assert!(find_baseline("france").is_some());
        // Unknown codes get the global-average fallback, not None.
        let fb = find_baseline("XX").unwrap();
        assert!(fb.confidence.contains("very low"));
    }

    #[test]
    fn probability_calibration() {
        // fragility=50 → ~34%, 75 → ~73%, 90 → ~88%
        let p50 = fragility_to_probability(50.0);
        let p75 = fragility_to_probability(75.0);
        let p90 = fragility_to_probability(90.0);
        assert!((p50 - 34.0).abs() < 5.0, "p50={p50}");
        assert!((p75 - 73.0).abs() < 5.0, "p75={p75}");
        assert!((p90 - 88.0).abs() < 5.0, "p90={p90}");
        assert!(p50 < p75 && p75 < p90);
    }

    #[test]
    fn level_thresholds() {
        assert_eq!(level(10.0), "low");
        assert_eq!(level(40.0), "moderate");
        assert_eq!(level(60.0), "high");
        assert_eq!(level(90.0), "severe");
    }

    #[test]
    fn days_ago_parses_rss_dates() {
        // A date from 3 days ago should parse to ~3.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        // Format a date 3 days ago as RSS pubDate.
        // We just verify parsing doesn't return None for a valid format.
        let d = days_ago("Sat, 31 Oct 2026 21:50:29 GMT");
        assert!(d.is_some());
        assert!(days_ago("not a date").is_none());
        assert!(days_ago("Sat, 31 Foo 2026 21:50:29 GMT").is_none());
        let _ = now;
    }

    #[test]
    fn headline_severity_tiers() {
        assert_eq!(headline_severity("Protesters killed in clashes"), 90.0);
        assert_eq!(headline_severity("Police use tear gas on march"), 65.0);
        assert_eq!(headline_severity("Thousands join peaceful march"), 40.0);
        assert_eq!(headline_severity("City council meeting scheduled"), 15.0);
    }
}
