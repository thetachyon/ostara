//! Country inspect: click-to-identify for the map UI.
//!
//! `GET /api/country/inspect?lat=&lon=` resolves a map click to a country
//! and returns that country's OSINT resource pack plus the country-tagged
//! items from the other feeds, in one response.
//!
//! **No polygon data is shipped.** Resolution uses a compact embedded table
//! (`data/country_bboxes.json`, built by `scripts/gen_country_bboxes.py`
//! from the public-domain datasets/geo-countries 110m GeoJSON): one bbox
//! per polygon per country — a single full-geometry bbox is broken-wide for
//! far-flung territories (NZL's spans 357° of longitude, which would make
//! the South Atlantic "New Zealand"). A point resolves to the country whose
//! containing polygon bbox it sits deepest inside (nearest-edge distance);
//! a point in no bbox resolves to the nearest metropolitan center (center of
//! the largest polygon's bbox) within 500 km, otherwise it is reported as
//! international waters. This is deliberately coarse at borders; the
//! response names the resolution method so the UI can caveat it.
//!
//! Feed sections reuse the exact TTL cache paths of the normal endpoints
//! (`cached_bytes` with the same keys/TTLs) — a cache miss fetches upstream
//! exactly as `/api/news` or `/api/conflicts` would. Only feeds whose
//! normalized structs genuinely carry a country field are included:
//! `news` (GDELT `sourcecountry`, via a `sourcecountry:` query), `conflicts`
//! (GDACS `gdacs:country` name), `nuclear` (static registry `country` name).
//! Verified NOT country-taggable and excluded: cameras (`location` is
//! city-level free text), fires (lat/lon only), quakes (place string),
//! flights (normalized struct has no country field), vessels (disabled),
//! cyber/threatfox/telegram/markets/helio/satellites/n2yo/weather (no
//! country field).

use std::sync::OnceLock;

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
    routes::{AppState, cached_bytes},
};

static BBOX_JSON: &str = include_str!("../data/country_bboxes.json");

/// Nearest-centroid fallback radius: beyond this the point is ocean.
const NEAREST_KM: f64 = 500.0;

#[derive(Debug, Deserialize, Clone)]
struct CountryBox {
    name: String,
    iso2: String,
    iso3: String,
    /// One bbox per polygon, [west, south, east, north]; west > east means
    /// antimeridian crossing. Per-polygon (not one full-geometry bbox):
    /// a single bbox is broken-wide for far-flung territories — NZL's spans
    /// 357° of longitude, which would make the South Atlantic "New Zealand".
    bboxes: Vec<[f64; 4]>,
    /// Center of the largest polygon's bbox — metropolitan-territory proxy
    /// for the ocean fallback. (A coordinate-mean centroid would be dragged
    /// by overseas territories, e.g. France's mean centroid lands in the
    /// Atlantic.)
    /// [lon, lat].
    metro: [f64; 2],
}

#[derive(Debug, Deserialize)]
struct BboxDataset {
    #[allow(dead_code)]
    meta: serde_json::Value,
    countries: Vec<CountryBox>,
}

fn boxes() -> &'static [CountryBox] {
    static DATA: OnceLock<BboxDataset> = OnceLock::new();
    DATA.get_or_init(|| serde_json::from_str(BBOX_JSON).expect("data/country_bboxes.json parses"))
        .countries
        .as_slice()
}

fn haversine_km(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    const R: f64 = 6371.0;
    let (p1, p2) = (lat1.to_radians(), lat2.to_radians());
    let dp = p2 - p1;
    let dl = (lon2 - lon1).to_radians();
    let a = (dp / 2.0).sin().powi(2) + p1.cos() * p2.cos() * (dl / 2.0).sin().powi(2);
    2.0 * R * a.sqrt().asin()
}

fn norm_lon(lon: f64) -> f64 {
    ((lon + 540.0) % 360.0) - 180.0
}

fn in_bbox(lat: f64, lon: f64, b: &[f64; 4]) -> bool {
    let [w, s, e, n] = *b;
    if lat < s || lat > n {
        return false;
    }
    if w <= e {
        lon >= w && lon <= e
    } else {
        // Antimeridian-crossing bbox.
        lon >= w || lon <= e
    }
}

/// Angular distance (degrees) from an interior point to the nearest bbox
/// edge. Used to break ties between overlapping polygon bboxes: the point
/// is assigned to the territory it sits deepest inside. (Ties happen because
/// overseas territories inflate overlaps — e.g. the Caribbean Netherlands'
/// bbox also covers Paris.)
fn edge_distance(lat: f64, lon: f64, b: &[f64; 4]) -> f64 {
    let [w, s, e, n] = *b;
    let d_lat = (lat - s).min(n - lat);
    let d_lon = if w <= e {
        (lon - w).min(e - lon)
    } else if lon >= w {
        // Western piece [w, 180]: east edge wraps across the antimeridian.
        (lon - w).min((180.0 - lon) + (e + 180.0))
    } else {
        // Eastern piece [-180, e]: west edge wraps across the antimeridian.
        ((lon + 180.0) + (180.0 - w)).min(e - lon)
    };
    d_lat.min(d_lon)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolveMethod {
    Bbox,
    NearestCountry,
}

#[derive(Debug, Clone)]
pub struct Resolved {
    pub name: String,
    pub iso2: String,
    pub iso3: String,
    pub method: ResolveMethod,
}

/// Nearest candidate to (lat, lon) by metropolitan-center distance.
fn nearest<'a>(lat: f64, lon: f64, cands: &[&'a CountryBox]) -> Option<(&'a CountryBox, f64)> {
    cands
        .iter()
        .map(|c| (*c, haversine_km(lat, lon, c.metro[1], c.metro[0])))
        .min_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
}

