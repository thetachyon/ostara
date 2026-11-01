//! Millisecond-epoch to ISO 8601 UTC conversion without external crates.
//!
//! Uses Howard Hinnant's civil-from-days algorithm on integer math only, so
//! it is correct across leap years and negative (pre-1970) timestamps.

/// Convert milliseconds since the Unix epoch to an ISO 8601 UTC string,
/// e.g. `2026-11-01T12:08:30.561Z`.
pub fn epoch_millis_to_iso8601(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let millis = ms.rem_euclid(1000);
    let days = secs.div_euclid(86_400);
    let secs_of_day = secs.rem_euclid(86_400);

    let (year, month, day) = civil_from_days(days);
    let hour = secs_of_day / 3600;
    let minute = (secs_of_day % 3600) / 60;
    let second = secs_of_day % 60;

    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z")
}

/// Days since 1970-01-01 -> (year, month, day). Hinnant's algorithm.
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

/// Current time as ISO 8601 UTC, for `generated_at` fields.
pub fn now_iso8601() -> String {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    epoch_millis_to_iso8601(ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch_zero() {
        assert_eq!(epoch_millis_to_iso8601(0), "1970-01-01T00:00:00.000Z");
    }

    #[test]
    fn negative_epoch_has_borrowed_millis() {
        // -1 ms == 1969-12-31T23:59:59.999Z
        assert_eq!(epoch_millis_to_iso8601(-1), "1969-12-31T23:59:59.999Z");
    }

    #[test]
    fn known_usgs_timestamp() {
        // Ground truth from CPython datetime, 2026-11-01.
        assert_eq!(
            epoch_millis_to_iso8601(1_793_534_910_561),
            "2026-11-01T12:08:30.561Z"
        );
    }

    #[test]
    fn leap_day() {
        // 2024-02-29T00:00:00Z
        assert_eq!(
            epoch_millis_to_iso8601(1_709_164_800_000),
            "2024-02-29T00:00:00.000Z"
        );
    }

    #[test]
    fn day_after_leap_day() {
        // 2024-03-01T00:00:00Z
        assert_eq!(
            epoch_millis_to_iso8601(1_709_251_200_000),
            "2024-03-01T00:00:00.000Z"
        );
    }
}
