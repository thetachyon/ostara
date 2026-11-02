//! Economic calendar: curated static registry (no key, no upstream).
//!
//! Free economic-calendar APIs are key-walled, so this is a hand-built
//! registry (`data/econ_calendar.json`, embedded via `include_str!`):
//! FOMC/ECB/BoE/BoJ policy meetings through 2027, US CPI + Nonfarm Payrolls
//! through mid-2027, OPEC+, G20, major elections. Every entry carries a
//! `confidence` tag: `published-schedule` (officially published meeting
//! calendars), `regular-pattern` (monthly series / projected 2027 central
//! bank dates), `announced` (OPEC / G20 — dates may shift), `scheduled`
//! (elections). Nothing before 2026-11-03 is in the registry.
//!
//! The response states the caveat plainly: this is a curated registry, not
//! a live feed — verify against official calendars before trading.

use axum::{
    extract::{Query, State},
    response::IntoResponse,
};
use serde::{Deserialize, Serialize};

use crate::{
    error::{ApiError, FeedError},
    routes::AppState,
    time::now_iso8601,
};

pub const FEED: &str = "calendar";
pub const SOURCE: &str = "curated registry — verify against official calendars before trading";
/// Default window: today → +90 days.
pub const DEFAULT_WINDOW_DAYS: i64 = 90;

pub const KINDS: &[&str] = &[
    "rates",
    "inflation",
    "employment",
    "energy",
    "elections",
    "summit",
];

/// One registry entry (as stored in the JSON).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EconEvent {
    pub id: String,
    pub name: String,
    pub kind: String,
    /// `YYYY-MM-DD`.
    pub date: String,
    /// `HH:MM` UTC, best effort; null where unknown.
    pub time_utc: Option<String>,
    pub country_org: String,
    pub importance: String,
    pub note: String,
    pub confidence: String,
}

/// One event in the response: registry fields plus `days_until`.
#[derive(Debug, Clone, Serialize)]
pub struct CalendarEvent {
    #[serde(flatten)]
    pub event: EconEvent,
    pub days_until: i64,
}

#[derive(Debug, Serialize)]
pub struct CalendarResponse {
    pub generated_at: String,
    pub source: String,
    pub count: usize,
    pub events: Vec<CalendarEvent>,
}

/// Load the embedded registry. A corrupt registry is a deploy-time bug.
pub fn load_registry() -> Result<Vec<EconEvent>, String> {
    const RAW: &str = include_str!("../../data/econ_calendar.json");
    serde_json::from_str(RAW).map_err(|e| format!("econ calendar registry invalid: {e}"))
}

/// Short detail string for the health endpoint.
pub async fn probe_detail() -> Result<String, FeedError> {
    let n = load_registry().map_err(|e| FeedError::new(FEED, e))?.len();
    Ok(format!("{n} events (curated registry)"))
}

// ---------------------------------------------------------------------------
// Date helpers (no chrono; integer math only)
// ---------------------------------------------------------------------------

