//! RECON: WHOIS lookups (passive).
//!
//! Classic port-43 WHOIS: query whois.iana.org for the thin registry record,
//! parse its `refer:` line, and follow exactly ONE referral to the
//! registrar's whois server. Both responses are parsed best-effort into a
//! small field set and returned with truncated raw text.

use std::time::Duration;

use axum::{
    Json, Router,
    extract::{Query, State},
    response::IntoResponse,
    routing::get,
};
use serde::Serialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::dns::{DomainQuery, validate_domain};
use crate::error::ApiError;
use crate::time::now_iso8601;

const IANA_HOST: &str = "whois.iana.org";
const WHOIS_PORT: u16 = 43;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(8);
/// Total budget for reading one whois response (server closes at EOF, but a
/// hung server must not hang the handler).
const READ_BUDGET: Duration = Duration::from_secs(20);
/// Hard cap on bytes kept per response; the rest is dropped, not buffered.
const MAX_BYTES: usize = 64 * 1024;
/// Raw text kept in the JSON response per side.
const RAW_TRUNCATE: usize = 8 * 1024;

// ---------------------------------------------------------------------------
// Pure parsing (unit-testable without network)
// ---------------------------------------------------------------------------

/// Best-effort field extraction from a whois body. Keys are matched
/// case-insensitively on the part before the first ':'; the first non-empty
/// value wins for single-value fields, name servers accumulate (deduped).
#[derive(Debug, Default, PartialEq)]
pub struct WhoisFields {
    pub registrar: Option<String>,
    pub creation_date: Option<String>,
    pub expiry_date: Option<String>,
    pub name_servers: Vec<String>,
}

pub fn parse_whois_fields(text: &str) -> WhoisFields {
    let mut out = WhoisFields::default();
    for line in text.lines() {
        let line = line.trim();
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim().to_ascii_lowercase();
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        match key.as_str() {
            "registrar" if out.registrar.is_none() => {
                out.registrar = Some(value.to_string());
            }
            "creation date" | "created" | "registered" | "registration time"
                if out.creation_date.is_none() =>
            {
                out.creation_date = Some(value.to_string());
            }
            "registry expiry date"
            | "expiry date"
            | "expiration date"
            | "expiration time"
            | "expires"
                if out.expiry_date.is_none() =>
            {
                out.expiry_date = Some(value.to_string());
            }
            "name server" | "nserver" => {
                let ns = value.to_ascii_lowercase();
                if !out.name_servers.contains(&ns) {
                    out.name_servers.push(ns);
                }
            }
            _ => {}
        }
    }
    out
}

/// Find the IANA `refer:` line (case-insensitive) and return the referred
/// whois hostname, if any.
pub fn parse_referral(text: &str) -> Option<String> {
    text.lines().find_map(|line| {
        let line = line.trim();
        let (key, value) = line.split_once(':')?;
        if key.trim().eq_ignore_ascii_case("refer") {
            let host = value.trim().to_string();
            if host.is_empty() { None } else { Some(host) }
        } else {
            None
        }
    })
}

/// Char-boundary-safe truncation of raw whois text for the JSON response.
fn truncate_raw(text: &str) -> String {
    if text.len() <= RAW_TRUNCATE {
        return text.to_string();
    }
    let mut end = RAW_TRUNCATE;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…[truncated]", &text[..end])
}

// ---------------------------------------------------------------------------
// Network
// ---------------------------------------------------------------------------

