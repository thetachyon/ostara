//! Integration tests for the RECON toolkit (Phase 2).
//!
//! Two layers:
//! 1. **Route contracts** — status codes and `{"error","detail"}` shapes for
//!    bad input, the gated scanner's 503-when-disabled, and a deterministic
//!    localhost scan (real TCP against a locally-bound listener; the only
//!    scan target that needs no authorization and no network).
//! 2. **Live upstream tests** — real requests to Google/Cloudflare DNS,
//!    whois.iana.org, crt.sh, ip-api.com, and NVD. These FAIL HONESTLY with
//!    diagnostics if the sandbox blocks the network: they never fake a
//!    response and are never silently skipped.

use std::sync::Arc;
use std::time::Duration;

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode, header},
};
use ostara::{cache::TtlCache, config::Config, recon, routes};
use tower::ServiceExt; // .oneshot()

fn test_client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent("ostara/0.1 phase2-recon-tests")
        .timeout(Duration::from_secs(20))
        .connect_timeout(Duration::from_secs(5))
        .pool_max_idle_per_host(4)
        .build()
        .expect("test client builds")
}

fn state_with(config: Config) -> routes::AppState {
    let config = Arc::new(config);
    routes::AppState {
        client: test_client(),
        cache: Arc::new(TtlCache::new()),
        vessels: ostara::feeds::vessels::VesselStream::new(config.ais_api_key.clone()),
        config,
    }
}

/// The exact RECON router the binary will mount: `recon::routes()`.
fn app_with(config: Config) -> Router {
    recon::routes().with_state(state_with(config))
}

fn app_disabled() -> Router {
    // Deterministic: no keys, recon disabled → degraded/gated paths.
    app_with(Config::test_default())
}

fn app_scan_enabled() -> Router {
    let mut cfg = Config::test_default();
    cfg.recon_enabled = true;
    app_with(cfg)
}

struct Got {
    status: StatusCode,
    body: serde_json::Value,
    raw: Vec<u8>,
}

async fn get(app: Router, uri: &str) -> Got {
    let req = Request::builder()
        .uri(uri)
        .body(Body::empty())
        .expect("request builds");
    roundtrip(app, req).await
}

async fn post_json(app: Router, uri: &str, payload: &serde_json::Value) -> Got {
    let req = Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            serde_json::to_vec(payload).expect("payload serializes"),
        ))
        .expect("request builds");
    roundtrip(app, req).await
}

async fn roundtrip(app: Router, req: Request<Body>) -> Got {
    let resp = app.oneshot(req).await.expect("oneshot serves");
    let status = resp.status();
    let raw = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("body reads")
        .to_vec();
    let body = serde_json::from_slice(&raw).unwrap_or(serde_json::Value::Null);
    Got { status, body, raw }
}

fn preview(g: &Got) -> String {
    String::from_utf8_lossy(&g.raw).chars().take(300).collect()
}

/// Every error response carries string `error` + `detail`.
fn assert_error_shape(g: &Got, route: &str, status: StatusCode) {
    assert_eq!(
        g.status, status,
        "{route}: expected {status}, got {}",
        g.status
    );
    assert!(
        g.body.get("error").and_then(|v| v.as_str()).is_some(),
        "{route}: body must carry string 'error', got: {}",
        preview(g)
    );
    assert!(
        g.body.get("detail").and_then(|v| v.as_str()).is_some(),
        "{route}: body must carry string 'detail', got: {}",
        preview(g)
    );
}

/// Live tests perform REAL network requests. On failure they panic with a
/// diagnostic — never a fake pass, never a silent skip.
fn live_fail(route: &str, g: &Got) -> ! {
    panic!(
        "LIVE TEST FAILED: {route} returned status {} with body: {}. \
         This test makes a real upstream request; the failure means the sandbox \
         blocked the network or the upstream errored — not a code defect. \
         See diagnostics above.",
        g.status,
        preview(g)
    )
}

// ---------------------------------------------------------------------------
// Route contracts: 400s
// ---------------------------------------------------------------------------

