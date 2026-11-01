//! Cyber threat-intel feed: abuse.ch sources (URLhaus + ThreatFox).
//!
//! - URLhaus recent URLs: `GET https://urlhaus-api.abuse.ch/v1/urls/recent/`
//!   → `{"query_status": "ok", "urls": [{"id", "url", "threat",
//!   "date_added", ...}]}` (documented 2026-11-01).
//! - ThreatFox recent IOCs: `POST https://threatfox-api.abuse.ch/api/v1/`
//!   with `{"query": "get_iocs", "days": 1}` → `{"query_status": "ok",
//!   "data": [{"ioc", "ioc_type", "threat_type", "first_seen", ...}]}`
//!   (documented 2026-11-01).
//!
//! AUTH (verified live 2026-11-01): **both** abuse.ch APIs require an
//! `Auth-Key` header (free at https://auth.abuse.ch) — keyless requests get
//! HTTP 401 `{"error": "Unauthorized"}`. The feed is therefore keyed on
//! `THREATFOX_API_KEY` (one Auth-Key works across abuse.ch APIs) and
//! answers 503 by design when unset. abuse.ch asks for reasonable use (the
//! ThreatFox `days:1` window can be large, so each source is capped before
//! merge).
//!
//! LIVENESS NOTE (2026-11-01): sandbox reachability of the abuse.ch APIs
//! could not be verified — the verification probe was declined at the
//! user-confirmation layer, and per policy it was not retried. The request
//! shapes above come from provider documentation, not a live call, and no
//! live test ships with this module. The integration test asserts the
//! 200-or-502 contract only.

use std::time::Duration;

use axum::{extract::State, response::IntoResponse};
use serde::{Deserialize, Serialize};

use super::{cached_bytes, json_bytes};
use crate::{
    config::Config,
    error::{ApiError, FeedError},
    routes::AppState,
    time::now_iso8601,
};

/// Cache TTL: threat feeds refresh continuously; 15 minutes.
pub const TTL: Duration = Duration::from_secs(900);
pub const FEED: &str = "cyber";

/// Per-source cap before merging: bounds response size when the upstream
/// window is large.
const PER_SOURCE_CAP: usize = 100;
/// Hard cap on the merged indicator list.
const MAX_INDICATORS: usize = 200;

/// One normalized threat indicator.
#[derive(Debug, Serialize)]
pub struct Indicator {
    /// "url" (URLhaus) or the ThreatFox `ioc_type` ("domain", "ip:port", ...).
    pub kind: String,
    pub value: String,
    /// URLhaus `threat` or ThreatFox `threat_type` (short form).
    pub threat_type: String,
    pub first_seen: String,
}

#[derive(Debug, Serialize)]
pub struct CyberResponse {
    pub count: usize,
    pub generated_at: String,
    pub indicators: Vec<Indicator>,
}

#[derive(Debug, Deserialize)]
struct UrlhausEntry {
    #[serde(default)]
    url: String,
    #[serde(default)]
    threat: String,
    #[serde(default)]
    date_added: String,
}

#[derive(Debug, Deserialize)]
struct ThreatFoxEntry {
    #[serde(default)]
    ioc: String,
    #[serde(default)]
    ioc_type: String,
    #[serde(default)]
    threat_type: String,
    #[serde(default)]
    first_seen: String,
}

