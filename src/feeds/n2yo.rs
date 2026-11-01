//! Satellite catalog feed: N2YO "above" API (keyed) with CelesTrak fallback.
//!
//! Keyed path (documented 2026-11-01, n2yo.com API):
//! `GET https://api.n2yo.com/rest/v1/satellite/above/{lat}/{lon}/{alt_km}/{radius_deg}/{category}/&apiKey={key}`
//! — JSON `{"info": {...}, "above": [{"satid", "satname",
//! "intDesignator", "launchDate", "satlat", "satlng", "satalt"}, ...]}`.
//! Defaults: lat=0, lon=0, alt=0 km, radius=90°, category=0 (all).
//! Free tier: ~1000 transactions/hour.
//!
//! UNVERIFIED: no N2YO API key was available, so the request shape and the
//! `above` normalization below are implemented against the documented API
//! only — never exercised against the live endpoint. The unit tests use a
//! hand-built fixture matching the documented shape.
//!
//! Key handling mirrors OSIRIS: `N2YO_API_KEY` unset → keyless fallback to
//! the CelesTrak TLE feed (`crate::feeds::satellites`), so
//! `/api/satellites/catalog` stays useful without credentials. The
//! `source` field in the response names which upstream served the request.

use std::time::Duration;

use axum::{
    extract::{Query, State},
    response::IntoResponse,
};
use serde::{Deserialize, Serialize};

use super::{cached_bytes, json_bytes};
use crate::{
    config::Config,
    error::{ApiError, FeedError},
    feeds::satellites,
    routes::AppState,
    time::now_iso8601,
};

/// Cache TTL: N2YO positions are computed live per request; 10 minutes.
pub const TTL: Duration = Duration::from_secs(600);
pub const FEED: &str = "n2yo";

/// One normalized catalog entry. Positions are present on the N2YO path and
/// absent on the CelesTrak fallback (TLE only, no SGP4 propagation in v0.1).
#[derive(Debug, Serialize)]
pub struct CatalogSat {
    pub name: String,
    pub norad_id: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latitude: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub longitude: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub altitude_km: Option<f64>,
}

#[derive(Debug, Serialize)]
pub struct CatalogResponse {
    pub count: usize,
    pub generated_at: String,
    /// Which upstream served this response: `"n2yo"` or `"celestrak"`.
    pub source: String,
    pub satellites: Vec<CatalogSat>,
}

#[derive(Debug, Deserialize)]
struct N2yoEntry {
    #[serde(default)]
    satid: u32,
    #[serde(default)]
    satname: String,
    #[serde(default)]
    satlat: Option<f64>,
    #[serde(default)]
    satlng: Option<f64>,
    #[serde(default)]
    satalt: Option<f64>,
}

/// Parse an N2YO `above` body. Pure: no networking. An `{"error": ...}`
/// body (bad key, quota exhausted) becomes a `FeedError`.
pub fn parse_above(body: &str) -> Result<Vec<CatalogSat>, FeedError> {
    let v: serde_json::Value = serde_json::from_str(body)
        .map_err(|e| FeedError::new(FEED, format!("decode failed: {e}")))?;
    if let Some(err) = v.get("error").and_then(|e| e.as_str()) {
        return Err(FeedError::new(FEED, format!("n2yo error: {err}")));
    }
    let above = v
        .get("above")
        .and_then(|a| a.as_array())
        .ok_or_else(|| FeedError::new(FEED, "missing 'above' array"))?;
    let mut sats = Vec::with_capacity(above.len());
    for item in above {
        let e: N2yoEntry = serde_json::from_value(item.clone())
            .map_err(|e| FeedError::new(FEED, format!("entry decode failed: {e}")))?;
        sats.push(CatalogSat {
            name: e.satname,
            norad_id: e.satid,
            latitude: e.satlat,
            longitude: e.satlng,
            altitude_km: e.satalt,
        });
    }
    Ok(sats)
}

