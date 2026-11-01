//! Vessel feed: AIS maritime tracking via AISStream.
//!
//! Architecture (verified against <https://aisstream.io/documentation>,
//! 2026-11-01):
//!
//! - AISStream is **WebSocket-only** (`wss://stream.aisstream.io/v0/stream`)
//!   — no REST API exists, so a polling feed is impossible. Instead this
//!   module keeps one long-lived subscription in a background task and
//!   serves a rolling in-memory snapshot (`VesselStream`) from
//!   `/api/vessels`. This is the sanctioned pattern: AISStream forbids
//!   direct browser connections, so the key stays server-side and the
//!   server proxies positions to clients.
//! - Auth: free API key (signup at <https://aisstream.io>), sent inside the
//!   JSON subscription message. One subscription must arrive within 3s of
//!   connect; resending replaces the configuration.
//! - Subscription: a single global bounding box `[[[90,-180],[-90,180]]]`
//!   (boxes are REQUIRED; corners are `[lat, lon]`) and message types
//!   `PositionReport`, `StandardClassBPositionReport`,
//!   `ExtendedClassBPositionReport`, `LongRangeAisBroadcastMessage`,
//!   `ShipStaticData`, `StaticDataReport`. Position messages carry
//!   lat/lon/speed/course; static messages carry name/type — merged per
//!   MMSI. The server pushes binary WebSocket frames of UTF-8 JSON.
//! - The snapshot is **best-effort sampled**: a global subscription is a
//!   firehose and AISStream drops messages for slow consumers (documented).
//!   We consume as fast as tokio lets us into a per-MMSI latest-fix map;
//!   fixes older than [`STALE_AFTER`] are pruned. No SLA is offered by the
//!   provider; reconnects use exponential backoff (5s → 5min) with jitter,
//!   per provider guidance. At most one connection is ever open (the
//!   account limit is 3).
//! - Compression: tokio-tungstenite does not implement `permessage-deflate`,
//!   so we connect uncompressed. Since September 2026 AISStream applies
//!   per-user bandwidth limits to uncompressed connections and drops
//!   excess messages — acceptable for a sampled snapshot; the health
//!   endpoint reports stream state honestly.
//!
//! Graceful degradation: with `AIS_API_KEY` unset no task is spawned and
//! `/api/vessels` answers 503 (mirroring OSIRIS's disabled-backend
//! convention), exactly like the other keyed feeds.

use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
    time::{Duration, Instant},
};

use axum::{
    extract::{Query, State},
    response::Response,
};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};

use crate::{
    error::{ApiError, FeedError},
    routes::AppState,
    time::now_iso8601,
};

/// Cache TTL for the serialized `/api/vessels` response. The underlying
/// snapshot is live-pushed, so this only avoids re-serializing thousands
/// of vessels per request; 60s mirrors the fast-moving-feed convention.
pub const TTL: Duration = Duration::from_secs(60);
pub const FEED: &str = "aisstream";

const WS_URL: &str = "wss://stream.aisstream.io/v0/stream";
/// Fixes older than this are pruned from the snapshot.
const STALE_AFTER: Duration = Duration::from_secs(30 * 60);
const PRUNE_INTERVAL: Duration = Duration::from_secs(60);
const MAX_BACKOFF: Duration = Duration::from_secs(300);

/// Normalized vessel fix served by `/api/vessels`. Field names match what
/// the web UI already renders (`web/layers.js`, `web/feeds.js`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Vessel {
    pub mmsi: u64,
    pub name: Option<String>,
    pub vessel_type: Option<String>,
    pub latitude: f64,
    pub longitude: f64,
    /// Speed over ground, knots.
    pub speed_knots: Option<f64>,
    pub cog_deg: Option<f64>,
    pub heading_deg: Option<u16>,
    /// Server receipt time (ISO-8601): AIS position reports carry only a
    /// UTC-second-of-minute, not a full timestamp.
    pub timestamp: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct VesselsResponse {
    pub count: usize,
    pub generated_at: String,
    pub source: String,
    pub vessels: Vec<Vessel>,
}

#[derive(Debug, Clone, Default)]
struct StaticInfo {
    name: Option<String>,
    vessel_type: Option<String>,
}

#[derive(Debug, Clone)]
struct VesselRecord {
    vessel: Vessel,
    received_at: Instant,
}

