//! RECON: CVE lookup via the NVD API v2 (passive).
//!
//! `GET https://services.nvd.nist.gov/rest/json/cves/2.0?cveId={id}`.
//! `NVD_API_KEY` (when set) is sent as the `apiKey` query parameter and
//! raises the rate limit; the lookup works keyless otherwise.
//!
//! An unknown CVE (NVD `totalResults: 0`) is a 404 with `{"error","detail"}`,
//! not a 502 — the upstream answered fine, the record just doesn't exist.

use std::time::Duration;

use axum::{
    Json, Router,
    extract::{Query, State},
    http::StatusCode,
    response::IntoResponse,
    routing::get,
};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::error::ApiError;
use crate::time::now_iso8601;

const LOOKUP_TIMEOUT: Duration = Duration::from_secs(15);

// ---------------------------------------------------------------------------
// Pure validation + parsing (unit-testable without network)
// ---------------------------------------------------------------------------

/// Validate `^CVE-\d{4}-\d+$` (case-insensitive on the `CVE` prefix).
/// Returns the uppercased canonical id.
pub fn validate_cve_id(raw: &str) -> Result<String, ApiError> {
    let id = raw.trim().to_ascii_uppercase();
    let parts: Vec<&str> = id.split('-').collect();
    let ok = parts.len() == 3
        && parts[0] == "CVE"
        && parts[1].len() == 4
        && parts[1].chars().all(|c| c.is_ascii_digit())
        && !parts[2].is_empty()
        && parts[2].chars().all(|c| c.is_ascii_digit());
    if !ok {
        return Err(ApiError::bad_request(
            "invalid cve id",
            "id must look like CVE-2024-1234",
        ));
    }
    Ok(id)
}

#[derive(Debug, Clone, Serialize)]
pub struct CveInfo {
    pub id: String,
    pub description_en: String,
    pub cvss_v31: Option<f64>,
    pub severity: Option<String>,
    pub published: String,
    pub last_modified: String,
}

#[derive(Debug, Deserialize)]
struct NvdRoot {
    #[serde(rename = "totalResults", default)]
    total_results: usize,
    #[serde(default)]
    vulnerabilities: Vec<NvdVuln>,
}

#[derive(Debug, Deserialize)]
struct NvdVuln {
    cve: NvdCve,
}

#[derive(Debug, Deserialize)]
struct NvdCve {
    #[serde(default)]
    descriptions: Vec<NvdDescription>,
    #[serde(default)]
    metrics: Option<NvdMetrics>,
    #[serde(default)]
    published: String,
    #[serde(rename = "lastModified", default)]
    last_modified: String,
}

#[derive(Debug, Deserialize)]
struct NvdDescription {
    #[serde(default)]
    lang: String,
    #[serde(default)]
    value: String,
}

#[derive(Debug, Deserialize)]
struct NvdMetrics {
    #[serde(rename = "cvssMetricV31", default)]
    cvss_metric_v31: Vec<NvdCvssV31>,
}

#[derive(Debug, Deserialize)]
struct NvdCvssV31 {
    #[serde(rename = "cvssData", default)]
    cvss_data: NvdCvssData,
}

#[derive(Debug, Deserialize, Default)]
struct NvdCvssData {
    #[serde(rename = "baseScore", default)]
    base_score: Option<f64>,
    #[serde(rename = "baseSeverity", default)]
    base_severity: Option<String>,
}

/// Parse an NVD API v2 response body. `Ok(None)` = the API answered but the
/// CVE is unknown (`totalResults: 0`).
pub fn parse_cve_response(text: &str) -> Result<Option<CveInfo>, String> {
    let root: NvdRoot =
        serde_json::from_str(text).map_err(|e| format!("NVD returned unparseable JSON: {e}"))?;
    let vuln = match root.vulnerabilities.into_iter().next() {
        Some(v) => v,
        None => return Ok(None),
    };
    let cve = vuln.cve;
    let description_en = cve
        .descriptions
        .iter()
        .find(|d| d.lang == "en")
        .map(|d| d.value.clone())
        .unwrap_or_default();
    let (cvss_v31, severity) = cve
        .metrics
        .as_ref()
        .and_then(|m| m.cvss_metric_v31.first())
        .map(|m| (m.cvss_data.base_score, m.cvss_data.base_severity.clone()))
        .unwrap_or((None, None));
    // total_results is the authoritative "known" signal; an empty
    // vulnerabilities array with totalResults > 0 would be a shape change.
    let _ = root.total_results;
    Ok(Some(CveInfo {
        id: String::new(), // filled in by the caller from the validated query
        description_en,
        cvss_v31,
        severity,
        published: cve.published,
        last_modified: cve.last_modified,
    }))
}

// ---------------------------------------------------------------------------
// Network
// ---------------------------------------------------------------------------

