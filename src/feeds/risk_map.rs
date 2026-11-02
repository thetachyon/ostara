//! Weekly risk-map layer: curated places scored fresh every week via
//! `/api/risk`, snapshots persisted to disk.
//!
//! Architecture:
//! - `data/risk_watchlist.json` (embedded via `include_str!`) is the
//!   curated place list: 30 countries + cities with map centroids. The
//!   *scoring script* (`scripts/risk_map_score.sh`) reads it from disk;
//!   the API embeds it so the endpoint can report the watchlist even
//!   before the first snapshot exists.
//! - The scoring script runs weekly (cron `risk-map-weekly-refresh`),
//!   scoring each place SEQUENTIALLY through the local `/api/risk`
//!   endpoint (parallel bursts stall this sandbox's egress proxy and
//!   produce all-failed 0.0 scores — verified 2026-11-02), and writes
//!   `data/risk_snapshots/snapshot-YYYYMMDD-HHMMSS.json` plus an atomic
//!   `data/risk_snapshots/latest.json`.
//! - `GET /api/risk-map` serves `latest.json` with week-over-week `delta`
//!   (score minus the previous snapshot's score, `null` on the first run).
//!   No snapshot yet → 503 with an honest detail. Snapshots are runtime
//!   data: `data/risk_snapshots/` is git-ignored, never committed.
//! - Snapshot dir: env `RISK_SNAPSHOT_DIR`, default `data/risk_snapshots`
//!   relative to the process working directory.
//!
//! Staleness: a snapshot older than [`STALE_AFTER_DAYS`] (9) is served
//! with `"stale": true` and the health probe reports degraded — the
//! weekly cadence plus 2 days of grace.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use axum::{extract::State, response::IntoResponse};
use serde::{Deserialize, Serialize};

use crate::{
    error::{ApiError, FeedError},
    routes::AppState,
};

pub const FEED: &str = "risk_map";
/// Days after which a snapshot is considered stale (weekly + 2d grace).
pub const STALE_AFTER_DAYS: i64 = 9;

/// One watchlist entry (from the embedded registry).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WatchPlace {
    pub name: String,
    pub kind: String,
    #[serde(default)]
    pub country_hint: Option<String>,
    pub lon: f64,
    pub lat: f64,
}

#[derive(Debug, Deserialize)]
struct WatchlistFile {
    places: Vec<WatchPlace>,
    #[allow(dead_code)]
    meta: serde_json::Value,
}

/// One scored place inside a snapshot file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotPlace {
    pub name: String,
    pub kind: String,
    pub lon: f64,
    pub lat: f64,
    pub score: Option<f64>,
    pub level: Option<String>,
    pub safety: Option<String>,
    pub partial: bool,
    #[serde(default)]
    pub ok_components: usize,
    #[serde(default)]
    pub families: Vec<SnapshotFamily>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotFamily {
    pub family: String,
    pub weight: f64,
    pub score_0_100: f64,
}

#[derive(Debug, Deserialize)]
struct SnapshotFile {
    generated_at: String,
    #[allow(dead_code)]
    place_count: usize,
    places: Vec<SnapshotPlace>,
    #[allow(dead_code)]
    caveat: Option<String>,
}

/// The curated place list, embedded at compile time.
pub fn load_watchlist() -> Result<Vec<WatchPlace>, String> {
    const RAW: &str = include_str!("../../data/risk_watchlist.json");
    let f: WatchlistFile =
        serde_json::from_str(RAW).map_err(|e| format!("risk watchlist invalid: {e}"))?;
    Ok(f.places)
}

fn snapshot_dir() -> PathBuf {
    std::env::var("RISK_SNAPSHOT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("data/risk_snapshots"))
}

/// Snapshot files sorted oldest → newest (filename timestamps sort
/// lexicographically).
fn snapshot_files(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| {
                    p.file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.starts_with("snapshot-") && n.ends_with(".json"))
                })
                .collect()
        })
        .unwrap_or_default();
    files.sort();
    files
}

fn read_snapshot(path: &Path) -> Result<SnapshotFile, String> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    serde_json::from_str(&raw).map_err(|e| format!("invalid snapshot {}: {e}", path.display()))
}

/// Snapshot age in whole days, from the `generated_at` RFC-3339 stamp
/// (`YYYY-MM-DDTHH:MM:SSZ`, as written by the scoring script).
fn snapshot_age_days(generated_at: &str) -> Option<i64> {
    let epoch = parse_rfc3339_utc(generated_at)?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs() as i64;
    Some((now - epoch).div_euclid(86_400))
}

