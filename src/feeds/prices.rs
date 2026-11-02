//! Prices feed: delayed market quotes via Yahoo Finance (keyless).
//!
//! Upstream (verified live 2026-11-02):
//! `GET https://query1.finance.yahoo.com/v8/finance/chart/{symbol}?interval=1d&range=1mo`
//! with a browser User-Agent — no auth, no crumb, no key. Per-symbol JSON:
//! `chart.result[0].meta` (symbol, currency, longName/shortName,
//! regularMarketPrice, chartPreviousClose, exchangeName),
//! `chart.result[0].indicators.quote[0]` (open/high/low/close/volume
//! arrays) and `chart.result[0].timestamp[]`. Unknown symbols return
//! `chart.error = {"code":"Not Found", ...}` — those land in `missing[]`,
//! never silently dropped.
//!
//! Stooq was evaluated first and REJECTED: `stooq.com/q/l/` now serves a
//! "page does not exist" page and `stooq.com/q/d/l/` serves a JS
//! proof-of-work browser challenge — neither is usable from a server.
//! (Checked 2026-11-02; re-verify before assuming it stays dead.)
//!
//! Coverage (all verified live): US equities, global indices (`^GSPC`),
//! FX (`EURUSD=X`), futures (`CL=F`), yields (`^TNX`). Quotes are delayed
//! (~15 min on most venues) — every response says so.
//!
//! One upstream request per symbol, fetched concurrently; the assembled
//! response is cached 300s under `prices:symbols=<sorted-lowercase>`, so
//! the default watchlist, `/api/stats`, and the health probe share one
//! cache entry.

use std::time::Duration;

use axum::{
    extract::{Query, State},
    response::IntoResponse,
};
use futures_util::future::join_all;
use serde::{Deserialize, Serialize};

use super::{cached_bytes, json_bytes};
use crate::{
    error::{ApiError, FeedError},
    routes::AppState,
    time::now_iso8601,
};

pub const TTL: Duration = Duration::from_secs(300);
pub const FEED: &str = "prices";
pub const MAX_SYMBOLS: usize = 40;

const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36";

/// Default watchlist: indices, Mag-7, FX majors, energy/metals/ags
/// futures, the 10y yield. Every symbol verified live 2026-11-02.
pub const DEFAULT_SYMBOLS: &[&str] = &[
    "^GSPC", "^NDX", "^DJI", "AAPL", "MSFT", "NVDA", "AMZN", "GOOG", "META", "TSLA", "EURUSD=X",
    "USDJPY=X", "GBPUSD=X", "CL=F", "BZ=F", "NG=F", "GC=F", "SI=F", "HG=F", "ZC=F", "ZS=F", "^TNX",
];

/// Minimal percent-encoding for the symbol path segment (`^` → `%5E`).
fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Unix timestamp → `YYYY-MM-DD` (Howard Hinnant's civil_from_days; no
/// chrono dependency for one conversion).
fn unix_to_ymd(ts: i64) -> String {
    let z = ts.div_euclid(86_400) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    format!("{:04}-{:02}-{:02}", if m <= 2 { y + 1 } else { y }, m, d)
}

// ---------------------------------------------------------------------------
// Yahoo v8 chart shapes (only the fields we use)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct ChartResponse {
    chart: ChartBody,
}

#[derive(Debug, Deserialize)]
struct ChartBody {
    error: Option<ChartError>,
    result: Option<Vec<ChartResult>>,
}

#[derive(Debug, Deserialize)]
struct ChartError {
    #[serde(default)]
    description: String,
}

#[derive(Debug, Default, Deserialize)]
struct ChartResult {
    #[serde(default)]
    meta: ChartMeta,
    #[serde(default)]
    timestamp: Vec<i64>,
    #[serde(default)]
    indicators: Indicators,
}

#[derive(Debug, Default, Deserialize)]
struct ChartMeta {
    #[serde(default)]
    symbol: String,
    #[serde(rename = "longName", default)]
    long_name: Option<String>,
    #[serde(rename = "shortName", default)]
    short_name: Option<String>,
    #[serde(default)]
    currency: Option<String>,
    #[serde(rename = "chartPreviousClose", default)]
    chart_previous_close: Option<f64>,
}

#[derive(Debug, Default, Deserialize)]
struct Indicators {
    #[serde(default)]
    quote: Vec<QuoteBars>,
}

#[derive(Debug, Default, Deserialize)]
struct QuoteBars {
    #[serde(default)]
    high: Vec<Option<f64>>,
    #[serde(default)]
    low: Vec<Option<f64>>,
    #[serde(default)]
    close: Vec<Option<f64>>,
    #[serde(default)]
    volume: Vec<Option<f64>>,
}

