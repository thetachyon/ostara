//! Curated WhatsApp Channels directory (data-only, NOT a live feed).
//!
//! `GET /api/whatsapp-channels` returns the curated directory of notable
//! public WhatsApp channels — name, invite URL, category, country, language,
//! verification flag. Optional `?country=<name|iso2|iso3|global>` and
//! `?category=<name>` filters.
//!
//! This is deliberately a *directory*, not a feed: WhatsApp Channels expose
//! no public read API (verified 2026-11-01) — every live-message path needs
//! a paired WhatsApp account. Each entry is an invite link the analyst opens
//! in WhatsApp themselves. See `data/whatsapp_channels.json` and its
//! provenance notes. URLs were never guessed: each was published by the
//! owning organisation or corroborated by a reputable channel directory.

use std::sync::OnceLock;

use axum::{Json, Router, extract::Query, routing::get};
use serde::{Deserialize, Serialize};

/// Curated directory, embedded at compile time like the camera registry.
static WHATSAPP_CHANNELS_JSON: &str = include_str!("../data/whatsapp_channels.json");

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct WhatsappChannel {
    pub name: String,
    pub invite_url: String,
    pub category: String,
    pub country: String,
    pub iso2: String,
    pub iso3: String,
    pub language: String,
    pub verified: bool,
    pub source_note: String,
}

#[derive(Debug, Deserialize)]
struct Dataset {
    #[allow(dead_code)]
    meta: serde_json::Value,
    channels: Vec<WhatsappChannel>,
}

fn dataset() -> &'static Dataset {
    static DATA: OnceLock<Dataset> = OnceLock::new();
    DATA.get_or_init(|| {
        serde_json::from_str(WHATSAPP_CHANNELS_JSON).expect("data/whatsapp_channels.json parses")
    })
}

/// All channels (cloned; the list is small).
pub(crate) fn all_channels() -> Vec<WhatsappChannel> {
    dataset().channels.clone()
}

/// Channels for one country, matched by name, ISO-2, or ISO-3
/// (case-insensitive). `"global"` matches the country-agnostic entries.
pub(crate) fn channels_for_country(q: &str) -> Vec<WhatsappChannel> {
    let q = q.trim().to_lowercase();
    dataset()
        .channels
        .iter()
        .filter(|c| {
            c.country.to_lowercase() == q
                || (!c.iso2.is_empty() && c.iso2.to_lowercase() == q)
                || (!c.iso3.is_empty() && c.iso3.to_lowercase() == q)
        })
        .cloned()
        .collect()
}

/// `?country=` and `?category=` filters; both optional.
#[derive(Debug, Deserialize)]
pub struct WhatsappQuery {
    pub country: Option<String>,
    pub category: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct WhatsappChannelsResponse {
    pub count: usize,
    pub directory_note: &'static str,
    pub channels: Vec<WhatsappChannel>,
}

/// `GET /api/whatsapp-channels[?country=][&category=]`.
///
/// Always 200 — an empty filter result is an empty list, not an error.
/// The `directory_note` is repeated so no consumer mistakes this for a feed.
pub async fn whatsapp_channels(Query(q): Query<WhatsappQuery>) -> Json<WhatsappChannelsResponse> {
    let mut channels = match q.country {
        Some(c) if !c.trim().is_empty() => channels_for_country(&c),
        _ => dataset().channels.clone(),
    };
    if let Some(cat) = q
        .category
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty())
    {
        channels.retain(|ch| ch.category.to_lowercase() == cat);
    }
    Json(WhatsappChannelsResponse {
        count: channels.len(),
        directory_note: "Curated directory of public WhatsApp channel invite links — not live message content. WhatsApp Channels have no public read API.",
        channels,
    })
}

pub fn routes() -> Router<crate::routes::AppState> {
    Router::new().route("/api/whatsapp-channels", get(whatsapp_channels))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dataset_parses_with_entries() {
        let d = dataset();
        assert!(d.channels.len() >= 10, "expected 10+ curated channels");
    }

    #[test]
    fn every_entry_has_a_real_invite_url() {
        for c in &dataset().channels {
            assert!(!c.name.is_empty(), "channel with empty name");
            assert!(
                c.invite_url.starts_with("https://whatsapp.com/channel/"),
                "bad invite URL for {}: {}",
                c.name,
                c.invite_url
            );
            assert!(
                c.invite_url.len() > "https://whatsapp.com/channel/".len() + 8,
                "truncated invite URL for {}",
                c.name
            );
            assert!(!c.category.is_empty(), "empty category for {}", c.name);
            assert!(!c.country.is_empty(), "empty country for {}", c.name);
        }
    }

    #[test]
    fn invite_urls_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for c in &dataset().channels {
            assert!(
                seen.insert(c.invite_url.clone()),
                "duplicate URL: {}",
                c.invite_url
            );
        }
    }

    #[test]
    fn country_filter_matches_name_and_isos() {
        assert!(!channels_for_country("India").is_empty());
        assert!(!channels_for_country("IN").is_empty());
        assert!(!channels_for_country("IND").is_empty());
        assert!(!channels_for_country("global").is_empty());
        assert!(channels_for_country("Narnia").is_empty());
        // Case-insensitive.
        assert_eq!(
            channels_for_country("india").len(),
            channels_for_country("IND").len()
        );
    }
}
