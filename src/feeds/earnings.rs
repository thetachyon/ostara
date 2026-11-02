//! Earnings calendar: Nasdaq's keyless earnings-calendar API.
//!
//! Upstream (verified live 2026-11-02):
//! `GET https://api.nasdaq.com/api/calendar/earnings?date=YYYY-MM-DD`
//! with a browser User-Agent — no auth, no key. Shape: `data.rows[]` with
//! `time` ("time-pre-market" | "time-after-hours"), `symbol`, `name`,
//! `marketCap` ("$92,963,168,000"), `fiscalQuarterEnding`, `epsForecast`
//! ("$3.90", "" when none, "($0.05)" for negatives), `noOfEsts`.
//! Empty dates (weekends) return `"rows": []`; far-future dates return
//! `"data": null` with status "Earnings Calendar: No record found." —
//! both normalize to an empty list, never an error.
//!
//! Limits, stated honestly: the upstream API is date-indexed. There is no
//! symbol-lookup endpoint, so `?symbol=` returns a 400 explaining that
//! (we do not fake it). There is no revenue-estimate field in the rows,
//! so none is served. Cache TTL is 3600s — earnings calendars change
//! slowly.

use std::time::Duration;

use axum::{
    extract::{Query, State},
    response::IntoResponse,
};
use serde::{Deserialize, Serialize};

use super::{cached_bytes, json_bytes};
use crate::{
    error::{ApiError, FeedError},
    feeds::econ_calendar::{parse_ymd as strict_ymd, today_ymd},
    routes::AppState,
    time::now_iso8601,
};

pub const TTL: Duration = Duration::from_secs(3600);
pub const FEED: &str = "earnings";
pub const SOURCE: &str = "Nasdaq earnings calendar — keyless, as reported";

const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36";

// ---------------------------------------------------------------------------
// Nasdaq shapes (only the fields we use)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct NasdaqResponse {
    data: Option<NasdaqData>,
}

#[derive(Debug, Deserialize)]
struct NasdaqData {
    #[serde(default)]
    rows: Option<Vec<NasdaqRow>>,
}

#[derive(Debug, Default, Deserialize)]
struct NasdaqRow {
    #[serde(default)]
    symbol: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    time: String,
    #[serde(rename = "marketCap", default)]
    market_cap: String,
    #[serde(rename = "fiscalQuarterEnding", default)]
    fiscal_quarter_ending: Option<String>,
    #[serde(rename = "epsForecast", default)]
    eps_forecast: String,
    #[serde(rename = "noOfEsts", default)]
    no_of_ests: String,
    #[serde(rename = "lastYearEPS", default)]
    last_year_eps: String,
}

// ---------------------------------------------------------------------------
// Normalized shapes
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct Earning {
    pub symbol: String,
    pub name: String,
    /// "bmo" (before market open) | "amc" (after market close) | "dmh"
    /// (during market hours).
    pub time: String,
    pub eps_estimate: Option<f64>,
    pub market_cap: Option<u64>,
    pub fiscal_quarter_ending: Option<String>,
    pub last_year_eps: Option<f64>,
    pub num_estimates: Option<u32>,
}

#[derive(Debug, Serialize)]
pub struct EarningsResponse {
    pub date: String,
    pub generated_at: String,
    pub source: String,
    pub count: usize,
    pub earnings: Vec<Earning>,
}

/// Parse a Nasdaq money string: "$3.90" → 3.90, "($0.05)" → -0.05,
/// "$92,963,168,000" → 92963168000.0, "" / "N/A" → None.
fn parse_money(s: &str) -> Option<f64> {
    let t = s.trim();
    if t.is_empty() || t.eq_ignore_ascii_case("n/a") {
        return None;
    }
    let neg = t.starts_with('(') && t.ends_with(')');
    let inner: String = t
        .trim_start_matches('(')
        .trim_end_matches(')')
        .chars()
        .filter(|c| *c != '$' && *c != ',')
        .collect();
    let v: f64 = inner.parse().ok()?;
    Some(if neg { -v } else { v })
}