#[derive(Debug, Default)]
struct StreamInner {
    vessels: HashMap<u64, VesselRecord>,
    /// Name/type from static messages, merged into position records by
    /// MMSI (a static message may arrive before any position report).
    static_info: HashMap<u64, StaticInfo>,
    last_msg_at: Option<Instant>,
    connected: bool,
}

/// Shared handle to the background AIS stream. `Clone` is cheap (Arc).
/// Construct with [`VesselStream::new`]; a `None` key yields a disabled
/// stream that answers 503 and never spawns a task.
#[derive(Clone)]
pub struct VesselStream {
    inner: Arc<RwLock<StreamInner>>,
    enabled: bool,
}

impl VesselStream {
    /// Build the stream handle. When `api_key` is `Some`, spawns the
    /// background consumer task — requires a live tokio runtime (true in
    /// `main` and in `#[tokio::test]` tests; never call with a key outside
    /// one).
    pub fn new(api_key: Option<String>) -> Self {
        let inner = Arc::new(RwLock::new(StreamInner::default()));
        let stream = Self {
            inner,
            enabled: api_key.is_some(),
        };
        if let Some(key) = api_key {
            let task_stream = stream.clone();
            tokio::spawn(run_forever(key, task_stream));
        }
        stream
    }

    /// Disabled handle for tests that must not touch the network.
    #[cfg(test)]
    fn disabled_for_test() -> Self {
        Self {
            inner: Arc::new(RwLock::new(StreamInner::default())),
            enabled: false,
        }
    }