// ---------------------------------------------------------------------------
// Normalized shapes
// ---------------------------------------------------------------------------

/// One normalized quote.
#[derive(Debug, Serialize, Deserialize)]
pub struct Quote {
    pub symbol: String,
    pub name: String,
    pub price: f64,
    pub prev_close: Option<f64>,
    pub change: Option<f64>,
    pub change_pct: Option<f64>,
    pub day_high: Option<f64>,
    pub day_low: Option<f64>,
    pub volume: Option<u64>,
    /// `YYYY-MM-DD` of the last bar.
    pub date: Option<String>,
    /// Last ≤10 closes, oldest → newest (for sparklines).
    pub spark: Vec<f64>,
    /// As reported by Yahoo (e.g. "USD"; ag futures report "USX" = cents).
    pub currency: Option<String>,
}

/// A symbol that produced no quote in this response.
#[derive(Debug, Serialize, Deserialize)]
pub struct MissingSymbol {
    pub symbol: String,
    pub reason: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PricesResponse {
    pub count: usize,
    pub generated_at: String,
    pub source: String,
    pub quotes: Vec<Quote>,
    pub missing: Vec<MissingSymbol>,
}

/// One daily bar: (unix ts, close, high, low, volume).
type Bar = (i64, f64, Option<f64>, Option<f64>, Option<u64>);

/// Per-symbol failure: unknown symbol vs transport problem.
#[derive(Debug)]
enum SymbolFailure {
    NotFound(String),
    Transport(String),
}

/// Parse one Yahoo v8 chart body into a quote. Pure: no networking.
fn parse_chart(requested: &str, body: &str) -> Result<Quote, SymbolFailure> {
    let resp: ChartResponse = serde_json::from_str(body)
        .map_err(|e| SymbolFailure::Transport(format!("decode failed: {e}")))?;
    if let Some(err) = resp.chart.error {
        let reason = if err.description.is_empty() {
            "symbol not found".to_string()
        } else {
            err.description
        };
        return Err(SymbolFailure::NotFound(reason));
    }
    let r = resp
        .chart
        .result
        .and_then(|mut v| v.pop())
        .ok_or_else(|| SymbolFailure::NotFound("no data returned".to_string()))?;

    let quote = r.indicators.quote.into_iter().next().unwrap_or_default();
    // Bars are driven by timestamp × close; high/low/volume are optional
    // per bar (indices/yields often lack volume).
    let n = r.timestamp.len().min(quote.close.len());
    let mut bars: Vec<Bar> = Vec::new();
    for i in 0..n {
        if let Some(close) = quote.close[i] {
            bars.push((
                r.timestamp[i],
                close,
                quote.high.get(i).copied().flatten(),
                quote.low.get(i).copied().flatten(),
                quote.volume.get(i).copied().flatten().and_then(|v| {
                    if v >= 0.0 {
                        Some(v.round() as u64)
                    } else {
                        None
                    }
                }),
            ));
        }
    }
    if bars.is_empty() {
        return Err(SymbolFailure::NotFound("no trading data".to_string()));
    }

    let (ts, price, day_high, day_low, volume) = bars[bars.len() - 1];
    let prev_close = if bars.len() >= 2 {
        Some(bars[bars.len() - 2].1)
    } else {
        r.meta.chart_previous_close
    };
    let (change, change_pct) = match prev_close {
        Some(pc) if pc != 0.0 => {
            let ch = price - pc;
            (Some(ch), Some(ch / pc * 100.0))
        }
        _ => (None, None),
    };
    let spark: Vec<f64> = bars
        .iter()
        .rev()
        .take(10)
        .map(|b| b.1)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();

    let symbol = if r.meta.symbol.is_empty() {
        requested.to_string()
    } else {
        r.meta.symbol.clone()
    };
    let name = r
        .meta
        .long_name
        .or(r.meta.short_name)
        .filter(|n| !n.trim().is_empty())
        .unwrap_or_else(|| {
            symbol
                .trim_start_matches('^')
                .trim_end_matches("=X")
                .trim_end_matches("=F")
                .to_string()
        });

    Ok(Quote {
        symbol,
        name,
        price,
        prev_close,
        change,
        change_pct,
        day_high,
        day_low,
        volume,
        date: Some(unix_to_ymd(ts)),
        spark,
        currency: r.meta.currency,
    })
}

async fn fetch_one(client: &reqwest::Client, symbol: &str) -> Result<Quote, SymbolFailure> {
    let url = format!(
        "https://query1.finance.yahoo.com/v8/finance/chart/{}?interval=1d&range=1mo",
        url_encode(symbol)
    );
    let resp = client
        .get(&url)
        .header(reqwest::header::USER_AGENT, USER_AGENT)
        .send()
        .await
        .map_err(|e| SymbolFailure::Transport(format!("request failed: {e}")))?;
    if !resp.status().is_success() {
        return Err(SymbolFailure::Transport(format!(
            "upstream status {}",
            resp.status()
        )));
    }
    let body = resp
        .text()
        .await
        .map_err(|e| SymbolFailure::Transport(format!("read body failed: {e}")))?;
    parse_chart(symbol, &body)
}

/// Fetch quotes for all symbols concurrently. Symbols that fail are
/// reported in `missing`; only a total transport failure is an error.
pub async fn fetch_quotes(
    client: &reqwest::Client,
    symbols: &[String],
) -> Result<PricesResponse, FeedError> {
    let results = join_all(symbols.iter().map(|s| fetch_one(client, s))).await;
    let mut quotes = Vec::with_capacity(symbols.len());
    let mut missing = Vec::new();
    for (symbol, res) in symbols.iter().zip(results) {
        match res {
            Ok(q) => quotes.push(q),
            Err(SymbolFailure::NotFound(reason)) | Err(SymbolFailure::Transport(reason)) => missing
                .push(MissingSymbol {
                    symbol: symbol.clone(),
                    reason,
                }),
        }
    }
    if quotes.is_empty() {
        return Err(FeedError::new(
            FEED,
            format!(
                "all {} upstream requests failed (e.g. {})",
                symbols.len(),
                missing
                    .first()
                    .map(|m| m.reason.as_str())
                    .unwrap_or("unknown error")
            ),
        ));
    }
    Ok(PricesResponse {
        count: quotes.len(),
        generated_at: now_iso8601(),
        source: "Yahoo Finance — delayed quotes (~15 min on most venues), keyless".to_string(),
        quotes,
        missing,
    })
}

/// Short detail string for the health endpoint (2-symbol probe).
pub async fn probe_detail(client: &reqwest::Client) -> Result<String, FeedError> {
    let r = fetch_quotes(client, &["AAPL".to_string(), "EURUSD=X".to_string()]).await?;
    Ok(format!(
        "{} quotes via Yahoo Finance (delayed, keyless)",
        r.count
    ))
}

/// Cache key for a symbol set (sorted, lowercased) — shared by the
/// handler and `/api/stats` so both hit the same entry.
pub fn cache_key(symbols: &[String]) -> String {
    let mut sorted: Vec<String> = symbols.iter().map(|s| s.to_lowercase()).collect();
    sorted.sort();
    format!("prices:symbols={}", sorted.join(","))
}

/// Normalize the `symbols` query param: default watchlist when omitted,
/// uppercase + dedupe, max 40.
fn normalize_symbols(raw: Option<&str>) -> Result<Vec<String>, ApiError> {
    let mut list: Vec<String> = match raw.map(str::trim).filter(|s| !s.is_empty()) {
        None => DEFAULT_SYMBOLS.iter().map(|s| s.to_string()).collect(),
        Some(csv) => csv
            .split(',')
            .map(|s| s.trim().to_uppercase())
            .filter(|s| !s.is_empty())
            .collect(),
    };
    if list.is_empty() {
        return Err(ApiError::bad_request(
            "no symbols",
            "provide at least one symbol, e.g. ?symbols=AAPL,EURUSD=X",
        ));
    }
    if list.len() > MAX_SYMBOLS {
        return Err(ApiError::bad_request(
            "too many symbols",
            format!("max {MAX_SYMBOLS} symbols per request"),
        ));
    }
    let mut seen = std::collections::HashSet::new();
    list.retain(|s| seen.insert(s.clone()));
    Ok(list)
}

// ---------------------------------------------------------------------------
// GET /api/prices?symbols=
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct PricesQuery {
    pub symbols: Option<String>,
}

pub async fn handler(
    State(state): State<AppState>,
    Query(q): Query<PricesQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let symbols = normalize_symbols(q.symbols.as_deref())?;
    let key = cache_key(&symbols);
    let (body, hit) = cached_bytes(&state, &key, TTL, || {
        let client = state.client.clone();
        async move { fetch_quotes(&client, &symbols).await }
    })
    .await?;
    Ok(json_bytes(body, hit))
}

#[cfg(test)]
mod tests {
    use super::*;