fn map_time(raw: &str) -> String {
    match raw {
        "time-pre-market" => "bmo".to_string(),
        "time-after-hours" => "amc".to_string(),
        "time-during-market" => "dmh".to_string(),
        other => other.trim_start_matches("time-").to_string(),
    }
}

/// Parse one Nasdaq calendar body into normalized earnings. Pure: no
/// networking. `data: null` and `rows: null` both mean "no earnings
/// scheduled" — an empty vec, not an error.
fn parse_earnings(body: &str) -> Result<Vec<Earning>, FeedError> {
    let resp: NasdaqResponse = serde_json::from_str(body)
        .map_err(|e| FeedError::new(FEED, format!("decode failed: {e}")))?;
    let rows = resp.data.and_then(|d| d.rows).unwrap_or_default();
    Ok(rows
        .into_iter()
        .filter(|r| !r.symbol.trim().is_empty())
        .map(|r| Earning {
            symbol: r.symbol.trim().to_string(),
            name: r.name.trim().to_string(),
            time: map_time(&r.time),
            eps_estimate: parse_money(&r.eps_forecast),
            market_cap: parse_money(&r.market_cap).and_then(|v| {
                if v >= 0.0 {
                    Some(v.round() as u64)
                } else {
                    None
                }
            }),
            fiscal_quarter_ending: r
                .fiscal_quarter_ending
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
            last_year_eps: parse_money(&r.last_year_eps),
            num_estimates: r.no_of_ests.trim().parse().ok(),
        })
        .collect())
}

pub async fn fetch_earnings(
    client: &reqwest::Client,
    date: &str,
) -> Result<EarningsResponse, FeedError> {
    let url = format!("https://api.nasdaq.com/api/calendar/earnings?date={date}");
    let resp = client
        .get(&url)
        .header(reqwest::header::USER_AGENT, USER_AGENT)
        .send()
        .await
        .map_err(|e| FeedError::new(FEED, format!("request failed: {e}")))?;
    if !resp.status().is_success() {
        return Err(FeedError::new(
            FEED,
            format!("upstream status {}", resp.status()),
        ));
    }
    let body = resp
        .text()
        .await
        .map_err(|e| FeedError::new(FEED, format!("read body failed: {e}")))?;
    let earnings = parse_earnings(&body)?;
    Ok(EarningsResponse {
        date: date.to_string(),
        generated_at: now_iso8601(),
        source: SOURCE.to_string(),
        count: earnings.len(),
        earnings,
    })
}

/// Cache key: one entry per date.
pub fn cache_key(date: &str) -> String {
    format!("earnings:date={date}")
}

/// Short detail string for the health endpoint (today's calendar).
pub async fn probe_detail(client: &reqwest::Client) -> Result<String, FeedError> {
    let r = fetch_earnings(client, &today_ymd()).await?;
    Ok(format!(
        "{} earnings on {} via Nasdaq (keyless)",
        r.count, r.date
    ))
}

// ---------------------------------------------------------------------------
// GET /api/earnings?date=
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct EarningsQuery {
    pub date: Option<String>,
    pub symbol: Option<String>,
}

