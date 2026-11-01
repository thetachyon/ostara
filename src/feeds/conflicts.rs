//! Conflict/humanitarian events feed: GDACS alerts (keyless).
//!
//! RESEARCH OUTCOME (2026-11-01, verified with live probes):
//!
//! - **ReliefWeb v1** (`api.reliefweb.int/v1/...`): returns **HTTP 410** —
//!   "The API version 'v1' has been decommissioned. Please use version 'v2'
//!   instead." The v1 URL from older docs is dead.
//! - **ReliefWeb v2** (`api.reliefweb.int/v2/reports`): returns **HTTP 403**
//!   "You are not using an approved appname" for unregistered appnames.
//!   Since 2025-11-01 ReliefWeb enforces pre-registered appnames, so v2 is
//!   effectively *keyed* — and there is no appname field in the Phase-1
//!   config. ReliefWeb is therefore unavailable keyless in this phase.
//! - **GDACS** (`www.gdacs.org`): public, keyless, no auth. The event-list
//!   REST endpoints tried (`/gdacsapi/api/events/geteventlist`,
//!   `/gdacsapi/api/events/search`) return 404, but the **RSS feed**
//!   `https://www.gdacs.org/xml/rss.xml` works (verified 200, 252 records)
//!   and carries `<gdacs:country>`, `<gdacs:fromdate>`, `<link>`,
//!   `<gdacs:eventtype>`, `<gdacs:alertlevel>` per item. This module parses
//!   that feed.
//!
//! HONEST SCOPE NOTE: GDACS tracks *natural disasters* (earthquakes,
//! tropical cyclones, floods, volcanoes, droughts, wildfires) — not armed
//! conflict. No keyless REST source for armed-conflict events was found:
//! **ACLED** is the keyed alternative (`https://api.acleddata.com/acled/read`
//! requires email + API-key registration; event types cover battles,
//! explosions, protests, etc.). The response carries a `note` stating this
//! plainly so the `/api/conflicts` route is never misread as armed-conflict
//! coverage. If an ACLED key becomes available, it should become the
//! primary source with GDACS as the humanitarian-disaster complement.
//!
//! The RSS is XML; with no XML crate in scope the parser is a small
//! hand-rolled item/tag extractor (the feed is machine-generated and
//! regular — field order is stable). Malformed items are skipped, never
//! fatal.

use std::time::Duration;

use axum::{extract::State, response::IntoResponse};
use serde::{Deserialize, Serialize};

use super::{cached_bytes, get_text, json_bytes};
use crate::{
    config::Config,
    error::{ApiError, FeedError},
    routes::AppState,
    time::now_iso8601,
};

/// Cache TTL: GDACS RSS refreshes every few minutes; 15 minutes.
pub const TTL: Duration = Duration::from_secs(900);
pub const FEED: &str = "gdacs";

/// Upper bound on served events: the RSS carries ~250 records; the API
/// serves the most recent 100.
const MAX_EVENTS: usize = 100;