    const AAPL_FIXTURE: &str = r#"{
      "chart": {
        "result": [{
          "meta": {"symbol": "AAPL", "currency": "USD", "longName": "Apple Inc.",
                   "exchangeName": "NMS", "chartPreviousClose": 338.5},
          "timestamp": [1761950400, 1762036800, 1762123200],
          "indicators": {"quote": [{
            "high": [340.0, 342.5, 343.0],
            "low": [336.0, 338.0, 339.5],
            "close": [339.0, 341.12, null],
            "volume": [100.0, 200.0, null]
          }]}
        }],
        "error": null
      }
    }"#;

    #[test]
    fn parses_bars_skips_null_close() {
        let q = parse_chart("AAPL", AAPL_FIXTURE).unwrap();
        assert_eq!(q.symbol, "AAPL");
        assert_eq!(q.name, "Apple Inc.");
        // last bar with a close is 341.12 (null close bar skipped)
        assert!((q.price - 341.12).abs() < 1e-9);
        assert!((q.prev_close.unwrap() - 339.0).abs() < 1e-9);
        assert!((q.change.unwrap() - 2.12).abs() < 1e-9);
        assert!((q.change_pct.unwrap() - 2.12 / 339.0 * 100.0).abs() < 1e-9);
        assert!((q.day_high.unwrap() - 342.5).abs() < 1e-9);
        assert!((q.day_low.unwrap() - 338.0).abs() < 1e-9);
        assert_eq!(q.volume, Some(200));
        assert_eq!(q.date.as_deref(), Some("2025-11-01"));
        assert_eq!(q.spark, vec![339.0, 341.12]);
        assert_eq!(q.currency.as_deref(), Some("USD"));
    }

