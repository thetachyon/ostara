//! Aviation feed: adsb.lol v2 API (keyless).
//!
//! Upstream: `https://api.adsb.lol/v2/mil` (military) or `/v2/all`.
//! Response shape (verified 2026-11-01): `{ac: [...], msg, now, total, ...}`.
//! Each aircraft carries `hex`, `flight` (often trailing-padded),
//! `r` (registration), `t` (type), `lat`/`lon`, `alt_baro` which is either a
//! number (feet) or the string `"ground"`, `gs` (ground speed, knots), etc.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::get_text;
use crate::error::FeedError;
use crate::time::now_iso8601;

/// Cache TTL: fast-moving feed, mirrors OSIRIS's 45-60s convention.
pub const TTL: Duration = Duration::from_secs(45);
pub const FEED: &str = "adsb.lol";

/// `alt_baro` is polymorphic: a number (feet) or the string "ground".
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum AltBaro {
    Feet(f64),
    /// The payload exists only so serde's untagged deserialization matches
    /// the JSON string "ground"; the value itself carries no information and
    /// is intentionally never read.
    #[allow(dead_code)]
    Ground(String),
}

#[derive(Debug, Deserialize)]
struct AdsbAircraft {
    hex: String,
    #[serde(default)]
    flight: Option<String>,
    #[serde(default)]
    r: Option<String>,
    #[serde(default)]
    t: Option<String>,
    #[serde(default)]
    lat: Option<f64>,
    #[serde(default)]
    lon: Option<f64>,
    #[serde(default)]
    alt_baro: Option<AltBaro>,
    /// Ground speed, knots.
    #[serde(default)]
    gs: Option<f64>,
    #[serde(default)]
    track: Option<f64>,
    #[serde(default)]
    emergency: Option<String>,
    /// Seconds since last position update.
    #[serde(default)]
    seen_pos: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct AdsbResponse {
    #[serde(default)]
    ac: Vec<AdsbAircraft>,
    #[serde(default)]
    total: u64,
}

/// Normalized aircraft record served by `/api/flights`.
#[derive(Debug, Serialize)]
pub struct Flight {
    pub hex: String,
    pub callsign: Option<String>,
    pub registration: Option<String>,
    pub aircraft_type: Option<String>,
    pub latitude: Option<f64>,
    pub longitude: Option<f64>,
    /// Altitude in meters (converted from feet); `None` when on ground.
    pub altitude_m: Option<f64>,
    pub on_ground: bool,
    /// Ground speed in knots, as reported upstream.
    pub speed_knots: Option<f64>,
    pub track_deg: Option<f64>,
    pub emergency: Option<String>,
    pub seen_pos_s: Option<f64>,
}

#[derive(Debug, Serialize)]
pub struct FlightsResponse {
    pub scope: String,
    pub count: usize,
    pub total_upstream: u64,
    pub generated_at: String,
    pub flights: Vec<Flight>,
}

fn normalize(ac: AdsbAircraft) -> Flight {
    let (altitude_m, on_ground) = match ac.alt_baro {
        Some(AltBaro::Feet(ft)) => (Some(ft * 0.3048), false),
        Some(AltBaro::Ground(_)) => (None, true),
        None => (None, false),
    };
    Flight {
        hex: ac.hex,
        callsign: ac
            .flight
            .map(|f| f.trim().to_string())
            .filter(|f| !f.is_empty()),
        registration: ac.r,
        aircraft_type: ac.t,
        latitude: ac.lat,
        longitude: ac.lon,
        altitude_m,
        on_ground,
        speed_knots: ac.gs,
        track_deg: ac.track,
        emergency: ac.emergency,
        seen_pos_s: ac.seen_pos,
    }
}

/// Parse + normalize a raw adsb.lol JSON body. Pure: no networking, the
/// criterion hot path.
pub fn parse_response(body: &str, scope: &str, limit: usize) -> Result<FlightsResponse, FeedError> {
    let resp: AdsbResponse = serde_json::from_str(body)
        .map_err(|e| FeedError::new(FEED, format!("decode failed: {e}")))?;
    let flights: Vec<Flight> = resp.ac.into_iter().take(limit).map(normalize).collect();
    Ok(FlightsResponse {
        scope: scope.to_string(),
        count: flights.len(),
        total_upstream: resp.total,
        generated_at: now_iso8601(),
        flights,
    })
}

/// Fetch, parse, and normalize. `scope` is `"mil"` or `"all"`.
pub async fn fetch(
    client: &reqwest::Client,
    scope: &str,
    limit: usize,
) -> Result<FlightsResponse, FeedError> {
    let url = format!("https://api.adsb.lol/v2/{scope}");
    let body: String = get_text(client, FEED, &url).await?;
    parse_response(&body, scope, limit)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("../../benches/fixtures/adsb_mil.json");

    #[test]
    fn parses_fixture_count() {
        let r = parse_response(FIXTURE, "mil", 1000).unwrap();
        assert_eq!(r.scope, "mil");
        assert!(r.count > 0);
        assert_eq!(r.count, r.total_upstream as usize);
    }

    #[test]
    fn limit_is_respected() {
        let r = parse_response(FIXTURE, "mil", 5).unwrap();
        assert_eq!(r.count, 5);
    }

    #[test]
    fn ground_and_altitude_normalization() {
        // First fixture aircraft: alt_baro "ground", flight "TREK136 ".
        let r = parse_response(FIXTURE, "mil", 1).unwrap();
        let f = &r.flights[0];
        assert!(f.on_ground);
        assert_eq!(f.altitude_m, None);
        assert_eq!(f.callsign.as_deref(), Some("TREK136"));
        assert_eq!(f.hex, "ae1450");
    }

    #[test]
    fn numeric_altitude_converted_to_meters() {
        let body = r#"{"ac":[{"hex":"abc123","alt_baro":35000}],"total":1}"#;
        let r = parse_response(body, "mil", 10).unwrap();
        let f = &r.flights[0];
        assert!(!f.on_ground);
        assert!((f.altitude_m.unwrap() - 35000.0 * 0.3048).abs() < 1e-9);
    }
}
