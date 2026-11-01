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
        // adsb.lol reports the literal string "none" for non-emergency
        // aircraft; normalize it to None so `emergency.is_some()` means a
        // real 7500/7600/7700 squawk.
        emergency: ac.emergency.filter(|e| e != "none"),
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

// ---------------------------------------------------------------------------
// OpenSky Network (keyed, preferred when credentials are configured).
//
// UNVERIFIED (2026-11-01): no OpenSky API client credentials were
// available, so the OAuth2 token exchange, the states/all request shape,
// and the state-vector normalization below are implemented against the
// documented API only — never exercised against the live endpoints.
//
// Auth: OAuth2 client-credentials grant against
// `https://auth.opensky-network.org/auth/realms/opensky-network/protocol/openid-connect/token`
// (form fields: grant_type=client_credentials, client_id, client_secret;
// response carries `access_token`, valid ~30 minutes). Data:
// `GET https://opensky-network.org/api/states/all` with
// `Authorization: Bearer <token>`; the body is `{"time": <unix>,
// "states": [[icao24, callsign, origin_country, time_position,
// last_contact, longitude, latitude, baro_altitude, on_ground, velocity,
// true_track, vertical_rate, sensors, geo_altitude, squawk, spi,
// position_source], ...]}` — 18-element arrays, any element nullable.
//
// NOTE: a fresh token is fetched on every cache miss. Tokens live ~30
// minutes, so caching the token and refreshing on expiry is the obvious
// future optimization; not done here to keep the keyed path simple.
// ---------------------------------------------------------------------------

const OPENSKY_FEED: &str = "opensky";
const OPENSKY_TOKEN_URL: &str =
    "https://auth.opensky-network.org/auth/realms/opensky-network/protocol/openid-connect/token";
const OPENSKY_STATES_URL: &str = "https://opensky-network.org/api/states/all";

/// Meters per second → knots.
const MS_TO_KNOTS: f64 = 1.943_84;

fn json_str(v: Option<&serde_json::Value>) -> Option<String> {
    v?.as_str().map(|s| {
        let t = s.trim();
        if t.is_empty() {
            None
        } else {
            Some(t.to_string())
        }
    })?
}

fn json_f64(v: Option<&serde_json::Value>) -> Option<f64> {
    v?.as_f64()
}

fn json_bool(v: Option<&serde_json::Value>) -> Option<bool> {
    v?.as_bool()
}

/// Normalize one OpenSky state vector (18-element array) into the shared
/// [`Flight`] struct. Returns `None` when the vector is unusable (missing
/// icao24).
fn normalize_state(sv: &[serde_json::Value], now: i64) -> Option<Flight> {
    let at = |i: usize| sv.get(i);
    let icao24 = json_str(at(0))?;
    let time_position = at(3).and_then(|v| v.as_i64());
    let squawk = json_str(at(14));
    let emergency = match squawk.as_deref() {
        Some("7500") | Some("7600") | Some("7700") => squawk,
        _ => None,
    };
    Some(Flight {
        hex: icao24,
        callsign: json_str(at(1)),
        // states/all carries no registration or type data.
        registration: None,
        aircraft_type: None,
        latitude: json_f64(at(6)),
        longitude: json_f64(at(5)),
        // baro_altitude is already meters; None reads as "no altitude".
        altitude_m: json_f64(at(7)),
        on_ground: json_bool(at(8)).unwrap_or(false),
        // velocity is m/s upstream; the struct carries knots.
        speed_knots: json_f64(at(9)).map(|v| v * MS_TO_KNOTS),
        track_deg: json_f64(at(10)),
        emergency,
        seen_pos_s: time_position.map(|t| (now - t).max(0) as f64),
    })
}

/// Parse + normalize an OpenSky states/all body. Pure: no networking.
pub fn parse_opensky_states(body: &str, limit: usize) -> Result<Vec<Flight>, FeedError> {
    let v: serde_json::Value = serde_json::from_str(body)
        .map_err(|e| FeedError::new(OPENSKY_FEED, format!("decode failed: {e}")))?;
    let now = v.get("time").and_then(|t| t.as_i64()).unwrap_or(0);
    let states = v
        .get("states")
        .and_then(|s| s.as_array())
        .ok_or_else(|| FeedError::new(OPENSKY_FEED, "missing 'states' array"))?;
    let flights: Vec<Flight> = states
        .iter()
        .filter_map(|sv| sv.as_array())
        .take(limit)
        .filter_map(|sv| normalize_state(sv, now))
        .collect();
    Ok(flights)
}

#[derive(Debug, Deserialize)]
struct OpenSkyToken {
    access_token: String,
}

async fn opensky_token(
    client: &reqwest::Client,
    client_id: &str,
    client_secret: &str,
) -> Result<String, FeedError> {
    let resp = client
        .post(OPENSKY_TOKEN_URL)
        .form(&[
            ("grant_type", "client_credentials"),
            ("client_id", client_id),
            ("client_secret", client_secret),
        ])
        .send()
        .await
        .map_err(|e| FeedError::new(OPENSKY_FEED, format!("token request failed: {e}")))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(FeedError::new(
            OPENSKY_FEED,
            format!("token upstream status {status}"),
        ));
    }
    let token: OpenSkyToken = resp
        .json()
        .await
        .map_err(|e| FeedError::new(OPENSKY_FEED, format!("token decode failed: {e}")))?;
    Ok(token.access_token)
}

