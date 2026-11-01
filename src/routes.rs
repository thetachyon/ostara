//! Axum route handlers.
//!
//! Every feed handler follows the same shape: check the TTL cache, on miss
//! fetch + normalize + serialize + store, then serve bytes. Cache hits cost
//! a hash lookup and a memcpy — no re-serialization.
//!
//! Upstream failure is normal: a failed feed returns 502
//! `{"error", "detail"}` and never takes down other feeds. `/api/stats`
//! goes further — it degrades per-feed instead of failing the request.

use std::{collections::HashMap, future::Future, pin::Pin, sync::Arc, time::Duration};

use axum::{
    Json,
    extract::{Query, State},
    http::{HeaderName, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};

use crate::{
    cache::TtlCache,
    error::{ApiError, FeedError},
    feeds,
    time::now_iso8601,
};

/// Shared server state: one pooled HTTP client, one TTL cache.
#[derive(Clone)]
pub struct AppState {
    pub client: reqwest::Client,
    pub cache: Arc<TtlCache>,
}

/// Fetch the normalized response (or reuse the cached bytes), serialize once,
/// store, and return the bytes plus a cache-hit flag.
async fn cached_bytes<T, Fut>(
    state: &AppState,
    key: &str,
    ttl: Duration,
    fetch: impl FnOnce() -> Fut,
) -> Result<(Vec<u8>, bool), ApiError>
where
    T: Serialize,
    Fut: Future<Output = Result<T, FeedError>>,
{
    if let Some(body) = state.cache.get(key) {
        return Ok((body, true));
    }
    let value = fetch().await?;
    let body = serde_json::to_vec(&value)
        .map_err(|e| ApiError::bad_gateway("serialize failed", e.to_string()))?;
    state.cache.put(key.to_string(), body.clone(), ttl);
    Ok((body, false))
}

fn json_bytes(body: Vec<u8>, hit: bool) -> Response {
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "application/json"),
            (
                HeaderName::from_static("x-cache"),
                if hit { "HIT" } else { "MISS" },
            ),
        ],
        body,
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// /healthz
// ---------------------------------------------------------------------------

