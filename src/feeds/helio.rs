//! Space weather feed: NOAA SWPC keyless JSON (GOES X-ray flux + Kp index).
//!
//! - X-ray flux: `GET https://services.swpc.noaa.gov/json/goes/primary/xrays-7-day.json`
//!   → array of `{"time_tag", "satellite", "flux", "observed_flux",
//!   "energy", ...}`. `flux` is W/m² in the band named by `energy`; the
//!   `0.1-0.8nm` band is the GOES X-ray class band (A/B/C/M/X classes).
//!   (Verified 2026-11-01 via live probe: 20,146 records, 1-minute cadence.)
//! - Planetary K index: `GET https://services.swpc.noaa.gov/json/planetary_k_index_1m.json`
//!   → array of `{"time_tag", "kp_index", "estimated_kp", "kp", ...}`.
//!   (Verified 2026-11-01: 358 records.)
//!
//! Both are keyless. Only the latest value of each series is served.

use std::time::Duration;

use axum::{extract::State, response::IntoResponse};
use serde::{Deserialize, Serialize};

use super::{cached_bytes, get_json, json_bytes};
use crate::{
    config::Config,
    error::{ApiError, FeedError},
    routes::AppState,
    time::now_iso8601,
};

/// Cache TTL: SWPC updates every minute; 10 minutes keeps it fresh enough
/// for a dashboard without hammering the service.
pub const TTL: Duration = Duration::from_secs(600);
pub const FEED: &str = "swpc";

#[derive(Debug, Serialize)]
pub struct HelioResponse {
    pub generated_at: String,
    /// Latest GOES 0.1–0.8nm X-ray flux in W/m² (X-ray class band).
    pub xray_flux_latest: Option<f64>,
    /// Latest estimated planetary K index (0–9 scale).
    pub kp_latest: Option<f64>,
    pub note: &'static str,
}

#[derive(Debug, Deserialize)]
struct XrayEntry {
    #[serde(default)]
    flux: Option<f64>,
    #[serde(default)]
    energy: String,
}

#[derive(Debug, Deserialize)]
struct KpEntry {
    #[serde(default)]
    estimated_kp: Option<f64>,
    #[serde(default)]
    kp_index: Option<f64>,
}

/// Latest flux in the 0.1–0.8nm (X-ray class) band. Pure: no networking.
pub fn parse_xray(body: &str) -> Result<Option<f64>, FeedError> {
    let entries: Vec<XrayEntry> = serde_json::from_str(body)
        .map_err(|e| FeedError::new(FEED, format!("xray decode failed: {e}")))?;
    Ok(entries
        .iter()
        .rev()
        .find(|e| e.energy == "0.1-0.8nm")
        .and_then(|e| e.flux))
}

/// Latest estimated Kp (falling back to kp_index). Pure: no networking.
pub fn parse_kp(body: &str) -> Result<Option<f64>, FeedError> {
    let entries: Vec<KpEntry> = serde_json::from_str(body)
        .map_err(|e| FeedError::new(FEED, format!("kp decode failed: {e}")))?;
    Ok(entries
        .iter()
        .rev()
        .find_map(|e| e.estimated_kp.or(e.kp_index)))
}

/// Fetch both SWPC series concurrently; one failing does not fail the feed.
pub async fn fetch(client: &reqwest::Client) -> Result<HelioResponse, FeedError> {
    let (xray, kp) = tokio::join!(
        get_json::<Vec<XrayEntry>>(
            client,
            FEED,
            "https://services.swpc.noaa.gov/json/goes/primary/xrays-7-day.json"
        ),
        get_json::<Vec<KpEntry>>(
            client,
            FEED,
            "https://services.swpc.noaa.gov/json/planetary_k_index_1m.json"
        ),
    );
    let xray_flux_latest = xray.ok().and_then(|entries| {
        entries
            .iter()
            .rev()
            .find(|e| e.energy == "0.1-0.8nm")
            .and_then(|e| e.flux)
    });
    // Kp is the secondary series: degrade gracefully if it fails.
    let kp_latest = kp.ok().and_then(|entries| {
        entries
            .iter()
            .rev()
            .find_map(|e| e.estimated_kp.or(e.kp_index))
    });
    if xray_flux_latest.is_none() && kp_latest.is_none() {
        return Err(FeedError::new(FEED, "all SWPC series failed"));
    }
    Ok(HelioResponse {
        generated_at: now_iso8601(),
        xray_flux_latest,
        kp_latest,
        note: "xray_flux_latest is GOES 0.1-0.8nm flux in W/m^2 (A/B/C/M/X classes at 1e-8/1e-7/1e-6/1e-5/1e-4); kp_latest is the estimated planetary K index on the 0-9 geomagnetic activity scale",
    })
}

/// Short detail string for the health endpoint.
pub async fn probe_detail(client: &reqwest::Client, _config: &Config) -> Result<String, FeedError> {
    let r = fetch(client).await?;
    Ok(format!(
        "xray={:.1e} W/m², kp={:.1}",
        r.xray_flux_latest.unwrap_or(f64::NAN),
        r.kp_latest.unwrap_or(f64::NAN),
    ))
}

// ---------------------------------------------------------------------------
// GET /api/space-weather
// ---------------------------------------------------------------------------

pub async fn handler(State(state): State<AppState>) -> Result<impl IntoResponse, ApiError> {
    let client = state.client.clone();
    let (body, hit) = cached_bytes(&state, "space-weather", TTL, || async move {
        fetch(&client).await
    })
    .await?;
    Ok(json_bytes(body, hit))
}

#[cfg(test)]
mod tests {
    use super::*;

    const XRAY_FIXTURE: &str = include_str!("../../benches/fixtures/swpc_xray.json");
    const KP_FIXTURE: &str = include_str!("../../benches/fixtures/swpc_kp.json");

    #[test]
    fn parses_latest_xray_in_class_band() {
        // Fixture ends with a 0.5-4.0nm record; the parser must skip it and
        // take the last 0.1-0.8nm record.
        let flux = parse_xray(XRAY_FIXTURE).unwrap();
        assert!((flux.unwrap() - 2.845366111614567e-07).abs() < 1e-20);
    }

    #[test]
    fn parses_latest_kp() {
        let kp = parse_kp(KP_FIXTURE).unwrap();
        assert!((kp.unwrap() - 2.33).abs() < 1e-9);
    }

    #[test]
    fn empty_series_yields_none() {
        assert_eq!(parse_xray("[]").unwrap(), None);
        assert_eq!(parse_kp("[]").unwrap(), None);
    }

    #[test]
    fn invalid_json_is_error() {
        assert!(parse_xray("not json").is_err());
        assert!(parse_kp("not json").is_err());
    }
}
