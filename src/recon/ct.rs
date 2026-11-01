//! RECON: Certificate Transparency via crt.sh (passive).
//!
//! Queries `https://crt.sh/?q=%25.{domain}&output=json` (`%25.` is the
//! URL-encoded `%.` wildcard prefix, so the query covers the apex and all
//! subdomains). Results are capped at 100, newest first by `not_before`.
//!
//! Liveness note (verified 2026-11-01): crt.sh is flaky from some networks —
//! it intermittently times out or returns 502 through an egress proxy. The
//! handler retries up to three times with backoff; a dead crt.sh surfaces
//! as 502, never a fake cert list.

use std::time::Duration;

use axum::{
    Json, Router,
    extract::{Query, State},
    response::IntoResponse,
    routing::get,
};
use serde::{Deserialize, Serialize};

use super::dns::{DomainQuery, validate_domain};
use crate::error::ApiError;
use crate::time::now_iso8601;

/// Cap on certs returned per lookup.
const MAX_CERTS: usize = 100;
/// Per-attempt budget; the handler tries up to three times with backoff
/// before giving up.
const CT_TIMEOUT: Duration = Duration::from_secs(15);
const CT_ATTEMPTS: u32 = 3;

// ---------------------------------------------------------------------------
// Pure parsing (unit-testable without network)
// ---------------------------------------------------------------------------

/// One crt.sh JSON row. All fields defaulted: a single malformed row must
/// not kill the whole parse.
#[derive(Debug, Deserialize)]
struct CrtRow {
    #[serde(default)]
    common_name: String,
    #[serde(default)]
    issuer_name: String,
    #[serde(default)]
    not_before: String,
    #[serde(default)]
    not_after: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Cert {
    pub common_name: String,
    pub issuer_name: String,
    pub not_before: String,
    pub not_after: String,
}

/// Parse a crt.sh `output=json` body into certs, newest first by
/// `not_before` (ISO-8601 strings sort lexicographically), capped at
/// [`MAX_CERTS`].
pub fn parse_certs(text: &str) -> Result<Vec<Cert>, String> {
    let mut rows: Vec<CrtRow> =
        serde_json::from_str(text).map_err(|e| format!("crt.sh returned unparseable JSON: {e}"))?;
    rows.sort_by(|a, b| b.not_before.cmp(&a.not_before));
    rows.truncate(MAX_CERTS);
    Ok(rows
        .into_iter()
        .map(|r| Cert {
            common_name: r.common_name,
            issuer_name: r.issuer_name,
            not_before: r.not_before,
            not_after: r.not_after,
        })
        .collect())
}

// ---------------------------------------------------------------------------
// Network
// ---------------------------------------------------------------------------

async fn fetch_certs(client: &reqwest::Client, domain: &str) -> Result<Vec<Cert>, String> {
    // q=%25.{domain} == q=%.{domain}: wildcard + apex in one query.
    let url = format!("https://crt.sh/?q=%25.{domain}&output=json");
    let mut last_err = String::new();
    for attempt in 1..=CT_ATTEMPTS {
        let outcome = tokio::time::timeout(CT_TIMEOUT, client.get(&url).send()).await;
        match outcome {
            Err(_) => last_err = format!("attempt {attempt}: timed out after {CT_TIMEOUT:?}"),
            Ok(Err(e)) => last_err = format!("attempt {attempt}: request failed: {e}"),
            Ok(Ok(resp)) => {
                if !resp.status().is_success() {
                    last_err = format!("attempt {attempt}: upstream status {}", resp.status());
                } else {
                    let text = resp
                        .text()
                        .await
                        .map_err(|e| format!("attempt {attempt}: body read failed: {e}"))?;
                    return parse_certs(&text);
                }
            }
        }
        if attempt < CT_ATTEMPTS {
            tokio::time::sleep(Duration::from_millis(500 * attempt as u64)).await;
        }
    }
    Err(format!("crt.sh unreachable: {last_err}"))
}

// ---------------------------------------------------------------------------
// Response + handler
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct CtResponse {
    pub domain: String,
    pub count: usize,
    pub certs: Vec<Cert>,
    pub generated_at: String,
}

/// GET /api/recon/ct?domain=example.com — certificates logged for the domain
/// and its subdomains, newest first, capped at 100.
pub async fn lookup(
    State(state): State<crate::routes::AppState>,
    Query(q): Query<DomainQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let domain = match q.domain {
        Some(d) => validate_domain(&d)?,
        None => {
            return Err(ApiError::bad_request(
                "missing domain",
                "query param domain is required, e.g. ?domain=example.com",
            ));
        }
    };
    let certs = fetch_certs(&state.client, &domain)
        .await
        .map_err(|detail| {
            ApiError::bad_gateway("certificate transparency lookup failed", detail)
        })?;
    Ok(Json(CtResponse {
        domain,
        count: certs.len(),
        certs,
        generated_at: now_iso8601(),
    }))
}

pub fn route() -> Router<crate::routes::AppState> {
    Router::new().route("/api/recon/ct", get(lookup))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal crt.sh-shaped fixture (not live data).
    const FIXTURE: &str = r#"[
        {"common_name": "example.com", "issuer_name": "C=US, O=Let's Encrypt, CN=R3",
         "not_before": "2024-01-01T00:00:00", "not_after": "2024-04-01T00:00:00",
         "name_value": "example.com"},
        {"common_name": "www.example.com", "issuer_name": "C=US, O=Let's Encrypt, CN=R3",
         "not_before": "2025-06-01T00:00:00", "not_after": "2025-09-01T00:00:00",
         "name_value": "www.example.com"},
        {"common_name": "old.example.com",
         "not_before": "2020-01-01T00:00:00", "not_after": "2020-04-01T00:00:00"}
    ]"#;

    #[test]
    fn parses_and_sorts_newest_first() {
        let certs = parse_certs(FIXTURE).unwrap();
        assert_eq!(certs.len(), 3);
        assert_eq!(certs[0].common_name, "www.example.com");
        assert_eq!(certs[2].common_name, "old.example.com");
        // Missing fields default, they don't fail the parse.
        assert_eq!(certs[2].issuer_name, "");
    }

    #[test]
    fn empty_array_parses_to_empty() {
        assert!(parse_certs("[]").unwrap().is_empty());
    }

    #[test]
    fn garbage_is_an_error_not_a_fake_list() {
        assert!(parse_certs("<html>nope</html>").is_err());
        assert!(parse_certs("").is_err());
    }

    #[test]
    fn cap_applies() {
        let row = r#"{"common_name":"x","issuer_name":"y","not_before":"2024-01-01T00:00:00","not_after":"2024-04-01T00:00:00"}"#;
        let big = format!("[{}]", vec![row; MAX_CERTS + 50].join(","));
        assert_eq!(parse_certs(&big).unwrap().len(), MAX_CERTS);
    }
}
