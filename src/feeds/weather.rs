//! Weather feed: Open-Meteo forecast API (keyless, no registration).
//!
//! Upstream: `https://api.open-meteo.com/v1/forecast?latitude=..&longitude=..&current=temperature_2m,wind_speed_10m`
//! Shape (verified 2026-11-01): `{latitude, longitude, elevation, current:
//! {time: "2026-11-01T16:00", temperature_2m, wind_speed_10m, ...}}` with
//! `timezone: "GMT"`, so `current.time` is UTC ("YYYY-MM-DDTHH:MM").

use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::get_json;
use crate::error::FeedError;
use crate::time::now_iso8601;

/// Cache TTL: 60s for current conditions.
pub const TTL: Duration = Duration::from_secs(60);
pub const FEED: &str = "open-meteo";

#[derive(Debug, Deserialize)]
struct Current {
    #[serde(default)]
    time: String,
    #[serde(default)]
    temperature_2m: Option<f64>,
    #[serde(default)]
    wind_speed_10m: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct Forecast {
    #[serde(default)]
    latitude: f64,
    #[serde(default)]
    longitude: f64,
    #[serde(default)]
    elevation: Option<f64>,
    #[serde(default)]
    current: Option<Current>,
}

/// Normalized weather record served by `/api/weather`.
#[derive(Debug, Serialize)]
pub struct WeatherResponse {
    pub latitude: f64,
    pub longitude: f64,
    pub elevation_m: Option<f64>,
    /// Observation time, ISO 8601 UTC.
    pub time: Option<String>,
    pub temperature_c: Option<f64>,
    pub wind_speed_kmh: Option<f64>,
    pub generated_at: String,
}

/// `current.time` arrives as `"2026-11-01T16:00"` in UTC; normalize to full
/// ISO 8601 with seconds and the Z suffix.
fn normalize_time(t: &str) -> String {
    if t.len() == 16 {
        format!("{t}:00Z")
    } else {
        t.to_string()
    }
}

pub async fn fetch(
    client: &reqwest::Client,
    lat: f64,
    lon: f64,
) -> Result<WeatherResponse, FeedError> {
    let url = format!(
        "https://api.open-meteo.com/v1/forecast?latitude={lat}&longitude={lon}&current=temperature_2m,wind_speed_10m"
    );
    let f: Forecast = get_json(client, FEED, &url).await?;
    let current = f.current.unwrap_or(Current {
        time: String::new(),
        temperature_2m: None,
        wind_speed_10m: None,
    });
    Ok(WeatherResponse {
        latitude: f.latitude,
        longitude: f.longitude,
        elevation_m: f.elevation,
        time: if current.time.is_empty() {
            None
        } else {
            Some(normalize_time(&current.time))
        },
        temperature_c: current.temperature_2m,
        wind_speed_kmh: current.wind_speed_10m,
        generated_at: now_iso8601(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn time_gets_seconds_and_zulu() {
        assert_eq!(normalize_time("2026-11-01T16:00"), "2026-11-01T16:00:00Z");
    }

    #[test]
    fn already_full_time_passes_through() {
        assert_eq!(
            normalize_time("2026-11-01T16:00:00Z"),
            "2026-11-01T16:00:00Z"
        );
    }
}
