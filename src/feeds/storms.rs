//! Active tropical cyclones: NOAA NHC `nhc_active.kml` (keyless).
//!
//! Upstream (verified live 2026-11-02):
//! `GET https://www.nhc.noaa.gov/gis/kml/nhc_active.kml` — the NHC's
//! experimental active-storms KML (minRefreshPeriod 120s). When storms
//! are active, each is a `<Folder>` carrying `<ExtendedData>` with
//! `<Data name="…"><value>…</value></Data>` entries (structure verified
//! against an archived 2024-09-27 copy captured during Hurricane Helene
//! and Tropical Storms Isaac/John):
//!
//! - `tcType` ("Hurricane" / "Tropical Storm" / "Tropical Depression" / …)
//! - `tcName`, `wallet` ("AT4"), `atcfID` ("AL092024" — basin prefix +
//!   number + year)
//! - `centerLat` / `centerLon`, `dateTime` ("8:00 AM EDT Fri Sep 27"),
//!   `movement` ("N at 30 mph"), `minimumPressure` ("972 mb"),
//!   `maxSustainedWind` ("60 mph"), `headline`
//!
//! A second `<ExtendedData xmlns:tc="…">` block repeats the same fields
//! as `<tc:name>` etc. (note: without the `tc` infix on `name` — the
//! parser tries both spellings). Storm folders are distinguished from
//! the wind-probability folders by the presence of an `atcfID` value.
//!
//! When no storms are active the KML carries no storm folders and the
//! endpoint returns `none_active: true` with an empty `active` array —
//! it never invents a storm.
//!
//! Category is derived from `maxSustainedWind` via Saffir-Simpson
//! thresholds (mph): TD ≤ 38, TS 39–73, Cat1 74–95, Cat2 96–110,
//! Cat3 111–129, Cat4 130–156, Cat5 ≥ 157. The `tcType` label wins for
//! depressions and post-tropical systems (`PT`).
//!
//! The assembled response is cached 600s under `storms:active`.

use std::time::Duration;

use axum::{extract::State, response::IntoResponse};
use serde::Serialize;

use super::{cached_bytes, json_bytes};
use crate::{
    error::{ApiError, FeedError},
    routes::AppState,
    time::now_iso8601,
};

pub const TTL: Duration = Duration::from_secs(600);
pub const FEED: &str = "storms";

const NHC_KML: &str = "https://www.nhc.noaa.gov/gis/kml/nhc_active.kml";

/// One active tropical cyclone.
#[derive(Debug, Serialize, Clone)]
pub struct Storm {
    /// Storm name, e.g. "Helene".
    pub name: String,
    /// "atlantic" | "east_pacific" | "central_pacific" | "unknown".
    pub basin: String,
    /// "TD" | "TS" | "Cat1".."Cat5" | "PT" (post-tropical/remnant).
    pub category: String,
    pub lat: f64,
    pub lon: f64,
    pub wind_kt: Option<f64>,
    pub pressure_mb: Option<f64>,
    pub movement: Option<String>,
    /// Advisory timestamp as published, e.g. "8:00 AM EDT Fri Sep 27".
    pub advisory_time: Option<String>,
    pub headline: Option<String>,
    /// e.g. "AL092024".
    pub atcf_id: String,
}

#[derive(Debug, Serialize)]
pub struct StormsResponse {
    pub generated_at: String,
    /// `true` when NHC reports no active tropical cyclones.
    pub none_active: bool,
    pub count: usize,
    pub active: Vec<Storm>,
    pub source: String,
}

// ---------------------------------------------------------------------------
// KML parsing (pure, unit-tested)
// ---------------------------------------------------------------------------

/// Split the KML into top-level `<Folder>…</Folder>` blocks, tracking
/// nesting depth (wind-probability folders nest sub-folders).
fn folder_blocks(kml: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(start_rel) = kml[i..].find("<Folder") {
        let start = i + start_rel;
        let mut depth = 0usize;
        let mut j = start;
        let mut closed = false;
        loop {
            let next_open = kml[j..].find("<Folder").map(|p| j + p);
            let next_close = kml[j..].find("</Folder>").map(|p| j + p);
            match (next_open, next_close) {
                (Some(o), Some(c)) if o < c => {
                    depth += 1;
                    j = o + "<Folder".len();
                }
                (_, Some(c)) => {
                    depth = depth.saturating_sub(1);
                    j = c + "</Folder>".len();
                    if depth == 0 {
                        out.push(&kml[start..j]);
                        closed = true;
                        break;
                    }
                }
                _ => break,
            }
        }
        i = if closed { j } else { kml.len() };
    }
    out
}