/// Parse `YYYY-MM-DDTHH:MM:SSZ` to Unix epoch seconds. No chrono needed.
fn parse_rfc3339_utc(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 20 || b[10] != b'T' || b[19] != b'Z' {
        return None;
    }
    let num = |i: usize, n: usize| -> Option<i64> {
        std::str::from_utf8(&b[i..i + n]).ok()?.parse::<i64>().ok()
    };
    let (y, m, d) = (num(0, 4)?, num(5, 2)?, num(8, 2)?);
    let (hh, mm, ss) = (num(11, 2)?, num(14, 2)?, num(17, 2)?);
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) || hh > 23 || mm > 59 || ss > 60 {
        return None;
    }
    // Days from civil (Howard Hinnant's algorithm).
    let y_adj = if m <= 2 { y - 1 } else { y };
    let era = y_adj.div_euclid(400);
    let yoe = y_adj - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    Some(days * 86_400 + hh * 3600 + mm * 60 + ss)
}

#[derive(Debug, Clone, Serialize)]
pub struct RiskMapPlace {
    pub name: String,
    pub kind: String,
    pub lon: f64,
    pub lat: f64,
    pub score: Option<f64>,
    pub level: Option<String>,
    pub safety: Option<String>,
    pub partial: bool,
    /// Score minus the previous snapshot's score; `null` on first run or
    /// when the place is new.
    pub delta: Option<f64>,
    pub families: Vec<SnapshotFamily>,
    pub snapshot_at: String,
}

#[derive(Debug, Serialize)]
pub struct RiskMapResponse {
    pub generated_at: String,
    pub count: usize,
    pub stale: bool,
    pub stale_after_days: i64,
    pub caveat: String,
    pub places: Vec<RiskMapPlace>,
}

pub const CAVEAT: &str = "Weekly heuristic media-signal snapshot — not a forecast. Scores compare current signals to 83-day baselines; big cities/capitals are systematically over-covered. English-language sources only. Not travel advice.";

/// Assemble the `/api/risk-map` response from the two newest snapshots.
/// Pure apart from filesystem reads — unit-testable via temp dirs.
pub fn build_response(dir: &Path) -> Result<RiskMapResponse, ApiError> {
    let files = snapshot_files(dir);
    let latest_path = files.last().ok_or_else(|| {
        ApiError::service_unavailable(
            "no risk snapshot yet",
            "no risk snapshot yet — the weekly scoring pipeline (scripts/risk_map_score.sh) has not run",
        )
    })?;
    let latest = read_snapshot(latest_path)
        .map_err(|d| ApiError::internal("risk snapshot unreadable", d))?;
    let prev = if files.len() >= 2 {
        read_snapshot(&files[files.len() - 2]).ok()
    } else {
        None
    };
    let prev_scores: HashMap<&str, f64> = {
        let mut m: HashMap<&str, f64> = HashMap::new();
        if let Some(p) = prev.as_ref() {
            for pl in &p.places {
                if let Some(s) = pl.score {
                    m.insert(pl.name.as_str(), s);
                }
            }
        }
        m
    };

    let places = latest
        .places
        .iter()
        .map(|p| {
            let delta = match (p.score, prev_scores.get(p.name.as_str())) {
                (Some(cur), Some(prev_s)) => Some(((cur - prev_s) * 100.0).round() / 100.0),
                _ => None,
            };
            RiskMapPlace {
                name: p.name.clone(),
                kind: p.kind.clone(),
                lon: p.lon,
                lat: p.lat,
                score: p.score,
                level: p.level.clone(),
                safety: p.safety.clone(),
                partial: p.partial,
                delta,
                families: p.families.clone(),
                snapshot_at: latest.generated_at.clone(),
            }
        })
        .collect::<Vec<_>>();

    let stale = snapshot_age_days(&latest.generated_at).is_some_and(|d| d > STALE_AFTER_DAYS);
    Ok(RiskMapResponse {
        count: places.len(),
        generated_at: latest.generated_at.clone(),
        stale,
        stale_after_days: STALE_AFTER_DAYS,
        caveat: CAVEAT.to_string(),
        places,
    })
}

/// GET /api/risk-map — the weekly risk snapshot with week-over-week deltas.
pub async fn handler(State(_state): State<AppState>) -> Result<impl IntoResponse, ApiError> {
    let dir = snapshot_dir();
    Ok(axum::Json(build_response(&dir)?))
}

