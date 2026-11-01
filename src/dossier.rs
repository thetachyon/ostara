//! Region dossier (Phase 3): one request, one point, every geo-capable feed
//! filtered by radius.
//!
//! `GET /api/dossier?lat=<f64>&lon=<f64>&radius_km=<f64>` fans out to the
//! geo-capable feeds concurrently, keeps only items within `radius_km` of
//! `(lat, lon)` (haversine), caps each section at [`MAX_ITEMS_PER_SECTION`]
//! items, and returns the assembled dossier. The whole response is cached
//! for [`DOSSIER_TTL`] under a rounded cache key (lat/lon to 2 decimals,
//! radius to the nearest 10 km).
//!
//! Feed coverage and the reasoning behind it:
//! - **flights** (`adsb.lol`, keyless; OpenSky preferred when its keys are
//!   set via `fetch_with_config`): aircraft carry `latitude`/`longitude`.
//!   Scope `"mil"` (the feed's own default) and a 250 fetch limit, matching
//!   `/api/flights`.
//! - **quakes** (USGS, keyless): `latitude`/`longitude` per event.
//! - **fires** (NASA FIRMS, **keyed**): fetched only when `FIRMS_API_KEY`
//!   is set; otherwise the section reports `ok: false` with an explicit
//!   disabled detail — no keyed fetch is ever attempted without a key, so
//!   the dossier never triggers a key-missing 503.
//! - **nuclear** / **cameras**: static embedded registries (`lat`/`lon` per
//!   entry); filtered in memory, no upstream traffic.
//! - **conflicts** (GDACS RSS): events carry only a country *name*, never
//!   coordinates, so they are **not geo-filterable**. The section reports
//!   the raw upstream count with `geocoded: false` and the feed's scope
//!   note instead of pretending the items are regional.
//! - **Excluded**: `vessels` (no keyed REST AIS adapter in Phase 1 —
//!   disabled by design) and `telegram` (MTProto authorized session
//!   deferred) cannot be fetched at all; `n2yo`/`satellites` are
//!   observer-point passes with no per-item coordinates;
//!   `weather`/`helio` are point probes; `cyber`/`markets`/`news` carry no
//!   coordinates. None of them can contribute to a regional dossier.
//!
//! A failed feed degrades its own section (`ok: false` + the feed's detail
//! string) and never fails the dossier — the same contract as `/api/stats`.

use std::time::Duration;