/// Extract one ExtendedData field. Tries the `<Data name="X"><value>`
/// form first, then the `<tc:X>` / `<tc:x>` namespace form.
fn data_value(block: &str, name: &str) -> Option<String> {
    let pat = format!("<Data name=\"{name}\">");
    if let Some(s) = block.find(&pat) {
        let rest = &block[s + pat.len()..];
        if let Some(vs) = rest.find("<value>") {
            let vs = vs + "<value>".len();
            if let Some(ve) = rest[vs..].find("</value>") {
                let v = rest[vs..vs + ve].trim();
                if !v.is_empty() {
                    return Some(v.to_string());
                }
            }
        }
    }
    // Namespace form: <tc:atcfID>…</tc:atcfID>; note `name` is spelled
    // without the `tc` infix there (<tc:name> vs <Data name="tcName">).
    let stripped = name.strip_prefix("tc").unwrap_or(name);
    let stripped = {
        let mut c = stripped.chars();
        match c.next() {
            Some(f) => f.to_lowercase().collect::<String>() + c.as_str(),
            None => String::new(),
        }
    };
    for tag in [name, stripped.as_str()] {
        let open = format!("<tc:{tag}>");
        let close = format!("</tc:{tag}>");
        if let Some(s) = block.find(&open) {
            let rest = &block[s + open.len()..];
            if let Some(e) = rest.find(&close) {
                let v = rest[..e].trim();
                if !v.is_empty() {
                    return Some(v.to_string());
                }
            }
        }
    }
    None
}

/// Leading number of a value like "972 mb" or "60 mph".
fn leading_number(s: &str) -> Option<f64> {
    s.split_whitespace()
        .next()
        .and_then(|w| w.trim_end_matches([',', '.']).parse().ok())
}

/// "60 mph" → 52.1 kt; values already in kt pass through.
fn wind_kt(s: &str) -> Option<f64> {
    let n = leading_number(s)?;
    let kt = if s.to_lowercase().contains("kt") {
        n
    } else {
        n * 0.868_976
    };
    Some((kt * 10.0).round() / 10.0)
}

fn basin_from_atcf(atcf: &str) -> &'static str {
    let p = atcf.get(..2).unwrap_or("").to_uppercase();
    match p.as_str() {
        "AL" => "atlantic",
        "EP" => "east_pacific",
        "CP" => "central_pacific",
        _ => "unknown",
    }
}

/// Saffir-Simpson category from the tcType label + wind in mph.
fn category(tc_type: &str, wind_mph: Option<f64>) -> String {
    let t = tc_type.to_lowercase();
    if t.contains("post-tropical") || t.contains("remnant") || t.contains("extratropical") {
        return "PT".to_string();
    }
    if t.contains("depression") {
        return "TD".to_string();
    }
    match wind_mph {
        Some(w) if w >= 157.0 => "Cat5".to_string(),
        Some(w) if w >= 130.0 => "Cat4".to_string(),
        Some(w) if w >= 111.0 => "Cat3".to_string(),
        Some(w) if w >= 96.0 => "Cat2".to_string(),
        Some(w) if w >= 74.0 => "Cat1".to_string(),
        Some(_) => "TS".to_string(),
        None => {
            if t.contains("hurricane") {
                "HU".to_string()
            } else {
                "TS".to_string()
            }
        }
    }
}

