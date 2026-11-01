//! Earthquake feed: USGS GeoJSON summary feed (keyless).
//!
//! Upstream: `https://earthquake.usgs.gov/earthquakes/feed/v1.0/summary/4.5_day.geojson`
//! Shape (verified 2026-11-01): GeoJSON `FeatureCollection`; each feature has
//! `properties` (`mag`, `place`, `time`/`updated` in epoch millis, `url`,
//! `tsunami`, `status`, `magType`) and `geometry.coordinates` as
//! `[longitude, latitude, depth_km]`.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::get_json;
use crate::error::FeedError;
use crate::time::{epoch_millis_to_iso8601, now_iso8601};

/// Cache TTL: 60s, mirroring OSIRIS's fast-feed convention.
pub const TTL: Duration = Duration::from_secs(60);
pub const FEED: &str = "usgs";

#[derive(Debug, Deserialize)]
struct Properties {
    #[serde(default)]
    mag: Option<f64>,
    #[serde(default)]
    place: Option<String>,
    #[serde(default)]
    time: i64,
    #[serde(default)]
    updated: i64,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    tsunami: Option<i32>,
    #[serde(default)]
    status: Option<String>,
    #[serde(rename = "magType", default)]
    mag_type: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Geometry {
    #[serde(default)]
    coordinates: Vec<f64>,
}

#[derive(Debug, Deserialize)]
struct Feature {
    id: String,
    properties: Properties,
    geometry: Option<Geometry>,
}

#[derive(Debug, Deserialize)]
struct Feed {
    #[serde(default)]
    features: Vec<Feature>,
}

/// Normalized earthquake record served by `/api/quakes`.
#[derive(Debug, Serialize)]
pub struct Quake {
    pub id: String,
    pub magnitude: Option<f64>,
    pub magnitude_type: Option<String>,
    pub place: Option<String>,
    pub time: String,
    pub updated: String,
    pub url: Option<String>,
    pub latitude: Option<f64>,
    pub longitude: Option<f64>,
    pub depth_km: Option<f64>,
    pub tsunami: bool,
    pub status: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct QuakesResponse {
    pub count: usize,
    pub generated_at: String,
    pub quakes: Vec<Quake>,
}

fn normalize(f: Feature) -> Quake {
    let (longitude, latitude, depth_km) = match f.geometry {
        Some(g) => (
            g.coordinates.first().copied(),
            g.coordinates.get(1).copied(),
            g.coordinates.get(2).copied(),
        ),
        None => (None, None, None),
    };
    Quake {
        id: f.id,
        magnitude: f.properties.mag,
        magnitude_type: f.properties.mag_type,
        place: f.properties.place,
        time: epoch_millis_to_iso8601(f.properties.time),
        updated: epoch_millis_to_iso8601(f.properties.updated),
        url: f.properties.url,
        latitude,
        longitude,
        depth_km,
        tsunami: f.properties.tsunami.unwrap_or(0) != 0,
        status: f.properties.status,
    }
}

/// Parse + normalize a raw USGS GeoJSON body. Pure: no networking, the
/// criterion hot path.
pub fn parse_feed(body: &str) -> Result<QuakesResponse, FeedError> {
    let feed: Feed = serde_json::from_str(body)
        .map_err(|e| FeedError::new(FEED, format!("decode failed: {e}")))?;
    let quakes: Vec<Quake> = feed.features.into_iter().map(normalize).collect();
    Ok(QuakesResponse {
        count: quakes.len(),
        generated_at: now_iso8601(),
        quakes,
    })
}

pub async fn fetch(client: &reqwest::Client) -> Result<QuakesResponse, FeedError> {
    let feed: Feed = get_json(
        client,
        FEED,
        "https://earthquake.usgs.gov/earthquakes/feed/v1.0/summary/4.5_day.geojson",
    )
    .await?;
    // Reuse the same normalization path as parse_feed.
    let quakes: Vec<Quake> = feed.features.into_iter().map(normalize).collect();
    Ok(QuakesResponse {
        count: quakes.len(),
        generated_at: now_iso8601(),
        quakes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("../../benches/fixtures/usgs.json");

    #[test]
    fn parses_fixture() {
        let r = parse_feed(FIXTURE).unwrap();
        assert_eq!(r.count, 15);
        let q = &r.quakes[0];
        assert_eq!(q.id, "us7000tupq");
        assert_eq!(q.magnitude, Some(4.8));
        assert_eq!(q.magnitude_type.as_deref(), Some("mb"));
        assert!((q.latitude.unwrap() - -13.8667).abs() < 1e-9);
        assert!((q.longitude.unwrap() - 165.6199).abs() < 1e-9);
        assert_eq!(q.depth_km, Some(10.0));
        assert_eq!(q.time, "2026-11-01T12:08:30.561Z");
        assert!(!q.tsunami);
    }

    #[test]
    fn missing_geometry_yields_nones() {
        let body =
            r#"{"features":[{"id":"x","properties":{"time":0,"updated":0},"geometry":null}]}"#;
        let r = parse_feed(body).unwrap();
        assert_eq!(r.quakes[0].latitude, None);
        assert_eq!(r.quakes[0].time, "1970-01-01T00:00:00.000Z");
    }
}