    #[test]
    fn falls_back_to_chart_previous_close() {
        let one_bar = r#"{"chart":{"result":[{
          "meta":{"symbol":"X","chartPreviousClose":10.0},
          "timestamp":[1761950400],
          "indicators":{"quote":[{"close":[11.0]}]}}],"error":null}}"#;
        let q = parse_chart("X", one_bar).unwrap();
        assert!((q.prev_close.unwrap() - 10.0).abs() < 1e-9);
        assert!((q.change_pct.unwrap() - 10.0).abs() < 1e-9);
    }

    #[test]
    fn name_falls_back_to_cleaned_symbol() {
        let no_name = r#"{"chart":{"result":[{
          "meta":{"symbol":"EURUSD=X"},
          "timestamp":[1761950400],
          "indicators":{"quote":[{"close":[1.12]}]}}],"error":null}}"#;
        let q = parse_chart("eurusd=x", no_name).unwrap();
        assert_eq!(q.name, "EURUSD");
    }

    #[test]
    fn not_found_error_is_missing_not_transport() {
        let body = r#"{"chart":{"result":null,"error":{"code":"Not Found","description":"No data found, symbol may be delisted"}}}"#;
        match parse_chart("ZZZZ", body) {
            Err(SymbolFailure::NotFound(_)) => {}
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    #[test]
    fn empty_result_is_missing() {
        let body = r#"{"chart":{"result":[],"error":null}}"#;
        assert!(matches!(
            parse_chart("ZZZZ", body),
            Err(SymbolFailure::NotFound(_))
        ));
    }

    #[test]
    fn all_null_closes_is_missing() {
        let body = r#"{"chart":{"result":[{
          "meta":{"symbol":"X"},"timestamp":[1761950400],
          "indicators":{"quote":[{"close":[null]}]}}],"error":null}}"#;
        assert!(matches!(
            parse_chart("X", body),
            Err(SymbolFailure::NotFound(_))
        ));
    }

    #[test]
    fn bad_json_is_transport_failure() {
        assert!(matches!(
            parse_chart("X", "not json"),
            Err(SymbolFailure::Transport(_))
        ));
    }

    #[test]
    fn unix_to_ymd_known_dates() {
        assert_eq!(unix_to_ymd(0), "1970-01-01");
        assert_eq!(unix_to_ymd(1761950400), "2025-10-31");
        assert_eq!(unix_to_ymd(1762036800), "2025-11-01");
    }

    #[test]
    fn normalize_defaults_and_dedupes() {
        let d = normalize_symbols(None).unwrap();
        assert_eq!(d.len(), DEFAULT_SYMBOLS.len());
        let q = normalize_symbols(Some("aapl, AAPL, eurusd=x, ")).unwrap();
        assert_eq!(q, vec!["AAPL".to_string(), "EURUSD=X".to_string()]);
    }

    #[test]
    fn normalize_rejects_empty_and_too_many() {
        assert!(normalize_symbols(Some(" , ")).is_err());
        let many = (0..41)
            .map(|i| format!("S{i}"))
            .collect::<Vec<_>>()
            .join(",");
        assert!(normalize_symbols(Some(&many)).is_err());
    }

    #[test]
    fn url_encode_handles_caret() {
        assert_eq!(url_encode("^GSPC"), "%5EGSPC");
        assert_eq!(url_encode("EURUSD=X"), "EURUSD%3DX");
    }
}
