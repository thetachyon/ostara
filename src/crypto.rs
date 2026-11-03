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
    crypto_risk,
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

/// Known-entity address labels, baked in at compile time.
/// Only verified addresses belong here — a wrong label is worse than none.
static LABELS_JSON: &str = include_str!("../data/crypto_labels.json");

fn label_for(address: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(LABELS_JSON).ok()?;
    v.get("labels")?
        .get(address)?
        .as_str()
        .map(|s| s.to_string())
}

/// Truncate an address for display, keeping start/end for recognizability.
fn trunc_addr(a: &str) -> String {
    if a.len() <= 16 {
        a.to_string()
    } else {
        format!("{}…{}", &a[..8], &a[a.len() - 4..])
    }
}

pub fn router() -> Router<AppState> {
    Router::new().route("/api/crypto/transactions", get(transactions))
}

#[derive(Debug, Serialize, Deserialize)]
struct Party {
    /// Truncated address for display; full address in `address_full`.
    address: String,
    address_full: String,
    /// Value in the currency's main unit (outputs only; None for inputs).
    value: Option<f64>,
    /// Known-entity label, if the address is in the label registry.
    label: Option<String>,
    /// Heuristic: likely change output back to sender (single input, 2 outputs,
    /// one output address matches an input address or is the smaller output).
    likely_change: bool,
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
    /// Transaction inputs = senders (on-chain; not verified identities).
    senders: Vec<Party>,
    /// Transaction outputs = recipients (on-chain; not verified identities).
    recipients: Vec<Party>,
    /// Suspicious-pattern score 0-100 (heuristic, not attribution).
    risk_score: u8,
    risk_level: String,
    risk_reasons: Vec<String>,
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

const DISCLAIMER: &str = "ON-CHAIN DATA. Recent transactions via BlockCypher's keyless API (rate-limited). Values are as reported on-chain; unconfirmed transactions may never confirm. Senders/recipients are on-chain addresses from transaction inputs/outputs — NOT verified identities. 'Buyer' vs 'seller' cannot be determined from on-chain data alone; an address's economic role requires off-chain attribution. 'Likely change' is a heuristic (single input, 2 outputs) and may be wrong. This is a transaction list, not wallet surveillance — no addresses are tracked.";

#[derive(Debug, Deserialize)]
struct TxQuery {
    /// btc | eth | ltc | doge | all (default: all)
    #[serde(default)]
    currency: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
    /// only return transactions with risk_score >= this (0-100)
    #[serde(default)]
    min_risk: Option<u8>,
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
            let vin_sz = t.get("vin_sz").and_then(|x| x.as_u64()).unwrap_or(0) as usize;
            let vout_sz = t.get("vout_sz").and_then(|x| x.as_u64()).unwrap_or(0) as usize;
            let min_output = t
                .get("outputs")
                .and_then(|o| o.as_array())
                .map(|outs| {
                    outs.iter()
                        .filter_map(|o| o.get("value").and_then(|v| v.as_f64()))
                        .fold(f64::INFINITY, f64::min)
                })
                .filter(|m| m.is_finite())
                .unwrap_or(0.0)
                / divisor;
            let value = f("total") / divisor;
            let fee = f("fees") / divisor;
            let risk = crypto_risk::score_tx(ticker, value, fee, vin_sz, vout_sz, min_output);
            // Senders = input addresses.
            let sender_addrs: Vec<String> = t
                .get("inputs")
                .and_then(|i| i.as_array())
                .map(|ins| {
                    ins.iter()
                        .filter_map(|inp| {
                            inp.get("addresses")
                                .and_then(|a| a.as_array())
                                .and_then(|a| a.first())
                                .and_then(|x| x.as_str())
                        })
                        .take(6)
                        .map(|x| x.to_string())
                        .collect()
                })
                .unwrap_or_default();
            // Recipients = output addresses with values.
            let outputs: Vec<(String, f64)> = t
                .get("outputs")
                .and_then(|o| o.as_array())
                .map(|outs| {
                    outs.iter()
                        .filter_map(|o| {
                            let addr = o
                                .get("addresses")
                                .and_then(|a| a.as_array())
                                .and_then(|a| a.first())
                                .and_then(|x| x.as_str())?
                                .to_string();
                            let val = o.get("value").and_then(|v| v.as_f64()).unwrap_or(0.0) / divisor;
                            Some((addr, val))
                        })
                        .take(8)
                        .collect()
                })
                .unwrap_or_default();
            // Likely-change heuristic: single input, 2 outputs, and the smaller
            // output returns to the sender's address (or is much smaller).
            let single_sender = sender_addrs.len() == 1;
            let senders: Vec<Party> = sender_addrs
                .iter()
                .map(|a| Party {
                    address: trunc_addr(a),
                    address_full: a.clone(),
                    value: None,
                    label: label_for(a),
                    likely_change: false,
                })
                .collect();
            let recipients: Vec<Party> = outputs
                .iter()
                .map(|(a, v)| {
                    let is_change = single_sender
                        && outputs.len() == 2
                        && (sender_addrs.first().map(|s| s == a).unwrap_or(false)
                            || {
                                let other = outputs.iter().find(|(oa, _)| oa != a);
                                other.map(|(_, ov)| *v < *ov * 0.5).unwrap_or(false)
                            });
                    Party {
                        address: trunc_addr(a),
                        address_full: a.clone(),
                        value: Some(*v),
                        label: label_for(a),
                        likely_change: is_change,
                    }
                })
                .collect();
            Tx {
                hash: s("hash"),
                currency: name.to_string(),
                ticker: ticker.to_string(),
                value,
                fee,
                confirmations: t.get("confirmations").and_then(|x| x.as_u64()).unwrap_or(0),
                received_at: s("received"),
                addresses: addrs,
                senders,
                recipients,
                risk_score: risk.score,
                risk_level: risk.level.to_string(),
                risk_reasons: risk.reasons,
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
    let min_risk = p.min_risk.unwrap_or(0).min(100);

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
    let key = format!("crypto:txs:{cur}:limit={limit}:min_risk={min_risk}");

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
            // Suspicious first when filtering by risk, else newest first.
            if min_risk > 0 {
                all.retain(|t| t.risk_score >= min_risk);
                all.sort_by(|a, b| {
                    b.risk_score
                        .cmp(&a.risk_score)
                        .then(b.received_at.cmp(&a.received_at))
                });
            } else {
                all.sort_by(|a, b| b.received_at.cmp(&a.received_at));
            }
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