pub async fn handler(
    State(state): State<AppState>,
    Query(q): Query<EarningsQuery>,
) -> Result<impl IntoResponse, ApiError> {
    // The Nasdaq calendar API is date-indexed; there is no symbol lookup.
    if let Some(sym) = q
        .symbol
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
    {
        return Err(ApiError::bad_request(
            "symbol lookup unsupported",
            format!(
                "the Nasdaq earnings calendar is date-indexed — no upstream endpoint \
                 resolves a next-earnings date for '{sym}'. Use ?date=YYYY-MM-DD instead."
            ),
        ));
    }
    let date = match q
        .date
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
    {
        None => today_ymd(),
        Some(s) => {
            if strict_ymd(&s).is_none() {
                return Err(ApiError::bad_request(
                    "bad date",
                    format!("expected YYYY-MM-DD, got '{s}'"),
                ));
            }
            s
        }
    };
    let key = cache_key(&date);
    let (body, hit) = cached_bytes(&state, &key, TTL, || {
        let client = state.client.clone();
        let date = date.clone();
        async move { fetch_earnings(&client, &date).await }
    })
    .await?;
    Ok(json_bytes(body, hit))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROWS_FIXTURE: &str = r#"{
      "data": {
        "asOf": "Mon, Nov 9, 2026",
        "rows": [
          {"symbol":"CEG","name":"Constellation Energy Corporation","time":"time-pre-market",
           "marketCap":"$92,963,168,000","fiscalQuarterEnding":"Sep/2026",
           "epsForecast":"$3.90","noOfEsts":"4","lastYearEPS":"$3.04"},
          {"symbol":"RKLB","name":"Rocket Lab Corporation","time":"time-after-hours",
           "marketCap":"$34,357,291,000","fiscalQuarterEnding":"Sep/2026",
           "epsForecast":"($0.05)","noOfEsts":"2","lastYearEPS":"($0.01)"},
          {"symbol":"IX","name":"ORIX Corporation","time":"time-pre-market",
           "marketCap":"$40,129,995,000","fiscalQuarterEnding":"Sep/2026",
           "epsForecast":"","noOfEsts":"N/A","lastYearEPS":"$0.64"}
        ]
      }
    }"#;

    #[test]
    fn parses_rows_money_and_time() {
        let e = parse_earnings(ROWS_FIXTURE).unwrap();
        assert_eq!(e.len(), 3);
        assert_eq!(e[0].symbol, "CEG");
        assert_eq!(e[0].time, "bmo");
        assert!((e[0].eps_estimate.unwrap() - 3.90).abs() < 1e-9);
        assert_eq!(e[0].market_cap, Some(92_963_168_000));
        assert_eq!(e[0].fiscal_quarter_ending.as_deref(), Some("Sep/2026"));
        assert!((e[0].last_year_eps.unwrap() - 3.04).abs() < 1e-9);
        assert_eq!(e[0].num_estimates, Some(4));
        // parenthesized negatives
        assert_eq!(e[1].time, "amc");
        assert!((e[1].eps_estimate.unwrap() + 0.05).abs() < 1e-9);
        assert!((e[1].last_year_eps.unwrap() + 0.01).abs() < 1e-9);
        // blank forecast / N/A estimates
        assert_eq!(e[2].eps_estimate, None);
        assert_eq!(e[2].num_estimates, None);
    }

    #[test]
    fn data_null_means_empty_not_error() {
        let body = r#"{"data":null,"message":null,"status":{"rCode":200}}"#;
        assert_eq!(parse_earnings(body).unwrap().len(), 0);
    }

    #[test]
    fn rows_null_means_empty_not_error() {
        let body = r#"{"data":{"asOf":"x","rows":null}}"#;
        assert_eq!(parse_earnings(body).unwrap().len(), 0);
    }

    #[test]
    fn empty_rows_is_empty() {
        let body = r#"{"data":{"asOf":"Sun, Nov 8, 2026","rows":[]}}"#;
        assert_eq!(parse_earnings(body).unwrap().len(), 0);
    }

    #[test]
    fn blank_symbol_rows_dropped() {
        let body = r#"{"data":{"rows":[{"symbol":" ","name":"x","time":"time-pre-market"}]}}"#;
        assert_eq!(parse_earnings(body).unwrap().len(), 0);
    }

    #[test]
    fn bad_json_is_feed_error() {
        assert!(parse_earnings("not json").is_err());
    }

    #[test]
    fn parse_money_cases() {
        assert_eq!(parse_money("$3.90"), Some(3.90));
        assert_eq!(parse_money("($0.05)"), Some(-0.05));
        assert_eq!(parse_money("$92,963,168,000"), Some(92_963_168_000.0));
        assert_eq!(parse_money(""), None);
        assert_eq!(parse_money("N/A"), None);
        assert_eq!(parse_money("n/a"), None);
        assert_eq!(parse_money("abc"), None);
    }

    #[test]
    fn cache_key_is_per_date() {
        assert_eq!(cache_key("2026-11-09"), "earnings:date=2026-11-09");
    }
}
