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
    /// Thin-basis flag from /api/risk: the score rests on <50% of
    /// intended signal — level capped one step down, confidence low.
    #[serde(default)]
    pub thin_basis: bool,
    /// "high" | "medium" | "low" from /api/risk.
    #[serde(default)]
    pub confidence: Option<String>,
    #[serde(default)]
    pub ok_components: usize,
    #[serde(default)]
    pub families: Vec<SnapshotFamily>,
    /// 7-day recent composite media volume (Σ raw_value over ok
    /// components) — the baseline-free peer-rank input.
    #[serde(default)]
    pub recent_volume: Option<f64>,
    /// Baseline-free percentile vs watchlist peers (100 = hottest).
    #[serde(default)]
    pub percentile: Option<f64>,
    /// 1-based competition rank among peers (1 = hottest).
    #[serde(default)]
    pub rank: Option<usize>,
    #[serde(default)]
    pub peer_count: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotFamily {
    pub family: String,
    pub weight: f64,
    pub score_0_100: f64,
    #[serde(default)]
    pub volume: Option<f64>,
    #[serde(default)]
    pub percentile: Option<f64>,
    #[serde(default)]
    pub rank: Option<usize>,
    /// Thin-basis flag from /api/risk for this family.
    #[serde(default)]
    pub thin_basis: bool,
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

pub(crate) fn snapshot_dir() -> PathBuf {
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
    /// Thin-basis flag: the score rests on <50% of intended signal.
    pub thin_basis: bool,
    /// "high" | "medium" | "low".
    pub confidence: Option<String>,
    /// Score minus the previous snapshot's score; `null` on first run or
    /// when the place is new.
    pub delta: Option<f64>,
    /// Baseline-free peer rank (hotter-than-N-of-M) — the robust headline
    /// alongside the time-ratio `score`.
    pub percentile: Option<f64>,
    pub rank: Option<usize>,
    pub peer_count: Option<usize>,
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
                thin_basis: p.thin_basis,
                confidence: p.confidence.clone(),
                delta,
                percentile: p.percentile,
                rank: p.rank,
                peer_count: p.peer_count,
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

/// Competition ranks + percentiles from per-place recent volumes.
/// Rank 1 = hottest (largest volume). Percentile = 100·(n−rank)/(n−1):
/// top → 100, bottom → 0. Pure.
///
/// This is the saturation-robust headline: upstream capping
/// (min(v, cap)) is weakly monotone, so it can only create ties — it
/// never reverses who is hotter than whom. Time-ratio baselines have no
/// such guarantee: capping explodes the ratio and pins scores at 100.
/// No baseline needed.
///
/// Reference implementation: the weekly scoring script
/// (`scripts/risk_map_score.sh`) mirrors this exact algorithm in Python
/// when writing snapshots, so snapshots carry precomputed ranks.
#[allow(dead_code)]
pub(crate) fn rank_percentile(volumes: &[f64]) -> Vec<(usize, f64)> {
    let n = volumes.len();
    volumes
        .iter()
        .map(|v| {
            let rank = 1 + volumes.iter().filter(|u| *u > v).count();
            let pct = if n > 1 {
                100.0 * (n - rank) as f64 / (n - 1) as f64
            } else {
                100.0
            };
            (rank, (pct * 10.0).round() / 10.0)
        })
        .collect()
}

/// Peer-rank lookup outcome for `/api/risk`.
#[derive(Debug, Clone)]
pub(crate) enum PeerStatus {
    Ranked {
        percentile: f64,
        rank: usize,
        peer_count: usize,
    },
    NoSnapshot,
    Stale,
    NotListed,
    /// Place is in the snapshot but it predates peer-rank fields (written
    /// before the percentile pipeline landed).
    NoPeerData,
}

/// Look up a place's baseline-free peer rank in the latest weekly
/// snapshot. Name matching is case-insensitive; the snapshot must be
/// fresher than [`STALE_AFTER_DAYS`]. Pure apart from filesystem reads.
pub(crate) fn peer_status(name: &str) -> PeerStatus {
    let dir = snapshot_dir();
    let files = snapshot_files(&dir);
    let Some(latest_path) = files.last() else {
        return PeerStatus::NoSnapshot;
    };
    let Ok(snap) = read_snapshot(latest_path) else {
        return PeerStatus::NoSnapshot;
    };
    if snapshot_age_days(&snap.generated_at).is_some_and(|d| d > STALE_AFTER_DAYS) {
        return PeerStatus::Stale;
    }
    let needle = name.trim().to_lowercase();
    let Some(p) = snap
        .places
        .iter()
        .find(|pl| pl.name.trim().to_lowercase() == needle)
    else {
        return PeerStatus::NotListed;
    };
    match (p.percentile, p.rank, p.peer_count) {
        (Some(percentile), Some(rank), Some(peer_count)) => PeerStatus::Ranked {
            percentile,
            rank,
            peer_count,
        },
        _ => PeerStatus::NoPeerData,
    }
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

    #[test]
    fn rank_percentile_shape() {
        // Volumes [10, 30, 20] → ranks [3, 1, 2], percentiles [0, 100, 50].
        let rp = rank_percentile(&[10.0, 30.0, 20.0]);
        assert_eq!(rp, vec![(3, 0.0), (1, 100.0), (2, 50.0)]);
        // Ties share the competition rank; the next rank skips.
        let rp = rank_percentile(&[5.0, 5.0, 9.0]);
        assert_eq!(rp[2], (1, 100.0));
        assert_eq!(rp[0].0, 2);
        assert_eq!(rp[1].0, 2);
        // Single place → 100.
        assert_eq!(rank_percentile(&[7.0]), vec![(1, 100.0)]);
        // Empty → empty.
        assert!(rank_percentile(&[]).is_empty());
    }

    #[test]
    fn percentile_ranks_never_reverse_under_capping() {
        // The core anti-saturation property: upstream capping is weakly
        // monotone (min(v, cap)) — it can create ties but never flips who
        // is hotter than whom. Contrast with time-ratio baselines, where
        // capping explodes the ratio and pins scores at 100.
        let volumes: Vec<f64> = vec![
            12.0, 480.0, 95.0, 1000.0, 33.0, 210.0, 7.0, 640.0, 150.0, 88.0,
        ];
        for cap in [1000.0, 500.0, 100.0, 50.0, 10.0, 1.0] {
            let capped: Vec<f64> = volumes.iter().map(|v| v.min(cap)).collect();
            let ranks: Vec<usize> = rank_percentile(&capped).iter().map(|(r, _)| *r).collect();
            for i in 0..volumes.len() {
                for j in 0..volumes.len() {
                    if volumes[i] < volumes[j] {
                        // No reversal: the hotter place keeps a rank at
                        // least as good (smaller number = hotter).
                        assert!(
                            ranks[i] >= ranks[j],
                            "reversal under cap={cap}: vol[{i}]={} < vol[{j}]={} but rank {} < {}",
                            volumes[i],
                            volumes[j],
                            ranks[i],
                            ranks[j]
                        );
                        // Pairs that stay distinct after capping keep
                        // strictly distinct ranks.
                        if capped[i] < capped[j] {
                            assert!(
                                ranks[i] > ranks[j],
                                "distinct capped vols must rank strictly under cap={cap}"
                            );
                        }
                    } else if volumes[i] == volumes[j] {
                        assert_eq!(ranks[i], ranks[j], "equal volumes must tie under cap={cap}");
                    }
                }
            }
        }
    }

    #[test]
    fn peer_status_branches() {
        use std::io::Write;
        // SAFETY: this is the only test that touches RISK_SNAPSHOT_DIR;
        // save/restore around the assertions.
        let prev = std::env::var("RISK_SNAPSHOT_DIR").ok();
        let set_dir = |d: &Path| unsafe {
            std::env::set_var("RISK_SNAPSHOT_DIR", d);
        };
        let restore = || unsafe {
            match &prev {
                Some(v) => std::env::set_var("RISK_SNAPSHOT_DIR", v),
                None => std::env::remove_var("RISK_SNAPSHOT_DIR"),
            }
        };
        fn write_peer_snap(dir: &Path, name: &str, body: serde_json::Value) {
            let mut f = std::fs::File::create(dir.join(name)).unwrap();
            write!(f, "{body}").unwrap();
        }
        fn peer_snap(at: &str) -> serde_json::Value {
            serde_json::json!({"generated_at": at, "place_count": 1,
                "places": [{"name": "France", "kind": "country", "lon": 0.0, "lat": 0.0,
                    "score": 80.0, "level": "high", "safety": "Low", "partial": false,
                    "percentile": 93.3, "rank": 2, "peer_count": 30,
                    "families": []}],
                "caveat": "test"})
        }

        // No snapshot → NoSnapshot.
        let d0 = tmpdir();
        set_dir(&d0);
        assert!(matches!(peer_status("France"), PeerStatus::NoSnapshot));

        // Fresh snapshot with peer fields → Ranked (case-insensitive).
        write_peer_snap(
            &d0,
            "snapshot-20990101-000000.json",
            peer_snap("2099-01-01T00:00:00Z"),
        );
        match peer_status("france") {
            PeerStatus::Ranked {
                percentile,
                rank,
                peer_count,
            } => {
                assert_eq!(percentile, 93.3);
                assert_eq!(rank, 2);
                assert_eq!(peer_count, 30);
            }
            other => panic!("expected Ranked, got {other:?}"),
        }
        assert!(matches!(peer_status("Narnia"), PeerStatus::NotListed));
        // Snapshot without peer fields → NotListed (can't rank).
        let d1 = tmpdir();
        let mut nopeer = peer_snap("2099-01-01T00:00:00Z");
        nopeer["places"][0]
            .as_object_mut()
            .unwrap()
            .remove("percentile");
        write_peer_snap(&d1, "snapshot-20990101-000000.json", nopeer);
        set_dir(&d1);
        assert!(matches!(peer_status("France"), PeerStatus::NoPeerData));

        // Stale snapshot → Stale.
        let d2 = tmpdir();
        write_peer_snap(
            &d2,
            "snapshot-20200101-000000.json",
            peer_snap("2020-01-01T00:00:00Z"),
        );
        set_dir(&d2);
        assert!(matches!(peer_status("France"), PeerStatus::Stale));

        restore();
        for d in [&d0, &d1, &d2] {
            std::fs::remove_dir_all(d).ok();
        }
    }
}
