//! Runtime configuration: every API key and operational knob comes from the
//! environment, optionally via a `.env` file (loaded with `dotenvy`).
//! See `.env.example` for the full documented list.
//!
//! Design rule (mirrors OSIRIS): a keyed feed DEGRADES when its key is unset —
//! it falls back to a keyless alternative where one exists, or answers 503
//! with `{"error","detail"}` like OSIRIS's own disabled RECON backend. A
//! missing key never panics and never breaks other feeds. Empty strings
//! count as unset.

use std::sync::Arc;

fn env_opt(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn env_bool(name: &str, default: bool) -> bool {
    std::env::var(name)
        .ok()
        .map(|v| {
            matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(default)
}

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(default)
}

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(default)
}

/// Listen port: container hosts (fly.io, Railway, Render) inject `PORT`.
/// It takes precedence; `OSIRIS_PORT` remains the explicit override for
/// self-hosted runs; default 3000. A value of 0 or an unparsable value
/// falls through to the next source.
fn port_from_env() -> u16 {
    for name in ["PORT", "OSIRIS_PORT"] {
        if let Ok(v) = std::env::var(name)
            && let Ok(p) = v.trim().parse::<u16>()
            && p != 0
        {
            return p;
        }
    }
    3000
}

fn env_list(name: &str) -> Vec<String> {
    std::env::var(name)
        .ok()
        .map(|v| {
            v.split(',')
                .map(|s| s.trim().trim_start_matches('@').to_string())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// All runtime configuration. Cloneable and shared via `Arc` in `AppState`.
#[derive(Debug, Clone)]
pub struct Config {
    // -- Keyed feeds (env names mirror OSIRIS's own conventions 1:1) --------
    /// NASA FIRMS API key — required for /api/fires (no keyless fallback).
    pub firms_api_key: Option<String>,
    /// OpenSky OAuth2 client credentials — preferred aviation source when
    /// set; falls back to adsb.lol (keyless).
    pub opensky_client_id: Option<String>,
    pub opensky_client_secret: Option<String>,
    /// N2YO API key — satellite catalog when set; falls back to CelesTrak TLE.
    pub n2yo_api_key: Option<String>,
    /// AIS vessel-tracking API key (e.g. AISStream) — unset means
    /// /api/vessels answers 503.
    pub ais_api_key: Option<String>,
    /// ThreatFox Auth-Key (free at https://auth.abuse.ch) — required for
    /// /api/threatfox and the ThreatFox half of /api/cyber/threats.
    /// Verified 2026-11-01: the Community API rejects keyless requests
    /// with HTTP 401.
    pub threatfox_api_key: Option<String>,

    // -- Telegram OSINT layer (keyless: t.me/s previews) ---------------------
    /// Comma-separated public channel names (no @); empty = any public
    /// channel via ?channel=, validated against this list when non-empty.
    pub telegram_channels: Vec<String>,

    // -- RECON enrichment keys (optional; keyless fallbacks exist) -----------
    /// NVD API key — raises CVE rate limits; the lookup works keyless.
    pub nvd_api_key: Option<String>,
    /// ipinfo.io token — richer IP enrichment; falls back to ip-api.com.
    pub ipinfo_token: Option<String>,

    // -- Active recon gating --------------------------------------------------
    /// Master switch for active scanning. Mirrors OSIRIS's disabled-by-default
    /// RECON backend: when false, /api/recon/scan answers 503 by design.
    /// Only scan infrastructure you own or are authorized to test.
    pub recon_enabled: bool,
    pub port_scan_max_concurrency: usize,
    pub port_scan_timeout_ms: u64,
    pub port_scan_max_ports: usize,

    // -- Authenticated ingest (reserved) --------------------------------------
    /// Shared secret for a future ingest endpoint. Fails closed when unset.
    pub sdk_ingest_key: Option<String>,

    // -- Server ---------------------------------------------------------------
    /// Mirrors OSIRIS's OSIRIS_PORT convention.
    pub port: u16,
}

impl Config {
    pub fn from_env() -> Self {
        // A missing .env is fine — real env vars take precedence either way.
        let _ = dotenvy::dotenv();
        Self {
            firms_api_key: env_opt("FIRMS_API_KEY"),
            opensky_client_id: env_opt("OPENSKY_CLIENT_ID"),
            opensky_client_secret: env_opt("OPENSKY_CLIENT_SECRET"),
            n2yo_api_key: env_opt("N2YO_API_KEY"),
            ais_api_key: env_opt("AIS_API_KEY"),
            threatfox_api_key: env_opt("THREATFOX_API_KEY"),
            telegram_channels: env_list("TELEGRAM_CHANNELS"),
            nvd_api_key: env_opt("NVD_API_KEY"),
            ipinfo_token: env_opt("IPINFO_TOKEN"),
            recon_enabled: env_bool("RECON_ENABLED", false),
            port_scan_max_concurrency: env_usize("PORT_SCAN_MAX_CONCURRENCY", 64),
            port_scan_timeout_ms: env_u64("PORT_SCAN_TIMEOUT_MS", 1500),
            port_scan_max_ports: env_usize("PORT_SCAN_MAX_PORTS", 1000),
            sdk_ingest_key: env_opt("SDK_INGEST_KEY"),
            port: port_from_env(),
        }
    }

    /// Convenience for handlers: is the named key configured?
    pub fn has(key: &Option<String>) -> bool {
        key.is_some()
    }

    /// Deterministic config for tests: no keys set, recon disabled, so
    /// keyed feeds exercise their degraded paths.
    pub fn test_default() -> Self {
        Self {
            firms_api_key: None,
            opensky_client_id: None,
            opensky_client_secret: None,
            n2yo_api_key: None,
            ais_api_key: None,
            threatfox_api_key: None,
            telegram_channels: Vec::new(),
            nvd_api_key: None,
            ipinfo_token: None,
            recon_enabled: false,
            port_scan_max_concurrency: 8,
            port_scan_timeout_ms: 500,
            port_scan_max_ports: 10,
            sdk_ingest_key: None,
            port: 3000,
        }
    }
}

/// Shared config handle stored in `AppState`.
pub type SharedConfig = Arc<Config>;

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, OnceLock};

    /// Config tests mutate process env vars, which is process-global:
    /// serialize them so parallel test threads cannot race each other.
    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    #[test]
    fn empty_string_counts_as_unset() {
        let _guard = env_lock().lock().unwrap();
        // SAFETY: single-threaded test manipulating process env for its own
        // assertions; no other test reads these vars concurrently.
        unsafe {
            std::env::set_var("FIRMS_API_KEY", "");
            std::env::set_var("RECON_ENABLED", "");
        }
        let c = Config::from_env();
        assert!(c.firms_api_key.is_none());
        assert!(!c.recon_enabled);
        unsafe {
            std::env::remove_var("FIRMS_API_KEY");
            std::env::remove_var("RECON_ENABLED");
        }
    }

    #[test]
    fn port_prefers_port_env_over_osiris_port() {
        let _guard = env_lock().lock().unwrap();
        // SAFETY: single-threaded test manipulating process env for its own
        // assertions; no other test reads these vars concurrently.
        unsafe {
            std::env::set_var("PORT", "8080");
            std::env::set_var("OSIRIS_PORT", "9000");
        }
        assert_eq!(Config::from_env().port, 8080);
        unsafe {
            std::env::remove_var("PORT");
        }
        assert_eq!(Config::from_env().port, 9000);
        unsafe {
            std::env::set_var("PORT", "not-a-port");
        }
        assert_eq!(Config::from_env().port, 9000);
        unsafe {
            std::env::remove_var("PORT");
            std::env::remove_var("OSIRIS_PORT");
        }
        assert_eq!(Config::from_env().port, 3000);
    }

    #[test]
    fn bool_parsing_accepts_common_spellings() {
        let _guard = env_lock().lock().unwrap();
        unsafe {
            std::env::set_var("RECON_ENABLED", "yes");
        }
        assert!(Config::from_env().recon_enabled);
        unsafe {
            std::env::set_var("RECON_ENABLED", "0");
        }
        assert!(!Config::from_env().recon_enabled);
        unsafe {
            std::env::remove_var("RECON_ENABLED");
        }
    }

    #[test]
    fn channel_list_parsing() {
        let _guard = env_lock().lock().unwrap();
        unsafe {
            std::env::set_var("TELEGRAM_CHANNELS", " @alpha,beta ,, gamma ");
        }
        let c = Config::from_env();
        assert_eq!(c.telegram_channels, vec!["alpha", "beta", "gamma"]);
        unsafe {
            std::env::remove_var("TELEGRAM_CHANNELS");
        }
    }
}
