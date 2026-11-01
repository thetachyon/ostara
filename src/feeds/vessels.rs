//! Vessel feed: AIS maritime tracking (keyed, Phase-1 deferred).
//!
//! RESEARCH OUTCOME (2026-11-01, verified against provider docs):
//!
//! - **AISStream** (`aisstream.io`) — the free, keyed option named in the
//!   config docs — is **WebSocket-only** (`wss://stream.aisstream.io/v0/stream`):
//!   clients send a JSON subscription (`APIKey`, `BoundingBoxes`, optional
//!   `FiltersShipMMSI` / `FilterMessageTypes`) and the server pushes AIS
//!   messages. The provider's own documentation states there is **no REST
//!   API**. A REST polling feed cannot be built on it, so it is not a
//!   workable keyed REST source for this phase.
//! - **openwaters.io** — an emerging AISStream-compatible service with a
//!   WebSocket/SSE streaming API and token issuance (`POST /v1/keys`); also
//!   streaming-first, not REST polling.
//! - **MarineTraffic** — keyed REST API (PS-series endpoints), but paid
//!   plans only; no free tier suitable for keyless evaluation.
//! - **VesselFinder** — keyed REST API, paid.
//! - **AISHub** (`data.aishub.net/ws.php`) — keyed HTTP API returning JSON,
//!   free for stations that contribute an AIS feed; the closest thing to a
//!   workable keyed REST option, but access is contingent on feed
//!   contribution, which this project does not have.
//!
//! CONCLUSION: no workable keyed REST vessel source exists for Phase 1, so
//! `/api/vessels` answers 503 by design — both when `AIS_API_KEY` is unset
//! (mirroring OSIRIS's disabled-backend convention) and when it is set but
//! no REST adapter is wired yet. The intended future normalization shape
//! (`Vessel` / `VesselsResponse`) is defined below so a Phase-2 adapter
//! (AISHub REST, or a WebSocket consumer — which needs a WS client crate,
//! not currently in `Cargo.toml`) has a contract to target.
//!
//! UNVERIFIED: no AIS key or feed access was available; nothing here has
//! touched a live AIS upstream.

use std::time::Duration;

use axum::{
    extract::State,
    response::{IntoResponse, Response},
};
use serde::Serialize;

use crate::{
    config::Config,
    error::{ApiError, FeedError},
    routes::AppState,
};

/// Cache TTL (applies once a REST adapter is wired; AIS data is fast-moving).
pub const TTL: Duration = Duration::from_secs(300);
pub const FEED: &str = "ais";

/// Intended normalization target for a future AIS adapter: one vessel fix.
#[derive(Debug, Serialize)]
pub struct Vessel {
    pub mmsi: u64,
    pub name: Option<String>,
    pub lat: f64,
    pub lon: f64,
    pub sog_knots: Option<f64>,
    pub cog_deg: Option<f64>,
    pub heading_deg: Option<u16>,
    pub timestamp: String,
}

/// Intended normalization target for a future AIS adapter.
#[derive(Debug, Serialize)]
pub struct VesselsResponse {
    pub count: usize,
    pub generated_at: String,
    pub source: String,
    pub vessels: Vec<Vessel>,
}

/// Short detail string for the health endpoint.
pub async fn probe_detail(_client: &reqwest::Client, config: &Config) -> Result<String, FeedError> {
    match &config.ais_api_key {
        None => Err(FeedError::new(
            FEED,
            "AIS_API_KEY unset — no keyless AIS REST source exists (AISStream is websocket-only)",
        )),
        Some(_) => Err(FeedError::new(
            FEED,
            "AIS REST adapter not wired in Phase 1 (see module docs for evaluated providers)",
        )),
    }
}

// ---------------------------------------------------------------------------
// GET /api/vessels
// ---------------------------------------------------------------------------

pub async fn handler(State(state): State<AppState>) -> Result<impl IntoResponse, ApiError> {
    // Always 503 in Phase 1 (see module docs); the turbofish pins the
    // opaque `impl IntoResponse` to `Response` for type inference.
    Err::<Response, ApiError>(match &state.config.ais_api_key {
        None => ApiError::service_unavailable(
            "vessels feed disabled",
            "set AIS_API_KEY to enable /api/vessels; no keyless vessel-AIS REST feed exists (AISStream is websocket-only). Keyed REST options for a future adapter: MarineTraffic, VesselFinder, AISHub — see src/feeds/vessels.rs",
        ),
        Some(_) => ApiError::service_unavailable(
            "vessels feed not implemented",
            "AIS_API_KEY is set but no AIS REST adapter is wired in Phase 1 (AISStream, the free keyed option, is websocket-only with no REST API). Evaluated providers and the intended normalization shape are documented in src/feeds/vessels.rs",
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn probe_detail_reports_unset_key() {
        let config = Config::test_default();
        let client = reqwest::Client::new();
        let err = probe_detail(&client, &config).await.unwrap_err();
        assert!(err.detail.contains("AIS_API_KEY unset"));
    }

    #[test]
    fn future_shape_serializes() {
        // The contract a Phase-2 adapter must satisfy.
        let r = VesselsResponse {
            count: 1,
            generated_at: "2026-11-01T00:00:00.000Z".to_string(),
            source: "aishub".to_string(),
            vessels: vec![Vessel {
                mmsi: 366999990,
                name: Some("EXAMPLE".to_string()),
                lat: 25.76,
                lon: -80.19,
                sog_knots: Some(12.5),
                cog_deg: Some(90.0),
                heading_deg: Some(90),
                timestamp: "2026-11-01T00:00:00.000Z".to_string(),
            }],
        };
        let body = serde_json::to_string(&r).unwrap();
        assert!(body.contains("\"count\":1"));
        assert!(body.contains("366999990"));
    }
}
