//! Markets feed: CoinGecko top coins by market cap (keyless).
//!
//! Upstream (verified 2026-11-01 via live probe):
//! `GET https://api.coingecko.com/api/v3/coins/markets?vs_currency=usd&order=market_cap_desc&per_page=25&page=1&sparkline=false`
//! → JSON array of `{"id", "symbol", "name", "current_price",
//! "market_cap", "price_change_percentage_24h", ...}`.
//!
//! Keyless. CoinGecko's free tier is rate-limited (~5–15 req/min shared),
//! so the 5-minute TTL also protects the quota.

use std::time::Duration;

use axum::{extract::State, response::IntoResponse};
use serde::{Deserialize, Serialize};

use super::{cached_bytes, json_bytes};
use crate::{
    config::Config,
    error::{ApiError, FeedError},
    routes::AppState,
    time::now_iso8601,
};

/// Cache TTL: crypto prices move fast, but the free tier is rate-limited.
pub const TTL: Duration = Duration::from_secs(300);
pub const FEED: &str = "coingecko";

/// One normalized market entry.
#[derive(Debug, Serialize)]
pub struct Market {
    pub id: String,
    pub symbol: String,
    pub name: String,
    pub price_usd: f64,
    pub market_cap_usd: Option<f64>,
    pub change_24h_pct: Option<f64>,
}

#[derive(Debug, Serialize)]
pub struct MarketsResponse {
    pub count: usize,
    pub generated_at: String,
    pub markets: Vec<Market>,
}

#[derive(Debug, Deserialize)]
struct GeckoEntry {
    #[serde(default)]
    id: String,
    #[serde(default)]
    symbol: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    current_price: f64,
    #[serde(default)]
    market_cap: Option<f64>,
    #[serde(default)]
    price_change_percentage_24h: Option<f64>,
}

/// Parse + normalize a CoinGecko markets array. Pure: no networking.
pub fn parse_markets(body: &str) -> Result<Vec<Market>, FeedError> {
    let entries: Vec<GeckoEntry> = serde_json::from_str(body)
        .map_err(|e| FeedError::new(FEED, format!("decode failed: {e}")))?;
    Ok(entries
        .into_iter()
        .filter(|e| !e.id.is_empty())
        .map(|e| Market {
            id: e.id,
            symbol: e.symbol,
            name: e.name,
            price_usd: e.current_price,
            market_cap_usd: e.market_cap,
            change_24h_pct: e.price_change_percentage_24h,
        })
        .collect())
}

/// Fetch the top-25 markets by market cap.
pub async fn fetch(client: &reqwest::Client) -> Result<MarketsResponse, FeedError> {
    let body: String = super::get_text(
        client,
        FEED,
        "https://api.coingecko.com/api/v3/coins/markets?vs_currency=usd&order=market_cap_desc&per_page=25&page=1&sparkline=false",
    )
    .await?;
    let markets = parse_markets(&body)?;
    Ok(MarketsResponse {
        count: markets.len(),
        generated_at: now_iso8601(),
        markets,
    })
}

/// Short detail string for the health endpoint.
pub async fn probe_detail(client: &reqwest::Client, _config: &Config) -> Result<String, FeedError> {
    let r = fetch(client).await?;
    Ok(format!("{} markets (top by market cap)", r.count))
}

// ---------------------------------------------------------------------------
// GET /api/markets
// ---------------------------------------------------------------------------

pub async fn handler(State(state): State<AppState>) -> Result<impl IntoResponse, ApiError> {
    let client = state.client.clone();
    let (body, hit) = cached_bytes(
        &state,
        "markets",
        TTL,
        || async move { fetch(&client).await },
    )
    .await?;
    Ok(json_bytes(body, hit))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("../../benches/fixtures/coingecko.json");

    #[test]
    fn parses_fixture_markets() {
        let markets = parse_markets(FIXTURE).unwrap();
        assert_eq!(markets.len(), 2);
        let btc = &markets[0];
        assert_eq!(btc.id, "bitcoin");
        assert_eq!(btc.symbol, "btc");
        assert_eq!(btc.name, "Bitcoin");
        assert!((btc.price_usd - 78050.0).abs() < 1e-9);
        assert!((btc.market_cap_usd.unwrap() - 1_569_211_142_826.0).abs() < 1.0);
        assert!((btc.change_24h_pct.unwrap() - 0.68806).abs() < 1e-9);
    }

    #[test]
    fn empty_id_entries_dropped() {
        let markets = parse_markets(r#"[{"id":"","symbol":"x"}]"#).unwrap();
        assert!(markets.is_empty());
    }

    #[test]
    fn invalid_json_is_error() {
        assert!(parse_markets("not json").is_err());
    }
}