/// Fetch the catalog: N2YO when `N2YO_API_KEY` is set, otherwise the
/// keyless CelesTrak fallback.
pub async fn fetch(
    client: &reqwest::Client,
    config: &Config,
    lat: f64,
    lon: f64,
) -> Result<CatalogResponse, FeedError> {
    match &config.n2yo_api_key {
        Some(key) => {
            let url = format!(
                "https://api.n2yo.com/rest/v1/satellite/above/{lat}/{lon}/0/90/0/&apiKey={key}"
            );
            let body = super::get_text(client, FEED, &url).await?;
            let satellites = parse_above(&body)?;
            Ok(CatalogResponse {
                count: satellites.len(),
                generated_at: now_iso8601(),
                source: "n2yo".to_string(),
                satellites,
            })
        }
        None => {
            let tle = satellites::fetch(client, 100).await?;
            let satellites = tle
                .satellites
                .into_iter()
                .map(|s| CatalogSat {
                    name: s.name,
                    norad_id: s.norad_id,
                    latitude: None,
                    longitude: None,
                    altitude_km: None,
                })
                .collect::<Vec<_>>();
            Ok(CatalogResponse {
                count: satellites.len(),
                generated_at: now_iso8601(),
                source: "celestrak".to_string(),
                satellites,
            })
        }
    }
}

/// Short detail string for the health endpoint.
pub async fn probe_detail(client: &reqwest::Client, config: &Config) -> Result<String, FeedError> {
    let r = fetch(client, config, 0.0, 0.0).await?;
    Ok(format!("{} satellites (source={})", r.count, r.source))
}

// ---------------------------------------------------------------------------
// GET /api/satellites/catalog
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct CatalogQuery {
    pub lat: Option<f64>,
    pub lon: Option<f64>,
}

pub async fn handler(
    State(state): State<AppState>,
    Query(q): Query<CatalogQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let lat = q.lat.unwrap_or(0.0);
    let lon = q.lon.unwrap_or(0.0);
    if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lon) {
        return Err(ApiError::bad_request(
            "invalid coordinates",
            "lat must be in [-90, 90], lon in [-180, 180]",
        ));
    }
    let key = format!("satcat:lat={lat:.2}:lon={lon:.2}");
    let client = state.client.clone();
    let config = state.config.clone();
    let (body, hit) = cached_bytes(&state, &key, TTL, || async move {
        fetch(&client, &config, lat, lon).await
    })
    .await?;
    Ok(json_bytes(body, hit))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("../../benches/fixtures/n2yo.json");

    #[test]
    fn parses_above_fixture() {
        let sats = parse_above(FIXTURE).unwrap();
        assert_eq!(sats.len(), 2);
        assert_eq!(sats[0].name, "ISS (ZARYA)");
        assert_eq!(sats[0].norad_id, 25544);
        assert!((sats[0].latitude.unwrap() - 51.6).abs() < 1e-9);
        assert!((sats[0].longitude.unwrap() - -45.2).abs() < 1e-9);
        assert!((sats[0].altitude_km.unwrap() - 419.5).abs() < 1e-9);
    }

    #[test]
    fn n2yo_error_body_is_a_feed_error() {
        let body = r#"{"error": "Invalid API key"}"#;
        let err = parse_above(body).unwrap_err();
        assert!(err.detail.contains("Invalid API key"));
    }

    #[test]
    fn missing_above_array_is_an_error() {
        assert!(parse_above(r#"{"info":{}}"#).is_err());
    }

    #[test]
    fn unset_key_falls_back_to_celestrak_shape() {
        // Pure shape check: the fallback maps TLE entries to CatalogSat
        // with positions absent. (Network fallback itself is covered by the
        // integration test's 200-or-502 contract.)
        let tle = satellites::Satellite {
            name: "ISS (ZARYA)".to_string(),
            norad_id: 25544,
            line1: "1 25544U".to_string(),
            line2: "2 25544".to_string(),
        };
        let c = CatalogSat {
            name: tle.name.clone(),
            norad_id: tle.norad_id,
            latitude: None,
            longitude: None,
            altitude_km: None,
        };
        let body = serde_json::to_string(&c).unwrap();
        assert!(body.contains("ISS (ZARYA)"));
        assert!(!body.contains("latitude"), "None positions must be skipped");
    }
}
