//! Wildfire feed: NASA FIRMS active-fire detections (keyed).
//!
//! Upstream (documented 2026-11-01, FIRMS API docs):
//! `GET https://firms.modaps.eosdis.nasa.gov/api/area/csv/{MAP_KEY}/VIIRS_SNPP_NRT/world/1`
//! — CSV, one row per hotspot, 24h window. Columns include `latitude`,
//! `longitude`, `bright_ti4` (I-4 brightness temperature, Kelvin),
//! `acq_date`, `acq_time`, `satellite`, `confidence`, `frp`, `daynight`.
//!
//! UNVERIFIED: no FIRMS MAP_KEY was available in this environment, so the
//! request shape, column mapping, and row parsing below are implemented
//! against the documented API only — never exercised against the live
//! endpoint. The unit tests use a hand-built fixture matching the
//! documented column layout.
//!
//! Key handling mirrors OSIRIS: `FIRMS_API_KEY` unset → 503
//! `{"error":"fires feed disabled","detail":"set FIRMS_API_KEY ..."}`.
//!
//! IMPORTANT (NASA Earthdata notice, 2026-10-27): the Suomi NPP VIIRS
//! active-fire data stream is retired after 2026-11-02 13:00 UTC —
//! `VIIRS_SNPP_NRT` stops updating. NASA's recommended drop-in
//! replacements are `VIIRS_NOAA20_NRT` / `VIIRS_NOAA21_NRT`. The URL below
//! keeps the contracted `VIIRS_SNPP_NRT` source; if it starts returning
//! errors, switch `SOURCE` to `VIIRS_NOAA20_NRT`.

use std::time::Duration;

use axum::{extract::State, response::IntoResponse};
use serde::Serialize;

use super::{cached_bytes, get_text, json_bytes};
use crate::{
    config::Config,
    error::{ApiError, FeedError},
    routes::AppState,
    time::now_iso8601,
};

/// Cache TTL: FIRMS NRT updates every few hours; 10 minutes is plenty.
pub const TTL: Duration = Duration::from_secs(600);
pub const FEED: &str = "firms";

/// Upper bound on served detections. A `world/1` VIIRS query can return
/// 30,000–100,000+ rows/day (per FIRMS docs); serving the raw list would
/// produce multi-megabyte responses, so the list is truncated and `count`
/// reflects the truncated length.
const MAX_FIRES: usize = 10_000;

/// One normalized active-fire detection.
#[derive(Debug, Serialize)]
pub struct Fire {
    pub lat: f64,
    pub lon: f64,
    /// I-4 channel brightness temperature (Kelvin).
    pub brightness: f64,
    pub acq_date: String,
    pub satellite: String,
}

#[derive(Debug, Serialize)]
pub struct FiresResponse {
    pub count: usize,
    pub generated_at: String,
    pub fires: Vec<Fire>,
}

/// Parse a FIRMS area-CSV body into detections. Pure: no networking.
/// Column lookup is by header name (order-independent); malformed rows are
/// skipped rather than failing the batch.
pub fn parse_csv(body: &str) -> Result<Vec<Fire>, FeedError> {
    let mut lines = body.lines();
    let header = lines
        .next()
        .ok_or_else(|| FeedError::new(FEED, "empty CSV body"))?;
    let cols: Vec<&str> = header.split(',').collect();
    let idx = |name: &str| {
        cols.iter()
            .position(|c| c.trim().eq_ignore_ascii_case(name))
            .ok_or_else(|| FeedError::new(FEED, format!("CSV missing column '{name}'")))
    };
    let (lat_i, lon_i, br_i, date_i, sat_i) = (
        idx("latitude")?,
        idx("longitude")?,
        idx("bright_ti4")?,
        idx("acq_date")?,
        idx("satellite")?,
    );
    let mut fires = Vec::new();
    for line in lines {
        if line.trim().is_empty() {
            continue;
        }
        let f: Vec<&str> = line.split(',').collect();
        let get = |i: usize| f.get(i).map(|s| s.trim()).unwrap_or("");
        let (Ok(lat), Ok(lon)) = (get(lat_i).parse::<f64>(), get(lon_i).parse::<f64>()) else {
            continue;
        };
        fires.push(Fire {
            lat,
            lon,
            brightness: get(br_i).parse::<f64>().unwrap_or(0.0),
            acq_date: get(date_i).to_string(),
            satellite: get(sat_i).to_string(),
        });
        if fires.len() >= MAX_FIRES {
            break;
        }
    }
    Ok(fires)
}

/// Fetch the 24h world VIIRS active-fire CSV and normalize it.
pub async fn fetch(client: &reqwest::Client, api_key: &str) -> Result<FiresResponse, FeedError> {
    // NOTE: VIIRS_SNPP_NRT retired 2026-11-02 — see module docs.
    let url = format!(
        "https://firms.modaps.eosdis.nasa.gov/api/area/csv/{api_key}/VIIRS_SNPP_NRT/world/1"
    );
    let body = get_text(client, FEED, &url).await?;
    let fires = parse_csv(&body)?;
    Ok(FiresResponse {
        count: fires.len(),
        generated_at: now_iso8601(),
        fires,
    })
}

/// Short detail string for the health endpoint.
pub async fn probe_detail(client: &reqwest::Client, config: &Config) -> Result<String, FeedError> {
    match &config.firms_api_key {
        None => Err(FeedError::new(FEED, "FIRMS_API_KEY unset — feed disabled")),
        Some(key) => {
            let r = fetch(client, key).await?;
            Ok(format!("{} fires (VIIRS 24h)", r.count))
        }
    }
}

// ---------------------------------------------------------------------------
// GET /api/fires
// ---------------------------------------------------------------------------

pub async fn handler(State(state): State<AppState>) -> Result<impl IntoResponse, ApiError> {
    let api_key = match &state.config.firms_api_key {
        Some(k) => k.clone(),
        None => {
            return Err(ApiError::service_unavailable(
                "fires feed disabled",
                "set FIRMS_API_KEY to a free NASA FIRMS MAP_KEY — request one at https://firms.modaps.eosdis.nasa.gov/api/area/ — to enable /api/fires",
            ));
        }
    };
    let client = state.client.clone();
    let (body, hit) = cached_bytes(&state, "fires", TTL, || async move {
        fetch(&client, &api_key).await
    })
    .await?;
    Ok(json_bytes(body, hit))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("../../benches/fixtures/firms.csv");

    #[test]
    fn parses_fixture_rows() {
        let fires = parse_csv(FIXTURE).unwrap();
        assert_eq!(fires.len(), 3);
        let f = &fires[0];
        assert!((f.lat - 34.123).abs() < 1e-9);
        assert!((f.lon - -118.456).abs() < 1e-9);
        assert!((f.brightness - 312.5).abs() < 1e-9);
        assert_eq!(f.acq_date, "2026-11-01");
        assert_eq!(f.satellite, "N");
    }

    #[test]
    fn skips_malformed_rows() {
        let body = "latitude,longitude,bright_ti4,acq_date,satellite\nbad,row\n34.0,-118.0,300.0,2026-11-01,N\n";
        let fires = parse_csv(body).unwrap();
        assert_eq!(fires.len(), 1);
    }

    #[test]
    fn missing_column_is_an_error() {
        let body = "latitude,longitude\n34.0,-118.0\n";
        assert!(parse_csv(body).is_err());
    }

    #[test]
    fn empty_body_is_an_error() {
        assert!(parse_csv("").is_err());
    }
}
