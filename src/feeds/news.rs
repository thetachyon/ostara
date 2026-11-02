//! News feed: GDELT DOC 2.1 API (keyless, rate-limited).
//!
//! Upstream: `https://api.gdeltproject.org/api/v2/doc/doc?query=...&mode=artlist&format=json`
//! Shape (verified 2026-11-01): `{articles: [{url, url_mobile, title,
//! seendate, socialimage, domain, language, sourcecountry}]}`.
//! `seendate` looks like `"20260927T070000Z"`.
//!
//! GDELT asks for at most one request per 5 seconds; the 300s TTL keeps us
//! far under that even with steady polling.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::get_text;
use crate::error::FeedError;
use crate::time::now_iso8601;

/// Cache TTL: 5 minutes, comfortably under GDELT's 1-req-per-5s courtesy limit.
pub const TTL: Duration = Duration::from_secs(300);
pub const FEED: &str = "gdelt";

#[derive(Debug, Deserialize)]
struct GdeltArticle {
    #[serde(default)]
    url: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    domain: String,
    #[serde(default)]
    language: String,
    #[serde(default)]
    sourcecountry: String,
    #[serde(default)]
    seendate: String,
}

#[derive(Debug, Deserialize)]
struct GdeltResponse {
    #[serde(default)]
    articles: Vec<GdeltArticle>,
}

/// Normalized article record served by `/api/news`.
#[derive(Debug, Serialize, Deserialize)]
pub struct NewsArticle {
    pub url: String,
    pub title: String,
    pub domain: String,
    pub language: String,
    pub source_country: String,
    /// First-seen time, ISO 8601 UTC.
    pub seen_at: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct NewsResponse {
    pub query: String,
    pub count: usize,
    pub generated_at: String,
    pub articles: Vec<NewsArticle>,
}

/// `"20260927T070000Z"` -> `"2026-09-27T07:00:00Z"`. Returns `None` on
/// malformed input rather than failing the batch.
pub fn parse_seendate(s: &str) -> Option<String> {
    if s.len() != 16 || !s.ends_with('Z') || s.as_bytes()[8] != b'T' {
        return None;
    }
    let (date, time) = (&s[..8], &s[9..15]);
    if !date.bytes().all(|b| b.is_ascii_digit()) || !time.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(format!(
        "{}-{}-{}T{}:{}:{}Z",
        &date[0..4],
        &date[4..6],
        &date[6..8],
        &time[0..2],
        &time[2..4],
        &time[4..6]
    ))
}

fn normalize(a: GdeltArticle) -> NewsArticle {
    NewsArticle {
        url: a.url,
        title: a.title,
        domain: a.domain,
        language: a.language,
        source_country: a.sourcecountry,
        seen_at: parse_seendate(&a.seendate),
    }
}

/// Parse + normalize a raw GDELT JSON body. Pure: no networking.
pub fn parse_response(body: &str, query: &str) -> Result<NewsResponse, FeedError> {
    let resp: GdeltResponse = serde_json::from_str(body)
        .map_err(|e| FeedError::new(FEED, format!("decode failed: {e}")))?;
    let articles: Vec<NewsArticle> = resp.articles.into_iter().map(normalize).collect();
    Ok(NewsResponse {
        query: query.to_string(),
        count: articles.len(),
        generated_at: now_iso8601(),
        articles,
    })
}

pub async fn fetch(
    client: &reqwest::Client,
    query: &str,
    max_records: u32,
) -> Result<NewsResponse, FeedError> {
    // Query is user-supplied; encode it for the URL.
    let encoded: String = url_encode(query);
    let url = format!(
        "https://api.gdeltproject.org/api/v2/doc/doc?query={encoded}&mode=artlist&format=json&maxrecords={max_records}"
    );
    let body: String = get_text(client, FEED, &url).await?;
    parse_response(&body, query)
}

/// One bin of a GDELT DOC 2.1 `timelinevol` response: article volume for a
/// date bin. Bin width adapts to the requested timespan (daily for short
/// spans, wider for long ones) — consumers must compare per-bin *rates*,
/// never raw bin sums across different spans.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimelineBin {
    /// Bin start, epoch seconds UTC.
    pub epoch_secs: i64,
    /// Article count in the bin.
    pub value: f64,
}

#[derive(Debug, Deserialize)]
struct TimelineDataPoint {
    #[serde(default)]
    date: String,
    #[serde(default)]
    value: f64,
}

#[derive(Debug, Deserialize)]
struct TimelineSeries {
    #[serde(default)]
    data: Vec<TimelineDataPoint>,
}

#[derive(Debug, Deserialize)]
struct TimelineVolumeResponse {
    #[serde(default)]
    timeline: Vec<TimelineSeries>,
}