/// Resolve a point to a country. `None` = international waters (no polygon
/// bbox hit and no metro center within [`NEAREST_KM`]).
pub fn resolve(lat: f64, lon: f64) -> Option<Resolved> {
    let lon = norm_lon(lon);

    // (country, best edge distance across its containing polygons,
    //  metro distance as a deterministic secondary key)
    let mut best: Option<(&CountryBox, f64, f64)> = None;
    for c in boxes() {
        let mut edge = f64::INFINITY;
        for b in &c.bboxes {
            if in_bbox(lat, lon, b) {
                edge = edge.min(edge_distance(lat, lon, b));
            }
        }
        if edge.is_finite() {
            let metro_d = haversine_km(lat, lon, c.metro[1], c.metro[0]);
            let take = match &best {
                Some((_, be, bm)) => (edge, metro_d) < (*be, *bm),
                None => true,
            };
            if take {
                best = Some((c, edge, metro_d));
            }
        }
    }
    if let Some((c, _, _)) = best {
        return Some(Resolved {
            name: c.name.clone(),
            iso2: c.iso2.clone(),
            iso3: c.iso3.clone(),
            method: ResolveMethod::Bbox,
        });
    }
    match nearest(lat, lon, &boxes().iter().collect::<Vec<_>>()) {
        Some((c, d)) if d <= NEAREST_KM => Some(Resolved {
            name: c.name.clone(),
            iso2: c.iso2.clone(),
            iso3: c.iso3.clone(),
            method: ResolveMethod::NearestCountry,
        }),
        _ => None,
    }
}

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
pub struct InspectFeeds {
    pub news: FeedSection<NewsItem>,
    pub conflicts: FeedSection<ConflictItem>,
    pub nuclear: FeedSection<NuclearItem>,
}

impl InspectFeeds {
    fn empty() -> Self {
        Self {
            news: FeedSection::empty(),
            conflicts: FeedSection::empty(),
            nuclear: FeedSection::empty(),
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

async fn inspect_feeds(state: &AppState, country: &CountryRef) -> InspectFeeds {
    // News and conflicts hit the network on cache miss; run them together.
    let (news, conflicts) = tokio::join!(
        news_section(state, &country.name),
        conflicts_section(state, &country.name)
    );
    InspectFeeds {
        news,
        conflicts,
        nuclear: nuclear_section(&country.name),
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

    let (country, resolution, resolution_note, osint) = match resolve(lat, lon) {
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
    fn paris_resolves_to_france() {
        let r = resolve(48.85, 2.35).expect("paris resolves");
        assert_eq!(r.iso3, "FRA");
        assert_eq!(r.method, ResolveMethod::Bbox);
    }

    #[test]
    fn tokyo_resolves_to_japan() {
        let r = resolve(35.68, 139.69).expect("tokyo resolves");
        assert_eq!(r.iso3, "JPN");
    }

    #[test]
    fn sydney_resolves_to_australia() {
        let r = resolve(-33.87, 151.21).expect("sydney resolves");
        assert_eq!(r.iso3, "AUS");
    }

    #[test]
    fn new_york_resolves_to_usa() {
        let r = resolve(40.71, -74.0).expect("nyc resolves");
        assert_eq!(r.iso3, "USA");
    }

    #[test]
    fn mid_south_atlantic_is_international_waters() {
        // (-45, -30): South Atlantic, in no polygon bbox, >500 km from any
        // metropolitan center.
        assert!(resolve(-45.0, -30.0).is_none());
    }

    #[test]
    fn hawaii_resolves_to_usa_via_island_polygon() {
        let r = resolve(19.6, -155.5).expect("hawaii resolves");
        assert_eq!(r.iso3, "USA");
        assert_eq!(r.method, ResolveMethod::Bbox);
    }

    #[test]
    fn moscow_resolves_to_russia() {
        let r = resolve(55.75, 37.62).expect("moscow resolves");
        assert_eq!(r.iso3, "RUS");
    }

    #[test]
    fn english_channel_resolves_to_france() {
        // (49.7, -2.0): in the Channel, inside metropolitan France's polygon
        // bbox — and several inflated neighbours' — deepest-inside wins.
        let r = resolve(49.7, -2.0).expect("channel resolves");
        assert_eq!(r.iso3, "FRA");
        assert_eq!(r.method, ResolveMethod::Bbox);
    }

    #[test]
    fn lon_wraps_around_antimeridian() {
        // Same point expressed at +190 == -170 (Bering Sea): the wrap must
        // not break resolution — either neighbour is a defensible answer.
        let r = resolve(65.0, 190.0).expect("wrapped lon resolves");
        assert!(r.iso3 == "RUS" || r.iso3 == "USA", "got {}", r.iso3);
        assert_eq!(r.method, ResolveMethod::Bbox);
    }

    #[test]
    fn name_canonicalization_covers_verified_divergences() {
        assert!(names_match("United States of America", "United States"));
        assert!(names_match("Czechia", "Czech Republic"));
        assert!(names_match("United Arab Emirates", "UAE"));
        assert!(names_match("Japan", "Japan"));
        assert!(!names_match("Japan", "China"));
    }
}