#[tokio::test]
async fn dns_requires_domain() {
    let g = get(app_disabled(), "/api/recon/dns").await;
    assert_error_shape(&g, "dns", StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn dns_rejects_junk_domains() {
    for uri in [
        "/api/recon/dns?domain=nodot",
        "/api/recon/dns?domain=bad!chars.com",
        "/api/recon/dns?domain=",
    ] {
        let g = get(app_disabled(), uri).await;
        assert_error_shape(&g, uri, StatusCode::BAD_REQUEST);
    }
}

#[tokio::test]
async fn whois_requires_domain() {
    let g = get(app_disabled(), "/api/recon/whois").await;
    assert_error_shape(&g, "whois", StatusCode::BAD_REQUEST);
    let g = get(app_disabled(), "/api/recon/whois?domain=nodot").await;
    assert_error_shape(&g, "whois", StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn ct_requires_domain() {
    let g = get(app_disabled(), "/api/recon/ct").await;
    assert_error_shape(&g, "ct", StatusCode::BAD_REQUEST);
    let g = get(app_disabled(), "/api/recon/ct?domain=nodot").await;
    assert_error_shape(&g, "ct", StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn ip_requires_valid_literal() {
    let g = get(app_disabled(), "/api/recon/ip").await;
    assert_error_shape(&g, "ip", StatusCode::BAD_REQUEST);
    for uri in ["/api/recon/ip?ip=not-an-ip", "/api/recon/ip?ip=999.1.1.1"] {
        let g = get(app_disabled(), uri).await;
        assert_error_shape(&g, uri, StatusCode::BAD_REQUEST);
    }
}

#[tokio::test]
async fn cve_requires_valid_id() {
    let g = get(app_disabled(), "/api/recon/cve").await;
    assert_error_shape(&g, "cve", StatusCode::BAD_REQUEST);
    for uri in ["/api/recon/cve?id=bogus", "/api/recon/cve?id=CVE-24-1"] {
        let g = get(app_disabled(), uri).await;
        assert_error_shape(&g, uri, StatusCode::BAD_REQUEST);
    }
}

// ---------------------------------------------------------------------------
// Scanner gating + validation contracts
// ---------------------------------------------------------------------------

#[tokio::test]
async fn scan_disabled_answers_503_by_design() {
    // Gate is checked FIRST: even a fully valid body gets 503, and no
    // connection attempt is made.
    let g = post_json(
        app_disabled(),
        "/api/recon/scan",
        &serde_json::json!({"target": "127.0.0.1", "ports": [80]}),
    )
    .await;
    assert_error_shape(&g, "scan", StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        g.body["error"].as_str(),
        Some("recon scanner disabled"),
        "unexpected error text: {}",
        preview(&g)
    );
}

#[tokio::test]
async fn scan_enabled_rejects_empty_ports() {
    let g = post_json(
        app_scan_enabled(),
        "/api/recon/scan",
        &serde_json::json!({"target": "127.0.0.1", "ports": []}),
    )
    .await;
    assert_error_shape(&g, "scan", StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn scan_enabled_rejects_too_many_ports() {
    // test_default caps at 10 ports per scan.
    let ports: Vec<u16> = (1..=11).collect();
    let g = post_json(
        app_scan_enabled(),
        "/api/recon/scan",
        &serde_json::json!({"target": "127.0.0.1", "ports": ports}),
    )
    .await;
    assert_error_shape(&g, "scan", StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn scan_enabled_rejects_empty_target() {
    let g = post_json(
        app_scan_enabled(),
        "/api/recon/scan",
        &serde_json::json!({"target": "  ", "ports": [80]}),
    )
    .await;
    assert_error_shape(&g, "scan", StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn scan_unresolvable_target_is_502() {
    // A 64-char DNS label is rejected by the resolver itself (no network
    // needed): deterministic resolution failure. Note: this sandbox's
    // system resolver wildcard-resolves ordinary names (e.g.
    // "nonexistent.invalid" -> 198.18.226.49), so a plain bogus name can
    // NOT be used to trigger the 502 path here.
    let bad_label = format!("{}.example.com", "a".repeat(64));
    let g = post_json(
        app_scan_enabled(),
        "/api/recon/scan",
        &serde_json::json!({"target": bad_label, "ports": [80]}),
    )
    .await;
    assert_error_shape(&g, "scan", StatusCode::BAD_GATEWAY);
}

/// Deterministic live scan: real TCP connects against a locally-bound
/// listener on 127.0.0.1 (no authorization questions, no network). The
/// bound port must report open; the adjacent port must not.
#[tokio::test]
async fn scan_localhost_finds_open_port() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("binds localhost");
    let open_port = listener.local_addr().expect("has addr").port();
    let closed_port = open_port.wrapping_add(1).max(1);

    let g = post_json(
        app_scan_enabled(),
        "/api/recon/scan",
        &serde_json::json!({"target": "127.0.0.1", "ports": [open_port, closed_port]}),
    )
    .await;
    drop(listener);

    assert_eq!(g.status, StatusCode::OK, "scan failed: {}", preview(&g));
    let open: Vec<u16> = serde_json::from_value(g.body["open_ports"].clone()).expect("open_ports");
    assert!(
        open.contains(&open_port),
        "bound port {open_port} not reported open: {}",
        preview(&g)
    );
    assert!(
        !open.contains(&closed_port),
        "adjacent port {closed_port} falsely reported open: {}",
        preview(&g)
    );
    assert_eq!(g.body["closed_count"].as_u64(), Some(1));
    assert_eq!(g.body["target"].as_str(), Some("127.0.0.1"));
    assert_eq!(g.body["resolved_ip"].as_str(), Some("127.0.0.1"));
    assert!(
        g.body["responsible_use"]
            .as_str()
            .is_some_and(|s| s.contains("authorized")),
        "responsible_use text missing: {}",
        preview(&g)
    );
}

// ---------------------------------------------------------------------------
// Live upstream tests — real network, honest failures with diagnostics.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn live_dns_example_com() {
    let g = get(app_disabled(), "/api/recon/dns?domain=example.com").await;
    if g.status != StatusCode::OK {
        live_fail("GET /api/recon/dns?domain=example.com", &g);
    }
    assert_eq!(g.body["domain"].as_str(), Some("example.com"));
    let a = g.body["a"].as_array().cloned().unwrap_or_default();
    assert!(
        !a.is_empty(),
        "example.com returned no A records: {}",
        preview(&g)
    );
    assert!(
        g.body["generated_at"].as_str().is_some(),
        "missing generated_at: {}",
        preview(&g)
    );
}

#[tokio::test]
async fn live_whois_example_com() {
    let g = get(app_disabled(), "/api/recon/whois?domain=example.com").await;
    if g.status != StatusCode::OK {
        live_fail("GET /api/recon/whois?domain=example.com", &g);
    }
    assert_eq!(g.body["domain"].as_str(), Some("example.com"));
    let raw = g.body["registry_whois"]["raw"].as_str().unwrap_or("");
    assert!(!raw.is_empty(), "empty registry raw text: {}", preview(&g));
    assert!(
        raw.len() <= 8 * 1024 + 64,
        "raw text not truncated: {}",
        raw.len()
    );
}

#[tokio::test]
async fn live_ct_example_com() {
    let g = get(app_disabled(), "/api/recon/ct?domain=example.com").await;
    if g.status != StatusCode::OK {
        live_fail("GET /api/recon/ct?domain=example.com", &g);
    }
    let count = g.body["count"].as_u64().unwrap_or(0);
    assert!(
        count >= 1,
        "example.com should have logged certs: {}",
        preview(&g)
    );
    let certs = g.body["certs"].as_array().cloned().unwrap_or_default();
    assert_eq!(certs.len() as u64, count);
    assert!(certs.len() <= 100, "cap of 100 exceeded");
    // Newest-first: not_before must be non-increasing.
    let dates: Vec<&str> = certs
        .iter()
        .filter_map(|c| c["not_before"].as_str())
        .collect();
    assert!(
        dates.windows(2).all(|w| w[0] >= w[1]),
        "certs not newest-first: {}",
        preview(&g)
    );
}

#[tokio::test]
async fn live_ip_8888() {
    // No token in test config → keyless ip-api.com path.
    let g = get(app_disabled(), "/api/recon/ip?ip=8.8.8.8").await;
    if g.status != StatusCode::OK {
        live_fail("GET /api/recon/ip?ip=8.8.8.8", &g);
    }
    assert_eq!(g.body["ip"].as_str(), Some("8.8.8.8"));
    assert_eq!(g.body["country_code"].as_str(), Some("US"));
    assert!(
        g.body["asn"].as_str().is_some_and(|s| s.contains("15169")),
        "unexpected asn: {}",
        preview(&g)
    );
}

#[tokio::test]
async fn live_cve_known() {
    let g = get(app_disabled(), "/api/recon/cve?id=CVE-2021-44228").await;
    if g.status != StatusCode::OK {
        live_fail("GET /api/recon/cve?id=CVE-2021-44228", &g);
    }
    assert_eq!(g.body["id"].as_str(), Some("CVE-2021-44228"));
    assert_eq!(g.body["cvss_v31"].as_f64(), Some(10.0));
    assert_eq!(g.body["severity"].as_str(), Some("CRITICAL"));
    assert!(
        g.body["description_en"]
            .as_str()
            .is_some_and(|d| d.contains("Log4j")),
        "unexpected description: {}",
        preview(&g)
    );
}

#[tokio::test]
async fn live_cve_unknown_is_404_not_502() {
    let g = get(app_disabled(), "/api/recon/cve?id=CVE-2024-99999999").await;
    if g.status != StatusCode::NOT_FOUND {
        live_fail("GET /api/recon/cve?id=CVE-2024-99999999 (expect 404)", &g);
    }
    assert_error_shape(&g, "cve", StatusCode::NOT_FOUND);
}