/// Parse the NHC active-storms KML into storms. Pure: no networking.
fn parse_kml(kml: &str) -> Vec<Storm> {
    let mut storms = Vec::new();
    for block in folder_blocks(kml) {
        // Storm folders carry an atcfID; the wind-probability folders
        // (and the legend overlay) do not.
        let atcf = match data_value(block, "atcfID") {
            Some(a) if !a.is_empty() => a,
            _ => continue,
        };
        let name = data_value(block, "tcName").unwrap_or_else(|| "unnamed".to_string());
        let tc_type = data_value(block, "tcType").unwrap_or_default();
        let lat = data_value(block, "centerLat").and_then(|s| s.parse::<f64>().ok());
        let lon = data_value(block, "centerLon").and_then(|s| s.parse::<f64>().ok());
        let (lat, lon) = match (lat, lon) {
            (Some(la), Some(lo)) => (la, lo),
            _ => continue, // no position — not mappable, skip
        };
        let wind_raw = data_value(block, "maxSustainedWind");
        let wind_mph = wind_raw.as_deref().and_then(leading_number);
        storms.push(Storm {
            category: category(&tc_type, wind_mph),
            basin: basin_from_atcf(&atcf).to_string(),
            wind_kt: wind_raw.as_deref().and_then(wind_kt),
            pressure_mb: data_value(block, "minimumPressure")
                .as_deref()
                .and_then(leading_number),
            movement: data_value(block, "movement"),
            advisory_time: data_value(block, "dateTime"),
            headline: data_value(block, "headline"),
            name,
            lat,
            lon,
            atcf_id: atcf,
        });
    }
    storms
}

pub(crate) async fn fetch(client: &reqwest::Client) -> Result<StormsResponse, FeedError> {
    let kml = client
        .get(NHC_KML)
        .send()
        .await
        .map_err(|e| FeedError::new(FEED, format!("request failed: {e}")))?
        .error_for_status()
        .map_err(|e| FeedError::new(FEED, format!("upstream status: {e}")))?
        .text()
        .await
        .map_err(|e| FeedError::new(FEED, format!("read body failed: {e}")))?;
    if kml.trim().is_empty() {
        return Err(FeedError::new(FEED, "empty response from NHC"));
    }
    let active = parse_kml(&kml);
    Ok(StormsResponse {
        generated_at: now_iso8601(),
        none_active: active.is_empty(),
        count: active.len(),
        active,
        source: "NOAA NHC nhc_active.kml — experimental active-storms product, keyless".to_string(),
    })
}

/// Keyless liveness probe for /api/health/feeds.
pub async fn probe_detail(client: &reqwest::Client) -> Result<String, FeedError> {
    let r = fetch(client).await?;
    Ok(if r.none_active {
        "no active tropical cyclones (NHC)".to_string()
    } else {
        format!("{} active storm(s) via NHC", r.count)
    })
}

// ---------------------------------------------------------------------------
// GET /api/storms
// ---------------------------------------------------------------------------

