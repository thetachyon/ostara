//! ThreatFox feed: abuse.ch threat-intel platform (malware IOCs).
//!
//! `POST https://threatfox-api.abuse.ch/api/v1/` with
//! `{"query": "get_iocs", "days": N}` → `{"query_status": "ok", "data":
//! [{"ioc", "ioc_type", "ioc_type_desc", "threat_type", "threat_type_desc",
//! "malware", "malware_printable", "confidence_level", "first_seen",
//! "last_seen", "reporter", "reference", ...}]}` (documented 2026-11-01).
//!
//! AUTH REQUIREMENT (verified live 2026-11-01): the ThreatFox Community API
//! requires an `Auth-Key` HTTP header (free at https://auth.abuse.ch).
//! Keyless requests are rejected with HTTP 401 `{"error": "Unauthorized"}` —
//! there is no keyless tier. When `THREATFOX_API_KEY` is unset, /api/threatfox
//! answers 503 by design (the keyed-feed degradation contract).
//!
//! Fixture note: no live data response is obtainable without an Auth-Key, so
//! `benches/fixtures/threatfox.json` is built from the documented sample
//! response shapes (docs page, 2026-11-01), not a live call. The 401 auth
//! rejection itself was observed live.

use std::time::Duration;

use axum::{
    extract::{Query, State},
    response::IntoResponse,
};
use serde::{Deserialize, Serialize};

use super::{cached_bytes, json_bytes};
use crate::{
    error::{ApiError, FeedError},
    routes::AppState,
    time::now_iso8601,
};

/// Cache TTL: 15 minutes (IOC windows refresh continuously).
pub const TTL: Duration = Duration::from_secs(900);
pub const FEED: &str = "threatfox";

const URL: &str = "https://threatfox-api.abuse.ch/api/v1/";
/// Hard cap on served IOCs: the `days:7` window can be large.
const MAX_IOCS: usize = 1000;

