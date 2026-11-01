//! Public-camera feed: a curated registry of keyless, intentionally-public
//! cameras (state DOT traffic cams, DriveBC highway cams, Finland road-weather
//! cams, NPS park cams).
//!
//! The registry lives in `data/cameras.json` and is embedded at compile time
//! via `include_str!`, so the binary stays self-contained and serving a
//! camera list is a pure in-memory filter — no upstream fetch, no cache.
//!
//! Deliberately NOT included: aggregators of unsecured private cameras
//! (insecam-style sites). Every entry is a camera its operator publishes
//! for public viewing. See README "Public cameras" for the bar contributors
//! must meet.

use std::{collections::HashSet, sync::LazyLock};

use serde::{Deserialize, Serialize};

/// One registry entry. Field names are the exact JSON served by /api/cameras.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Camera {
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
pub struct CamerasResponse {
    pub count: usize,
    pub generated_at: String,
    pub cameras: Vec<Camera>,
}

static REGISTRY_JSON: &str = include_str!("../../data/cameras.json");

/// Parse and validate the embedded registry. One code path used by the
/// route, /api/stats, /api/health/feeds, and the tests — no panics, so a
/// corrupt registry degrades gracefully instead of crashing the server.
pub fn load_registry() -> Result<Vec<Camera>, String> {
    let cameras: Vec<Camera> =
        serde_json::from_str(REGISTRY_JSON).map_err(|e| format!("parse failed: {e}"))?;
    let mut problems = Vec::new();
    let mut ids = HashSet::new();
    for (i, c) in cameras.iter().enumerate() {
        let tag = format!("entry {i} (id='{}')", c.id);
        if c.id.is_empty() {
            problems.push(format!("{tag}: empty id"));
        } else if !ids.insert(c.id.clone()) {
            problems.push(format!("{tag}: duplicate id"));
        }
        if c.name.is_empty() {
            problems.push(format!("{tag}: empty name"));
        }
        if c.category.is_empty() {
            problems.push(format!("{tag}: empty category"));
        }
        if !(c.image_url.starts_with("https://") || c.image_url.starts_with("http://")) {
            problems.push(format!("{tag}: image_url is not http(s): {}", c.image_url));
        }
        if c.page_url.is_empty()
            || !(c.page_url.starts_with("https://") || c.page_url.starts_with("http://"))
        {
            problems.push(format!("{tag}: page_url is not http(s)"));
        }
        if !(-90.0..=90.0).contains(&c.lat) || !(-180.0..=180.0).contains(&c.lon) {
            problems.push(format!(
                "{tag}: lat/lon out of range ({}, {})",
                c.lat, c.lon
            ));
        }
    }
    if problems.is_empty() {
        Ok(cameras)
    } else {
        Err(problems.join("; "))
    }
}

/// Validated registry, parsed once. Falls back to empty on (impossible
/// unless the shipped JSON is corrupt) failure; /api/health/feeds reports
/// the underlying error via `load_registry()`.
static REGISTRY: LazyLock<Vec<Camera>> = LazyLock::new(|| load_registry().unwrap_or_default());

pub fn registry() -> &'static [Camera] {
    &REGISTRY
}

/// Pure filter: bbox as (minlon, minlat, maxlon, maxlat), category matched
/// case-insensitively, limit clamped by the caller.
pub fn filter_cameras(
    cameras: &[Camera],
    bbox: Option<(f64, f64, f64, f64)>,
    category: Option<&str>,
    limit: usize,
) -> Vec<Camera> {
    cameras
        .iter()
        .filter(|c| category.is_none_or(|cat| c.category.eq_ignore_ascii_case(cat)))
        .filter(|c| {
            bbox.is_none_or(|(minlon, minlat, maxlon, maxlat)| {
                c.lon >= minlon && c.lon <= maxlon && c.lat >= minlat && c.lat <= maxlat
            })
        })
        .take(limit)
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_parses_and_validates() {
        let cams = load_registry().expect("embedded cameras.json must be valid");
        assert!(
            cams.len() >= 25,
            "registry should hold 25+ cameras, got {}",
            cams.len()
        );
    }

    #[test]
    fn every_entry_has_http_image_url_and_sane_coords() {
        let cams = load_registry().expect("registry valid");
        for c in &cams {
            assert!(
                c.image_url.starts_with("https://") || c.image_url.starts_with("http://"),
                "{}: image_url not http(s): {}",
                c.id,
                c.image_url
            );
            assert!(
                (-90.0..=90.0).contains(&c.lat) && (-180.0..=180.0).contains(&c.lon),
                "{}: lat/lon out of range",
                c.id
            );
            assert!(
                !c.id.is_empty() && !c.name.is_empty(),
                "{}: empty id/name",
                c.id
            );
        }
        let ids: HashSet<_> = cams.iter().map(|c| &c.id).collect();
        assert_eq!(ids.len(), cams.len(), "duplicate camera ids");
    }

    #[test]
    fn category_filter_is_case_insensitive() {
        let cams = load_registry().expect("registry valid");
        let lower = filter_cameras(&cams, None, Some("traffic"), 200);
        let upper = filter_cameras(&cams, None, Some("TRAFFIC"), 200);
        assert!(!lower.is_empty());
        assert_eq!(lower.len(), upper.len());
        assert!(lower.iter().all(|c| c.category == "traffic"));
    }

    #[test]
    fn bbox_filter_keeps_only_inside() {
        let cams = load_registry().expect("registry valid");
        // California bounding box: keeps Bay Area cams, drops Finland/Nebraska.
        let inside = filter_cameras(&cams, Some((-125.0, 32.0, -114.0, 42.0)), None, 200);
        assert!(!inside.is_empty(), "expected California cameras in bbox");
        for c in &inside {
            assert!(
                c.lon >= -125.0 && c.lon <= -114.0 && c.lat >= 32.0 && c.lat <= 42.0,
                "{} outside bbox",
                c.id
            );
        }
        // A bbox over the mid-Atlantic should match nothing.
        let empty = filter_cameras(&cams, Some((-50.0, 30.0, -40.0, 40.0)), None, 200);
        assert!(empty.is_empty());
    }

    #[test]
    fn limit_is_respected() {
        let cams = load_registry().expect("registry valid");
        assert_eq!(filter_cameras(&cams, None, None, 5).len(), 5);
        assert_eq!(filter_cameras(&cams, None, None, 10000).len(), cams.len());
    }
}