/// Parse a URLhaus `urls/recent` body. Pure: no networking.
pub fn parse_urlhaus(body: &str, limit: usize) -> Result<Vec<Indicator>, FeedError> {
    let v: serde_json::Value = serde_json::from_str(body)
        .map_err(|e| FeedError::new(FEED, format!("urlhaus decode failed: {e}")))?;
    if v.get("query_status").and_then(|s| s.as_str()) != Some("ok") {
        return Err(FeedError::new(
            FEED,
            format!(
                "urlhaus query_status != ok: {}",
                v.get("query_status").unwrap_or(&serde_json::Value::Null)
            ),
        ));
    }
    let urls = v
        .get("urls")
        .and_then(|u| u.as_array())
        .ok_or_else(|| FeedError::new(FEED, "urlhaus missing 'urls' array"))?;
    let mut out = Vec::new();
    for item in urls.iter().take(limit) {
        let e: UrlhausEntry = serde_json::from_value(item.clone())
            .map_err(|e| FeedError::new(FEED, format!("urlhaus entry decode failed: {e}")))?;
        if e.url.is_empty() {
            continue;
        }
        out.push(Indicator {
            kind: "url".to_string(),
            value: e.url,
            threat_type: e.threat,
            first_seen: e.date_added,
        });
    }
    Ok(out)
}

/// Parse a ThreatFox `get_iocs` body. Pure: no networking.
pub fn parse_threatfox(body: &str, limit: usize) -> Result<Vec<Indicator>, FeedError> {
    let v: serde_json::Value = serde_json::from_str(body)
        .map_err(|e| FeedError::new(FEED, format!("threatfox decode failed: {e}")))?;
    if v.get("query_status").and_then(|s| s.as_str()) != Some("ok") {
        return Err(FeedError::new(
            FEED,
            format!(
                "threatfox query_status != ok: {}",
                v.get("query_status").unwrap_or(&serde_json::Value::Null)
            ),
        ));
    }
    let data = v
        .get("data")
        .and_then(|d| d.as_array())
        .ok_or_else(|| FeedError::new(FEED, "threatfox missing 'data' array"))?;
    let mut out = Vec::new();
    for item in data.iter().take(limit) {
        let e: ThreatFoxEntry = serde_json::from_value(item.clone())
            .map_err(|e| FeedError::new(FEED, format!("threatfox entry decode failed: {e}")))?;
        if e.ioc.is_empty() {
            continue;
        }
        out.push(Indicator {
            kind: if e.ioc_type.is_empty() {
                "ioc".to_string()
            } else {
                e.ioc_type
            },
            value: e.ioc,
            threat_type: e.threat_type,
            first_seen: e.first_seen,
        });
    }
    Ok(out)
}

/// Fetch both sources concurrently and merge. One source failing does not
/// fail the feed; both failing does. The abuse.ch `Auth-Key` is required
/// for both halves (HTTP 401 keyless, verified 2026-11-01).
pub async fn fetch(client: &reqwest::Client, api_key: &str) -> Result<CyberResponse, FeedError> {
    let key = api_key.to_string();
    let (uh, tf) = tokio::join!(
        async {
            let resp = client
                .get("https://urlhaus-api.abuse.ch/v1/urls/recent/")
                .header("Auth-Key", &key)
                .send()
                .await
                .map_err(|e| FeedError::new(FEED, format!("urlhaus request failed: {e}")))?;
            if !resp.status().is_success() {
                return Err(FeedError::new(
                    FEED,
                    format!("urlhaus upstream status {}", resp.status()),
                ));
            }
            let body = resp
                .text()
                .await
                .map_err(|e| FeedError::new(FEED, format!("urlhaus read body failed: {e}")))?;
            parse_urlhaus(&body, PER_SOURCE_CAP)
        },
        async {
            let resp = client
                .post("https://threatfox-api.abuse.ch/api/v1/")
                .header("Auth-Key", &key)
                .json(&serde_json::json!({"query": "get_iocs", "days": 1}))
                .send()
                .await
                .map_err(|e| FeedError::new(FEED, format!("threatfox request failed: {e}")))?;
            if !resp.status().is_success() {
                return Err(FeedError::new(
                    FEED,
                    format!("threatfox upstream status {}", resp.status()),
                ));
            }
            let body = resp
                .text()
                .await
                .map_err(|e| FeedError::new(FEED, format!("threatfox read body failed: {e}")))?;
            parse_threatfox(&body, PER_SOURCE_CAP)
        },
    );
    let mut indicators = Vec::new();
    let mut errors = Vec::new();
    match uh {
        Ok(mut v) => indicators.append(&mut v),
        Err(e) => errors.push(format!("urlhaus: {}", e.detail)),
    }
    match tf {
        Ok(mut v) => indicators.append(&mut v),
        Err(e) => errors.push(format!("threatfox: {}", e.detail)),
    }
    if indicators.is_empty() {
        return Err(FeedError::new(
            FEED,
            format!("all sources failed: {}", errors.join("; ")),
        ));
    }
    indicators.truncate(MAX_INDICATORS);
    Ok(CyberResponse {
        count: indicators.len(),
        generated_at: now_iso8601(),
        indicators,
    })
}