async fn fetch_cve(
    client: &reqwest::Client,
    id: &str,
    api_key: Option<&str>,
) -> Result<Option<CveInfo>, String> {
    let mut req = client
        .get("https://services.nvd.nist.gov/rest/json/cves/2.0")
        .query(&[("cveId", id)]);
    if let Some(key) = api_key {
        req = req.query(&[("apiKey", key)]);
    }
    let resp = tokio::time::timeout(LOOKUP_TIMEOUT, req.send())
        .await
        .map_err(|_| format!("NVD timed out after {LOOKUP_TIMEOUT:?}"))?
        .map_err(|e| format!("NVD request failed: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("NVD upstream status {}", resp.status()));
    }
    let text = resp
        .text()
        .await
        .map_err(|e| format!("NVD body read failed: {e}"))?;
    let mut info = parse_cve_response(&text)?;
    if let Some(i) = info.as_mut() {
        i.id = id.to_string();
    }
    Ok(info)
}

// ---------------------------------------------------------------------------
// Response + handler
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct CveResponse {
    #[serde(flatten)]
    pub info: CveInfo,
    pub generated_at: String,
}

#[derive(Debug, Deserialize)]
pub struct CveQuery {
    pub id: Option<String>,
}

/// GET /api/recon/cve?id=CVE-2024-1234 — 200 with the record, 404 when NVD
/// has no such CVE, 502 when NVD itself is unreachable.
pub async fn lookup(
    State(state): State<crate::routes::AppState>,
    Query(q): Query<CveQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let id = match q.id {
        Some(raw) => validate_cve_id(&raw)?,
        None => {
            return Err(ApiError::bad_request(
                "missing id",
                "query param id is required, e.g. ?id=CVE-2024-1234",
            ));
        }
    };
    match fetch_cve(&state.client, &id, state.config.nvd_api_key.as_deref()).await {
        Err(detail) => Err(ApiError::bad_gateway("cve lookup failed", detail)),
        Ok(None) => Ok((
            StatusCode::NOT_FOUND,
            Json(json!({
                "error": "cve not found",
                "detail": format!("NVD has no record for {id}"),
            })),
        )
            .into_response()),
        Ok(Some(info)) => Ok(Json(CveResponse {
            info,
            generated_at: now_iso8601(),
        })
        .into_response()),
    }
}

pub fn route() -> Router<crate::routes::AppState> {
    Router::new().route("/api/recon/cve", get(lookup))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Abbreviated real NVD v2 shape for CVE-2021-44228 (fixture, not live).
    const NVD_HIT: &str = r#"{
        "resultsPerPage": 1, "startIndex": 0, "totalResults": 1,
        "vulnerabilities": [{
            "cve": {
                "id": "CVE-2021-44228",
                "published": "2021-12-10T10:15:09.143",
                "lastModified": "2026-08-11T19:33:44.513",
                "descriptions": [
                    {"lang": "en", "value": "Apache Log4j2 JNDI features do not protect against attacker controlled LDAP."},
                    {"lang": "es", "value": "irrelevant"}
                ],
                "metrics": {
                    "cvssMetricV31": [{
                        "source": "nvd@nist.gov",
                        "cvssData": {"baseScore": 10.0, "baseSeverity": "CRITICAL"}
                    }]
                }
            }
        }]
    }"#;

    const NVD_MISS: &str = r#"{
        "resultsPerPage": 0, "startIndex": 0, "totalResults": 0,
        "vulnerabilities": []
    }"#;

    #[test]
    fn id_validation() {
        assert_eq!(validate_cve_id("CVE-2024-1234").unwrap(), "CVE-2024-1234");
        assert_eq!(
            validate_cve_id(" cve-2021-44228 ").unwrap(),
            "CVE-2021-44228"
        );
        for bad in [
            "",
            "CVE-24-1234",
            "CVE-2024",
            "CVE-2024-abc",
            "XVE-2024-1234",
            "CVE-2024-1234-5",
            "CVE--1234",
        ] {
            assert!(validate_cve_id(bad).is_err(), "should reject {bad:?}");
        }
    }

    #[test]
    fn parses_hit_with_english_description_and_cvss() {
        let info = parse_cve_response(NVD_HIT)
            .unwrap()
            .expect("should be Some");
        assert!(info.description_en.starts_with("Apache Log4j2"));
        assert_eq!(info.cvss_v31, Some(10.0));
        assert_eq!(info.severity.as_deref(), Some("CRITICAL"));
        assert_eq!(info.published, "2021-12-10T10:15:09.143");
        assert_eq!(info.last_modified, "2026-08-11T19:33:44.513");
    }

    #[test]
    fn miss_parses_to_none() {
        assert!(parse_cve_response(NVD_MISS).unwrap().is_none());
    }

    #[test]
    fn garbage_is_an_error() {
        assert!(parse_cve_response("not json").is_err());
    }

    #[test]
    fn missing_metrics_yields_null_cvss() {
        let text = NVD_HIT.replace(r#""metrics": {"#, r#""no_metrics": {"#);
        let info = parse_cve_response(&text).unwrap().expect("should be Some");
        assert!(info.cvss_v31.is_none());
        assert!(info.severity.is_none());
    }
}
