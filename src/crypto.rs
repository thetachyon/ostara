//! Crypto transactions: recent on-chain transactions across multiple
//! currencies, keyless via BlockCypher.
//!
//! - **BlockCypher** (`api.blockcypher.com/v1/<chain>/main/txs`): recent
//!   unconfirmed + latest confirmed transactions for BTC, ETH, LTC, DOGE.
//!   Keyless free tier is rate-limited — cached accordingly.
//!
//! Values are normalized from base units (satoshi/wei/...) to the currency's
//! main unit. This is a *transaction list*, not wallet tracking: no
//! addresses are watched, no balances queried.

use std::time::Duration;

use axum::{Json, Router, extract::Query, routing::get};
use serde::{Deserialize, Serialize};

use crate::{
    error::{ApiError, FeedError},
    routes::{AppState, cached_bytes},
    time::now_iso8601,
};

const FEED: &str = "crypto";
const BLOCKCYPHER: &str = "https://api.blockcypher.com/v1";
/// BlockCypher keyless tier is heavily rate-limited; cache for 5 minutes.
const TTL: Duration = Duration::from_secs(300);

/// Supported chains: (api slug, display name, ticker, base-unit divisor).
const CHAINS: &[(&str, &str, &str, f64)] = &[
    ("btc", "Bitcoin", "BTC", 1e8),
    ("eth", "Ethereum", "ETH", 1e18),
    ("ltc", "Litecoin", "LTC", 1e8),
    ("doge", "Dogecoin", "DOGE", 1e8),
];

pub fn router() -> Router<AppState> {
    Router::new().route("/api/crypto/transactions", get(transactions))
}

#[derive(Debug, Serialize, Deserialize)]
struct Tx {
    hash: String,
    currency: String,
    ticker: String,
    /// Total value moved, in the currency's main unit.
    value: f64,
    /// Fee paid, in the currency's main unit.
    fee: f64,
    confirmations: u64,
    received_at: String,
    /// First few involved addresses (truncated for display).
    addresses: Vec<String>,
}

#[derive(Debug, Serialize)]
struct TransactionsResponse {
    currency: String,
    count: usize,
    transactions: Vec<Tx>,
    source_ok: bool,
    source_detail: String,
    scanned_at: String,
    disclaimer: String,
}

const DISCLAIMER: &str = "ON-CHAIN DATA. Recent transactions via BlockCypher's keyless API (rate-limited). Values are as reported on-chain; unconfirmed transactions may never confirm. This is a transaction list, not wallet surveillance — no addresses are tracked.";

#[derive(Debug, Deserialize)]
struct TxQuery {
    /// btc | eth | ltc | doge | all (default: all)
    #[serde(default)]
    currency: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

fn chain_info(slug: &str) -> Option<(&'static str, &'static str, &'static str, f64)> {
    CHAINS.iter().find(|c| c.0 == slug).copied()
}

async fn fetch_chain(
    client: &reqwest::Client,
    slug: &str,
    name: &str,
    ticker: &str,
    divisor: f64,
    limit: usize,
) -> Result<Vec<Tx>, FeedError> {
    let url = format!("{BLOCKCYPHER}/{slug}/main/txs?limit={}", limit.min(20));
    let resp = client
        .get(&url)
        .header("User-Agent", "ostara/0.1 (crypto intel)")
        .send()
        .await
        .map_err(|e| FeedError::new(FEED, format!("{ticker} request failed: {e}")))?;
    if !resp.status().is_success() {
        return Err(FeedError::new(
            FEED,
            format!("{ticker} upstream status {}", resp.status()),
        ));
    }
    let v: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| FeedError::new(FEED, format!("{ticker} decode failed: {e}")))?;
    let arr = match v {
        serde_json::Value::Array(a) => a,
        _ => {
            return Err(FeedError::new(
                FEED,
                format!("{ticker} unexpected response shape"),
            ));
        }
    };
    Ok(arr
        .into_iter()
        .map(|t| {
            let s = |k: &str| t.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
            let f = |k: &str| t.get(k).and_then(|x| x.as_f64()).unwrap_or(0.0);
            let addrs: Vec<String> = t
                .get("addresses")
                .and_then(|a| a.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str())
                        .take(4)
                        .map(|x| x.to_string())
                        .collect()
                })
                .unwrap_or_default();
            Tx {
                hash: s("hash"),
                currency: name.to_string(),
                ticker: ticker.to_string(),
                value: f("total") / divisor,
                fee: f("fees") / divisor,
                confirmations: t.get("confirmations").and_then(|x| x.as_u64()).unwrap_or(0),
                received_at: s("received"),
                addresses: addrs,
            }
        })
        .filter(|t| !t.hash.is_empty())
        .collect())
}

async fn transactions(
    axum::extract::State(state): axum::extract::State<AppState>,
    Query(p): Query<TxQuery>,
) -> Result<Json<TransactionsResponse>, ApiError> {
    use ApiError;
    let cur = p
        .currency
        .as_deref()
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "all".to_string());
    let limit = p.limit.unwrap_or(10).clamp(1, 20);

    let slugs: Vec<&str> = if cur == "all" {
        CHAINS.iter().map(|c| c.0).collect()
    } else if chain_info(&cur).is_some() {
        vec![cur.as_str()]
    } else {
        return Err(ApiError::bad_request(
            "invalid currency",
            "currency must be one of: btc, eth, ltc, doge, all",
        ));
    };

    // Per-currency limit when fetching "all" so no single chain dominates.
    let per_chain = if slugs.len() > 1 {
        (limit * 2).clamp(3, 10)
    } else {
        limit
    };
    let key = format!("crypto:txs:{cur}:limit={limit}");

    let fetch = || {
        let client = state.client.clone();
        let slugs2 = slugs.clone();
        async move {
            let mut all = Vec::new();
            let mut ok = 0;
            let mut detail = String::from("ok");
            for slug in slugs2 {
                let (s, name, ticker, divisor) = chain_info(slug).unwrap();
                match fetch_chain(&client, s, name, ticker, divisor, per_chain).await {
                    Ok(mut txs) => {
                        ok += 1;
                        all.append(&mut txs);
                    }
                    Err(e) => {
                        detail = e.detail;
                    }
                }
            }
            if ok == 0 {
                return Err(FeedError::new(FEED, detail));
            }
            // Newest first; cap at the requested limit.
            all.sort_by(|a, b| b.received_at.cmp(&a.received_at));
            all.truncate(limit);
            Ok::<Vec<Tx>, FeedError>(all)
        }
    };

    let (txs, source_ok, source_detail) =
        match cached_bytes::<Vec<Tx>, _>(&state, &key, TTL, fetch).await {
            Ok((body, _)) => (
                serde_json::from_slice::<Vec<Tx>>(&body).unwrap_or_default(),
                true,
                "ok".to_string(),
            ),
            Err(e) => (Vec::new(), false, e.user_message()),
        };

    Ok(Json(TransactionsResponse {
        currency: cur,
        count: txs.len(),
        transactions: txs,
        source_ok,
        source_detail,
        scanned_at: now_iso8601(),
        disclaimer: DISCLAIMER.into(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chain_info_resolves() {
        assert!(chain_info("btc").is_some());
        assert!(chain_info("eth").is_some());
        assert!(chain_info("xrp").is_none());
    }
}
