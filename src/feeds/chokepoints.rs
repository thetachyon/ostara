//! Maritime chokepoints: curated static reference (no key, no upstream).
//!
//! The registry is compile-time data (`data/chokepoints.json`): 6 major
//! maritime chokepoints and 4 currently congested container ports, embedded
//! via `include_str!`. No TTL, no cache: filtering is microseconds, like the
//! nuclear registry.
//!
//! DATASET SCOPE & LIMITS (read before citing):
//! - Compiled 2026-11-02 from maritime press (Alphaliner, Journal of
//!   Commerce, gCaptain, Xeneta, C.H. Robinson, Splash247) for the OSTARA
//!   marine-traffic briefing. Provenance recorded in the JSON meta.
//! - Locations are STATIC reference points (strait/canal/port centroids,
//!   suitable for mapping, not for targeting-grade geolocation).
//! - Statuses are a DATED SNAPSHOT as of 2026-11-02 — verify before
//!   operational use. This is reference data, not a live feed; statuses
//!   will go stale and are labeled as such on every response and in the UI.
//! - v1 is points only (no route polylines).

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

pub const FEED: &str = "chokepoints";
// NOTE: no TTL — the registry is static data, served without caching.

/// One chokepoint or congested port.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Chokepoint {
    pub id: String,
    pub name: String,
    /// "chokepoint" | "congestion_port"
    pub category: String,
    pub latitude: f64,
    pub longitude: f64,
    /// Current status as of `status_date` — a dated snapshot, not live.
    pub status: String,
    pub status_date: String,
}

#[derive(Debug, Deserialize)]
struct Dataset {
    entries: Vec<Chokepoint>,
    #[allow(dead_code)]
    meta: serde_json::Value,
}

#[derive(Debug, Serialize)]
pub struct ChokepointsResponse {
    pub count: usize,
    pub generated_at: String,
    /// "Statuses are a dated snapshot (2026-11-02) — verify before
    /// operational use. Reference data, not a live feed."
    pub caveat: String,
    pub entries: Vec<Chokepoint>,
}

/// Dated-snapshot caveat carried on every response.
pub const CAVEAT: &str = "Statuses are a dated snapshot (2026-11-02) — verify before operational use. Reference data, not a live feed.";

/// Load the embedded registry. A corrupt registry is a deploy-time bug.
pub fn load_registry() -> Result<Vec<Chokepoint>, String> {
    const RAW: &str = include_str!("../../data/chokepoints.json");
    let ds: Dataset =
        serde_json::from_str(RAW).map_err(|e| format!("chokepoints registry invalid: {e}"))?;
    Ok(ds.entries)
}

/// Filter by category ("chokepoint" | "congestion_port", case-insensitive);
/// `None` returns all.
pub fn filter_category(entries: &[Chokepoint], category: Option<&str>) -> Vec<Chokepoint> {
    match category.map(str::trim).filter(|c| !c.is_empty()) {
        None => entries.to_vec(),
        Some(want) => entries
            .iter()
            .filter(|e| e.category.eq_ignore_ascii_case(want))
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
    Ok(format!(
        "{n} entries (static registry, statuses as of 2026-11-02)"
    ))
}

// ---------------------------------------------------------------------------
// GET /api/chokepoints?category=
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct ChokepointsQuery {
    pub category: Option<String>,
}

pub async fn handler(
    State(_state): State<AppState>,
    Query(q): Query<ChokepointsQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let registry = load_registry()
        .map_err(|detail| ApiError::internal("chokepoints registry invalid", detail))?;
    let entries = filter_category(&registry, q.category.as_deref());
    Ok(axum::Json(ChokepointsResponse {
        count: entries.len(),
        generated_at: now_iso8601(),
        caveat: CAVEAT.to_string(),
        entries,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_loads_with_expected_entries() {
        let r = load_registry().unwrap();
        assert_eq!(r.len(), 10, "expected 10 entries, got {}", r.len());
        assert_eq!(
            r.iter().filter(|e| e.category == "chokepoint").count(),
            6,
            "expected 6 chokepoints"
        );
        assert_eq!(
            r.iter().filter(|e| e.category == "congestion_port").count(),
            4,
            "expected 4 congested ports"
        );
    }

    #[test]
    fn every_entry_has_coords_name_category_status_and_date() {
        for e in load_registry().unwrap() {
            assert!(!e.id.is_empty(), "empty id");
            assert!(!e.name.is_empty(), "empty name");
            assert!(
                ["chokepoint", "congestion_port"].contains(&e.category.as_str()),
                "bad category: {}",
                e.name
            );
            assert!(
                (-90.0..=90.0).contains(&e.latitude) && (-180.0..=180.0).contains(&e.longitude),
                "coords out of range: {} ({}, {})",
                e.name,
                e.latitude,
                e.longitude
            );
            assert!(!e.status.is_empty(), "empty status: {}", e.name);
            assert_eq!(e.status_date, "2026-11-02", "bad status_date: {}", e.name);
        }
    }

    #[test]
    fn ids_are_unique() {
        let r = load_registry().unwrap();
        let mut ids: Vec<&str> = r.iter().map(|e| e.id.as_str()).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), r.len(), "duplicate ids in registry");
    }

    #[test]
    fn category_filter_is_case_insensitive() {
        let r = load_registry().unwrap();
        let cp = filter_category(&r, Some("Chokepoint"));
        assert_eq!(cp.len(), 6);
        assert!(cp.iter().all(|e| e.category == "chokepoint"));
        let ports = filter_category(&r, Some("congestion_port"));
        assert_eq!(ports.len(), 4);
    }

    #[test]
    fn unknown_category_yields_empty() {
        let r = load_registry().unwrap();
        assert!(filter_category(&r, Some("atlantis")).is_empty());
    }

    #[test]
    fn no_filter_returns_all() {
        let r = load_registry().unwrap();
        assert_eq!(filter_category(&r, None).len(), r.len());
    }

    #[test]
    fn caveat_names_the_snapshot_date() {
        assert!(CAVEAT.contains("2026-11-02"));
        assert!(CAVEAT.contains("not a live feed"));
    }
}
