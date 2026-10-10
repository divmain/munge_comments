//! RFC 3339 timestamps without a date/time dependency.
//!
//! The artifact's `createdAt` is UTC at seconds precision
//! (`"2026-10-09T14:03:11Z"`). The civil date comes from Howard Hinnant's
//! `civil_from_days` algorithm, which is exact over the proleptic Gregorian
//! calendar.

use std::time::{SystemTime, UNIX_EPOCH};

const SECONDS_PER_DAY: i64 = 86_400;

/// Formats `time` as an RFC 3339 UTC timestamp at seconds precision
/// (sub-second parts are truncated toward the past), e.g.
/// `"2026-10-09T14:03:11Z"`. Instants before the Unix epoch are supported.
pub fn utc_rfc3339(time: SystemTime) -> String {
    let seconds: i64 = match time.duration_since(UNIX_EPOCH) {
        Ok(after) => i64::try_from(after.as_secs()).unwrap_or(i64::MAX),
        Err(before) => {
            // Truncate toward the past: -0.5 s is 1969-12-31T23:59:59Z.
            let before = before.duration();
            let whole = i64::try_from(before.as_secs()).unwrap_or(i64::MAX);
            let partial = i64::from(before.subsec_nanos() > 0);
            -whole.saturating_add(partial)
        }
    };
    let days = seconds.div_euclid(SECONDS_PER_DAY);
    let of_day = seconds.rem_euclid(SECONDS_PER_DAY);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        of_day / 3600,
        (of_day % 3600) / 60,
        of_day % 60
    )
}

/// Converts days since 1970-01-01 into a proleptic Gregorian
/// `(year, month, day)`.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097); // [0, 146096]
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365; // [0, 399]
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100); // [0, 365]
    let shifted_month = (5 * day_of_year + 2) / 153; // [0, 11], March-based
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1; // [1, 31]
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    }; // [1, 12]
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    // Both values are range-bounded by construction (see the comments).
    (year, month as u32, day as u32)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn at(seconds: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(seconds)
    }

    #[test]
    fn formats_the_epoch() {
        assert_eq!(utc_rfc3339(UNIX_EPOCH), "1970-01-01T00:00:00Z");
    }

    #[test]
    fn formats_known_instants() {
        // `date -u -r <secs>` reference values.
        assert_eq!(utc_rfc3339(at(951_782_400)), "2000-02-29T00:00:00Z");
        assert_eq!(utc_rfc3339(at(1_234_567_890)), "2009-02-13T23:31:30Z");
        assert_eq!(utc_rfc3339(at(1_709_251_199)), "2024-02-29T23:59:59Z");
        assert_eq!(utc_rfc3339(at(1_791_554_591)), "2026-10-09T14:03:11Z");
        assert_eq!(utc_rfc3339(at(4_107_542_400)), "2100-03-01T00:00:00Z");
        assert_eq!(utc_rfc3339(at(2_147_483_647)), "2038-01-19T03:14:07Z");
    }

    #[test]
    fn truncates_sub_second_precision() {
        let t = at(1_234_567_890) + Duration::from_millis(999);
        assert_eq!(utc_rfc3339(t), "2009-02-13T23:31:30Z");
    }

    #[test]
    fn handles_instants_before_the_epoch() {
        assert_eq!(
            utc_rfc3339(UNIX_EPOCH - Duration::from_secs(1)),
            "1969-12-31T23:59:59Z"
        );
        assert_eq!(
            utc_rfc3339(UNIX_EPOCH - Duration::from_millis(500)),
            "1969-12-31T23:59:59Z"
        );
        assert_eq!(
            utc_rfc3339(UNIX_EPOCH - Duration::from_secs(86_400 * 365)),
            "1969-01-01T00:00:00Z"
        );
    }
}