    /// Enabled handle with an empty snapshot and no background task, for
    /// route-contract tests (no network, no key needed). Not for production
    /// use.
    #[doc(hidden)]
    pub fn enabled_for_test() -> Self {
        Self {
            inner: Arc::new(RwLock::new(StreamInner::default())),
            enabled: true,
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    fn set_connected(&self, connected: bool) {
        if let Ok(mut inner) = self.inner.write() {
            inner.connected = connected;
        }
    }

    fn prune(&self) {
        if let Ok(mut inner) = self.inner.write() {
            let now = Instant::now();
            inner
                .vessels
                .retain(|_, r| now.duration_since(r.received_at) < STALE_AFTER);
        }
    }

    /// Rolling snapshot: latest fix per MMSI, stale entries pruned,
    /// deterministic MMSI order.
    pub fn snapshot(&self) -> Vec<Vessel> {
        self.prune();
        let inner = match self.inner.read() {
            Ok(g) => g,
            Err(_) => return Vec::new(),
        };
        let mut out: Vec<Vessel> = inner.vessels.values().map(|r| r.vessel.clone()).collect();
        out.sort_by_key(|v| v.mmsi);
        out
    }

    pub fn len(&self) -> usize {
        self.inner.read().map(|i| i.vessels.len()).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn status(&self) -> StreamStatus {
        let inner = match self.inner.read() {
            Ok(g) => g,
            Err(_) => {
                return StreamStatus {
                    connected: false,
                    vessel_count: 0,
                    last_msg_age_s: None,
                };
            }
        };
        StreamStatus {
            connected: inner.connected,
            vessel_count: inner.vessels.len(),
            last_msg_age_s: inner
                .last_msg_at
                .map(|t| Instant::now().duration_since(t).as_secs()),
        }
    }

    #[cfg(test)]
    fn inject_envelope(&self, text: &str, now: Instant) {
        if let Ok(mut inner) = self.inner.write() {
            apply_envelope(&mut inner, text, now);
        }
    }
}

#[derive(Debug)]
pub struct StreamStatus {
    pub connected: bool,
    pub vessel_count: usize,
    pub last_msg_age_s: Option<u64>,
}

// ---------------------------------------------------------------------------
// AISStream wire protocol
// ---------------------------------------------------------------------------

fn subscription_message(api_key: &str) -> String {
    serde_json::json!({
        "APIKey": api_key,
        // Corners are [lat, lon]; one world-spanning box (required field).
        "BoundingBoxes": [[[90.0, -180.0], [-90.0, 180.0]]],
        "FilterMessageTypes": [
            "PositionReport",
            "StandardClassBPositionReport",
            "ExtendedClassBPositionReport",
            "LongRangeAisBroadcastMessage",
            "ShipStaticData",
            "StaticDataReport",
        ],
    })
    .to_string()
}

/// Clean an AIS ship name: the protocol pads empty names with spaces or
/// "@", so blank-ish values must not surface as real names.
fn clean_name(s: &str) -> Option<String> {
    let t = s.trim().trim_matches('@').trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

fn json_str(v: &serde_json::Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|k| v.get(k).and_then(|x| x.as_str()).and_then(clean_name))
}

/// Apply one AISStream envelope (UTF-8 JSON) to the snapshot. Pure apart
/// from the map write: unknown shapes are ignored, never panicked on.
///
/// `ShipStaticData` / `StaticDataReport` field names are UNVERIFIED
/// (2026-11-01): the docs' schema explorer is JS-driven and no key was
/// available to capture a live static message, so several candidate keys
/// are tried defensively.
fn apply_envelope(inner: &mut StreamInner, text: &str, now: Instant) {
    let v: serde_json::Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(_) => return,
    };
    let msg_type = v.get("MessageType").and_then(|m| m.as_str()).unwrap_or("");
    match msg_type {
        "SubscriptionConfirmation" | "" => return,
        "Error" => {
            tracing::warn!(feed = FEED, "aisstream error message: {text:.200}");
            return;
        }
        _ => {}
    }
    let meta = match v.get("MetaData") {
        Some(m) => m,
        None => return,
    };
    let mmsi = match meta.get("MMSI").and_then(|m| m.as_u64()) {
        Some(m) if m != 0 => m,
        _ => return,
    };
    inner.last_msg_at = Some(now);

    let body = v.get("Message").and_then(|m| m.get(msg_type));

    match msg_type {
        "PositionReport"
        | "StandardClassBPositionReport"
        | "ExtendedClassBPositionReport"
        | "LongRangeAisBroadcastMessage" => {
            // Docs put lat/lon in MetaData; fall back to the message body.
            let lat = meta.get("Latitude").and_then(|x| x.as_f64()).or_else(|| {
                body.and_then(|b| b.get("Latitude"))
                    .and_then(|x| x.as_f64())
            });
            let lon = meta.get("Longitude").and_then(|x| x.as_f64()).or_else(|| {
                body.and_then(|b| b.get("Longitude"))
                    .and_then(|x| x.as_f64())
            });
            let (lat, lon) = match (lat, lon) {
                (Some(a), Some(o)) => (a, o),
                _ => return,
            };
            if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lon) {
                return;
            }
            let static_info = inner.static_info.get(&mmsi).cloned().unwrap_or_default();
            let name = meta
                .get("ShipName")
                .and_then(|s| s.as_str())
                .and_then(clean_name)
                .or(static_info.name);
            let rec = inner.vessels.entry(mmsi).or_insert_with(|| VesselRecord {
                vessel: Vessel {
                    mmsi,
                    name: None,
                    vessel_type: None,
                    latitude: lat,
                    longitude: lon,
                    speed_knots: None,
                    cog_deg: None,
                    heading_deg: None,
                    timestamp: now_iso8601(),
                },
                received_at: now,
            });
            // A later static message can still fill name/type (see below).
            if rec.vessel.name.is_none() {
                rec.vessel.name =
                    name.or_else(|| inner.static_info.get(&mmsi).and_then(|s| s.name.clone()));
            } else if let Some(n) = meta
                .get("ShipName")
                .and_then(|s| s.as_str())
                .and_then(clean_name)
            {
                rec.vessel.name = Some(n);
            }
            if rec.vessel.vessel_type.is_none() {
                rec.vessel.vessel_type = static_info.vessel_type;
            }
            if let Some(b) = body {
                rec.vessel.speed_knots = b.get("Sog").and_then(|x| x.as_f64());
                rec.vessel.cog_deg = b.get("Cog").and_then(|x| x.as_f64());
                rec.vessel.heading_deg = b
                    .get("TrueHeading")
                    .and_then(|x| x.as_u64())
                    .and_then(|h| u16::try_from(h).ok())
                    // 511 = "not available" per the AIS spec.
                    .filter(|h| *h != 511);
            }
            rec.vessel.latitude = lat;
            rec.vessel.longitude = lon;
            rec.vessel.timestamp = now_iso8601();
            rec.received_at = now;
        }
        "ShipStaticData" | "StaticDataReport" => {
            let b = match body {
                Some(b) => b,
                None => return,
            };
            let info = StaticInfo {
                name: json_str(b, &["Name", "ShipName", "name"]),
                vessel_type: json_str(b, &["ShipType", "Type", "shiptype"]),
            };
            if info.name.is_none() && info.vessel_type.is_none() {
                return;
            }
            inner.static_info.insert(mmsi, info.clone());
            // Backfill a position record that arrived before its static data.
            if let Some(rec) = inner.vessels.get_mut(&mmsi) {
                if rec.vessel.name.is_none() {
                    rec.vessel.name = info.name;
                }
                if rec.vessel.vessel_type.is_none() {
                    rec.vessel.vessel_type = info.vessel_type;
                }
            }
        }
        _ => {}
    }
}

/// Connect, subscribe, and consume until the stream ends. Returns on clean
/// close or any failure; the caller reconnects with backoff.
async fn connect_and_consume(api_key: &str, stream: &VesselStream) -> Result<(), String> {
    let (ws, _) = tokio_tungstenite::connect_async(WS_URL)
        .await
        .map_err(|e| format!("websocket connect failed: {e}"))?;
    let (mut sink, mut source) = ws.split();
    // The subscription must arrive within 3s of connect (provider rule).
    sink.send(tokio_tungstenite::tungstenite::Message::Text(
        subscription_message(api_key).into(),
    ))
    .await
    .map_err(|e| format!("subscription send failed: {e}"))?;
    stream.set_connected(true);
    tracing::info!(feed = FEED, "aisstream subscribed (global bbox)");

    let mut prune_tick = tokio::time::interval(PRUNE_INTERVAL);
    loop {
        tokio::select! {
            msg = source.next() => {
                match msg {
                    Some(Ok(tokio_tungstenite::tungstenite::Message::Binary(b))) => {
                        let text = String::from_utf8_lossy(&b);
                        if let Ok(mut inner) = stream.inner.write() {
                            apply_envelope(&mut inner, &text, Instant::now());
                        }
                    }
                    Some(Ok(tokio_tungstenite::tungstenite::Message::Text(t))) => {
                        if let Ok(mut inner) = stream.inner.write() {
                            apply_envelope(&mut inner, &t, Instant::now());
                        }
                    }
                    // Ping/Pong are answered automatically by tungstenite.
                    Some(Ok(_)) => {}
                    Some(Err(e)) => return Err(format!("ws read failed: {e}")),
                    None => return Ok(()),
                }
            }
            _ = prune_tick.tick() => {
                stream.prune();
            }
        }
    }
}

async fn run_forever(api_key: String, stream: VesselStream) {
    let mut backoff = Duration::from_secs(5);
    loop {
        match connect_and_consume(&api_key, &stream).await {
            Ok(()) => tracing::warn!(feed = FEED, "aisstream closed cleanly; reconnecting"),
            Err(e) => tracing::warn!(feed = FEED, "aisstream ended ({e}); reconnecting"),
        }
        stream.set_connected(false);
        // Jitter without a rand dependency: sub-second nanos of now.
        let jitter = Duration::from_millis(
            (std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0)
                % 1000) as u64,
        );
        tokio::time::sleep(backoff + jitter).await;
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

// ---------------------------------------------------------------------------
// Snapshot → response
// ---------------------------------------------------------------------------

/// Filter + limit a snapshot. Pure: unit-tested without networking.
pub(crate) fn filter_and_limit(
    mut vessels: Vec<Vessel>,
    bbox: Option<(f64, f64, f64, f64)>,
    limit: usize,
) -> Vec<Vessel> {
    if let Some((minlon, minlat, maxlon, maxlat)) = bbox {
        vessels.retain(|v| {
            v.longitude >= minlon
                && v.longitude <= maxlon
                && v.latitude >= minlat
                && v.latitude <= maxlat
        });
    }
    vessels.truncate(limit);
    vessels
}

/// Short detail string for the health endpoint.
pub fn probe_detail(stream: &VesselStream) -> Result<String, FeedError> {
    if !stream.is_enabled() {
        return Err(FeedError::new(FEED, "disabled: AIS_API_KEY unset"));
    }
    let st = stream.status();
    match (st.connected, st.last_msg_age_s) {
        (true, Some(age)) => Ok(format!(
            "{} vessels in snapshot, last message {}s ago",
            st.vessel_count, age
        )),
        _ => Err(FeedError::new(
            FEED,
            "stream connecting (no messages received yet)",
        )),
    }
}

// ---------------------------------------------------------------------------
// GET /api/vessels
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct VesselsQuery {
    /// Bounding box "minlon,minlat,maxlon,maxlat", e.g. "-125,32,-114,42".
    pub bbox: Option<String>,
    /// Max vessels returned. Default 250, capped at 1000.
    pub limit: Option<usize>,
}

pub async fn handler(
    State(state): State<AppState>,
    Query(q): Query<VesselsQuery>,
) -> Result<Response, ApiError> {
    let stream = state.vessels.clone();
    if !stream.is_enabled() {
        return Err(ApiError::service_unavailable(
            "vessels feed disabled",
            "set AIS_API_KEY (free signup at https://aisstream.io) to enable /api/vessels; AISStream is websocket-only, so the server keeps a live subscription and serves the rolling snapshot",
        ));
    }
    let limit = q.limit.unwrap_or(250).clamp(1, 1000);
    let bbox = match &q.bbox {
        Some(b) => Some(crate::routes::parse_bbox(b)?),
        None => None,
    };
    let key = format!(
        "vessels:bbox={}:limit={limit}",
        q.bbox.as_deref().unwrap_or("world")
    );
    let (body, hit) = crate::feeds::cached_bytes(&state, &key, TTL, || async move {
        let vessels = filter_and_limit(stream.snapshot(), bbox, limit);
        Ok::<_, FeedError>(VesselsResponse {
            count: vessels.len(),
            generated_at: now_iso8601(),
            source: FEED.to_string(),
            vessels,
        })
    })
    .await?;
    Ok(crate::feeds::json_bytes(body, hit))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Doc-derived sample envelopes (shapes from
    /// <https://aisstream.io/documentation>; NOT a live capture —
    /// ShipStaticData field names are unverified).
    const FIXTURE: &str = include_str!("../../benches/fixtures/aisstream.json");

    fn test_stream() -> VesselStream {
        VesselStream::disabled_for_test()
    }

    fn envelopes() -> Vec<String> {
        let v: serde_json::Value = serde_json::from_str(FIXTURE).unwrap();
        v.as_array()
            .unwrap()
            .iter()
            .map(|e| e.to_string())
            .collect()
    }

    #[test]
    fn parses_position_report_envelope() {
        let s = test_stream();
        let now = Instant::now();
        // Fixture[0]: PositionReport for MMSI 368207620.
        s.inject_envelope(&envelopes()[0], now);
        let snap = s.snapshot();
        assert_eq!(snap.len(), 1);
        let v = &snap[0];
        assert_eq!(v.mmsi, 368207620);
        assert_eq!(v.name.as_deref(), Some("EXAMPLE VESSEL"));
        assert!((v.latitude - 25.7617).abs() < 1e-9);
        assert!((v.longitude - -80.1918).abs() < 1e-9);
        assert!((v.speed_knots.unwrap() - 12.4).abs() < 1e-9);
        assert!((v.cog_deg.unwrap() - 86.7).abs() < 1e-9);
        assert_eq!(v.heading_deg, Some(87));
    }

    #[test]
    fn static_data_merges_regardless_of_order() {
        // Static first, then position.
        let s = test_stream();
        let now = Instant::now();
        let envs = envelopes();
        s.inject_envelope(&envs[2], now); // ShipStaticData, MMSI 211111111
        s.inject_envelope(&envs[1], now); // PositionReport, MMSI 211111111
        let v = &s.snapshot()[0];
        assert_eq!(v.name.as_deref(), Some("NORDIC STAR"));
        assert_eq!(v.vessel_type.as_deref(), Some("Cargo"));

        // Position first, then static (backfill path).
        let s2 = test_stream();
        s2.inject_envelope(&envs[1], now);
        s2.inject_envelope(&envs[2], now);
        let v2 = &s2.snapshot()[0];
        assert_eq!(v2.name.as_deref(), Some("NORDIC STAR"));
        assert_eq!(v2.vessel_type.as_deref(), Some("Cargo"));
    }

    #[test]
    fn garbage_and_unknown_shapes_are_ignored() {
        let s = test_stream();
        let now = Instant::now();
        s.inject_envelope("not json at all", now);
        s.inject_envelope(
            r#"{"MessageType":"SubscriptionConfirmation","Message":{}}"#,
            now,
        );
        s.inject_envelope(r#"{"MessageType":"PositionReport"}"#, now); // no MetaData
        s.inject_envelope(
            r#"{"MessageType":"PositionReport","MetaData":{"MMSI":0,"Latitude":1.0,"Longitude":2.0}}"#,
            now,
        );
        // Out-of-range coordinates are dropped.
        s.inject_envelope(
            r#"{"MessageType":"PositionReport","MetaData":{"MMSI":123,"Latitude":91.0,"Longitude":2.0}}"#,
            now,
        );
        assert!(s.snapshot().is_empty());
        // Heading 511 ("not available") normalizes to None.
        s.inject_envelope(&envelopes()[3], now);
        let v = &s.snapshot()[0];
        assert_eq!(v.mmsi, 999999999);
        assert_eq!(v.heading_deg, None);
    }

    #[test]
    fn stale_records_are_pruned() {
        let s = test_stream();
        // checked_sub: Instant arithmetic panics on underflow on machines
        // with short uptimes.
        let old = Instant::now()
            .checked_sub(STALE_AFTER + Duration::from_secs(1))
            .unwrap_or_else(Instant::now);
        s.inject_envelope(&envelopes()[0], old);
        // Only assert staleness when the clock actually went backwards far
        // enough; otherwise the record is legitimately fresh.
        if Instant::now().duration_since(old) > STALE_AFTER {
            assert!(s.snapshot().is_empty());
        }
        s.inject_envelope(&envelopes()[0], Instant::now());
        assert_eq!(s.snapshot().len(), 1);
    }

    #[test]
    fn snapshot_order_is_deterministic() {
        let s = test_stream();
        let now = Instant::now();
        for e in envelopes().iter().take(4) {
            s.inject_envelope(e, now);
        }
        let snap = s.snapshot();
        let mmsis: Vec<u64> = snap.iter().map(|v| v.mmsi).collect();
        let mut sorted = mmsis.clone();
        sorted.sort_unstable();
        assert_eq!(mmsis, sorted);
    }

    #[test]
    fn filter_and_limit_behaves() {
        let mk = |mmsi: u64, lat: f64, lon: f64| Vessel {
            mmsi,
            name: None,
            vessel_type: None,
            latitude: lat,
            longitude: lon,
            speed_knots: None,
            cog_deg: None,
            heading_deg: None,
            timestamp: "t".to_string(),
        };
        let vs = vec![mk(1, 25.0, -80.0), mk(2, 60.0, 10.0), mk(3, 25.5, -80.5)];
        // Florida bbox keeps 1 and 3.
        let out = filter_and_limit(vs.clone(), Some((-81.0, 24.0, -79.0, 26.0)), 10);
        assert_eq!(out.len(), 2);
        // Limit truncates.
        let out = filter_and_limit(vs.clone(), None, 2);
        assert_eq!(out.len(), 2);
        // Empty bbox match.
        let out = filter_and_limit(vs, Some((0.0, 0.0, 1.0, 1.0)), 10);
        assert!(out.is_empty());
    }

    #[test]
    fn subscription_message_shape() {
        let msg: serde_json::Value = serde_json::from_str(&subscription_message("KEY123")).unwrap();
        assert_eq!(msg["APIKey"], "KEY123");
        let boxes = msg["BoundingBoxes"].as_array().unwrap();
        assert_eq!(boxes.len(), 1);
        let types = msg["FilterMessageTypes"].as_array().unwrap();
        let names: Vec<&str> = types.iter().filter_map(|t| t.as_str()).collect();
        assert!(names.contains(&"PositionReport"));
        assert!(names.contains(&"ShipStaticData"));
    }

    #[test]
    fn probe_detail_disabled_without_key() {
        let s = test_stream();
        let err = probe_detail(&s).unwrap_err();
        assert!(err.detail.contains("AIS_API_KEY unset"));
    }

    #[test]
    fn probe_detail_connecting_with_key_but_no_messages() {
        let s = VesselStream {
            inner: Arc::new(RwLock::new(StreamInner::default())),
            enabled: true,
        };
        let err = probe_detail(&s).unwrap_err();
        assert!(err.detail.contains("connecting"));
    }

    #[test]
    fn response_shape_serializes() {
        let r = VesselsResponse {
            count: 1,
            generated_at: "2026-11-01T00:00:00.000Z".to_string(),
            source: FEED.to_string(),
            vessels: vec![Vessel {
                mmsi: 366999990,
                name: Some("EXAMPLE".to_string()),
                vessel_type: Some("Cargo".to_string()),
                latitude: 25.76,
                longitude: -80.19,
                speed_knots: Some(12.5),
                cog_deg: Some(90.0),
                heading_deg: Some(90),
                timestamp: "2026-11-01T00:00:00.000Z".to_string(),
            }],
        };
        let body = serde_json::to_string(&r).unwrap();
        assert!(body.contains("\"count\":1"));
        assert!(body.contains("366999990"));
        assert!(body.contains("\"source\":\"aisstream\""));
    }
}
