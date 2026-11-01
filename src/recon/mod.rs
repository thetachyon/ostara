//! RECON toolkit: passive OSINT lookups and one gated active scanner.
//!
//! Passive lookups (DNS, WHOIS, certificate transparency, IP/ASN, CVE) query
//! third-party datasets about the subject — they generate no traffic against
//! the subject itself and are always on. The TCP port scanner is ACTIVE:
//! it is gated by `RECON_ENABLED` (default false → 503 by design, mirroring
//! OSIRIS's disabled scanner backend) and concurrency/timeout/port caps from
//! `Config`.
//!
//! RESPONSIBLE USE: only point the scanner at infrastructure you own or have
//! written authorization to test.

pub mod ct;
pub mod cve;
pub mod dns;
pub mod ip;
pub mod scan;
pub mod whois;

/// Canonical RECON router: all six Phase 2 endpoints in one place.
/// The parent wires this into the main router; handlers take
/// `State<crate::routes::AppState>` so no extra state plumbing is needed.
pub fn routes() -> axum::Router<crate::routes::AppState> {
    ct::route()
        .merge(cve::route())
        .merge(dns::route())
        .merge(ip::route())
        .merge(scan::route())
        .merge(whois::route())
}