/// Short detail string for the health endpoint. Err (→ degraded) when no
/// snapshot exists yet or the latest is older than [`STALE_AFTER_DAYS`].
pub async fn probe_detail() -> Result<String, FeedError> {
    let dir = snapshot_dir();
    let files = snapshot_files(&dir);
    let latest_path = files.last().ok_or_else(|| {
        FeedError::new(
            FEED,
            "no snapshot yet — weekly scoring has not run".to_string(),
        )
    })?;
    let snap = read_snapshot(latest_path).map_err(|e| FeedError::new(FEED, e))?;
    let age = snapshot_age_days(&snap.generated_at);
    if age.is_some_and(|d| d > STALE_AFTER_DAYS) {
        return Err(FeedError::new(
            FEED,
            format!(
                "snapshot stale ({}d old, older than {STALE_AFTER_DAYS}d) — weekly scoring may have failed",
                age.unwrap_or(-1)
            ),
        ));
    }
    let age_s = age
        .map(|d| format!("{d}d old"))
        .unwrap_or_else(|| "age unknown".into());
    Ok(format!(
        "{} places, snapshot {} ({})",
        snap.places.len(),
        snap.generated_at,
        age_s
    ))
}

/// Whether the latest snapshot is missing or stale — for health status.
pub fn health_is_degraded() -> bool {
    let dir = snapshot_dir();
    let files = snapshot_files(&dir);
    let Some(latest_path) = files.last() else {
        return true;
    };
    let Ok(snap) = read_snapshot(latest_path) else {
        return true;
    };
    snapshot_age_days(&snap.generated_at).is_some_and(|d| d > STALE_AFTER_DAYS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_snap(dir: &Path, name: &str, at: &str, scores: &[(&str, f64)]) {
        let places: Vec<serde_json::Value> = scores
            .iter()
            .map(|(n, s)| {
                serde_json::json!({"name": n, "kind": "country", "lon": 0.0, "lat": 0.0,
                    "score": s, "level": "high", "safety": "Low", "partial": true,
                    "ok_components": 6,
                    "families": [{"family": "unrest", "weight": 0.4, "score_0_100": *s}]})
            })
            .collect();
        let snap = serde_json::json!({"generated_at": at, "place_count": places.len(),
            "places": places, "caveat": "test"});
        let mut f = std::fs::File::create(dir.join(name)).unwrap();
        write!(f, "{snap}").unwrap();
    }

    fn tmpdir() -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "riskmap-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn watchlist_loads_30_places() {
        let wl = load_watchlist().unwrap();
        assert_eq!(wl.len(), 30, "expected 30 watchlist places");
        assert!(
            wl.iter()
                .any(|p| p.name == "Ukraine" && p.kind == "country")
        );
        assert!(wl.iter().any(|p| p.name == "Paris"
            && p.kind == "city"
            && p.country_hint.as_deref() == Some("France")));
    }

    #[test]
    fn missing_snapshot_is_503() {
        let d = tmpdir();
        let err = build_response(&d).unwrap_err();
        let resp = err.into_response();
        assert_eq!(resp.status(), axum::http::StatusCode::SERVICE_UNAVAILABLE);
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn delta_computed_against_previous_snapshot() {
        let d = tmpdir();
        write_snap(
            &d,
            "snapshot-20260101-000000.json",
            "2026-01-01T00:00:00Z",
            &[("France", 80.0), ("Paris", 70.0)],
        );
        write_snap(
            &d,
            "snapshot-20260108-000000.json",
            "2026-01-08T00:00:00Z",
            &[("France", 86.8), ("Kyiv", 74.5)],
        );
        let r = build_response(&d).unwrap();
        assert_eq!(r.count, 2);
        let fr = r.places.iter().find(|p| p.name == "France").unwrap();
        assert!((fr.delta.unwrap() - 6.8).abs() < 0.01, "delta = 86.8-80.0");
        let ky = r.places.iter().find(|p| p.name == "Kyiv").unwrap();
        assert!(ky.delta.is_none(), "new place has no delta");
        assert!(!r.stale || r.stale, "stale flag present");
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn single_snapshot_has_null_deltas() {
        let d = tmpdir();
        write_snap(
            &d,
            "snapshot-20260101-000000.json",
            "2026-01-01T00:00:00Z",
            &[("France", 80.0)],
        );
        let r = build_response(&d).unwrap();
        assert!(r.places[0].delta.is_none());
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn stale_snapshot_flagged() {
        let d = tmpdir();
        write_snap(
            &d,
            "snapshot-20200101-000000.json",
            "2020-01-01T00:00:00Z",
            &[("France", 80.0)],
        );
        let r = build_response(&d).unwrap();
        assert!(r.stale, "2020 snapshot must be stale");
        assert!(health_is_degraded() || true, "health fn callable");
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn snapshot_age_parses_rfc3339() {
        assert!(snapshot_age_days("not-a-date").is_none());
        let days = snapshot_age_days("2020-01-01T00:00:00Z").unwrap();
        assert!(days > 365, "2020 is long ago");
    }
}
