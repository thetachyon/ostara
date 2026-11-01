//! RECON: DNS lookups (passive).
//!
//! Uses explicit Google (8.8.8.8) + Cloudflare (1.1.1.1) name servers — never
//! the system resolv.conf, which may not exist in sandboxed deployments.
//!
//! Transport note: queries go over DNS-over-TCP/53. Both providers serve TCP
//! on port 53, and TCP traverses restrictive egress networks that drop
//! outbound UDP datagrams (verified: this build sandbox returns EPERM on
//! UDP sendto to 8.8.8.8:53 while TCP/53 connects fine). Slightly more
//! handshake overhead per query than UDP, but the lookup actually works
//! from locked-down networks.

use std::net::SocketAddr;
use std::time::Duration;

use axum::{
    Json, Router,
    extract::{Query, State},
    response::IntoResponse,
    routing::get,
};
use hickory_resolver::{
    TokioAsyncResolver,
    config::{NameServerConfig, Protocol, ResolverConfig, ResolverOpts},
    proto::rr::{RData, RecordType},
};
use serde::{Deserialize, Serialize};

use crate::error::ApiError;
use crate::time::now_iso8601;

/// Per-record-type query budget: five types are queried concurrently, so the
/// worst case adds ~this on top of resolver retries.
const QUERY_TIMEOUT: Duration = Duration::from_secs(8);

/// Minimal domain validation: non-empty, DNS-legal characters, at least one
/// dot (avoids junk like "localhost" or bare words), labels non-empty and not
/// starting/ending with '-'. Returns the lowercased domain with any trailing
/// root dot stripped.
pub fn validate_domain(raw: &str) -> Result<String, ApiError> {
    let bad = |detail: &'static str| ApiError::bad_request("invalid domain", detail);
    let domain = raw.trim().trim_end_matches('.').to_ascii_lowercase();
    if domain.is_empty() {
        return Err(bad("domain must not be empty"));
    }
    if domain.len() > 253 {
        return Err(bad("domain exceeds 253 characters"));
    }
    if !domain.contains('.') {
        return Err(bad(
            "domain must contain at least one dot, e.g. ?domain=example.com",
        ));
    }
    if !domain
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.')
    {
        return Err(bad("domain contains invalid characters"));
    }
    for label in domain.split('.') {
        if label.is_empty() {
            return Err(bad("domain has an empty label"));
        }
        if label.len() > 63 {
            return Err(bad("a label exceeds 63 characters"));
        }
        if label.starts_with('-') || label.ends_with('-') {
            return Err(bad("labels must not start or end with '-'"));
        }
    }
    Ok(domain)
}

/// Resolver pinned to explicit public name servers (see module docs for why
/// TCP is used). Built per-request: construction is cheap (no I/O) and keeps
/// the handler free of shared mutable resolver state.
fn build_resolver() -> TokioAsyncResolver {
    let mut config = ResolverConfig::new();
    for addr in ["8.8.8.8:53", "1.1.1.1:53"] {
        let socket_addr: SocketAddr = addr.parse().expect("hardcoded socket addr parses");
        config.add_name_server(NameServerConfig::new(socket_addr, Protocol::Tcp));
    }
    let mut opts = ResolverOpts::default();
    opts.timeout = Duration::from_secs(5);
    opts.attempts = 2;
    TokioAsyncResolver::tokio(config, opts)
}

/// One record-type lookup. `Ok` on success (possibly zero records — a domain
/// with no MX records is not an error); `Err` with the resolver's message on
/// timeout / NXDOMAIN / transport failure.
async fn query_type(
    resolver: &TokioAsyncResolver,
    domain: &str,
    record_type: RecordType,
) -> Result<Vec<RData>, String> {
    let lookup = tokio::time::timeout(QUERY_TIMEOUT, resolver.lookup(domain, record_type))
        .await
        .map_err(|_| format!("{record_type} query timed out after {QUERY_TIMEOUT:?}"))?
        .map_err(|e| format!("{record_type} lookup failed: {e}"))?;
    Ok(lookup.iter().cloned().collect())
}

// ---------------------------------------------------------------------------
// Pure record extraction (unit-testable without network)
// ---------------------------------------------------------------------------

/// Trim the root dot hickory keeps on names ("example.com." -> "example.com").
fn trim_root_dot(name: &str) -> String {
    name.trim_end_matches('.').to_string()
}

pub fn extract_a(records: &[RData]) -> Vec<String> {
    records
        .iter()
        .filter_map(|r| match r {
            RData::A(a) => Some(a.0.to_string()),
            _ => None,
        })
        .collect()
}

pub fn extract_aaaa(records: &[RData]) -> Vec<String> {
    records
        .iter()
        .filter_map(|r| match r {
            RData::AAAA(a) => Some(a.0.to_string()),
            _ => None,
        })
        .collect()
}

#[derive(Debug, Clone, Serialize)]
pub struct MxRecord {
    pub preference: u16,
    pub exchange: String,
}

pub fn extract_mx(records: &[RData]) -> Vec<MxRecord> {
    let mut out: Vec<MxRecord> = records
        .iter()
        .filter_map(|r| match r {
            RData::MX(mx) => Some(MxRecord {
                preference: mx.preference(),
                exchange: trim_root_dot(&mx.exchange().to_string()),
            }),
            _ => None,
        })
        .collect();
    out.sort_by_key(|m| m.preference);
    out
}

