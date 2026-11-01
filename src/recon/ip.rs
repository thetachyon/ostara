//! RECON: IP enrichment (passive).
//!
//! Two sources, chosen by config:
//! - `IPINFO_TOKEN` set → ipinfo.io (`https://ipinfo.io/{ip}?token=…`), richer
//!   enrichment (hostname, postal, timezone in the raw tier).
//! - unset → keyless ip-api.com (`http://ip-api.com/json/{ip}`), 45 req/min
//!   rate limit — one request per lookup keeps us comfortably under it.
//!
//! Private/reserved literals are allowed (operator utility: enriching RFC1918
//! addresses from your own inventory is a legitimate use); the response
//! simply carries nulls where the upstream has no data.

use std::net::IpAddr;
use std::time::Duration;

use axum::{
    Json, Router,
    extract::{Query, State},
    response::IntoResponse,
    routing::get,
};
use serde::{Deserialize, Serialize};

use crate::error::ApiError;
use crate::time::now_iso8601;

const LOOKUP_TIMEOUT: Duration = Duration::from_secs(10);

/// Fields requested from ip-api.com (keyless tier).
const IP_API_FIELDS: &str =
    "status,message,country,countryCode,regionName,city,lat,lon,isp,org,as,query";

// ---------------------------------------------------------------------------
// Pure normalization (unit-testable without network)
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct IpInfo {
    pub ip: String,
    pub country: Option<String>,
    pub country_code: Option<String>,
    pub region: Option<String>,
    pub city: Option<String>,
    pub lat: Option<f64>,
    pub lon: Option<f64>,
    pub isp: Option<String>,
    pub org: Option<String>,
    pub asn: Option<String>,
}

