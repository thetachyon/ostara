//! Axum route handlers.
//!
//! Every feed handler follows the same shape: check the TTL cache, on miss
//! fetch + normalize + serialize + store, then serve bytes. Cache hits cost
//! a hash lookup and a memcpy — no re-serialization.
//!
//! Upstream failure is normal: a failed feed returns 502
//! `{"error", "detail"}` and never takes down other feeds. `/api/stats`
//! goes further — it degrades per-feed instead of failing the request.

use std::{
    collections::HashMap,
    future::Future,
    pin::Pin,
    sync::Arc,
    time::{Duration, Instant},
};

use axum::{
    Json, Router,
    extract::{Query, State},
    http::{HeaderName, StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use serde::{Deserialize, Serialize};
use tower_http::services::ServeDir;

use crate::{
    cache::TtlCache,
    config::SharedConfig,
    error::{ApiError, FeedError},
    feeds,
    time::now_iso8601,
};

/// Shared server state: one pooled HTTP client, one TTL cache, one config,
/// and the vessel AIS stream handle (disabled when AIS_API_KEY is unset).
#[derive(Clone)]
pub struct AppState {
    pub client: reqwest::Client,
    pub cache: Arc<TtlCache>,
    pub config: SharedConfig,
    pub vessels: feeds::vessels::VesselStream,
}

/// Fetch the normalized response (or reuse the cached bytes), serialize once,
/// store, and return the bytes plus a cache-hit flag.
///
/// `pub(crate)` so `country_inspect` can reuse the exact TTL cache paths of
/// the normal feed endpoints.
pub(crate) async fn cached_bytes<T, Fut>(
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
    // Phase 1 (keyed feeds): pass config through so OpenSky is preferred
    // when OPENSKY_CLIENT_ID + OPENSKY_CLIENT_SECRET are set; otherwise the
    // keyless adsb.lol path is used unchanged.
    let config = state.config.clone();
    let (body, hit) = cached_bytes(&state, &key, feeds::flights::TTL, || async move {
        feeds::flights::fetch_with_config(&client, &config, &scope, limit).await
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
// /api/cameras — public cameras from the embedded registry.
//
// The registry is compile-time data (data/cameras.json): no upstream fetch,
// no cache — filtering 48 entries is microseconds. `image_url` values are
// live snapshots served by the camera operators themselves; we link, we
// never proxy or store frames.
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct CamerasQuery {
    /// Bounding box "minlon,minlat,maxlon,maxlat", e.g. "-125,32,-114,42".
    pub bbox: Option<String>,
    /// Category filter, case-insensitive (traffic, weather, park).
    pub category: Option<String>,
    /// Max cameras returned. Default 50, clamped at 200.
    pub limit: Option<usize>,
}

pub(crate) fn parse_bbox(s: &str) -> Result<(f64, f64, f64, f64), ApiError> {
    let bad = || {
        ApiError::bad_request(
            "invalid bbox",
            "bbox must be \"minlon,minlat,maxlon,maxlat\", e.g. ?bbox=-125,32,-114,42",
        )
    };
    let parts: Vec<f64> = s
        .split(',')
        .map(str::parse)
        .collect::<Result<_, _>>()
        .map_err(|_| bad())?;
    if parts.len() != 4 {
        return Err(bad());
    }
    let (minlon, minlat, maxlon, maxlat) = (parts[0], parts[1], parts[2], parts[3]);
    if !(-180.0..=180.0).contains(&minlon)
        || !(-180.0..=180.0).contains(&maxlon)
        || !(-90.0..=90.0).contains(&minlat)
        || !(-90.0..=90.0).contains(&maxlat)
        || minlon > maxlon
        || minlat > maxlat
    {
        return Err(bad());
    }
    Ok((minlon, minlat, maxlon, maxlat))
}

pub async fn cameras(Query(q): Query<CamerasQuery>) -> Result<impl IntoResponse, ApiError> {
    let limit = q.limit.unwrap_or(50).clamp(1, 200);
    let bbox = match &q.bbox {
        Some(b) => Some(parse_bbox(b)?),
        None => None,
    };
    // A corrupt embedded registry is a deploy-time bug: surface it as 500
    // with the validation detail rather than serving an empty list.
    let registry = feeds::cameras::load_registry()
        .map_err(|detail| ApiError::internal("camera registry invalid", detail))?;
    let filtered = feeds::cameras::filter_cameras(&registry, bbox, q.category.as_deref(), limit);
    Ok(Json(feeds::cameras::CamerasResponse {
        count: filtered.len(),
        generated_at: now_iso8601(),
        cameras: filtered,
    }))
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

/// Helio serves point values (no count); /api/stats reports availability,
/// like weather_probe.
async fn helio_probe(state: &AppState) -> (String, FeedStat) {
    match feeds::helio::fetch(&state.client).await {
        Ok(_) => (
            "helio".to_string(),
            FeedStat {
                ok: true,
                count: None,
                error: None,
            },
        ),
        Err(e) => (
            "helio".to_string(),
            FeedStat {
                ok: false,
                count: None,
                error: Some(e.detail),
            },
        ),
    }
}

pub async fn stats(State(state): State<AppState>) -> impl IntoResponse {
    // Concurrent fan-out: all feeds resolve in parallel, so /api/stats
    // latency ~= the slowest feed, not the sum.
    let (
        flights,
        quakes,
        satellites,
        news,
        weather,
        fires,
        n2yo,
        cyber,
        threatfox,
        markets,
        conflicts,
        helio,
        telegram,
        bluesky,
        mastodon,
        rss,
        gnews,
    ) = tokio::join!(
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
        // Phase 1 keyed feeds. Disabled-by-design feeds report ok=false
        // with an explicit detail rather than attempting a fetch.
        feed_count(&state, "fires", "fires".to_string(), feeds::fires::TTL, {
            let config = state.config.clone();
            move |c| {
                Box::pin(async move {
                    let key = match &config.firms_api_key {
                        Some(k) => k.clone(),
                        None => {
                            return Err(FeedError::new(
                                feeds::fires::FEED,
                                "disabled: FIRMS_API_KEY unset",
                            ));
                        }
                    };
                    let r = feeds::fires::fetch(&c, &key).await?;
                    serde_json::to_value(&r).map_err(|e| {
                        FeedError::new(feeds::fires::FEED, format!("serialize failed: {e}"))
                    })
                }) as BoxFut<_>
            }
        }),
        feed_count(
            &state,
            "n2yo",
            "n2yo:lat=0:lon=0".to_string(),
            feeds::n2yo::TTL,
            {
                let config = state.config.clone();
                move |c| {
                    Box::pin(async move {
                        let r = feeds::n2yo::fetch(&c, &config, 0.0, 0.0).await?;
                        serde_json::to_value(&r).map_err(|e| {
                            FeedError::new(feeds::n2yo::FEED, format!("serialize failed: {e}"))
                        })
                    }) as BoxFut<_>
                }
            }
        ),
        feed_count(&state, "cyber", "cyber".to_string(), feeds::cyber::TTL, {
            let config = state.config.clone();
            move |c| {
                Box::pin(async move {
                    let key = match &config.threatfox_api_key {
                        Some(k) => k.clone(),
                        None => {
                            return Err(FeedError::new(
                                feeds::cyber::FEED,
                                "disabled: THREATFOX_API_KEY unset",
                            ));
                        }
                    };
                    let r = feeds::cyber::fetch(&c, &key).await?;
                    serde_json::to_value(&r).map_err(|e| {
                        FeedError::new(feeds::cyber::FEED, format!("serialize failed: {e}"))
                    })
                }) as BoxFut<_>
            }
        }),
        feed_count(
            &state,
            "threatfox",
            "threatfox:days=1".to_string(),
            feeds::threatfox::TTL,
            {
                let config = state.config.clone();
                move |c| {
                    Box::pin(async move {
                        let key = match &config.threatfox_api_key {
                            Some(k) => k.clone(),
                            None => {
                                return Err(FeedError::new(
                                    feeds::threatfox::FEED,
                                    "disabled: THREATFOX_API_KEY unset",
                                ));
                            }
                        };
                        let r = feeds::threatfox::fetch(&c, &key, 1).await?;
                        serde_json::to_value(&r).map_err(|e| {
                            FeedError::new(feeds::threatfox::FEED, format!("serialize failed: {e}"))
                        })
                    }) as BoxFut<_>
                }
            }
        ),
        feed_count(
            &state,
            "markets",
            "markets".to_string(),
            feeds::markets::TTL,
            |c| Box::pin(async move {
                let r = feeds::markets::fetch(&c).await?;
                serde_json::to_value(&r).map_err(|e| {
                    FeedError::new(feeds::markets::FEED, format!("serialize failed: {e}"))
                })
            }) as BoxFut<_>
        ),
        feed_count(
            &state,
            "conflicts",
            "conflicts".to_string(),
            feeds::conflicts::TTL,
            |c| Box::pin(async move {
                let r = feeds::conflicts::fetch(&c).await?;
                serde_json::to_value(&r).map_err(|e| {
                    FeedError::new(feeds::conflicts::FEED, format!("serialize failed: {e}"))
                })
            }) as BoxFut<_>
        ),
        feed_count(
            &state,
            "telegram",
            "telegram:stats".to_string(),
            feeds::telegram::TTL,
            {
                let config = state.config.clone();
                move |c| {
                    Box::pin(async move {
                        // Keyless: probe the first allowlisted channel, else
                        // Telegram's own announcement channel.
                        let channel = config
                            .telegram_channels
                            .first()
                            .cloned()
                            .unwrap_or_else(|| "telegram".to_string());
                        let r = feeds::telegram::fetch(&c, &channel).await?;
                        serde_json::to_value(&r).map_err(|e| {
                            FeedError::new(feeds::telegram::FEED, format!("serialize failed: {e}"))
                        })
                    }) as BoxFut<_>
                }
            }
        ),
        feed_count(
            &state,
            "bluesky",
            "bluesky:stats".to_string(),
            feeds::bluesky::TTL,
            |c| {
                Box::pin(async move {
                    let r = feeds::bluesky::fetch(&c, "news", 5).await?;
                    serde_json::to_value(&r).map_err(|e| {
                        FeedError::new(feeds::bluesky::FEED, format!("serialize failed: {e}"))
                    })
                }) as BoxFut<_>
            }
        ),
        feed_count(
            &state,
            "mastodon",
            "mastodon:stats".to_string(),
            feeds::mastodon::TTL,
            |c| {
                Box::pin(async move {
                    let r = feeds::mastodon::fetch(&c, None, 5).await?;
                    serde_json::to_value(&r).map_err(|e| {
                        FeedError::new(feeds::mastodon::FEED, format!("serialize failed: {e}"))
                    })
                }) as BoxFut<_>
            }
        ),
        feed_count(
            &state,
            "rss",
            "rss:stats".to_string(),
            feeds::rss::TTL,
            |c| {
                Box::pin(async move {
                    let r = feeds::rss::fetch(&c, Some("bbc"), 5).await?;
                    serde_json::to_value(&r).map_err(|e| {
                        FeedError::new(feeds::rss::FEED, format!("serialize failed: {e}"))
                    })
                }) as BoxFut<_>
            }
        ),
        feed_count(
            &state,
            "gnews",
            "gnews:stats".to_string(),
            feeds::gnews::TTL,
            |c| {
                Box::pin(async move {
                    let r = feeds::gnews::fetch(&c, "news", 5).await?;
                    serde_json::to_value(&r).map_err(|e| {
                        FeedError::new(feeds::gnews::FEED, format!("serialize failed: {e}"))
                    })
                }) as BoxFut<_>
            }
        ),
        helio_probe(&state),
    );

    let mut feeds_map = HashMap::new();
    let mut degraded = false;
    // Cameras need no upstream fetch: the registry is embedded. Count is
    // the registry length; a corrupt registry reports ok=false honestly.
    let cameras_stat = match feeds::cameras::load_registry() {
        Ok(cams) => (
            "cameras".to_string(),
            FeedStat {
                ok: true,
                count: Some(cams.len()),
                error: None,
            },
        ),
        Err(detail) => (
            "cameras".to_string(),
            FeedStat {
                ok: false,
                count: None,
                error: Some(format!("registry invalid: {detail}")),
            },
        ),
    };
    // Nuclear facilities: static embedded registry, counted like cameras.
    let nuclear_stat = match feeds::nuclear::load_registry() {
        Ok(facilities) => (
            "nuclear".to_string(),
            FeedStat {
                ok: true,
                count: Some(facilities.len()),
                error: None,
            },
        ),
        Err(detail) => (
            "nuclear".to_string(),
            FeedStat {
                ok: false,
                count: None,
                error: Some(format!("registry invalid: {detail}")),
            },
        ),
    };
    // Vessels: live AISStream snapshot when AIS_API_KEY is set; disabled
    // by design otherwise (honestly reported, never fetched).
    let vessels_stat = if state.vessels.is_enabled() {
        (
            "vessels".to_string(),
            FeedStat {
                ok: true,
                count: Some(state.vessels.len()),
                error: None,
            },
        )
    } else {
        (
            "vessels".to_string(),
            FeedStat {
                ok: false,
                count: None,
                error: Some("disabled: AIS_API_KEY unset".to_string()),
            },
        )
    };
    for (name, stat) in [
        flights,
        quakes,
        satellites,
        news,
        weather,
        cameras_stat,
        fires,
        n2yo,
        cyber,
        threatfox,
        markets,
        conflicts,
        helio,
        nuclear_stat,
        vessels_stat,
        telegram,
        bluesky,
        mastodon,
        rss,
        gnews,
    ] {
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

// ---------------------------------------------------------------------------
// /api/health/feeds — live upstream health probes.
//
// One entry per input source: status, latency, and a human-readable detail.
// A UI status page renders directly from this shape. One dead feed never
// fails the endpoint — each feed reports its own status.
// ---------------------------------------------------------------------------

/// Per-feed health classification:
/// - `ok`: upstream returned 2xx with a parseable body within the slow threshold.
/// - `degraded`: upstream reachable but slow, returned non-2xx, or returned a
///   body we could not parse.
/// - `down`: connection failure, DNS failure, or probe timeout.
#[derive(Debug, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum FeedHealth {
    Ok,
    Degraded,
    Down,
}

#[derive(Debug, Serialize)]
pub struct FeedHealthReport {
    pub status: FeedHealth,
    pub latency_ms: u64,
    pub detail: String,
}

#[derive(Debug, Serialize)]
pub struct FeedsHealthResponse {
    pub generated_at: String,
    pub feeds: HashMap<String, FeedHealthReport>,
}

/// Hard per-probe budget: the endpoint stays fast even when an upstream
/// hangs. Never served from cache — this is a liveness probe, not data.
const PROBE_TIMEOUT: Duration = Duration::from_secs(4);
/// Above this latency a successful fetch is reported as degraded (slow).
const SLOW_THRESHOLD_MS: u64 = 3000;

async fn run_probe<F, Fut>(
    client: reqwest::Client,
    name: &'static str,
    probe: F,
) -> (String, FeedHealthReport)
where
    F: FnOnce(reqwest::Client) -> Fut,
    Fut: Future<Output = Result<String, FeedError>>,
{
    let start = Instant::now();
    let outcome = tokio::time::timeout(PROBE_TIMEOUT, probe(client)).await;
    let latency_ms = start.elapsed().as_millis() as u64;
    let (status, detail) = match outcome {
        Err(_) => (
            FeedHealth::Down,
            format!("probe timed out after {}s", PROBE_TIMEOUT.as_secs()),
        ),
        Ok(Ok(detail)) => {
            if latency_ms > SLOW_THRESHOLD_MS {
                (
                    FeedHealth::Degraded,
                    format!("slow upstream ({latency_ms}ms): {detail}"),
                )
            } else {
                (FeedHealth::Ok, detail)
            }
        }
        Ok(Err(e)) => {
            // FeedError details are feed-attributed strings from feeds/mod.rs:
            // "request failed: ..." -> transport failure (timeout/DNS/connect)
            // "upstream status ..."  -> reachable, but the upstream errored
            // anything else           -> reachable, but the body was unusable
            if e.detail.starts_with("request failed") {
                (FeedHealth::Down, e.detail)
            } else {
                (FeedHealth::Degraded, e.detail)
            }
        }
    };
    (
        name.to_string(),
        FeedHealthReport {
            status,
            latency_ms,
            detail,
        },
    )
}

/// Cameras health: the registry must parse (else Down — the only feed
/// whose "upstream" is us), plus a live spot-check of up to 5 camera image
/// URLs, 4s each. Degraded if >40% of spot-checks fail.
async fn cameras_probe(client: &reqwest::Client) -> (String, FeedHealthReport) {
    let start = Instant::now();
    let cams = match feeds::cameras::load_registry() {
        Ok(c) => c,
        Err(e) => {
            return (
                "cameras".to_string(),
                FeedHealthReport {
                    status: FeedHealth::Down,
                    latency_ms: start.elapsed().as_millis() as u64,
                    detail: format!("registry broken: {e}"),
                },
            );
        }
    };
    // Rotating 5-minute window over the registry: deterministic (no rand
    // dependency), and every camera gets spot-checked over time.
    let n = cams.len();
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0) as usize;
    let sample: Vec<&feeds::cameras::Camera> = (0..5.min(n))
        .map(|i| &cams[(now_secs / 300 + i) % n.max(1)])
        .collect();
    let mut handles = Vec::with_capacity(sample.len());
    for cam in sample {
        let c = client.clone();
        let url = cam.image_url.clone();
        handles.push(tokio::spawn(async move {
            let outcome = tokio::time::timeout(Duration::from_secs(4), c.get(&url).send()).await;
            match outcome {
                Err(_) | Ok(Err(_)) => false,
                Ok(Ok(resp)) => {
                    let ok_status = resp.status().is_success();
                    let is_image = resp
                        .headers()
                        .get("content-type")
                        .and_then(|v| v.to_str().ok())
                        .is_some_and(|ct| ct.starts_with("image/"));
                    // Body deliberately not consumed: headers answer liveness.
                    ok_status && is_image
                }
            }
        }));
    }
    let mut failed = 0;
    for h in handles {
        if !h.await.unwrap_or(false) {
            failed += 1;
        }
    }
    let total = 5.min(n);
    let latency_ms = start.elapsed().as_millis() as u64;
    let (status, detail) = if total > 0 && failed * 100 / total > 40 {
        (
            FeedHealth::Degraded,
            format!("{failed}/{total} camera spot-checks failed"),
        )
    } else {
        (
            FeedHealth::Ok,
            format!(
                "registry ok ({} entries); {}/{} spot-checks reachable",
                n,
                total - failed,
                total
            ),
        )
    };
    (
        "cameras".to_string(),
        FeedHealthReport {
            status,
            latency_ms,
            detail,
        },
    )
}

/// Probe every upstream concurrently with a small, cheap request.
/// Keyed feeds report their disabled-by-design state honestly via their
/// `probe_detail` (classified `degraded` with a "disabled" detail, never
/// `down` — nothing is broken, the key is simply unset).
pub async fn feeds_health(State(state): State<AppState>) -> impl IntoResponse {
    let config = state.config.clone();
    let (
        flights,
        quakes,
        weather,
        satellites,
        news,
        cameras,
        fires,
        n2yo,
        vessels,
        cyber,
        threatfox,
        telegram,
        markets,
        nuclear,
        conflicts,
        helio,
        bluesky,
        mastodon,
        rss,
        gnews,
    ) = tokio::join!(
        run_probe(state.client.clone(), "flights", |c| async move {
            feeds::flights::fetch(&c, "mil", 1)
                .await
                .map(|r| format!("{} aircraft (scope=mil, limit=1)", r.count))
        }),
        run_probe(state.client.clone(), "quakes", |c| async move {
            feeds::quakes::fetch(&c)
                .await
                .map(|r| format!("{} quakes (M4.5+ day feed)", r.count))
        }),
        run_probe(state.client.clone(), "weather", |c| async move {
            feeds::weather::fetch(&c, 0.0, 0.0)
                .await
                .map(|r| format!("probe point 0,0 ok (temp_c={:?})", r.temperature_c))
        }),
        run_probe(state.client.clone(), "satellites", |c| async move {
            feeds::satellites::fetch(&c, 1)
                .await
                .map(|r| format!("{} objects (stations group, limit=1)", r.count))
        }),
        run_probe(state.client.clone(), "news", |c| async move {
            feeds::news::fetch(&c, "test", 1)
                .await
                .map(|r| format!("{} articles (q=test, n=1)", r.count))
        }),
        cameras_probe(&state.client),
        run_probe(state.client.clone(), "fires", {
            let config = config.clone();
            move |c| async move { feeds::fires::probe_detail(&c, &config).await }
        }),
        run_probe(state.client.clone(), "n2yo", {
            let config = config.clone();
            move |c| async move { feeds::n2yo::probe_detail(&c, &config).await }
        }),
        run_probe(state.client.clone(), "vessels", {
            let vessels = state.vessels.clone();
            move |_c| {
                let v = vessels.clone();
                async move { feeds::vessels::probe_detail(&v) }
            }
        }),
        run_probe(state.client.clone(), "cyber", {
            let config = config.clone();
            move |c| async move { feeds::cyber::probe_detail(&c, &config).await }
        }),
        run_probe(state.client.clone(), "threatfox", {
            let config = config.clone();
            move |c| async move { feeds::threatfox::probe_detail(&c, &config).await }
        }),
        run_probe(state.client.clone(), "telegram", {
            let config = config.clone();
            move |c| async move { feeds::telegram::probe_detail(&c, &config).await }
        }),
        run_probe(state.client.clone(), "markets", {
            let config = config.clone();
            move |c| async move { feeds::markets::probe_detail(&c, &config).await }
        }),
        run_probe(state.client.clone(), "nuclear", {
            let config = config.clone();
            move |c| async move { feeds::nuclear::probe_detail(&c, &config).await }
        }),
        run_probe(state.client.clone(), "conflicts", {
            let config = config.clone();
            move |c| async move { feeds::conflicts::probe_detail(&c, &config).await }
        }),
        run_probe(state.client.clone(), "helio", {
            let config = config.clone();
            move |c| async move { feeds::helio::probe_detail(&c, &config).await }
        }),
        run_probe(state.client.clone(), "bluesky", |c| async move {
            feeds::bluesky::probe_detail(&c).await
        }),
        run_probe(state.client.clone(), "mastodon", |c| async move {
            feeds::mastodon::probe_detail(&c).await
        }),
        run_probe(state.client.clone(), "rss", |c| async move {
            feeds::rss::probe_detail(&c).await
        }),
        run_probe(state.client.clone(), "gnews", |c| async move {
            feeds::gnews::probe_detail(&c).await
        }),
    );

    let mut feeds_map = HashMap::new();
    for (name, report) in [
        flights, quakes, weather, satellites, news, cameras, fires, n2yo, vessels, cyber,
        threatfox, telegram, markets, nuclear, conflicts, helio, bluesky, mastodon, rss, gnews,
    ] {
        feeds_map.insert(name, report);
    }

    Json(FeedsHealthResponse {
        generated_at: now_iso8601(),
        feeds: feeds_map,
    })
}

// ---------------------------------------------------------------------------
// Canonical router: every route in one place so `main.rs` and the
// integration tests build the exact same app.
// ---------------------------------------------------------------------------

pub fn app(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/api/flights", get(flights))
        .route("/api/quakes", get(quakes))
        .route("/api/weather", get(weather))
        .route("/api/satellites", get(satellites))
        .route("/api/news", get(news))
        .route("/api/cameras", get(cameras))
        .route("/api/stats", get(stats))
        .route("/api/health/feeds", get(feeds_health))
        // Phase 1 (keyed feeds) and Phase 2 (RECON) register their own
        // routers; merged here so `app()` stays the single canonical router
        // that main.rs and the integration tests share.
        .merge(feeds::routes())
        .merge(crate::recon::routes())
        .merge(crate::dossier::routes())
        .merge(crate::country_osint::routes())
        .merge(crate::country_inspect::routes())
        .merge(crate::whatsapp_channels::routes())
        .merge(crate::graph::routes())
        .merge(crate::intel::routes())
        .merge(crate::unrest::routes())
        // Phase 5: MapLibre web UI. Explicit API routes above take
        // precedence; anything unmatched falls through to the static bundle.
        .fallback_service(ServeDir::new("web"))
        .with_state(state)
}