/// One normalized event.
#[derive(Debug, Serialize, Deserialize)]
pub struct ConflictEvent {
    pub title: String,
    pub country: Option<String>,
    pub date: String,
    pub url: String,
    /// Always "gdacs" in this revision.
    pub source: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ConflictsResponse {
    pub count: usize,
    pub generated_at: String,
    /// Scope caveat: GDACS covers natural disasters, not armed conflict;
    /// ACLED (keyed) is the armed-conflict alternative.
    pub note: String,
    pub events: Vec<ConflictEvent>,
}

pub const SCOPE_NOTE: &str = "Keyless source is GDACS natural-disaster alerts (earthquakes, cyclones, floods, volcanoes, droughts, wildfires) — not armed conflict. ReliefWeb v2 requires a pre-registered appname; ACLED (keyed) is the armed-conflict alternative.";

/// Extract the text of the first `<tag>...</tag>` occurrence (namespace
/// prefixes like `gdacs:country` included verbatim in `tag`). The five
/// predefined XML entities are unescaped; anything else is left as-is.
fn tag(item: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = item.find(&open)? + open.len();
    let end = item[start..].find(&close)?;
    let text = item[start..start + end].trim();
    if text.is_empty() {
        None
    } else {
        Some(
            text.replace("&amp;", "&")
                .replace("&lt;", "<")
                .replace("&gt;", ">")
                .replace("&quot;", "\"")
                .replace("&#39;", "'"),
        )
    }
}

/// Parse a GDACS RSS body into events. Pure: no networking. Items missing
/// a title or link are skipped.
pub fn parse_rss(body: &str) -> Result<Vec<ConflictEvent>, FeedError> {
    if !body.contains("<rss") && !body.contains("<feed") {
        return Err(FeedError::new(FEED, "body is not an RSS feed"));
    }
    let mut events = Vec::new();
    // Split on item boundaries; the channel header before the first <item>
    // is discarded.
    for chunk in body.split("<item>").skip(1) {
        let item = chunk.split("</item>").next().unwrap_or("");
        let (Some(title), Some(url)) = (tag(item, "title"), tag(item, "link")) else {
            continue;
        };
        events.push(ConflictEvent {
            title,
            country: tag(item, "gdacs:country"),
            date: tag(item, "gdacs:fromdate")
                .or_else(|| tag(item, "pubDate"))
                .unwrap_or_default(),
            url,
            source: FEED.to_string(),
        });
        if events.len() >= MAX_EVENTS {
            break;
        }
    }
    if events.is_empty() {
        return Err(FeedError::new(FEED, "no parseable items in RSS feed"));
    }
    Ok(events)
}

/// Fetch the GDACS RSS feed and normalize it.
pub async fn fetch(client: &reqwest::Client) -> Result<ConflictsResponse, FeedError> {
    let body = get_text(client, FEED, "https://www.gdacs.org/xml/rss.xml").await?;
    let events = parse_rss(&body)?;
    Ok(ConflictsResponse {
        count: events.len(),
        generated_at: now_iso8601(),
        note: SCOPE_NOTE.to_string(),
        events,
    })
}

/// Short detail string for the health endpoint.
pub async fn probe_detail(client: &reqwest::Client, _config: &Config) -> Result<String, FeedError> {
    let r = fetch(client).await?;
    Ok(format!("{} events (GDACS rss)", r.count))
}

// ---------------------------------------------------------------------------
// GET /api/conflicts
// ---------------------------------------------------------------------------

pub async fn handler(State(state): State<AppState>) -> Result<impl IntoResponse, ApiError> {
    let client = state.client.clone();
    let (body, hit) = cached_bytes(
        &state,
        "conflicts",
        TTL,
        || async move { fetch(&client).await },
    )
    .await?;
    Ok(json_bytes(body, hit))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("../../benches/fixtures/gdacs.rss");

    #[test]
    fn parses_rss_fixture() {
        let events = parse_rss(FIXTURE).unwrap();
        assert_eq!(events.len(), 2);
        let e = &events[0];
        assert!(e.title.contains("Vanuatu"));
        assert_eq!(e.country.as_deref(), Some("Vanuatu"));
        assert_eq!(e.date, "Sun, 01 Nov 2026 11:47:03 GMT");
        assert_eq!(
            e.url,
            "http://www.gdacs.org/report.aspx?eventtype=EQ&eventid=1575068"
        );
        assert_eq!(e.source, "gdacs");
    }

    #[test]
    fn items_missing_title_or_link_are_skipped() {
        let body = r#"<?xml version="1.0"?><rss><channel><item><title>Only title</title></item><item><title>Full</title><link>http://x/</link></item></channel></rss>"#;
        let events = parse_rss(body).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].title, "Full");
    }

    #[test]
    fn non_rss_body_is_an_error() {
        assert!(parse_rss("<html>nope</html>").is_err());
    }

    #[test]
    fn empty_feed_is_an_error() {
        assert!(parse_rss(r#"<?xml version="1.0"?><rss><channel></channel></rss>"#).is_err());
    }
}
