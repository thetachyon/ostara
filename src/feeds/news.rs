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
#[derive(Debug, Serialize)]
pub struct NewsArticle {
    pub url: String,
    pub title: String,
    pub domain: String,
    pub language: String,
    pub source_country: String,
    /// First-seen time, ISO 8601 UTC.
    pub seen_at: Option<String>,
}

#[derive(Debug, Serialize)]
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
}