pub fn extract_txt(records: &[RData]) -> Vec<String> {
    records
        .iter()
        .filter_map(|r| match r {
            RData::TXT(t) => {
                let joined: String = t
                    .txt_data()
                    .iter()
                    .map(|chunk| String::from_utf8_lossy(chunk).into_owned())
                    .collect();
                Some(joined)
            }
            _ => None,
        })
        .collect()
}

pub fn extract_ns(records: &[RData]) -> Vec<String> {
    records
        .iter()
        .filter_map(|r| match r {
            RData::NS(ns) => Some(trim_root_dot(&ns.to_string())),
            _ => None,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Response + handler
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct DnsResponse {
    pub domain: String,
    pub a: Vec<String>,
    pub aaaa: Vec<String>,
    pub mx: Vec<MxRecord>,
    pub txt: Vec<String>,
    pub ns: Vec<String>,
    pub generated_at: String,
}

#[derive(Debug, Deserialize)]
pub struct DomainQuery {
    pub domain: Option<String>,
}

/// GET /api/recon/dns?domain=example.com — A, AAAA, MX, TXT, NS queried
/// concurrently. Partial success is served (a type with no records, or a
/// failed type, yields an empty list); if every type fails the upstream is
/// down and the route answers 502.
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
    let resolver = build_resolver();
    let (a, aaaa, mx, txt, ns) = tokio::join!(
        query_type(&resolver, &domain, RecordType::A),
        query_type(&resolver, &domain, RecordType::AAAA),
        query_type(&resolver, &domain, RecordType::MX),
        query_type(&resolver, &domain, RecordType::TXT),
        query_type(&resolver, &domain, RecordType::NS),
    );
    let failures: Vec<&String> = [&a, &aaaa, &mx, &txt, &ns]
        .into_iter()
        .filter_map(|r| r.as_ref().err())
        .collect();
    if failures.len() == 5 {
        return Err(ApiError::bad_gateway(
            "dns lookup failed",
            failures[0].clone(),
        ));
    }
    let ok = |r: Result<Vec<RData>, String>| r.unwrap_or_default();
    Ok(Json(DnsResponse {
        domain,
        a: extract_a(&ok(a)),
        aaaa: extract_aaaa(&ok(aaaa)),
        mx: extract_mx(&ok(mx)),
        txt: extract_txt(&ok(txt)),
        ns: extract_ns(&ok(ns)),
        generated_at: now_iso8601(),
    }))
}

pub fn route() -> Router<crate::routes::AppState> {
    Router::new().route("/api/recon/dns", get(lookup))
}

#[cfg(test)]
mod tests {
    use super::*;
    use hickory_resolver::proto::rr::domain::Name;
    use hickory_resolver::proto::rr::rdata::{A, AAAA, MX, NS, TXT};
    use std::str::FromStr;

    #[test]
    fn valid_domain_normalizes() {
        assert_eq!(validate_domain("Example.COM.").unwrap(), "example.com");
        assert_eq!(
            validate_domain("  sub.example.co.uk ").unwrap(),
            "sub.example.co.uk"
        );
    }

    #[test]
    fn invalid_domains_rejected() {
        for bad in [
            "",
            "nodot",
            "bad!chars.com",
            "-lead.example.com",
            "trail-.example.com",
            "empty..label.com",
            ".leadingdot.com",
        ] {
            assert!(validate_domain(bad).is_err(), "should reject {bad:?}");
        }
    }

    fn fixture_records() -> Vec<RData> {
        vec![
            RData::A(A(std::net::Ipv4Addr::new(93, 184, 216, 34))),
            RData::AAAA(AAAA(std::net::Ipv6Addr::new(
                0x2606, 0x2800, 0x220, 1, 0x248, 0x1893, 0x25c8, 0x1946,
            ))),
            RData::MX(MX::new(10, Name::from_str("mail.example.com.").unwrap())),
            RData::MX(MX::new(5, Name::from_str("mx1.example.com.").unwrap())),
            RData::TXT(TXT::new(vec!["v=spf1 ".to_string(), "-all".to_string()])),
            RData::NS(NS(Name::from_str("ns1.example.com.").unwrap())),
        ]
    }

    #[test]
    fn extract_a_and_aaaa() {
        let r = fixture_records();
        assert_eq!(extract_a(&r), vec!["93.184.216.34"]);
        assert_eq!(extract_aaaa(&r), vec!["2606:2800:220:1:248:1893:25c8:1946"]);
    }

    #[test]
    fn extract_mx_sorted_by_preference_and_root_dot_trimmed() {
        let m = extract_mx(&fixture_records());
        assert_eq!(m.len(), 2);
        assert_eq!(m[0].preference, 5);
        assert_eq!(m[0].exchange, "mx1.example.com");
        assert_eq!(m[1].exchange, "mail.example.com");
    }

    #[test]
    fn extract_txt_joins_chunks() {
        assert_eq!(extract_txt(&fixture_records()), vec!["v=spf1 -all"]);
    }

    #[test]
    fn extract_ns_trims_root_dot() {
        assert_eq!(extract_ns(&fixture_records()), vec!["ns1.example.com"]);
    }

    #[test]
    fn extract_on_empty_input() {
        let empty: Vec<RData> = vec![];
        assert!(extract_a(&empty).is_empty());
        assert!(extract_mx(&empty).is_empty());
    }
}
