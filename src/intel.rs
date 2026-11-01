//! SSE intel stream: `GET /api/intel/stream`.
//!
//! Server-sent events carrying a compact intel digest every 15 s — feed
//! counts plus auto-extracted highlights (M6+ quakes, emergency-squawk
//! aircraft). This is a thin convenience layer over the feed endpoints, not
//! the OI assist/forecast engine (explicitly out of scope per PARITY.md).
//!
//! A failing feed contributes nothing to a digest; the stream itself never
//! errors. Clients reconnect with the browser's native `EventSource`
//! retry behavior.

use std::convert::Infallible;
use std::time::Duration;

use axum::{
    Router,
    extract::State,
    response::sse::{Event, KeepAlive, Sse},
    routing::get,
};
use serde::Serialize;
use tokio_stream::{Stream, StreamExt, wrappers::IntervalStream};

use crate::feeds;
use crate::routes::AppState;
use crate::time::now_iso8601;

/// Mount the intel stream route.
pub fn routes() -> Router<AppState> {
    Router::new().route("/api/intel/stream", get(intel_stream))
}

#[derive(Debug, Serialize)]
struct Digest {
    generated_at: String,
    counts: DigestCounts,
    highlights: Vec<String>,
}

#[derive(Debug, Serialize, Default)]
struct DigestCounts {
    quakes: Option<usize>,
    flights: Option<usize>,
}

/// Build one digest: quake + military-flight snapshots, highlights extracted.
async fn build_digest(state: &AppState) -> Digest {
    let (quakes, flights) = tokio::join!(
        feeds::quakes::fetch(&state.client),
        feeds::flights::fetch_with_config(&state.client, &state.config, "mil", 250),
    );

    let mut counts = DigestCounts::default();
    let mut highlights = Vec::new();

    if let Ok(r) = quakes {
        counts.quakes = Some(r.count);
        for q in r.quakes.iter().filter_map(|q| {
            let m = q.magnitude?;
            (m >= 6.0).then_some((m, q))
        }) {
            let place = q.1.place.as_deref().unwrap_or("unknown location");
            highlights.push(format!("M{:.1} earthquake — {}", q.0, place));
        }
    }

    if let Ok(r) = flights {
        counts.flights = Some(r.count);
        for f in r.flights.iter().filter(|f| f.emergency.is_some()) {
            let code = f.emergency.as_deref().unwrap_or("?");
            let id = f.callsign.as_deref().unwrap_or(f.hex.as_str());
            highlights.push(format!("EMERGENCY squawk {code} — {id}"));
        }
    }

    Digest {
        generated_at: now_iso8601(),
        counts,
        highlights,
    }
}

/// SSE handler: a `hello` event, then a `digest` event every 15 s.
async fn intel_stream(
    State(state): State<AppState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let hello = tokio_stream::once(Ok::<_, Infallible>(
        Event::default()
            .event("hello")
            .data("{\"service\":\"ostara-intel\"}"),
    ));

    let ticks =
        IntervalStream::new(tokio::time::interval(Duration::from_secs(15))).then(move |_| {
            let state = state.clone();
            async move {
                let digest = build_digest(&state).await;
                let data = serde_json::to_string(&digest)
                    .unwrap_or_else(|_| "{\"error\":\"serialize failed\"}".to_string());
                Ok::<_, Infallible>(Event::default().event("digest").data(data))
            }
        });

    Sse::new(hello.chain(ticks)).keep_alive(KeepAlive::default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digest_serializes_with_empty_counts() {
        let d = Digest {
            generated_at: "t".to_string(),
            counts: DigestCounts::default(),
            highlights: vec![],
        };
        let v = serde_json::to_value(&d).unwrap();
        assert!(v["counts"]["quakes"].is_null());
        assert!(v["highlights"].as_array().unwrap().is_empty());
    }
}