pub async fn healthz() -> impl IntoResponse {
    Json(serde_json::json!({
        "status": "ok",
        "service": "ostara",
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

// ---------------------------------------------------------------------------
// /api/flights
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct FlightsQuery {
    /// "mil" (default) or "all".
    pub scope: Option<String>,
    /// Max aircraft returned. Default 250, capped at 1000.
    pub limit: Option<usize>,
}

pub async fn flights(
    State(state): State<AppState>,
    Query(q): Query<FlightsQuery>,
) -> Result<Response, ApiError> {
    let scope = q.scope.unwrap_or_else(|| "mil".to_string());
    if scope != "mil" && scope != "all" {
        return Err(ApiError::bad_request(
            "invalid scope",
            "scope must be \"mil\" or \"all\"",
        ));
    }
    let limit = q.limit.unwrap_or(250).clamp(1, 1000);
    let key = format!("flights:scope={scope}:limit={limit}");
    let client = state.client.clone();
    let (body, hit) = cached_bytes(&state, &key, feeds::flights::TTL, || async move {
        feeds::flights::fetch(&client, &scope, limit).await
    })
    .await?;
    Ok(json_bytes(body, hit))
}

// ---------------------------------------------------------------------------
// /api/quakes
// ---------------------------------------------------------------------------

pub async fn quakes(State(state): State<AppState>) -> Result<Response, ApiError> {
    let client = state.client.clone();
    let (body, hit) = cached_bytes(&state, "quakes", feeds::quakes::TTL, || async move {
        feeds::quakes::fetch(&client).await
    })
    .await?;
    Ok(json_bytes(body, hit))
}

// ---------------------------------------------------------------------------
// /api/weather
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct WeatherQuery {
    pub lat: Option<f64>,
    pub lon: Option<f64>,
}

pub async fn weather(
    State(state): State<AppState>,
    Query(q): Query<WeatherQuery>,
) -> Result<Response, ApiError> {
    let (lat, lon) = match (q.lat, q.lon) {
        (Some(lat), Some(lon)) => (lat, lon),
        _ => {
            return Err(ApiError::bad_request(
                "missing coordinates",
                "query params lat and lon are required, e.g. ?lat=40.71&lon=-74.00",
            ));
        }
    };
    if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lon) {
        return Err(ApiError::bad_request(
            "invalid coordinates",
            "lat must be in [-90, 90], lon in [-180, 180]",
        ));
    }
    // Normalize the key so equivalent coords share cache entries.
    let key = format!("weather:lat={lat:.4}:lon={lon:.4}");
    let client = state.client.clone();
    let (body, hit) = cached_bytes(&state, &key, feeds::weather::TTL, || async move {
        feeds::weather::fetch(&client, lat, lon).await
    })
    .await?;
    Ok(json_bytes(body, hit))
}

// ---------------------------------------------------------------------------
// /api/satellites
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct LimitQuery {
    pub limit: Option<usize>,
}

pub async fn satellites(
    State(state): State<AppState>,
    Query(q): Query<LimitQuery>,
) -> Result<Response, ApiError> {
    let limit = q.limit.unwrap_or(100).clamp(1, 1000);
    let key = format!("satellites:limit={limit}");
    let client = state.client.clone();
    let (body, hit) = cached_bytes(&state, &key, feeds::satellites::TTL, || async move {
        feeds::satellites::fetch(&client, limit).await
    })
    .await?;
    Ok(json_bytes(body, hit))
}

// ---------------------------------------------------------------------------
// /api/news
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct NewsQuery {
    /// GDELT query string, e.g. ?q=earthquake
    pub q: Option<String>,
    pub max_records: Option<u32>,
}

pub async fn news(
    State(state): State<AppState>,
    Query(q): Query<NewsQuery>,
) -> Result<Response, ApiError> {
    let query = q.q.unwrap_or_default().trim().to_string();
    if query.is_empty() {
        return Err(ApiError::bad_request(
            "missing query",
            "query param q is required, e.g. ?q=earthquake",
        ));
    }
    if query.len() > 200 {
        return Err(ApiError::bad_request(
            "query too long",
            "q must be at most 200 characters",
        ));
    }
    let max_records = q.max_records.unwrap_or(25).clamp(1, 250);
    let key = format!("news:q={query}:n={max_records}");
    let client = state.client.clone();
    let (body, hit) = cached_bytes(&state, &key, feeds::news::TTL, || async move {
        feeds::news::fetch(&client, &query, max_records).await
    })
    .await?;
    Ok(json_bytes(body, hit))
}

// ---------------------------------------------------------------------------
// /api/stats — lightweight aggregate counters, the cheap polling endpoint.
// ---------------------------------------------------------------------------

/// `/api/stats` fans out to every feed concurrently, reuses the same cached
/// byte entries the individual routes serve (so a warm cache costs ~zero
/// upstream traffic), and degrades per-feed instead of 502ing the request.
#[derive(Debug, Deserialize)]
struct CountOnly {
    count: usize,
}

#[derive(Debug, Serialize)]
struct FeedStat {
    ok: bool,
    count: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Debug, Serialize)]
struct StatsResponse {
    generated_at: String,
    degraded: bool,
    feeds: HashMap<String, FeedStat>,
}

type BoxFut<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// Fetch one feed's normalized response as bytes (via the shared cache),
/// then extract just its `count` field.
async fn feed_count(
    state: &AppState,
    name: &'static str,
    key: String,
    ttl: Duration,
    fetch: impl FnOnce(reqwest::Client) -> BoxFut<Result<serde_json::Value, FeedError>> + Send,
) -> (String, FeedStat) {
    let fail = |detail: String| {
        (
            name.to_string(),
            FeedStat {
                ok: false,
                count: None,
                error: Some(detail),
            },
        )
    };
    let bytes = if let Some(b) = state.cache.get(&key) {
        b
    } else {
        let value = match fetch(state.client.clone()).await {
            Ok(v) => v,
            Err(e) => return fail(e.detail),
        };
        match serde_json::to_vec(&value) {
            Ok(b) => {
                state.cache.put(key, b.clone(), ttl);
                b
            }
            Err(e) => return fail(format!("serialize failed: {e}")),
        }
    };
    match serde_json::from_slice::<CountOnly>(&bytes) {
        Ok(c) => (
            name.to_string(),
            FeedStat {
                ok: true,
                count: Some(c.count),
                error: None,
            },
        ),
        Err(e) => fail(format!("count extract failed: {e}")),
    }
}

/// Weather has no meaningful global count; /api/stats reports availability.
/// Served through the same TTL cache as /api/weather (the probe key matches
/// the route's normalized key format), so a warm cache costs zero upstream
/// traffic.
async fn weather_probe(state: &AppState) -> (String, FeedStat) {
    // Fixed probe point; key format matches the /api/weather route so both
    // share the entry.
    let key = "weather:lat=0.0000:lon=0.0000";
    let result = if state.cache.get(key).is_some() {
        Ok(())
    } else {
        match feeds::weather::fetch(&state.client, 0.0, 0.0).await {
            Ok(r) => match serde_json::to_vec(&r) {
                Ok(b) => {
                    state.cache.put(key.to_string(), b, feeds::weather::TTL);
                    Ok(())
                }
                Err(e) => Err(format!("serialize failed: {e}")),
            },
            Err(e) => Err(e.detail),
        }
    };
    match result {
        Ok(()) => (
            "weather".to_string(),
            FeedStat {
                ok: true,
                count: None,
                error: None,
            },
        ),
        Err(detail) => (
            "weather".to_string(),
            FeedStat {
                ok: false,
                count: None,
                error: Some(detail),
            },
        ),
    }
}

pub async fn stats(State(state): State<AppState>) -> impl IntoResponse {
    // Concurrent fan-out: all feeds resolve in parallel, so /api/stats
    // latency ~= the slowest feed, not the sum.
    let (flights, quakes, satellites, news, weather) = tokio::join!(
        feed_count(
            &state,
            "flights",
            "flights:scope=mil:limit=250".to_string(),
            feeds::flights::TTL,
            |c| Box::pin(async move {
                let r = feeds::flights::fetch(&c, "mil", 250).await?;
                serde_json::to_value(&r).map_err(|e| {
                    FeedError::new(feeds::flights::FEED, format!("serialize failed: {e}"))
                })
            }) as BoxFut<_>
        ),
        feed_count(
            &state,
            "quakes",
            "quakes".to_string(),
            feeds::quakes::TTL,
            |c| Box::pin(async move {
                let r = feeds::quakes::fetch(&c).await?;
                serde_json::to_value(&r).map_err(|e| {
                    FeedError::new(feeds::quakes::FEED, format!("serialize failed: {e}"))
                })
            }) as BoxFut<_>
        ),
        feed_count(
            &state,
            "satellites",
            "satellites:limit=100".to_string(),
            feeds::satellites::TTL,
            |c| Box::pin(async move {
                let r = feeds::satellites::fetch(&c, 100).await?;
                serde_json::to_value(&r).map_err(|e| {
                    FeedError::new(feeds::satellites::FEED, format!("serialize failed: {e}"))
                })
            }) as BoxFut<_>
        ),
        feed_count(
            &state,
            "news",
            "news:q=breaking news:n=10".to_string(),
            feeds::news::TTL,
            |c| Box::pin(async move {
                let r = feeds::news::fetch(&c, "breaking news", 10).await?;
                serde_json::to_value(&r).map_err(|e| {
                    FeedError::new(feeds::news::FEED, format!("serialize failed: {e}"))
                })
            }) as BoxFut<_>
        ),
        weather_probe(&state),
    );

    let mut feeds_map = HashMap::new();
    let mut degraded = false;
    for (name, stat) in [flights, quakes, satellites, news, weather] {
        if !stat.ok {
            degraded = true;
        }
        feeds_map.insert(name, stat);
    }

    Json(StatsResponse {
        generated_at: now_iso8601(),
        degraded,
        feeds: feeds_map,
    })
}