pub async fn handler(State(state): State<AppState>) -> Result<impl IntoResponse, ApiError> {
    let (body, hit) = cached_bytes(&state, "storms:active", TTL, || {
        let client = state.client.clone();
        async move { fetch(&client).await }
    })
    .await?;
    Ok(json_bytes(body, hit))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal storm folder in the real NHC shape (values from the
    /// archived 2024-09-27 Helene folder), plus a wind-probability
    /// folder with a nested sub-folder (no atcfID — must be skipped).
    const KML_ACTIVE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<kml xmlns="http://www.opengis.net/kml/2.2">
  <Document id="active">
    <name>Active Tropical Cyclones</name>
    <Folder id="wsp">
      <name>Wind Speed Probabilities (120 hours)</name>
      <Folder id="wspValues">
        <name>Values</name>
      </Folder>
    </Folder>
    <Folder id="at4">
      <name>Tropical Storm Helene</name>
      <ExtendedData>
        <Data name="tcType"><value>Tropical Storm</value></Data>
        <Data name="tcName"><value>Helene</value></Data>
        <Data name="wallet"><value>AT4</value></Data>
        <Data name="atcfID"><value>AL092024</value></Data>
        <Data name="centerLat"><value>34.2</value></Data>
        <Data name="centerLon"><value>-83.0</value></Data>
        <Data name="dateTime"><value>8:00 AM EDT Fri Sep 27</value></Data>
        <Data name="movement"><value>N at 30 mph</value></Data>
        <Data name="minimumPressure"><value>972 mb</value></Data>
        <Data name="maxSustainedWind"><value>60 mph</value></Data>
        <Data name="headline"><value>...HELENE PRODUCING DAMAGING WINDS...</value></Data>
      </ExtendedData>
    </Folder>
    <Folder id="ep10">
      <name>Hurricane John</name>
      <ExtendedData xmlns:tc="https://www.nhc.noaa.gov/cyclones">
        <tc:type>Hurricane</tc:type>
        <tc:name>John</tc:name>
        <tc:atcfID>EP102024</tc:atcfID>
        <tc:centerLat>16.1</tc:centerLat>
        <tc:centerLon>-99.4</tc:centerLon>
        <tc:dateTime>2:00 PM CST Fri Sep 27</tc:dateTime>
        <tc:movement>NE at 5 mph</tc:movement>
        <tc:minimumPressure>950 mb</tc:minimumPressure>
        <tc:maxSustainedWind>120 mph</tc:maxSustainedWind>
      </ExtendedData>
    </Folder>
  </Document>
</kml>"#;

    /// The real empty shape (2026-11-02): no storm folders at all.
    const KML_EMPTY: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<kml xmlns="http://www.opengis.net/kml/2.2">
  <Document id="active">
    <name>Active Tropical Cyclones</name>
    <description>No storms</description>
  </Document>
</kml>"#;

    #[test]
    fn parses_active_storms_both_extendeddata_forms() {
        let storms = parse_kml(KML_ACTIVE);
        assert_eq!(storms.len(), 2);
        let h = &storms[0];
        assert_eq!(h.name, "Helene");
        assert_eq!(h.basin, "atlantic");
        assert_eq!(h.category, "TS"); // 60 mph
        assert!((h.lat - 34.2).abs() < 1e-9);
        assert!((h.lon + 83.0).abs() < 1e-9);
        assert!((h.wind_kt.unwrap() - 52.1).abs() < 0.05);
        assert!((h.pressure_mb.unwrap() - 972.0).abs() < 1e-9);
        assert_eq!(h.movement.as_deref(), Some("N at 30 mph"));
        assert_eq!(h.advisory_time.as_deref(), Some("8:00 AM EDT Fri Sep 27"));
        assert_eq!(h.atcf_id, "AL092024");
        // Namespace form: <tc:name> without the `tc` infix.
        let j = &storms[1];
        assert_eq!(j.name, "John");
        assert_eq!(j.basin, "east_pacific");
        assert_eq!(j.category, "Cat3"); // 120 mph
        assert!((j.wind_kt.unwrap() - 104.3).abs() < 0.05);
    }

    #[test]
    fn empty_kml_yields_no_storms() {
        assert!(parse_kml(KML_EMPTY).is_empty());
        // Malformed XML degrades to empty, never panics.
        assert!(parse_kml("not xml at all").is_empty());
        assert!(parse_kml("").is_empty());
    }

    #[test]
    fn skips_folders_without_position() {
        let kml = KML_ACTIVE.replace("<Data name=\"centerLat\"><value>34.2</value></Data>", "");
        let storms = parse_kml(&kml);
        // Helene dropped (no position); John kept.
        assert_eq!(storms.len(), 1);
        assert_eq!(storms[0].name, "John");
    }

    #[test]
    fn category_thresholds() {
        assert_eq!(category("Hurricane", Some(160.0)), "Cat5");
        assert_eq!(category("Hurricane", Some(157.0)), "Cat5");
        assert_eq!(category("Hurricane", Some(140.0)), "Cat4");
        assert_eq!(category("Hurricane", Some(115.0)), "Cat3");
        assert_eq!(category("Hurricane", Some(100.0)), "Cat2");
        assert_eq!(category("Hurricane", Some(85.0)), "Cat1");
        assert_eq!(category("Hurricane", Some(74.0)), "Cat1");
        assert_eq!(category("Hurricane", Some(73.0)), "TS");
        assert_eq!(category("Tropical Storm", Some(60.0)), "TS");
        assert_eq!(category("Tropical Depression", Some(35.0)), "TD");
        assert_eq!(category("Post-Tropical Cyclone", Some(50.0)), "PT");
        assert_eq!(category("Remnant Low", Some(30.0)), "PT");
        assert_eq!(category("Hurricane", None), "HU");
    }

    #[test]
    fn wind_and_pressure_units() {
        assert!((wind_kt("60 mph").unwrap() - 52.1).abs() < 0.05);
        assert!((wind_kt("100 kt").unwrap() - 100.0).abs() < 1e-9);
        assert!((leading_number("972 mb").unwrap() - 972.0).abs() < 1e-9);
        assert!(leading_number("n/a").is_none());
    }

    #[test]
    fn basin_mapping() {
        assert_eq!(basin_from_atcf("AL092024"), "atlantic");
        assert_eq!(basin_from_atcf("EP102024"), "east_pacific");
        assert_eq!(basin_from_atcf("CP032024"), "central_pacific");
        assert_eq!(basin_from_atcf(""), "unknown");
    }
}