/// One normalized ThreatFox indicator of compromise.
#[derive(Debug, Serialize)]
pub struct ThreatFoxIoc {
    pub ioc: String,
    /// Normalized ioc_type: "ip:port" | "domain" | "url" | "md5" | "sha256"
    /// (upstream values passed through when unrecognized).
    pub ioc_type: String,
    pub threat_type: String,
    pub malware: String,
    pub malware_printable: String,
    pub confidence_level: u8,
    pub first_seen: String,
    pub last_seen: Option<String>,
    pub reporter: String,
    pub reference: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ThreatFoxResponse {
    pub count: usize,
    pub days: u32,
    pub generated_at: String,
    pub iocs: Vec<ThreatFoxIoc>,
}

/// Raw upstream entry. `#[serde(default)]` everywhere: ThreatFox omits
/// nulls and adds fields over time; a missing field must not kill the feed.
#[derive(Debug, Deserialize, Default)]
struct RawEntry {
    #[serde(default)]
    ioc: String,
    #[serde(default)]
    ioc_type: String,
    #[serde(default)]
    threat_type: String,
    #[serde(default)]
    malware: String,
    #[serde(default)]
    malware_printable: String,
    #[serde(default)]
    confidence_level: u8,
    #[serde(default)]
    first_seen: String,
    #[serde(default)]
    last_seen: Option<String>,
    #[serde(default)]
    reporter: String,
    #[serde(default)]
    reference: Option<String>,
}

fn normalize(entry: RawEntry) -> Option<ThreatFoxIoc> {
    if entry.ioc.trim().is_empty() {
        return None;
    }
    let ioc_type = match entry.ioc_type.as_str() {
        "ip:port" | "domain" | "url" | "md5" | "sha256" => entry.ioc_type,
        other => {
            // Pass through unrecognized types rather than dropping the IOC.
            if other.is_empty() {
                "unknown".to_string()
            } else {
                other.to_string()
            }
        }
    };
    Some(ThreatFoxIoc {
        ioc: entry.ioc,
        ioc_type,
        threat_type: entry.threat_type,
        malware: entry.malware,
        malware_printable: entry.malware_printable,
        confidence_level: entry.confidence_level,
        first_seen: entry.first_seen,
        last_seen: entry.last_seen,
        reporter: entry.reporter,
        reference: entry.reference,
    })
}

/// Parse a ThreatFox `get_iocs` body. Pure: no networking.
pub fn parse(body: &str, limit: usize) -> Result<Vec<ThreatFoxIoc>, FeedError> {
    let v: serde_json::Value = serde_json::from_str(body)
        .map_err(|e| FeedError::new(FEED, format!("decode failed: {e}")))?;
    if v.get("query_status").and_then(|s| s.as_str()) != Some("ok") {
        return Err(FeedError::new(
            FEED,
            format!(
                "query_status != ok: {}",
                v.get("query_status").unwrap_or(&serde_json::Value::Null)
            ),
        ));
    }
    let data = v
        .get("data")
        .and_then(|d| d.as_array())
        .ok_or_else(|| FeedError::new(FEED, "missing 'data' array"))?;
    let mut out = Vec::new();
    for item in data.iter().take(limit) {
        let raw: RawEntry = serde_json::from_value(item.clone())
            .map_err(|e| FeedError::new(FEED, format!("entry decode failed: {e}")))?;
        if let Some(ioc) = normalize(raw) {
            out.push(ioc);
        }
    }
    Ok(out)
}

/// Fetch recent IOCs. `days` is 1..=7 per the docs (endpoint clamps the rest).
/// The `Auth-Key` header is required — keyless requests get HTTP 401.
pub async fn fetch(
    client: &reqwest::Client,
    api_key: &str,
    days: u32,
) -> Result<ThreatFoxResponse, FeedError> {
    let body = client
        .post(URL)
        .header("Auth-Key", api_key)
        .json(&serde_json::json!({"query": "get_iocs", "days": days}))
        .send()
        .await
        .map_err(|e| FeedError::new(FEED, format!("request failed: {e}")))?;
    let status = body.status();
    if status == reqwest::StatusCode::UNAUTHORIZED {
        return Err(FeedError::new(
            FEED,
            "HTTP 401 Unauthorized — Auth-Key missing or invalid (get one free at https://auth.abuse.ch)",
        ));
    }
    if !status.is_success() {
        return Err(FeedError::new(FEED, format!("upstream status {status}")));
    }
    let text = body
        .text()
        .await
        .map_err(|e| FeedError::new(FEED, format!("read body failed: {e}")))?;
    let iocs = parse(&text, MAX_IOCS)?;
    Ok(ThreatFoxResponse {
        count: iocs.len(),
        days,
        generated_at: now_iso8601(),
        iocs,
    })
}

/// Short detail string for the health endpoint.
pub async fn probe_detail(
    client: &reqwest::Client,
    config: &crate::config::Config,
) -> Result<String, FeedError> {
    match &config.threatfox_api_key {
        None => Err(FeedError::new(
            FEED,
            "THREATFOX_API_KEY unset — feed disabled",
        )),
        Some(key) => {
            let r = fetch(client, key, 1).await?;
            Ok(format!("{} IOCs (24h window)", r.count))
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct ThreatFoxQuery {
    pub days: Option<u32>,
    pub limit: Option<usize>,
    pub malware: Option<String>,
}

// ---------------------------------------------------------------------------
// GET /api/threatfox?days=&limit=&malware=
// ---------------------------------------------------------------------------

pub async fn handler(
    State(state): State<AppState>,
    Query(q): Query<ThreatFoxQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let api_key = match &state.config.threatfox_api_key {
        Some(k) => k.clone(),
        None => {
            return Err(ApiError::service_unavailable(
                "threatfox feed disabled",
                "set THREATFOX_API_KEY to a free Auth-Key from https://auth.abuse.ch — request one there — to enable /api/threatfox (verified 2026-11-01: keyless requests get HTTP 401)",
            ));
        }
    };
    let days = q.days.unwrap_or(1).clamp(1, 7);
    let limit = q.limit.unwrap_or(100).clamp(1, MAX_IOCS);
    let malware_filter = q.malware.map(|m| m.to_lowercase());
    let key = format!(
        "threatfox:days={days}:limit={limit}:malware={}",
        malware_filter.as_deref().unwrap_or("")
    );
    let client = state.client.clone();
    let (body, hit) = cached_bytes(&state, &key, TTL, || async move {
        let mut r = fetch(&client, &api_key, days).await?;
        if let Some(mw) = &malware_filter {
            r.iocs.retain(|ioc| {
                ioc.malware.to_lowercase().contains(mw.as_str())
                    || ioc.malware_printable.to_lowercase().contains(mw.as_str())
            });
            r.count = r.iocs.len();
        }
        r.iocs.truncate(limit);
        r.count = r.iocs.len();
        serde_json::to_vec(&r).map_err(|e| FeedError::new(FEED, format!("serialize failed: {e}")))
    })
    .await?;
    Ok(json_bytes(body, hit))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("../../benches/fixtures/threatfox.json");

    #[test]
    fn parses_fixture_iocs() {
        let iocs = parse(FIXTURE, 100).unwrap();
        assert!(iocs.len() >= 4, "fixture should cover several ioc types");
        let domain = iocs.iter().find(|i| i.ioc == "evil-example.com").unwrap();
        assert_eq!(domain.ioc_type, "domain");
        assert_eq!(domain.threat_type, "botnet_cc");
        assert_eq!(domain.malware, "emotet");
        assert_eq!(domain.malware_printable, "Emotet");
        assert_eq!(domain.confidence_level, 85);
        assert_eq!(domain.first_seen, "2026-11-01 08:30:00 UTC");
        assert_eq!(domain.reporter, "alice");
    }

    #[test]
    fn ip_port_and_hash_types_preserved() {
        let iocs = parse(FIXTURE, 100).unwrap();
        let ip = iocs.iter().find(|i| i.ioc == "203.0.113.9:4443").unwrap();
        assert_eq!(ip.ioc_type, "ip:port");
        let sha = iocs
            .iter()
            .find(|i| i.ioc_type == "sha256")
            .expect("fixture must include a sha256 IOC");
        assert_eq!(sha.ioc.len(), 64);
        assert!(sha.reference.is_some());
    }

    #[test]
    fn empty_ioc_skipped() {
        let body = r#"{"query_status":"ok","data":[{"ioc":"","ioc_type":"domain"}]}"#;
        let iocs = parse(body, 100).unwrap();
        assert!(iocs.is_empty());
    }

    #[test]
    fn non_ok_status_is_error() {
        assert!(parse(r#"{"query_status":"no_result"}"#, 10).is_err());
    }

    #[test]
    fn unauthorized_shape_is_error() {
        // The 401 body is not a query_status envelope at all.
        assert!(parse(r#"{"error": "Unauthorized"}"#, 10).is_err());
    }

    #[test]
    fn missing_data_array_is_error() {
        assert!(parse(r#"{"query_status":"ok"}"#, 10).is_err());
    }

    #[test]
    fn limit_respected() {
        let iocs = parse(FIXTURE, 2).unwrap();
        assert_eq!(iocs.len(), 2);
    }
}
