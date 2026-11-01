//! RECON: TCP port scanner (ACTIVE — gated).
//!
//! ═══════════════════════════════════════════════════════════════════════
//!  RESPONSIBLE USE — READ THIS BEFORE CALLING /api/recon/scan:
//!
//!  Port scanning is ACTIVE reconnaissance: it opens TCP connections to the
//!  target. Only scan infrastructure you OWN or have WRITTEN AUTHORIZATION
//!  to test. Unauthorized scanning can violate computer-misuse law (e.g.
//!  CFAA in the US), your employer's acceptable-use policy, and your
//!  provider's terms of service.
//!
//!  The scanner is DISABLED BY DEFAULT: with `RECON_ENABLED` unset/false
//!  the route answers 503 by design (mirroring OSIRIS's disabled scanner
//!  backend). Enabling it is an explicit operator decision, recorded in
//!  the deployment's environment — not something a request can flip.
//! ═══════════════════════════════════════════════════════════════════════
//!
//! Design: resolve the target once (`tokio::net::lookup_host`, first IP),
//! then connect to each port with a per-port timeout through a semaphore
//! capped at `Config::port_scan_max_concurrency`. Open = TCP handshake
//! completed within the timeout; anything else (refused, filtered, timed
//! out) counts as closed — the scanner does not distinguish filtered from
//! closed, and says so.

use std::time::{Duration, Instant};

use axum::{Json, Router, extract::State, response::IntoResponse, routing::post};
use serde::{Deserialize, Serialize};
use tokio::sync::Semaphore;

use crate::config::Config;
use crate::error::ApiError;
use crate::time::now_iso8601;

/// Echoed in every scan response, alongside the 503 detail when disabled.
pub const RESPONSIBLE_USE: &str = "Only scan infrastructure you own or are authorized to test.";

#[derive(Debug, Deserialize)]
pub struct ScanRequest {
    pub target: String,
    pub ports: Vec<u16>,
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Serialize)]
pub struct ScanResponse {
    pub target: String,
    pub resolved_ip: String,
    pub scanned_at: String,
    pub open_ports: Vec<u16>,
    pub closed_count: usize,
    pub duration_ms: u64,
    pub responsible_use: String,
}

/// Pure request validation: non-empty target, non-empty port list within the
/// configured cap. Returns the deduplicated, sorted port list.
pub fn validate_scan_request(req: &ScanRequest, config: &Config) -> Result<Vec<u16>, ApiError> {
    let target = req.target.trim();
    if target.is_empty() {
        return Err(ApiError::bad_request(
            "invalid target",
            "target must not be empty (hostname or IP literal)",
        ));
    }
    if target.len() > 253 {
        return Err(ApiError::bad_request(
            "invalid target",
            "target exceeds 253 characters",
        ));
    }
    if req.ports.is_empty() {
        return Err(ApiError::bad_request(
            "no ports specified",
            "ports must be a non-empty list, e.g. {\"target\":\"…\",\"ports\":[80,443]}",
        ));
    }
    if req.ports.len() > config.port_scan_max_ports {
        return Err(ApiError::bad_request(
            "too many ports",
            format!(
                "at most {} ports per scan (configured limit)",
                config.port_scan_max_ports
            ),
        ));
    }
    let mut ports = req.ports.clone();
    ports.sort_unstable();
    ports.dedup();
    Ok(ports)
}

/// Per-port timeout: an explicit override is honored but never exceeds the
/// configured default; a zero/omitted override falls back to the default.
pub fn effective_timeout_ms(req: &ScanRequest, config: &Config) -> u64 {
    match req.timeout_ms {
        Some(t) if t > 0 => t.min(config.port_scan_timeout_ms),
        _ => config.port_scan_timeout_ms,
    }
}