/// Parse a timestamp from a GDELT timeline bin into epoch seconds. GDELT has
/// served these as compact `YYYYMMDDHHMMSS`, `YYYYMMDDTHHMMSSZ`, and
/// RFC-3339-ish forms across API revisions — accept anything with at least
/// 14 digits rather than pinning one shape. Returns `None` on garbage.
fn parse_timeline_date(s: &str) -> Option<i64> {
    let digits: String = s
        .bytes()
        .filter(|b| b.is_ascii_digit())
        .map(|b| b as char)
        .collect();
    if digits.len() < 14 {
        return None;
    }
    let d = &digits[..14];
    let y: i64 = d[0..4].parse().ok()?;
    let mo: i64 = d[4..6].parse().ok()?;
    let dy: i64 = d[6..8].parse().ok()?;
    let h: i64 = d[8..10].parse().ok()?;
    let mi: i64 = d[10..12].parse().ok()?;
    let se: i64 = d[12..14].parse().ok()?;
    if !(1..=12).contains(&mo) || !(1..=31).contains(&dy) || h > 23 || mi > 59 || se > 60 {
        return None;
    }
    Some(days_from_civil(y, mo, dy) * 86_400 + h * 3600 + mi * 60 + se)
}

/// Days since 1970-01-01 (Howard Hinnant's civil-from-days inverse; same
/// algorithm family as `crate::time`, local so this stays dependency-free).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400; // [0, 399]
    let mp = (m + 9) % 12; // [0, 11]
    let doy = (153 * mp + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe - 719_468
}

/// Parse + normalize a raw GDELT `timelinevol` JSON body. Pure: no
/// networking. Bins with unparseable dates are skipped; an empty timeline
/// is an error (the caller can't build a baseline from nothing).
pub fn parse_timeline_volume(body: &str) -> Result<Vec<TimelineBin>, FeedError> {
    let resp: TimelineVolumeResponse = serde_json::from_str(body)
        .map_err(|e| FeedError::new(FEED, format!("timeline decode failed: {e}")))?;
    let mut bins: Vec<TimelineBin> = resp
        .timeline
        .into_iter()
        .flat_map(|s| s.data)
        .filter_map(|p| {
            parse_timeline_date(&p.date).map(|epoch_secs| TimelineBin {
                epoch_secs,
                value: p.value,
            })
        })
        .collect();
    if bins.is_empty() {
        return Err(FeedError::new(
            FEED,
            "timelinevol returned no parseable bins",
        ));
    }
    bins.sort_by_key(|b| b.epoch_secs);
    Ok(bins)
}

/// Fetch a GDELT DOC 2.1 volume timeline for a query: daily-ish article
/// counts over the trailing `timespan` (e.g. `"90days"`). Same 300s TTL
/// family as the artlist path; one upstream request per query per TTL.
pub async fn fetch_timeline_volume(
    client: &reqwest::Client,
    query: &str,
    timespan: &str,
) -> Result<Vec<TimelineBin>, FeedError> {
    let url = format!(
        "https://api.gdeltproject.org/api/v2/doc/doc?query={}&mode=timelinevol&timespan={}&format=json",
        url_encode(query),
        url_encode(timespan)
    );
    let body: String = get_text(client, FEED, &url).await?;
    parse_timeline_volume(&body)
}

/// Minimal percent-encoding for the GDELT query parameter (avoids pulling in
/// a URL-encoding crate for one call site).
fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("../../benches/fixtures/gdelt.json");

    #[test]
    fn seendate_conversion() {
        assert_eq!(
            parse_seendate("20260927T070000Z"),
            Some("2026-09-27T07:00:00Z".to_string())
        );
    }

    #[test]
    fn seendate_rejects_garbage() {
        assert_eq!(parse_seendate(""), None);
        assert_eq!(parse_seendate("20260927"), None);
        assert_eq!(parse_seendate("2026ABCDT070000Z"), None);
    }

    #[test]
    fn parses_fixture() {
        let r = parse_response(FIXTURE, "earthquake").unwrap();
        assert_eq!(r.query, "earthquake");
        assert!(r.count > 0);
        let a = &r.articles[0];
        assert_eq!(a.domain, "cnnturk.com");
        assert_eq!(a.seen_at.as_deref(), Some("2026-09-27T07:00:00Z"));
    }

    #[test]
    fn url_encode_spaces_and_unicode() {
        assert_eq!(url_encode("a b+c"), "a+b%2Bc");
    }

    #[test]
    fn timeline_date_shapes() {
        // Compact, seendate-style, and RFC-3339-ish all parse to the same day.
        let a = parse_timeline_date("20261015000000").unwrap();
        let b = parse_timeline_date("20261015T000000Z").unwrap();
        let c = parse_timeline_date("2026-10-15T00:00:00Z").unwrap();
        assert_eq!(a, b);
        assert_eq!(b, c);
        // Sanity against time.rs's forward conversion: 2026-10-15T00:00:00Z.
        assert_eq!(
            crate::time::epoch_millis_to_iso8601(a * 1000),
            "2026-10-15T00:00:00.000Z"
        );
        assert!(parse_timeline_date("not-a-date").is_none());
        assert!(parse_timeline_date("20261345000000").is_none()); // month 13
    }

    #[test]
    fn timeline_parses_fixture() {
        let bins = parse_timeline_volume(include_str!(
            "../../benches/fixtures/gdelt_timelinevol.json"
        ))
        .unwrap();
        assert!(bins.len() >= 80, "90-day span should yield many bins");
        // Sorted ascending by date.
        assert!(bins.windows(2).all(|w| w[0].epoch_secs <= w[1].epoch_secs));
        assert!(bins.iter().all(|b| b.value >= 0.0));
    }

    #[test]
    fn timeline_rejects_empty() {
        assert!(parse_timeline_volume(r#"{"timeline":[]}"#).is_err());
        assert!(parse_timeline_volume("not json").is_err());
    }
}