/// Days since 1970-01-01 (inverse of `civil_from_days` in `crate::time`).
fn ymd_to_days(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Days since 1970-01-01 → `YYYY-MM-DD`.
fn days_to_ymd(z: i64) -> String {
    let z = z + 719_468;
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

/// Strict `YYYY-MM-DD` → days since epoch. Rejects bad shapes, months,
/// and impossible days (leap years handled).
pub(crate) fn parse_ymd(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() != 10 || b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    let num = |i: usize, n: usize| -> Option<i64> {
        let mut v: i64 = 0;
        for &byte in b.iter().skip(i).take(n) {
            if !byte.is_ascii_digit() {
                return None;
            }
            v = v * 10 + (byte - b'0') as i64;
        }
        Some(v)
    };
    let (y, m, d) = (num(0, 4)?, num(5, 2)?, num(8, 2)?);
    if !(1..=12).contains(&m) {
        return None;
    }
    let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    let dim = match m {
        2 => {
            if leap {
                29
            } else {
                28
            }
        }
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    };
    if !(1..=dim).contains(&d) {
        return None;
    }
    Some(ymd_to_days(y, m, d))
}

/// Today's date (`YYYY-MM-DD`, UTC).
pub(crate) fn today_ymd() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    days_to_ymd(secs.div_euclid(86_400))
}

/// Filter + window + sort the registry. Pure: takes `today` as days so
/// tests control the clock. Past events are never returned: the window
/// start clamps to today.
pub fn window_events(
    registry: &[EconEvent],
    today_days: i64,
    from: Option<&str>,
    to: Option<&str>,
    kind: Option<&str>,
) -> Result<Vec<CalendarEvent>, ApiError> {
    let from_days = match from.map(str::trim).filter(|s| !s.is_empty()) {
        None => today_days,
        Some(s) => parse_ymd(s).ok_or_else(|| {
            ApiError::bad_request("bad from date", format!("expected YYYY-MM-DD, got '{s}'"))
        })?,
    };
    let to_days = match to.map(str::trim).filter(|s| !s.is_empty()) {
        None => today_days + DEFAULT_WINDOW_DAYS,
        Some(s) => parse_ymd(s).ok_or_else(|| {
            ApiError::bad_request("bad to date", format!("expected YYYY-MM-DD, got '{s}'"))
        })?,
    };
    if to_days < from_days {
        return Err(ApiError::bad_request(
            "empty window",
            "to must not be before from",
        ));
    }
    if let Some(k) = kind.map(str::trim).filter(|s| !s.is_empty())
        && !KINDS.contains(&k)
    {
        return Err(ApiError::bad_request(
            "bad kind",
            format!("expected one of {}", KINDS.join(", ")),
        ));
    }

    let start = from_days.max(today_days);
    let mut out: Vec<CalendarEvent> = registry
        .iter()
        .filter(|e| match kind.map(str::trim).filter(|s| !s.is_empty()) {
            Some(k) => e.kind == k,
            None => true,
        })
        .filter_map(|e| {
            let d = parse_ymd(&e.date)?;
            if d < start || d > to_days {
                return None;
            }
            Some(CalendarEvent {
                event: e.clone(),
                days_until: d - today_days,
            })
        })
        .collect();
    out.sort_by(|a, b| {
        a.event
            .date
            .cmp(&b.event.date)
            .then_with(|| a.event.id.cmp(&b.event.id))
    });
    Ok(out)
}

// ---------------------------------------------------------------------------
// GET /api/calendar?from=&to=&kind=
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct CalendarQuery {
    pub from: Option<String>,
    pub to: Option<String>,
    pub kind: Option<String>,
}

pub async fn handler(
    State(_state): State<AppState>,
    Query(q): Query<CalendarQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let registry = load_registry()
        .map_err(|detail| ApiError::internal("calendar registry invalid", detail))?;
    let today = parse_ymd(&today_ymd()).unwrap_or(0);
    let events = window_events(
        &registry,
        today,
        q.from.as_deref(),
        q.to.as_deref(),
        q.kind.as_deref(),
    )?;
    Ok(axum::Json(CalendarResponse {
        generated_at: now_iso8601(),
        source: SOURCE.to_string(),
        count: events.len(),
        events,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Vec<EconEvent> {
        vec![
            EconEvent {
                id: "past".into(),
                name: "Past".into(),
                kind: "rates".into(),
                date: "2026-10-01".into(),
                time_utc: None,
                country_org: "X".into(),
                importance: "high".into(),
                note: "".into(),
                confidence: "published-schedule".into(),
            },
            EconEvent {
                id: "today".into(),
                name: "Today".into(),
                kind: "inflation".into(),
                date: "2026-11-02".into(),
                time_utc: Some("13:30".into()),
                country_org: "US".into(),
                importance: "high".into(),
                note: "".into(),
                confidence: "regular-pattern".into(),
            },
            EconEvent {
                id: "soon".into(),
                name: "Soon".into(),
                kind: "rates".into(),
                date: "2026-11-10".into(),
                time_utc: None,
                country_org: "Y".into(),
                importance: "medium".into(),
                note: "".into(),
                confidence: "announced".into(),
            },
            EconEvent {
                id: "far".into(),
                name: "Far".into(),
                kind: "rates".into(),
                date: "2027-06-01".into(),
                time_utc: None,
                country_org: "Z".into(),
                importance: "high".into(),
                note: "".into(),
                confidence: "published-schedule".into(),
            },
        ]
    }

    fn days(s: &str) -> i64 {
        parse_ymd(s).unwrap()
    }

    #[test]
    fn default_window_excludes_past_and_counts_down() {
        // Default window is today → +90 days, so "far" (2027-06-01) is out
        // and "past" is dropped by the never-show-past rule.
        let ev = window_events(&fixture(), days("2026-11-02"), None, None, None).unwrap();
        assert_eq!(ev.len(), 2);
        assert_eq!(ev[0].event.id, "today");
        assert_eq!(ev[0].days_until, 0);
        assert_eq!(ev[1].event.id, "soon");
        assert_eq!(ev[1].days_until, 8);
    }

    #[test]
    fn explicit_past_from_clamps_to_today() {
        let ev = window_events(
            &fixture(),
            days("2026-11-02"),
            Some("2026-01-01"),
            Some("2027-12-31"),
            None,
        )
        .unwrap();
        assert_eq!(ev.len(), 3, "past events never shown even with old from");
        // widened window reaches "far"
        assert_eq!(ev[2].event.id, "far");
        assert_eq!(ev[2].days_until, 211);
    }

    #[test]
    fn kind_filter_and_bad_kind() {
        let ev = window_events(
            &fixture(),
            days("2026-11-02"),
            None,
            None,
            Some("inflation"),
        )
        .unwrap();
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].event.id, "today");
        assert!(window_events(&fixture(), days("2026-11-02"), None, None, Some("bogus")).is_err());
    }

    #[test]
    fn window_bounds_respected() {
        let ev = window_events(
            &fixture(),
            days("2026-11-02"),
            Some("2026-11-02"),
            Some("2026-11-10"),
            None,
        )
        .unwrap();
        assert_eq!(ev.len(), 2);
        // to before from → 400
        assert!(
            window_events(
                &fixture(),
                days("2026-11-02"),
                Some("2026-12-01"),
                Some("2026-11-01"),
                None
            )
            .is_err()
        );
    }

    #[test]
    fn bad_dates_rejected() {
        assert!(
            window_events(
                &fixture(),
                days("2026-11-02"),
                Some("11/02/2026"),
                None,
                None
            )
            .is_err()
        );
        assert!(
            window_events(
                &fixture(),
                days("2026-11-02"),
                None,
                Some("2026-13-01"),
                None
            )
            .is_err()
        );
        assert!(
            window_events(
                &fixture(),
                days("2026-11-02"),
                Some("2027-02-29"),
                None,
                None
            )
            .is_err()
        );
        assert!(
            window_events(
                &fixture(),
                days("2026-11-02"),
                Some("2024-02-29"),
                None,
                None
            )
            .is_ok()
        );
    }

    #[test]
    fn registry_loads_and_validates() {
        let r = load_registry().unwrap();
        assert!(r.len() >= 50, "expected 50+ events, got {}", r.len());
        let ids: std::collections::HashSet<&str> = r.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids.len(), r.len(), "duplicate ids");
        for e in &r {
            assert!(KINDS.contains(&e.kind.as_str()), "bad kind: {}", e.kind);
            assert!(["high", "medium"].contains(&e.importance.as_str()));
            assert!(
                [
                    "published-schedule",
                    "regular-pattern",
                    "announced",
                    "scheduled"
                ]
                .contains(&e.confidence.as_str()),
                "bad confidence: {}",
                e.id
            );
            assert!(
                parse_ymd(&e.date).is_some(),
                "bad date: {} {}",
                e.id,
                e.date
            );
            assert!(e.date.as_str() >= "2026-11-03", "pre-cutoff: {}", e.id);
            if let Some(t) = &e.time_utc {
                assert!(
                    t.len() == 5 && t.as_bytes()[2] == b':',
                    "bad time_utc: {}",
                    e.id
                );
            }
        }
    }

    #[test]
    fn ymd_round_trips() {
        for d in ["2026-11-02", "2027-02-28", "2024-02-29", "2000-01-01"] {
            assert_eq!(days_to_ymd(parse_ymd(d).unwrap()), d);
        }
    }
}
