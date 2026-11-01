//! Satellite feed: CelesTrak TLE sets (keyless).
//!
//! Upstream: `https://celestrak.org/NORAD/elements/gp.php?GROUP=stations&FORMAT=tle`
//! Shape (verified 2026-11-01): 3-line sets — name line, line 1 (`1 NNNNN…`),
//! line 2 (`2 NNNNN…`). 16 objects in the `stations` group at fetch time.
//!
//! Full SGP4 propagation is out of scope for v0.1 (see README roadmap); this
//! module parses and serves the element sets themselves.

use std::time::Duration;

use serde::Serialize;

use super::get_text;
use crate::error::FeedError;
use crate::time::now_iso8601;

/// Cache TTL: TLE sets update a few times per day; 10 minutes is plenty.
pub const TTL: Duration = Duration::from_secs(600);
pub const FEED: &str = "celestrak";

/// A parsed two-line element set.
#[derive(Debug, Serialize, PartialEq)]
pub struct Satellite {
    pub name: String,
    pub norad_id: u32,
    pub line1: String,
    pub line2: String,
}

#[derive(Debug, Serialize)]
pub struct SatellitesResponse {
    pub count: usize,
    pub generated_at: String,
    /// Full SGP4 propagation is out of scope for v0.1; positions are not computed.
    pub note: &'static str,
    pub satellites: Vec<Satellite>,
}

/// Parse TLE text into satellite records. Pure: no networking, the criterion
/// hot path. Lenient: skips malformed groups instead of failing the batch.
pub fn parse_tle(text: &str) -> Vec<Satellite> {
    let lines: Vec<&str> = text.lines().map(str::trim_end).collect();
    let mut sats = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if let Some(l1) = line.strip_prefix('1') {
            // Expect line 2 next; the name is the nearest preceding non-empty,
            // non-TLE line (or UNKNOWN).
            let l2 = lines.get(i + 1).copied().unwrap_or("");
            if l2.starts_with('2') {
                let name = lines[..i]
                    .iter()
                    .rev()
                    .find(|l| !l.is_empty() && !l.starts_with('1') && !l.starts_with('2'))
                    .map(|s| s.trim().to_string())
                    .unwrap_or_else(|| "UNKNOWN".to_string());
                let norad_id = l1
                    .get(1..6)
                    .and_then(|s| s.trim().parse::<u32>().ok())
                    .unwrap_or(0);
                sats.push(Satellite {
                    name,
                    norad_id,
                    line1: format!("1{l1}"),
                    line2: l2.to_string(),
                });
                i += 2;
                continue;
            }
        }
        i += 1;
    }
    sats
}

pub async fn fetch(
    client: &reqwest::Client,
    limit: usize,
) -> Result<SatellitesResponse, FeedError> {
    let text = get_text(
        client,
        FEED,
        "https://celestrak.org/NORAD/elements/gp.php?GROUP=stations&FORMAT=tle",
    )
    .await?;
    let mut sats = parse_tle(&text);
    sats.truncate(limit);
    Ok(SatellitesResponse {
        count: sats.len(),
        generated_at: now_iso8601(),
        note: "TLE element sets only; SGP4 propagation is out of scope for v0.1",
        satellites: sats,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("../../benches/fixtures/celestrak.tle");

    #[test]
    fn parses_fixture_groups() {
        let sats = parse_tle(FIXTURE);
        assert_eq!(sats.len(), 16);
        assert_eq!(sats[0].name, "ISS (ZARYA)");
        assert_eq!(sats[0].norad_id, 25544);
        assert!(sats[0].line1.starts_with("1 25544U"));
        assert!(sats[0].line2.starts_with("2 25544"));
    }

    #[test]
    fn skips_malformed_groups() {
        let sats = parse_tle("garbage\n1 12345U x\nnot line two\n");
        assert!(sats.is_empty());
    }

    #[test]
    fn empty_input_yields_empty() {
        assert!(parse_tle("").is_empty());
    }
}