use axum::{
    Router,
    extract::{Query, State},
    http::{HeaderName, StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use serde::{Deserialize, Serialize};

use crate::{
    error::ApiError,
    feeds::{
        cameras::Camera,
        conflicts::{ConflictEvent, SCOPE_NOTE},
        fires::Fire,
        flights::Flight,
        nuclear::NuclearFacility,
        quakes::Quake,
    },
    routes::AppState,
    time::now_iso8601,
};

/// The assembled dossier is worth caching longer than any single fast feed
/// (flights/quakes refresh in ~45-60s) because it is the expensive call —
/// six feeds at once. 300s keeps it fresh enough for a regional picture
/// without re-fanning-out on every poll.
const DOSSIER_TTL: Duration = Duration::from_secs(300);

/// Per-section item cap. Geo-filtering runs *before* the cap, so the 50
/// served items are always the first 50 in-radius hits, not a random sample
/// of the upstream list.
const MAX_ITEMS_PER_SECTION: usize = 50;

/// Flights fetched per dossier before radius filtering; matches the
/// `/api/flights` default so the dossier sees the same aircraft list.
const FLIGHTS_FETCH_LIMIT: usize = 250;

/// Great-circle distance between two WGS-84 points, in kilometers.
/// Pure: no I/O, the unit-test target.
pub fn haversine_km(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    const R_KM: f64 = 6371.0;
    let dlat = (lat2 - lat1).to_radians();
    let dlon = (lon2 - lon1).to_radians();
    let a = (dlat / 2.0).sin().powi(2)
        + lat1.to_radians().cos() * lat2.to_radians().cos() * (dlon / 2.0).sin().powi(2);
    2.0 * R_KM * a.sqrt().asin()
}

/// Keep items whose coordinates fall within `radius_km` of `(lat, lon)`;
/// items with no coordinates are dropped. The per-section cap is applied
/// *after* filtering.
fn geo_filter<T>(
    items: Vec<T>,
    lat: f64,
    lon: f64,
    radius_km: f64,
    coords: impl Fn(&T) -> Option<(f64, f64)>,
) -> Vec<T> {
    items
        .into_iter()
        .filter(|item| {
            coords(item).is_some_and(|(la, lo)| haversine_km(lat, lon, la, lo) <= radius_km)
        })
        .take(MAX_ITEMS_PER_SECTION)
        .collect()
}

#[derive(Debug, Serialize)]
pub struct DossierSection<T: Serialize> {
    pub ok: bool,
    /// Items after radius filtering and the per-section cap.
    pub count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub items: Vec<T>,
}

impl<T: Serialize> DossierSection<T> {
    fn failed(detail: String) -> Self {
        Self {
            ok: false,
            count: 0,
            error: Some(detail),
            items: Vec::new(),
        }
    }
}

/// Conflicts are country-level text, not coordinates: the dossier reports
/// the raw upstream count honestly instead of geo-filtering.
#[derive(Debug, Serialize)]
pub struct ConflictsSection {
    pub ok: bool,
    /// Raw upstream event count — NOT radius-filtered (see `geocoded`).
    pub count: usize,
    /// Always `false`: GDACS items carry no coordinates.
    pub geocoded: bool,
    pub note: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Empty by design: without coordinates there is nothing honest to put
    /// in a *regional* dossier. Fetch `/api/conflicts` for the list.
    pub items: Vec<ConflictEvent>,
}

#[derive(Debug, Serialize)]
pub struct Sections {
    pub flights: DossierSection<Flight>,
    pub quakes: DossierSection<Quake>,
    pub fires: DossierSection<Fire>,
    pub nuclear: DossierSection<NuclearFacility>,
    pub cameras: DossierSection<Camera>,
    pub conflicts: ConflictsSection,
}

#[derive(Debug, Serialize)]
pub struct DossierResponse {
    pub lat: f64,
    pub lon: f64,
    pub radius_km: f64,
    pub generated_at: String,
    pub sections: Sections,
}

#[derive(Debug, Deserialize)]
pub struct DossierQuery {
    pub lat: Option<f64>,
    pub lon: Option<f64>,
    pub radius_km: Option<f64>,
}

async fn flights_section(
    client: &reqwest::Client,
    config: &crate::config::Config,
    lat: f64,
    lon: f64,
    radius_km: f64,
) -> DossierSection<Flight> {
    match crate::feeds::flights::fetch_with_config(client, config, "mil", FLIGHTS_FETCH_LIMIT).await
    {
        Err(e) => DossierSection::failed(e.detail),
        Ok(r) => {
            let items = geo_filter(r.flights, lat, lon, radius_km, |f| {
                f.latitude.zip(f.longitude)
            });
            DossierSection {
                ok: true,
                count: items.len(),
                error: None,
                items,
            }
        }
    }
}

async fn quakes_section(
    client: &reqwest::Client,
    lat: f64,
    lon: f64,
    radius_km: f64,
) -> DossierSection<Quake> {
    match crate::feeds::quakes::fetch(client).await {
        Err(e) => DossierSection::failed(e.detail),
        Ok(r) => {
            let items = geo_filter(r.quakes, lat, lon, radius_km, |q| {
                q.latitude.zip(q.longitude)
            });
            DossierSection {
                ok: true,
                count: items.len(),
                error: None,
                items,
            }
        }
    }
}

async fn fires_section(
    client: &reqwest::Client,
    config: &crate::config::Config,
    lat: f64,
    lon: f64,
    radius_km: f64,
) -> DossierSection<Fire> {
    // Keyed feed: never attempt the fetch without a key. The disabled
    // state is reported in-section, mirroring /api/stats.
    let Some(api_key) = &config.firms_api_key else {
        return DossierSection::failed(
            "disabled: FIRMS_API_KEY unset — fires is a keyed feed with no keyless fallback"
                .to_string(),
        );
    };
    match crate::feeds::fires::fetch(client, api_key).await {
        Err(e) => DossierSection::failed(e.detail),
        Ok(r) => {
            let items = geo_filter(r.fires, lat, lon, radius_km, |f| Some((f.lat, f.lon)));
            DossierSection {
                ok: true,
                count: items.len(),
                error: None,
                items,
            }
        }
    }
}

async fn nuclear_section(lat: f64, lon: f64, radius_km: f64) -> DossierSection<NuclearFacility> {
    match crate::feeds::nuclear::load_registry() {
        Err(detail) => DossierSection::failed(format!("registry invalid: {detail}")),
        Ok(facilities) => {
            let items = geo_filter(facilities, lat, lon, radius_km, |f| Some((f.lat, f.lon)));
            DossierSection {
                ok: true,
                count: items.len(),
                error: None,
                items,
            }
        }
    }
}

async fn cameras_section(lat: f64, lon: f64, radius_km: f64) -> DossierSection<Camera> {
    match crate::feeds::cameras::load_registry() {
        Err(detail) => DossierSection::failed(format!("registry invalid: {detail}")),
        Ok(cams) => {
            let items = geo_filter(cams, lat, lon, radius_km, |c| Some((c.lat, c.lon)));
            DossierSection {
                ok: true,
                count: items.len(),
                error: None,
                items,
            }
        }
    }
}

async fn conflicts_section(client: &reqwest::Client) -> ConflictsSection {
    match crate::feeds::conflicts::fetch(client).await {
        Err(e) => ConflictsSection {
            ok: false,
            count: 0,
            geocoded: false,
            note: SCOPE_NOTE.to_string(),
            error: Some(e.detail),
            items: Vec::new(),
        },
        Ok(r) => ConflictsSection {
            ok: true,
            count: r.count,
            geocoded: false,
            note: SCOPE_NOTE.to_string(),
            error: None,
            items: Vec::new(),
        },
    }
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

/// `GET /api/dossier?lat=&lon=&radius_km=`.
///
/// Validation: `lat`/`lon` are required (400 when missing or out of range);
/// `radius_km` defaults to 100.0 and is clamped to [1, 5000] (400 when not
/// a finite number).
pub async fn dossier(
    State(state): State<AppState>,
    Query(q): Query<DossierQuery>,
) -> Result<Response, ApiError> {
    let lat = q.lat.ok_or_else(|| {
        ApiError::bad_request(
            "missing coordinates",
            "query params lat and lon are required, e.g. ?lat=40.71&lon=-74.00",
        )
    })?;
    let lon = q.lon.ok_or_else(|| {
        ApiError::bad_request(
            "missing coordinates",
            "query params lat and lon are required, e.g. ?lat=40.71&lon=-74.00",
        )
    })?;
    if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lon) {
        return Err(ApiError::bad_request(
            "invalid coordinates",
            "lat must be in [-90, 90], lon in [-180, 180]",
        ));
    }
    let radius_km = match q.radius_km {
        Some(r) if !r.is_finite() => {
            return Err(ApiError::bad_request(
                "invalid radius_km",
                "radius_km must be a finite number of kilometers",
            ));
        }
        Some(r) => r.clamp(1.0, 5000.0),
        None => 100.0,
    };

    // Rounded cache key: lat/lon to 2 decimals (~1 km), radius to the
    // nearest 10 km, so continuous inputs collapse onto shared entries.
    let r_bucket = (radius_km / 10.0).round() * 10.0;
    let key = format!("dossier:lat={lat:.2}:lon={lon:.2}:r={r_bucket:.0}");

    if let Some(body) = state.cache.get(&key) {
        return Ok(json_bytes(body, true));
    }

    // Concurrent fan-out: dossier latency ~= the slowest feed, not the sum.
    // Each section degrades internally, so one dead feed never fails the
    // dossier.
    let (flights, quakes, fires, nuclear, cameras, conflicts) = tokio::join!(
        flights_section(&state.client, &state.config, lat, lon, radius_km),
        quakes_section(&state.client, lat, lon, radius_km),
        fires_section(&state.client, &state.config, lat, lon, radius_km),
        nuclear_section(lat, lon, radius_km),
        cameras_section(lat, lon, radius_km),
        conflicts_section(&state.client),
    );

    let response = DossierResponse {
        lat,
        lon,
        radius_km,
        generated_at: now_iso8601(),
        sections: Sections {
            flights,
            quakes,
            fires,
            nuclear,
            cameras,
            conflicts,
        },
    };
    let body = serde_json::to_vec(&response)
        .map_err(|e| ApiError::bad_gateway("serialize failed", e.to_string()))?;
    state.cache.put(key, body.clone(), DOSSIER_TTL);
    Ok(json_bytes(body, false))
}

/// Mount the dossier router. Merged into the canonical router by the parent
/// (`routes::app` gains `.merge(dossier::routes())`); kept as a separate
/// `routes()` so the module stays self-contained.
pub fn routes() -> Router<AppState> {
    Router::new().route("/api/dossier", get(dossier))
}