/// Short detail string for the health endpoint.
pub async fn probe_detail(client: &reqwest::Client, config: &Config) -> Result<String, FeedError> {
    match &config.threatfox_api_key {
        None => Err(FeedError::new(
            FEED,
            "THREATFOX_API_KEY unset — feed disabled (abuse.ch APIs need an Auth-Key)",
        )),
        Some(key) => {
            let r = fetch(client, key).await?;
            Ok(format!("{} indicators (urlhaus+threatfox)", r.count))
        }
    }
}

// ---------------------------------------------------------------------------
// GET /api/cyber/threats
// ---------------------------------------------------------------------------

pub async fn handler(State(state): State<AppState>) -> Result<impl IntoResponse, ApiError> {
    let api_key = match &state.config.threatfox_api_key {
        Some(k) => k.clone(),
        None => {
            return Err(ApiError::service_unavailable(
                "cyber feed disabled",
                "set THREATFOX_API_KEY to a free Auth-Key from https://auth.abuse.ch — request one there — to enable /api/cyber/threats (verified 2026-11-01: the abuse.ch APIs reject keyless requests with HTTP 401)",
            ));
        }
    };
    let client = state.client.clone();
    let (body, hit) = cached_bytes(&state, "cyber:threats", TTL, || async move {
        fetch(&client, &api_key).await
    })
    .await?;
    Ok(json_bytes(body, hit))
}

#[cfg(test)]
mod tests {
    use super::*;

    const URLHAUS_FIXTURE: &str = include_str!("../../benches/fixtures/urlhaus.json");
    const THREATFOX_FIXTURE: &str = include_str!("../../benches/fixtures/threatfox.json");

    #[test]
    fn parses_urlhaus_fixture() {
        let inds = parse_urlhaus(URLHAUS_FIXTURE, 100).unwrap();
        assert_eq!(inds.len(), 2);
        assert_eq!(inds[0].kind, "url");
        assert_eq!(inds[0].value, "http://192.0.2.44:8080/payload.exe");
        assert_eq!(inds[0].threat_type, "malware_download");
        assert_eq!(inds[0].first_seen, "2026-11-01 12:00:00 UTC");
    }

    #[test]
    fn urlhaus_limit_respected() {
        let inds = parse_urlhaus(URLHAUS_FIXTURE, 1).unwrap();
        assert_eq!(inds.len(), 1);
    }

    #[test]
    fn urlhaus_non_ok_status_is_error() {
        assert!(parse_urlhaus(r#"{"query_status":"no_results"}"#, 10).is_err());
    }

    #[test]
    fn parses_threatfox_fixture() {
        let inds = parse_threatfox(THREATFOX_FIXTURE, 100).unwrap();
        assert_eq!(inds.len(), 4);
        assert_eq!(inds[0].kind, "domain");
        assert_eq!(inds[0].value, "evil-example.com");
        assert_eq!(inds[0].threat_type, "botnet_cc");
        assert_eq!(inds[0].first_seen, "2026-11-01 08:30:00 UTC");
        assert_eq!(inds[1].kind, "ip:port");
        assert_eq!(inds[2].kind, "url");
        assert_eq!(inds[3].kind, "sha256");
    }

    #[test]
    fn threatfox_non_ok_status_is_error() {
        assert!(parse_threatfox(r#"{"query_status":"no_result"}"#, 10).is_err());
    }
}