/// One port-43 query: connect, send "{domain}\r\n", read until EOF or the
/// read budget expires. Returns the raw response text (lossy UTF-8: whois
/// bodies are overwhelmingly ASCII/Latin-1, never fail the request on
/// decoding).
async fn whois_query(host: &str, domain: &str) -> Result<String, String> {
    let mut stream = tokio::time::timeout(
        CONNECT_TIMEOUT,
        tokio::net::TcpStream::connect((host, WHOIS_PORT)),
    )
    .await
    .map_err(|_| format!("connect to {host}:{WHOIS_PORT} timed out"))?
    .map_err(|e| format!("connect to {host}:{WHOIS_PORT} failed: {e}"))?;

    let query = format!("{domain}\r\n");
    tokio::time::timeout(READ_BUDGET, stream.write_all(query.as_bytes()))
        .await
        .map_err(|_| format!("write to {host} timed out"))?
        .map_err(|e| format!("write to {host} failed: {e}"))?;

    let mut buf = Vec::with_capacity(4096);
    let read_all = async {
        let mut chunk = [0u8; 4096];
        loop {
            let n = stream
                .read(&mut chunk)
                .await
                .map_err(|e| format!("read from {host} failed: {e}"))?;
            if n == 0 {
                break; // EOF: server is done
            }
            let room = MAX_BYTES.saturating_sub(buf.len());
            if room == 0 {
                break; // cap reached: stop reading, keep what we have
            }
            buf.extend_from_slice(&chunk[..n.min(room)]);
        }
        Ok::<_, String>(())
    };
    tokio::time::timeout(READ_BUDGET, read_all)
        .await
        .map_err(|_| format!("read from {host} timed out after {READ_BUDGET:?}"))??;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

// ---------------------------------------------------------------------------
// Response + handler
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct WhoisInfo {
    pub registrar: Option<String>,
    pub creation_date: Option<String>,
    pub expiry_date: Option<String>,
    pub name_servers: Vec<String>,
    pub raw: String,
}

impl WhoisInfo {
    fn from_text(text: &str) -> Self {
        let f = parse_whois_fields(text);
        Self {
            registrar: f.registrar,
            creation_date: f.creation_date,
            expiry_date: f.expiry_date,
            name_servers: f.name_servers,
            raw: truncate_raw(text),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct WhoisResponse {
    pub domain: String,
    pub registry_whois: WhoisInfo,
    pub registrar_whois: Option<WhoisInfo>,
    pub generated_at: String,
}

/// GET /api/recon/whois?domain=example.com — thin IANA record plus one
/// referral hop. A failed referral still returns the registry record with
/// `registrar_whois: null` (honest partial, not a 502).
pub async fn lookup(
    State(_state): State<crate::routes::AppState>,
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
    let registry_text = whois_query(IANA_HOST, &domain)
        .await
        .map_err(|detail| ApiError::bad_gateway("whois lookup failed", detail))?;
    let registry_whois = WhoisInfo::from_text(&registry_text);

    // Follow exactly one referral; a dead registrar server degrades to null.
    let registrar_whois = match parse_referral(&registry_text) {
        Some(host) if !host.eq_ignore_ascii_case(IANA_HOST) => whois_query(&host, &domain)
            .await
            .ok()
            .map(|t| WhoisInfo::from_text(&t)),
        _ => None,
    };

    Ok(Json(WhoisResponse {
        domain,
        registry_whois,
        registrar_whois,
        generated_at: now_iso8601(),
    }))
}

pub fn route() -> Router<crate::routes::AppState> {
    Router::new().route("/api/recon/whois", get(lookup))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Abbreviated real IANA response shape for .com (fixture, not live).
    const IANA_COM: &str = "\
% IANA WHOIS server
domain:       COM
whois:        whois.verisign-grs.com

refer:        whois.verisign-grs.com
";

    /// Abbreviated Verisign thin-response shape (fixture, not live).
    const VERISIGN_COM: &str = "\
Domain Name: EXAMPLE.COM
Registrar: RESERVED-INTERNET ASSIGNED NUMBERS AUTHORITY
Creation Date: 1995-08-14T04:00:00Z
Registry Expiry Date: 2026-08-13T04:00:00Z
Name Server: A.IANA-SERVERS.NET
Name Server: B.IANA-SERVERS.NET
Name Server: A.IANA-SERVERS.NET
";

    #[test]
    fn referral_parsed_case_insensitively() {
        assert_eq!(
            parse_referral(IANA_COM),
            Some("whois.verisign-grs.com".to_string())
        );
        assert_eq!(parse_referral("no refer here\n"), None);
        assert_eq!(
            parse_referral("REFER:   WHOIS.EXAMPLE  \n"),
            Some("WHOIS.EXAMPLE".to_string())
        );
    }

    #[test]
    fn fields_extracted_best_effort() {
        let f = parse_whois_fields(VERISIGN_COM);
        assert_eq!(
            f.registrar.as_deref(),
            Some("RESERVED-INTERNET ASSIGNED NUMBERS AUTHORITY")
        );
        assert_eq!(f.creation_date.as_deref(), Some("1995-08-14T04:00:00Z"));
        assert_eq!(f.expiry_date.as_deref(), Some("2026-08-13T04:00:00Z"));
        assert_eq!(
            f.name_servers,
            vec!["a.iana-servers.net", "b.iana-servers.net"]
        );
    }

    #[test]
    fn nserver_alias_supported() {
        let f = parse_whois_fields("nserver: NS1.EXAMPLE.COM\nnserver: ns1.example.com\n");
        assert_eq!(f.name_servers, vec!["ns1.example.com"]);
    }

    #[test]
    fn empty_body_yields_empty_fields() {
        assert_eq!(parse_whois_fields(""), WhoisFields::default());
    }

    #[test]
    fn raw_truncation_is_char_boundary_safe() {
        let long = "é".repeat(RAW_TRUNCATE); // 2 bytes each -> over the byte cap
        let t = truncate_raw(&long);
        assert!(t.ends_with("…[truncated]"));
        assert!(t.len() <= RAW_TRUNCATE + "…[truncated]".len());
        let short = "abc";
        assert_eq!(truncate_raw(short), "abc");
    }
}