fn opt_str(v: &serde_json::Value, key: &str) -> Option<String> {
    v.get(key)?
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn opt_f64(v: &serde_json::Value, key: &str) -> Option<f64> {
    v.get(key)?.as_f64()
}

/// Normalize an ipinfo.io response. Free tier carries no country *name*
/// (only the ISO code), and no separate ISP field — those stay null rather
/// than being invented.
pub fn normalize_ipinfo(ip: &str, v: &serde_json::Value) -> IpInfo {
    let (lat, lon) = v
        .get("loc")
        .and_then(|l| l.as_str())
        .and_then(|loc| {
            let (a, b) = loc.split_once(',')?;
            Some((a.trim().parse::<f64>().ok()?, b.trim().parse::<f64>().ok()?))
        })
        .unzip();
    // org looks like "AS15169 Google LLC"; the dedicated asn object wins
    // when present.
    let org = opt_str(v, "org");
    let asn = v
        .get("asn")
        .and_then(|a| a.get("asn"))
        .and_then(|a| a.as_str())
        .map(str::to_string)
        .or_else(|| {
            org.as_deref().and_then(|o| {
                o.split_whitespace()
                    .next()
                    .filter(|t| t.starts_with("AS"))
                    .map(str::to_string)
            })
        });
    IpInfo {
        ip: ip.to_string(),
        country: None,
        country_code: opt_str(v, "country"),
        region: opt_str(v, "region"),
        city: opt_str(v, "city"),
        lat,
        lon,
        isp: None,
        org,
        asn,
    }
}

/// Normalize an ip-api.com response. `status != "success"` (e.g. private
/// range, reserved, or query failure) is an Err with the upstream message.
pub fn normalize_ipapi(v: &serde_json::Value) -> Result<IpInfo, String> {
    if v.get("status").and_then(|s| s.as_str()) != Some("success") {
        let msg = v
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("unknown ip-api.com failure");
        return Err(format!("ip-api.com: {msg}"));
    }
    Ok(IpInfo {
        ip: opt_str(v, "query").unwrap_or_default(),
        country: opt_str(v, "country"),
        country_code: opt_str(v, "countryCode"),
        region: opt_str(v, "regionName"),
        city: opt_str(v, "city"),
        lat: opt_f64(v, "lat"),
        lon: opt_f64(v, "lon"),
        isp: opt_str(v, "isp"),
        org: opt_str(v, "org"),
        asn: opt_str(v, "as").and_then(|s| {
            s.split_whitespace()
                .next()
                .filter(|t| t.starts_with("AS"))
                .map(str::to_string)
        }),
    })
}

// ---------------------------------------------------------------------------
// Network
// ---------------------------------------------------------------------------

async fn fetch_ipinfo(client: &reqwest::Client, ip: &str, token: &str) -> Result<IpInfo, String> {
    let url = format!("https://ipinfo.io/{ip}");
    let resp = tokio::time::timeout(
        LOOKUP_TIMEOUT,
        client.get(&url).query(&[("token", token)]).send(),
    )
    .await
    .map_err(|_| format!("ipinfo.io timed out after {LOOKUP_TIMEOUT:?}"))?
    .map_err(|e| format!("ipinfo.io request failed: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("ipinfo.io upstream status {}", resp.status()));
    }
    let v: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("ipinfo.io returned unparseable JSON: {e}"))?;
    Ok(normalize_ipinfo(ip, &v))
}

async fn fetch_ipapi(client: &reqwest::Client, ip: &str) -> Result<IpInfo, String> {
    let url = format!("http://ip-api.com/json/{ip}");
    let resp = tokio::time::timeout(
        LOOKUP_TIMEOUT,
        client.get(&url).query(&[("fields", IP_API_FIELDS)]).send(),
    )
    .await
    .map_err(|_| format!("ip-api.com timed out after {LOOKUP_TIMEOUT:?}"))?
    .map_err(|e| format!("ip-api.com request failed: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("ip-api.com upstream status {}", resp.status()));
    }
    let v: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("ip-api.com returned unparseable JSON: {e}"))?;
    normalize_ipapi(&v)
}

// ---------------------------------------------------------------------------
// Response + handler
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct IpResponse {
    #[serde(flatten)]
    pub info: IpInfo,
    pub generated_at: String,
}

#[derive(Debug, Deserialize)]
pub struct IpQuery {
    pub ip: Option<String>,
}

/// GET /api/recon/ip?ip=8.8.8.8 — validate the literal, then enrich via
/// ipinfo.io (token configured) or keyless ip-api.com.
pub async fn lookup(
    State(state): State<crate::routes::AppState>,
    Query(q): Query<IpQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let raw = match q.ip {
        Some(ip) => ip,
        None => {
            return Err(ApiError::bad_request(
                "missing ip",
                "query param ip is required, e.g. ?ip=8.8.8.8",
            ));
        }
    };
    let addr: IpAddr = raw.trim().parse().map_err(|_| {
        ApiError::bad_request("invalid ip", "ip must be a valid IPv4 or IPv6 literal")
    })?;
    let ip = addr.to_string();

    let info = if let Some(token) = state.config.ipinfo_token.as_deref() {
        fetch_ipinfo(&state.client, &ip, token)
            .await
            .map_err(|detail| ApiError::bad_gateway("ip enrichment failed", detail))?
    } else {
        fetch_ipapi(&state.client, &ip)
            .await
            .map_err(|detail| ApiError::bad_gateway("ip enrichment failed", detail))?
    };
    Ok(Json(IpResponse {
        info,
        generated_at: now_iso8601(),
    }))
}

pub fn route() -> Router<crate::routes::AppState> {
    Router::new().route("/api/recon/ip", get(lookup))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn ipinfo_normalizes_loc_and_asn() {
        let v = json!({
            "ip": "8.8.8.8",
            "city": "Mountain View",
            "region": "California",
            "country": "US",
            "loc": "37.4056,-122.0775",
            "org": "AS15169 Google LLC",
            "postal": "94043",
            "timezone": "America/Los_Angeles"
        });
        let i = normalize_ipinfo("8.8.8.8", &v);
        assert_eq!(i.ip, "8.8.8.8");
        assert_eq!(i.country_code.as_deref(), Some("US"));
        assert!(i.country.is_none()); // free tier has no country name: stays null
        assert_eq!(i.city.as_deref(), Some("Mountain View"));
        assert_eq!(i.lat, Some(37.4056));
        assert_eq!(i.lon, Some(-122.0775));
        assert_eq!(i.asn.as_deref(), Some("AS15169"));
        assert_eq!(i.org.as_deref(), Some("AS15169 Google LLC"));
        assert!(i.isp.is_none());
    }

    #[test]
    fn ipinfo_prefers_dedicated_asn_object() {
        let v = json!({
            "ip": "1.1.1.1",
            "country": "AU",
            "loc": "-33.8688,151.2093",
            "org": "AS13335 Cloudflare, Inc.",
            "asn": {"asn": "AS13335", "name": "Cloudflare, Inc."}
        });
        let i = normalize_ipinfo("1.1.1.1", &v);
        assert_eq!(i.asn.as_deref(), Some("AS13335"));
    }

    #[test]
    fn ipinfo_bad_loc_yields_null_coords() {
        let v = json!({"ip": "9.9.9.9", "loc": "bogus"});
        let i = normalize_ipinfo("9.9.9.9", &v);
        assert!(i.lat.is_none() && i.lon.is_none());
    }

    #[test]
    fn ipapi_success_normalizes() {
        let v = json!({
            "status": "success",
            "country": "United States",
            "countryCode": "US",
            "regionName": "Virginia",
            "city": "Ashburn",
            "lat": 39.03,
            "lon": -77.5,
            "isp": "Google LLC",
            "org": "Google Public DNS",
            "as": "AS15169 Google LLC",
            "query": "8.8.8.8"
        });
        let i = normalize_ipapi(&v).unwrap();
        assert_eq!(i.country.as_deref(), Some("United States"));
        assert_eq!(i.country_code.as_deref(), Some("US"));
        assert_eq!(i.region.as_deref(), Some("Virginia"));
        assert_eq!(i.isp.as_deref(), Some("Google LLC"));
        assert_eq!(i.asn.as_deref(), Some("AS15169"));
        assert_eq!(i.lat, Some(39.03));
    }

    #[test]
    fn ipapi_failure_is_an_error_with_message() {
        let v = json!({"status": "fail", "message": "private range", "query": "10.0.0.1"});
        let err = normalize_ipapi(&v).unwrap_err();
        assert!(err.contains("private range"), "got: {err}");
    }

    #[test]
    fn ip_literal_validation() {
        assert!("8.8.8.8".parse::<IpAddr>().is_ok());
        assert!("2001:4860:4860::8888".parse::<IpAddr>().is_ok());
        assert!("not-an-ip".parse::<IpAddr>().is_err());
        assert!("999.1.1.1".parse::<IpAddr>().is_err());
    }
}