/// Fetch via OpenSky (token + states/all) and normalize into the existing
/// [`Flight`] struct. The adsb.lol `scope` concept does not exist here —
/// states/all is global — so the response scope is `"opensky"`.
pub async fn fetch_opensky(
    client: &reqwest::Client,
    client_id: &str,
    client_secret: &str,
    limit: usize,
) -> Result<FlightsResponse, FeedError> {
    let token = opensky_token(client, client_id, client_secret).await?;
    let body: String = {
        let resp = client
            .get(OPENSKY_STATES_URL)
            .bearer_auth(&token)
            .send()
            .await
            .map_err(|e| FeedError::new(OPENSKY_FEED, format!("request failed: {e}")))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(FeedError::new(
                OPENSKY_FEED,
                format!("upstream status {status}"),
            ));
        }
        resp.text()
            .await
            .map_err(|e| FeedError::new(OPENSKY_FEED, format!("read body failed: {e}")))?
    };
    let all: Vec<Flight> = parse_opensky_states(&body, usize::MAX)?;
    let total_upstream = all.len() as u64;
    let flights: Vec<Flight> = all.into_iter().take(limit).collect();
    Ok(FlightsResponse {
        scope: "opensky".to_string(),
        count: flights.len(),
        total_upstream,
        generated_at: now_iso8601(),
        flights,
    })
}

/// Config-aware entry point used by the `/api/flights` handler: prefers
/// OpenSky when `OPENSKY_CLIENT_ID` + `OPENSKY_CLIENT_SECRET` are both set,
/// otherwise the existing keyless adsb.lol path (unchanged).
pub async fn fetch_with_config(
    client: &reqwest::Client,
    config: &crate::config::Config,
    scope: &str,
    limit: usize,
) -> Result<FlightsResponse, FeedError> {
    match (&config.opensky_client_id, &config.opensky_client_secret) {
        (Some(id), Some(secret)) => fetch_opensky(client, id, secret, limit).await,
        _ => fetch(client, scope, limit).await,
    }
}

/// Short detail string for the health endpoint.
pub async fn probe_detail(
    client: &reqwest::Client,
    config: &crate::config::Config,
) -> Result<String, FeedError> {
    let r = fetch_with_config(client, config, "mil", 1).await?;
    Ok(format!("{} aircraft (scope={})", r.count, r.scope))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("../../benches/fixtures/adsb_mil.json");
    const OPENSKY_FIXTURE: &str = include_str!("../../benches/fixtures/opensky.json");

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

    #[test]
    fn adsb_none_emergency_normalizes_to_none() {
        // Fixture aircraft report the literal string "none" (plus two real
        // emergencies: "general" and "minfuel"). "none" must not surface as
        // a real emergency squawk; real values must survive.
        let r = parse_response(FIXTURE, "mil", 1000).unwrap();
        assert!(
            r.flights
                .iter()
                .all(|f| f.emergency.as_deref() != Some("none"))
        );
        let codes: Vec<_> = r
            .flights
            .iter()
            .filter_map(|f| f.emergency.as_deref())
            .collect();
        assert!(codes.contains(&"general"));
        assert!(codes.contains(&"minfuel"));

        let body = r#"{"ac":[{"hex":"abc123","emergency":"7700"}],"total":1}"#;
        let r = parse_response(body, "mil", 10).unwrap();
        assert_eq!(r.flights[0].emergency.as_deref(), Some("7700"));
    }

    #[test]
    fn opensky_states_normalize() {
        let flights = parse_opensky_states(OPENSKY_FIXTURE, 100).unwrap();
        // Third vector has null icao24 and is dropped.
        assert_eq!(flights.len(), 2);

        let f = &flights[0];
        assert_eq!(f.hex, "4b19d6");
        assert_eq!(f.callsign.as_deref(), Some("SWR123"));
        assert!((f.latitude.unwrap() - 47.46).abs() < 1e-9);
        assert!((f.longitude.unwrap() - 8.55).abs() < 1e-9);
        // baro_altitude is already meters: no conversion.
        assert!((f.altitude_m.unwrap() - 10668.0).abs() < 1e-9);
        assert!(!f.on_ground);
        // velocity m/s -> knots.
        assert!((f.speed_knots.unwrap() - 250.5 * 1.943_84).abs() < 1e-6);
        assert!((f.track_deg.unwrap() - 90.0).abs() < 1e-9);
        assert_eq!(f.emergency, None);
        // seen_pos_s = time - time_position = 1762012345 - 1762012340.
        assert_eq!(f.seen_pos_s, Some(5.0));
        assert_eq!(f.registration, None);
        assert_eq!(f.aircraft_type, None);
    }

    #[test]
    fn opensky_emergency_squawk_surfaced() {
        let flights = parse_opensky_states(OPENSKY_FIXTURE, 100).unwrap();
        let f = &flights[1];
        assert!(f.on_ground);
        assert_eq!(f.altitude_m, None);
        assert_eq!(f.emergency.as_deref(), Some("7700"));
        assert_eq!(f.seen_pos_s, None);
    }

    #[test]
    fn opensky_limit_respected() {
        let flights = parse_opensky_states(OPENSKY_FIXTURE, 1).unwrap();
        assert_eq!(flights.len(), 1);
    }

    #[test]
    fn opensky_missing_states_is_error() {
        assert!(parse_opensky_states(r#"{"time":1}"#, 10).is_err());
    }
}
