//! Country inspect: click-to-identify for the map UI.
//!
//! `GET /api/country/inspect?lat=&lon=` resolves a map click to a country
//! (see [`crate::geo`]) and returns that country's OSINT resource pack plus
//! the country-tagged items from the other feeds, in one response.
//!
//! Feed sections reuse the exact TTL cache paths of the normal endpoints
//! (`cached_bytes` with the same keys/TTLs) — a cache miss fetches upstream
//! exactly as the normal endpoint would.
//!
//! Country-taggable feeds and their mechanisms:
//! - `news` — GDELT `sourcecountry`, via a `sourcecountry:` query;
//! - `conflicts` — GDACS `gdacs:country` free-text name;
//! - `nuclear` — static registry `country` name;
//! - `flights` — aircraft lat/lon tested against the country's polygon
//!   bboxes ([`crate::geo::point_in_country_bboxes`]);
//! - `quakes` — epicenter lat/lon, same bbox test;
//! - `cameras` — registry lat/lon, same bbox test.
//!
//! Deliberately excluded:
//! - `markets` — CoinGecko crypto top-coins. Crypto assets are inherently
//!   global; no country attribution exists in the data, and any
//!   instrument→country mapping would be guesswork. Left out, not faked.
//! - `cyber` / `threatfox` — IOCs are IPs/domains/URLs/hashes. Country
//!   resolution would need a GeoIP database, which we do not ship; a small
//!   embedded prefix table would be far too coarse and actively dangerous
//!   for intel use, and reverse-DNS is not geolocation. Left out, not faked.
//! - `fires` (lat/lon only, no country semantics needed), `vessels`
//!   (disabled), `telegram`/`helio`/`satellites`/`n2yo`/`weather` (no
//!   country field).

use axum::{
    Json, Router,
    extract::{Query, State},
    routing::get,
};
use serde::{Deserialize, Serialize};

use crate::{
    country_osint,
    error::ApiError,
    feeds,
    geo::{ResolveMethod, point_in_country_bboxes, resolve_country},
    routes::{AppState, cached_bytes},
};

/// Canonicalize a country name for cross-dataset matching. The feed datasets
/// use common names while the bbox table uses Natural Earth names; only the
/// verified divergences are aliased (checked 2026-11-01 against the nuclear
/// registry and GDACS fixture names).
fn canonical_country_name(s: &str) -> String {
    match s.trim().to_lowercase().as_str() {
        "united states of america" => "united states".to_string(),
        "czechia" => "czech republic".to_string(),
        "united arab emirates" | "uae" => "uae".to_string(),
        other => other.to_string(),
    }
}

fn names_match(a: &str, b: &str) -> bool {
    canonical_country_name(a) == canonical_country_name(b)
}

// ---------------------------------------------------------------------------
// Response shapes
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct CountryRef {
    pub name: String,
    pub iso2: String,
    pub iso3: String,
}

