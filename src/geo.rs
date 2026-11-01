//! Shared country-resolution geometry.
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
//! international waters. This is deliberately coarse at borders.
//!
//! Feeds use [`country_polygon_bboxes`] / [`point_in_country_bboxes`] to
//! test membership against ONE country's boxes (not all 4,274): a flights
//! snapshot can hold thousands of aircraft, so per-item cost is bounded by
//! one country's polygon count (~tens of bbox checks).

use std::collections::HashMap;
use std::sync::OnceLock;

use serde::Deserialize;

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

fn by_iso3() -> &'static HashMap<&'static str, &'static CountryBox> {
    static MAP: OnceLock<HashMap<&'static str, &'static CountryBox>> = OnceLock::new();
    MAP.get_or_init(|| {
        boxes()
            .iter()
            .map(|c| (c.iso3.as_str(), c))
            .collect::<HashMap<_, _>>()
    })
}

/// The polygon bboxes of ONE country, looked up by ISO-3166 alpha-3.
/// Empty slice when the code is unknown. Use this (not a scan of all
/// countries) when filtering a feed's items: per-item cost is bounded by
/// the country's polygon count.
pub fn country_polygon_bboxes(iso3: &str) -> &'static [[f64; 4]] {
    by_iso3()
        .get(iso3)
        .map(|c| c.bboxes.as_slice())
        .unwrap_or(&[])
}

/// True when (lat, lon) falls inside any polygon bbox of the country
/// identified by ISO-3166 alpha-3. False for unknown codes.
pub fn point_in_country_bboxes(lat: f64, lon: f64, iso3: &str) -> bool {
    let lon = norm_lon(lon);
    country_polygon_bboxes(iso3)
        .iter()
        .any(|b| in_bbox(lat, lon, b))
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

pub fn in_bbox(lat: f64, lon: f64, b: &[f64; 4]) -> bool {
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
pub fn resolve_country(lat: f64, lon: f64) -> Option<Resolved> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paris_resolves_to_france() {
        let r = resolve_country(48.85, 2.35).expect("paris resolves");
        assert_eq!(r.iso3, "FRA");
        assert_eq!(r.method, ResolveMethod::Bbox);
    }

    #[test]
    fn tokyo_resolves_to_japan() {
        let r = resolve_country(35.68, 139.69).expect("tokyo resolves");
        assert_eq!(r.iso3, "JPN");
    }

    #[test]
    fn sydney_resolves_to_australia() {
        let r = resolve_country(-33.87, 151.21).expect("sydney resolves");
        assert_eq!(r.iso3, "AUS");
    }

    #[test]
    fn new_york_resolves_to_usa() {
        let r = resolve_country(40.71, -74.0).expect("nyc resolves");
        assert_eq!(r.iso3, "USA");
    }

    #[test]
    fn mid_south_atlantic_is_international_waters() {
        // (-45, -30): South Atlantic, in no polygon bbox, >500 km from any
        // metropolitan center.
        assert!(resolve_country(-45.0, -30.0).is_none());
    }

    #[test]
    fn hawaii_resolves_to_usa_via_island_polygon() {
        let r = resolve_country(19.6, -155.5).expect("hawaii resolves");
        assert_eq!(r.iso3, "USA");
        assert_eq!(r.method, ResolveMethod::Bbox);
    }

    #[test]
    fn moscow_resolves_to_russia() {
        let r = resolve_country(55.75, 37.62).expect("moscow resolves");
        assert_eq!(r.iso3, "RUS");
    }

    #[test]
    fn english_channel_resolves_to_france() {
        // (49.7, -2.0): in the Channel, inside metropolitan France's polygon
        // bbox — and several inflated neighbours' — deepest-inside wins.
        let r = resolve_country(49.7, -2.0).expect("channel resolves");
        assert_eq!(r.iso3, "FRA");
        assert_eq!(r.method, ResolveMethod::Bbox);
    }

    #[test]
    fn lon_wraps_around_antimeridian() {
        // Same point expressed at +190 == -170 (Bering Sea): the wrap must
        // not break resolution — either neighbour is a defensible answer.
        let r = resolve_country(65.0, 190.0).expect("wrapped lon resolves");
        assert!(r.iso3 == "RUS" || r.iso3 == "USA", "got {}", r.iso3);
        assert_eq!(r.method, ResolveMethod::Bbox);
    }

    #[test]
    fn single_country_membership_matches_resolver() {
        // point_in_country_bboxes agrees with resolve_country for known
        // points, and is false for unknown ISO codes.
        assert!(point_in_country_bboxes(48.85, 2.35, "FRA"));
        assert!(!point_in_country_bboxes(48.85, 2.35, "DEU"));
        assert!(point_in_country_bboxes(35.68, 139.69, "JPN"));
        assert!(!point_in_country_bboxes(35.68, 139.69, "KOR"));
        assert!(!point_in_country_bboxes(48.85, 2.35, "XXX"));
        assert!(country_polygon_bboxes("FRA").len() > 1); // metro + territories
        assert!(country_polygon_bboxes("XXX").is_empty());
    }
}