/// POST /api/recon/scan — gated ACTIVE scan. Gate is checked FIRST, before
/// any validation or DNS: a disabled scanner must not leak whether the
/// target resolves.
pub async fn scan(
    State(state): State<crate::routes::AppState>,
    Json(req): Json<ScanRequest>,
) -> Result<impl IntoResponse, ApiError> {
    // GATE FIRST: disabled by default → 503 by design.
    if !state.config.recon_enabled {
        return Err(ApiError::service_unavailable(
            "recon scanner disabled",
            "set RECON_ENABLED=true to enable; only scan infrastructure you own or are authorized to test",
        ));
    }

    let config = &state.config;
    let ports = validate_scan_request(&req, config)?;
    let per_port = Duration::from_millis(effective_timeout_ms(&req, config));

    // Resolve once; DNS failure is a 502 (upstream failure is normal).
    let target = req.target.trim().to_string();
    let resolved_ip = tokio::net::lookup_host((target.as_str(), 0))
        .await
        .map_err(|e| {
            ApiError::bad_gateway(
                "dns resolution failed",
                format!("could not resolve {target:?}: {e}"),
            )
        })?
        .next()
        .map(|sa| sa.ip())
        .ok_or_else(|| {
            ApiError::bad_gateway(
                "dns resolution failed",
                format!("no addresses returned for {target:?}"),
            )
        })?;

    let started = Instant::now();
    let semaphore = std::sync::Arc::new(Semaphore::new(config.port_scan_max_concurrency.max(1)));
    let port_count = ports.len();
    let mut handles = Vec::with_capacity(port_count);
    for port in ports {
        let sem = semaphore.clone();
        handles.push(tokio::spawn(async move {
            let _permit = sem.acquire_owned().await.expect("semaphore never closed");
            let open = tokio::time::timeout(
                per_port,
                tokio::net::TcpStream::connect((resolved_ip, port)),
            )
            .await
            .is_ok_and(|r| r.is_ok());
            (port, open)
        }));
    }
    let mut open_ports = Vec::new();
    for h in handles {
        let (port, open) = h.await.expect("scan task panicked");
        if open {
            open_ports.push(port);
        }
    }
    open_ports.sort_unstable();
    let closed_count = port_count - open_ports.len();

    Ok(Json(ScanResponse {
        target,
        resolved_ip: resolved_ip.to_string(),
        scanned_at: now_iso8601(),
        open_ports,
        closed_count,
        duration_ms: started.elapsed().as_millis() as u64,
        responsible_use: RESPONSIBLE_USE.to_string(),
    }))
}

pub fn route() -> Router<crate::routes::AppState> {
    Router::new().route("/api/recon/scan", post(scan))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(target: &str, ports: Vec<u16>, timeout_ms: Option<u64>) -> ScanRequest {
        ScanRequest {
            target: target.to_string(),
            ports,
            timeout_ms,
        }
    }

    #[test]
    fn validation_accepts_and_dedupes() {
        let cfg = Config::test_default();
        let ports =
            validate_scan_request(&req("127.0.0.1", vec![443, 80, 80], None), &cfg).unwrap();
        assert_eq!(ports, vec![80, 443]);
    }

    #[test]
    fn validation_rejects() {
        let cfg = Config::test_default(); // max_ports = 10
        assert!(validate_scan_request(&req("", vec![80], None), &cfg).is_err());
        assert!(validate_scan_request(&req("x", vec![], None), &cfg).is_err());
        assert!(validate_scan_request(&req("x", (1..=11).collect(), None), &cfg).is_err());
    }

    #[test]
    fn timeout_override_never_exceeds_default() {
        let cfg = Config::test_default(); // default 500ms
        assert_eq!(effective_timeout_ms(&req("x", vec![80], None), &cfg), 500);
        assert_eq!(
            effective_timeout_ms(&req("x", vec![80], Some(100)), &cfg),
            100
        );
        assert_eq!(
            effective_timeout_ms(&req("x", vec![80], Some(99999)), &cfg),
            500
        );
        assert_eq!(
            effective_timeout_ms(&req("x", vec![80], Some(0)), &cfg),
            500
        );
    }
}