#[derive(Debug, Serialize)]
pub struct FeedSection<T: Serialize> {
    pub ok: bool,
    pub count: usize,
    pub items: Vec<T>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl<T: Serialize> FeedSection<T> {
    fn ok(items: Vec<T>) -> Self {
        Self {
            count: items.len(),
            items,
            ok: true,
            error: None,
        }
    }

    fn err(detail: String) -> Self {
        Self {
            ok: false,
            count: 0,
            items: Vec::new(),
            error: Some(detail),
        }
    }

    fn empty() -> Self {
        Self::ok(Vec::new())
    }
}

#[derive(Debug, Serialize)]
pub struct NewsItem {
    pub title: String,
    pub url: String,
    pub domain: String,
    pub seen_at: Option<String>,
    pub source_country: String,
}

#[derive(Debug, Serialize)]
pub struct ConflictItem {
    pub title: String,
    pub date: String,
    pub url: String,
}

#[derive(Debug, Serialize)]
pub struct NuclearItem {
    pub name: String,
    pub lat: f64,
    pub lon: f64,
    pub status: String,
}

#[derive(Debug, Serialize)]
pub struct FlightItem {
    pub hex: String,
    pub callsign: Option<String>,
    pub registration: Option<String>,
    pub aircraft_type: Option<String>,
    pub latitude: f64,
    pub longitude: f64,
    pub altitude_m: Option<f64>,
    pub speed_knots: Option<f64>,
}

#[derive(Debug, Serialize)]
pub struct QuakeItem {
    pub id: String,
    pub magnitude: Option<f64>,
    pub place: Option<String>,
    pub time: String,
    pub latitude: f64,
    pub longitude: f64,
    pub depth_km: Option<f64>,
    pub url: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct CameraItem {
    pub id: String,
    pub name: String,
    pub lat: f64,
    pub lon: f64,
    pub category: String,
    pub image_url: String,
    pub page_url: String,
    pub location: String,
}

#[derive(Debug, Serialize)]
pub struct InspectFeeds {
    pub news: FeedSection<NewsItem>,
    pub conflicts: FeedSection<ConflictItem>,
    pub nuclear: FeedSection<NuclearItem>,
    pub flights: FeedSection<FlightItem>,
    pub quakes: FeedSection<QuakeItem>,
    pub cameras: FeedSection<CameraItem>,
}

impl InspectFeeds {
    fn empty() -> Self {
        Self {
            news: FeedSection::empty(),
            conflicts: FeedSection::empty(),
            nuclear: FeedSection::empty(),
            flights: FeedSection::empty(),
            quakes: FeedSection::empty(),
            cameras: FeedSection::empty(),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct CountryInspect {
    pub lat: f64,
    pub lon: f64,
    pub country: Option<CountryRef>,
    /// "bbox" | "nearest-country" | "international-waters".
    pub resolution: &'static str,
    pub resolution_note: String,
    pub osint: Option<country_osint::Country>,
    pub feeds: InspectFeeds,
}

#[derive(Debug, Deserialize)]
pub struct InspectQuery {
    pub lat: Option<f64>,
    pub lon: Option<f64>,
}

/// GDELT news for a country: same TTL cache path as `/api/news`, keyed by a
/// `sourcecountry:` query (GDELT DOC 2.1 operator; multi-word names quoted).
async fn news_section(state: &AppState, name: &str) -> FeedSection<NewsItem> {
    let q = if name.contains(' ') {
        format!("sourcecountry:\"{name}\"")
    } else {
        format!("sourcecountry:{name}")
    };
    let key = format!("news:q={q}:n=25");
    let client = state.client.clone();
    let body = match cached_bytes(state, &key, feeds::news::TTL, || async move {
        feeds::news::fetch(&client, &q, 25).await
    })
    .await
    {
        Ok((body, _)) => body,
        Err(e) => return FeedSection::err(format!("{e:?}")),
    };
    let resp: feeds::news::NewsResponse = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => return FeedSection::err(format!("decode failed: {e}")),
    };
    FeedSection::ok(
        resp.articles
            .into_iter()
            .take(10)
            .map(|a| NewsItem {
                title: a.title,
                url: a.url,
                domain: a.domain,
                seen_at: a.seen_at,
                source_country: a.source_country,
            })
            .collect(),
    )
}

/// GDACS events for a country: same cache key/TTL as `/api/conflicts`,
/// filtered by the free-text country name.
async fn conflicts_section(state: &AppState, name: &str) -> FeedSection<ConflictItem> {
    let client = state.client.clone();
    let body = match cached_bytes(state, "conflicts", feeds::conflicts::TTL, || async move {
        feeds::conflicts::fetch(&client).await
    })
    .await
    {
        Ok((body, _)) => body,
        Err(e) => return FeedSection::err(format!("{e:?}")),
    };
    let resp: feeds::conflicts::ConflictsResponse = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => return FeedSection::err(format!("decode failed: {e}")),
    };
    FeedSection::ok(
        resp.events
            .into_iter()
            .filter(|e| e.country.as_deref().is_some_and(|c| names_match(c, name)))
            .take(10)
            .map(|e| ConflictItem {
                title: e.title,
                date: e.date,
                url: e.url,
            })
            .collect(),
    )
}

/// Nuclear facilities for a country: static embedded registry, filtered in
/// memory (no cache, no upstream traffic).
fn nuclear_section(name: &str) -> FeedSection<NuclearItem> {
    let registry = match feeds::nuclear::load_registry() {
        Ok(r) => r,
        Err(e) => return FeedSection::err(e),
    };
    FeedSection::ok(
        registry
            .into_iter()
            .filter(|f| names_match(&f.country, name))
            .take(10)
            .map(|f| NuclearItem {
                name: f.name,
                lat: f.lat,
                lon: f.lon,
                status: f.status,
            })
            .collect(),
    )
}

/// Keep the flights whose reported position falls inside the country's
/// polygon bboxes. Pure: unit-testable without network or cache.
fn flights_items(resp: feeds::flights::FlightsResponse, iso3: &str) -> Vec<FlightItem> {
    resp.flights
        .into_iter()
        .filter_map(|f| {
            let (lat, lon) = (f.latitude?, f.longitude?);
            if point_in_country_bboxes(lat, lon, iso3) {
                Some(FlightItem {
                    hex: f.hex,
                    callsign: f.callsign,
                    registration: f.registration,
                    aircraft_type: f.aircraft_type,
                    latitude: lat,
                    longitude: lon,
                    altitude_m: f.altitude_m,
                    speed_knots: f.speed_knots,
                })
            } else {
                None
            }
        })
        .take(50)
        .collect()
}

/// Aircraft over a country: `scope=all` (military-only would be a strange
/// subset for a country panel), 500 upstream so small countries still get
/// hits. Same `cached_bytes` TTL pattern as `/api/flights`; the
/// `scope=all:limit=500` key is distinct from the endpoint's default key.
async fn flights_section(state: &AppState, iso3: &str) -> FeedSection<FlightItem> {
    let client = state.client.clone();
    let config = state.config.clone();
    let key = "flights:scope=all:limit=500";
    let body = match cached_bytes(state, key, feeds::flights::TTL, || async move {
        feeds::flights::fetch_with_config(&client, &config, "all", 500).await
    })
    .await
    {
        Ok((body, _)) => body,
        Err(e) => return FeedSection::err(format!("{e:?}")),
    };
    let resp: feeds::flights::FlightsResponse = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => return FeedSection::err(format!("decode failed: {e}")),
    };
    FeedSection::ok(flights_items(resp, iso3))
}

/// Keep the quakes whose epicenter falls inside the country's polygon
/// bboxes. Pure: unit-testable. The USGS feed is newest-first, so `take`
/// preserves that order.
fn quakes_items(resp: feeds::quakes::QuakesResponse, iso3: &str) -> Vec<QuakeItem> {
    resp.quakes
        .into_iter()
        .filter_map(|q| {
            let (lat, lon) = (q.latitude?, q.longitude?);
            if point_in_country_bboxes(lat, lon, iso3) {
                Some(QuakeItem {
                    id: q.id,
                    magnitude: q.magnitude,
                    place: q.place,
                    time: q.time,
                    latitude: lat,
                    longitude: lon,
                    depth_km: q.depth_km,
                    url: q.url,
                })
            } else {
                None
            }
        })
        .take(10)
        .collect()
}

/// Earthquakes in a country: same cache key/TTL as `/api/quakes`.
async fn quakes_section(state: &AppState, iso3: &str) -> FeedSection<QuakeItem> {
    let client = state.client.clone();
    let body = match cached_bytes(state, "quakes", feeds::quakes::TTL, || async move {
        feeds::quakes::fetch(&client).await
    })
    .await
    {
        Ok((body, _)) => body,
        Err(e) => return FeedSection::err(format!("{e:?}")),
    };
    let resp: feeds::quakes::QuakesResponse = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => return FeedSection::err(format!("decode failed: {e}")),
    };
    FeedSection::ok(quakes_items(resp, iso3))
}

/// Keep the cameras whose coordinates fall inside the country's polygon
/// bboxes. Pure: unit-testable. (The registry carries concrete lat/lon, so
/// no city→country guessing is needed.)
fn cameras_items(cams: Vec<feeds::cameras::Camera>, iso3: &str) -> Vec<CameraItem> {
    cams.into_iter()
        .filter(|c| point_in_country_bboxes(c.lat, c.lon, iso3))
        .map(|c| CameraItem {
            id: c.id,
            name: c.name,
            lat: c.lat,
            lon: c.lon,
            category: c.category,
            image_url: c.image_url,
            page_url: c.page_url,
            location: c.location,
        })
        .collect()
}

/// Public cameras in a country: static embedded registry, filtered in
/// memory (no cache, no upstream traffic).
fn cameras_section(iso3: &str) -> FeedSection<CameraItem> {
    match feeds::cameras::load_registry() {
        Ok(cams) => FeedSection::ok(cameras_items(cams, iso3)),
        Err(e) => FeedSection::err(e),
    }
}

async fn inspect_feeds(state: &AppState, country: &CountryRef) -> InspectFeeds {
    // Network sections run together on cache miss; the static sections are
    // computed inline (cheap).
    let (news, conflicts, flights, quakes) = tokio::join!(
        news_section(state, &country.name),
        conflicts_section(state, &country.name),
        flights_section(state, &country.iso3),
        quakes_section(state, &country.iso3),
    );
    InspectFeeds {
        news,
        conflicts,
        nuclear: nuclear_section(&country.name),
        flights,
        quakes,
        cameras: cameras_section(&country.iso3),
    }
}

/// `GET /api/country/inspect?lat=&lon=`.
pub async fn inspect(
    State(state): State<AppState>,
    Query(q): Query<InspectQuery>,
) -> Result<Json<CountryInspect>, ApiError> {
    let lat = q.lat.ok_or_else(|| {
        ApiError::bad_request("missing lat", "query params lat and lon are required")
    })?;
    let lon = q.lon.ok_or_else(|| {
        ApiError::bad_request("missing lon", "query params lat and lon are required")
    })?;
    if !lat.is_finite()
        || !lon.is_finite()
        || !(-90.0..=90.0).contains(&lat)
        || !(-180.0..=180.0).contains(&lon)
    {
        return Err(ApiError::bad_request(
            "invalid coordinates",
            "lat must be in [-90, 90] and lon in [-180, 180]",
        ));
    }

    let (country, resolution, resolution_note, osint) = match resolve_country(lat, lon) {
        Some(r) => {
            let (resolution, note) = match r.method {
                ResolveMethod::Bbox => (
                    "bbox",
                    "point falls inside the country's bounding box".to_string(),
                ),
                ResolveMethod::NearestCountry => (
                    "nearest-country",
                    "no bounding box contains this point; resolved to the nearest \
                     country's metropolitan center — borders are approximate at this resolution"
                        .to_string(),
                ),
            };
            // OSINT packs are keyed by ISO code or common name; the bbox
            // table's Natural Earth names may differ from the pack names, so
            // try the ISO code first.
            let osint = country_osint::find_country(&r.iso3)
                .or_else(|| country_osint::find_country(&r.name))
                .cloned();
            (
                Some(CountryRef {
                    name: r.name.clone(),
                    iso2: r.iso2.clone(),
                    iso3: r.iso3.clone(),
                }),
                resolution,
                note,
                osint,
            )
        }
        None => (
            None,
            "international-waters",
            "no country within 500 km — this looks like international waters".to_string(),
            None,
        ),
    };

    let feeds = match &country {
        Some(c) => inspect_feeds(&state, c).await,
        None => InspectFeeds::empty(),
    };

    Ok(Json(CountryInspect {
        lat,
        lon,
        country,
        resolution,
        resolution_note,
        osint,
        feeds,
    }))
}

/// Router fragment merged into the canonical app in `routes::app`.
pub fn routes() -> Router<crate::routes::AppState> {
    Router::new().route("/api/country/inspect", get(inspect))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_canonicalization_covers_verified_divergences() {
        assert!(names_match("United States of America", "United States"));
        assert!(names_match("Czechia", "Czech Republic"));
        assert!(names_match("United Arab Emirates", "UAE"));
        assert!(names_match("Japan", "Japan"));
        assert!(!names_match("Japan", "China"));
    }

    fn flight_at(hex: &str, lat: Option<f64>, lon: Option<f64>) -> feeds::flights::Flight {
        feeds::flights::Flight {
            hex: hex.to_string(),
            callsign: None,
            registration: None,
            aircraft_type: None,
            latitude: lat,
            longitude: lon,
            altitude_m: None,
            on_ground: false,
            speed_knots: None,
            track_deg: None,
            emergency: None,
            seen_pos_s: None,
        }
    }

    #[test]
    fn flights_filter_keeps_only_in_country_aircraft() {
        let resp = feeds::flights::FlightsResponse {
            scope: "all".to_string(),
            count: 4,
            total_upstream: 4,
            generated_at: "t".to_string(),
            flights: vec![
                flight_at("a1", Some(35.7), Some(139.7)), // Tokyo -> JPN
                flight_at("a2", Some(48.9), Some(2.3)),   // Paris -> FRA
                flight_at("a3", None, None),              // no position
                flight_at("a4", Some(0.0), Some(-150.0)), // mid-Pacific
            ],
        };
        let items = flights_items(resp, "JPN");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].hex, "a1");
        assert!((items[0].latitude - 35.7).abs() < 1e-9);
    }

    fn quake_at(id: &str, lat: Option<f64>, lon: Option<f64>) -> feeds::quakes::Quake {
        feeds::quakes::Quake {
            id: id.to_string(),
            magnitude: Some(5.0),
            magnitude_type: None,
            place: None,
            time: "t".to_string(),
            updated: "t".to_string(),
            url: None,
            latitude: lat,
            longitude: lon,
            depth_km: None,
            tsunami: false,
            status: None,
        }
    }

    #[test]
    fn quakes_filter_keeps_only_in_country_epicenters() {
        let resp = feeds::quakes::QuakesResponse {
            count: 3,
            generated_at: "t".to_string(),
            quakes: vec![
                quake_at("q1", Some(35.7), Some(139.7)), // Tokyo -> JPN
                quake_at("q2", Some(40.7), Some(-74.0)), // NYC -> USA
                quake_at("q3", None, None),              // no geometry
            ],
        };
        let items = quakes_items(resp, "JPN");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, "q1");
    }

    #[test]
    fn cameras_filter_keeps_only_in_country_cameras() {
        let cams = vec![
            feeds::cameras::Camera {
                id: "c1".to_string(),
                name: "Shibuya".to_string(),
                lat: 35.66,
                lon: 139.70,
                category: "traffic".to_string(),
                image_url: "u".to_string(),
                page_url: "p".to_string(),
                location: "Tokyo".to_string(),
            },
            feeds::cameras::Camera {
                id: "c2".to_string(),
                name: "Times Sq".to_string(),
                lat: 40.76,
                lon: -73.99,
                category: "traffic".to_string(),
                image_url: "u".to_string(),
                page_url: "p".to_string(),
                location: "New York".to_string(),
            },
        ];
        let items = cameras_items(cams, "JPN");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, "c1");
    }
}
