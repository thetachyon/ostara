//! Nuclear facilities: curated static reference (no key, no upstream).
//!
//! The registry is compile-time data (`data/nuclear.json`): 189 nuclear
//! power stations — 173 in service, 16 under construction — across 34
//! countries, embedded via `include_str!`. No TTL, no cache: filtering is
//! microseconds, like the cameras registry.
//!
//! DATASET SCOPE & LIMITS (read before citing):
//! - Source: the station list is derived from the IAEA PRIS-based table on
//!   Wikipedia's "List of nuclear power stations" (page crawled 2026-10-05;
//!   dataset compiled 2026-11-01). Every entry carries its source string.
//! - Scope is power *stations* (sites), not individual reactors; a station
//!   with 6 units is one entry. Only in-service and under-construction
//!   stations are included — permanently shut-down sites (e.g. Chernobyl,
//!   Fukushima Daiichi) are out of scope for this revision.
//! - Coordinates are station-level (decimal degrees from the page's geo
//!   templates), suitable for mapping, not for targeting-grade geolocation.
//! - `reactor_type` is intentionally OMITTED: the source table does not
//!   carry per-station types, and cross-referencing the per-reactor table by
//!   fuzzy plant-name matching risked silent misattribution. The
//!   authoritative per-unit source is IAEA PRIS itself (or Wikipedia's
//!   "List of commercial nuclear reactors").
//! - Statuses are as of the crawl date; the table is a snapshot, not a live
//!   feed. One normalization was applied: Krško (in Krško, Slovenia; jointly
//!   owned with Croatia) is listed under "Slovenia".

use axum::{
    extract::{Query, State},
    response::IntoResponse,
};
use serde::{Deserialize, Serialize};

use crate::{
    config::Config,
    error::{ApiError, FeedError},
    routes::AppState,
    time::now_iso8601,
};

pub const FEED: &str = "nuclear";
// NOTE: no TTL — the registry is static data, served without caching.

/// One nuclear power station.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NuclearFacility {
    pub id: String,
    pub name: String,
    pub lat: f64,
    pub lon: f64,
    pub country: String,
    /// Omitted by design in this revision — see module docs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reactor_type: Option<String>,
    pub status: String,
    pub source: String,
}

#[derive(Debug, Serialize)]
pub struct NuclearResponse {
    pub count: usize,
    pub generated_at: String,
    pub facilities: Vec<NuclearFacility>,
}

/// Load the embedded registry. A corrupt registry is a deploy-time bug.
pub fn load_registry() -> Result<Vec<NuclearFacility>, String> {
    const RAW: &str = include_str!("../../data/nuclear.json");
    serde_json::from_str(RAW).map_err(|e| format!("nuclear registry invalid: {e}"))
}

/// Filter by country (case-insensitive exact match); `None` returns all.
pub fn filter_country(
    facilities: &[NuclearFacility],
    country: Option<&str>,
) -> Vec<NuclearFacility> {
    match country.map(str::trim).filter(|c| !c.is_empty()) {
        None => facilities.to_vec(),
        Some(want) => facilities
            .iter()
            .filter(|f| f.country.eq_ignore_ascii_case(want))
            .cloned()
            .collect(),
    }
}

/// Short detail string for the health endpoint.
pub async fn probe_detail(
    _client: &reqwest::Client,
    _config: &Config,
) -> Result<String, FeedError> {
    let n = load_registry().map_err(|e| FeedError::new(FEED, e))?.len();
    Ok(format!("{n} facilities (static registry)"))
}

// ---------------------------------------------------------------------------
// GET /api/nuclear?country=
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct NuclearQuery {
    pub country: Option<String>,
}

pub async fn handler(
    State(_state): State<AppState>,
    Query(q): Query<NuclearQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let registry =
        load_registry().map_err(|detail| ApiError::internal("nuclear registry invalid", detail))?;
    let facilities = filter_country(&registry, q.country.as_deref());
    Ok(axum::Json(NuclearResponse {
        count: facilities.len(),
        generated_at: now_iso8601(),
        facilities,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_loads_with_50_plus_entries() {
        let r = load_registry().unwrap();
        assert!(r.len() >= 50, "expected 50+ facilities, got {}", r.len());
    }

    #[test]
    fn every_entry_has_coords_name_country_source() {
        for f in load_registry().unwrap() {
            assert!(!f.id.is_empty(), "empty id");
            assert!(!f.name.is_empty(), "empty name");
            assert!(!f.country.is_empty(), "empty country: {}", f.name);
            assert!(!f.source.is_empty(), "unsourced entry: {}", f.name);
            assert!(
                (-90.0..=90.0).contains(&f.lat) && (-180.0..=180.0).contains(&f.lon),
                "coords out of range: {} ({}, {})",
                f.name,
                f.lat,
                f.lon
            );
            assert!(
                ["operational", "under_construction"].contains(&f.status.as_str()),
                "bad status: {}",
                f.name
            );
        }
    }

    #[test]
    fn country_filter_is_case_insensitive() {
        let r = load_registry().unwrap();
        let jp = filter_country(&r, Some("japan"));
        assert!(!jp.is_empty());
        assert!(jp.iter().all(|f| f.country == "Japan"));
        assert_eq!(jp.len(), filter_country(&r, Some("Japan")).len());
    }

    #[test]
    fn unknown_country_yields_empty() {
        let r = load_registry().unwrap();
        assert!(filter_country(&r, Some("Atlantis")).is_empty());
    }

    #[test]
    fn no_filter_returns_all() {
        let r = load_registry().unwrap();
        assert_eq!(filter_country(&r, None).len(), r.len());
    }
}
