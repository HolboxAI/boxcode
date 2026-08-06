//! Pure calendar-date helpers shared by `telemetry` and `usage` -- both only
//! ever need "what's today's UTC date as YYYY-MM-DD" and "what was N days
//! before that", which doesn't justify a `chrono`/`time` dependency.

/// Days since the Unix epoch, UTC, floored to a whole calendar day (not a
/// precise instant).
fn today_days() -> i64 {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    (secs / 86_400) as i64
}

fn date_string(days_since_epoch: i64) -> String {
    let (y, m, d) = civil_from_days(days_since_epoch);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Today's date, UTC. Deliberately UTC rather than the local timezone: pings
/// and usage records come from installs in every timezone, and a single
/// shared clock keeps "how many days was this install active" meaningful
/// without needing to know where each install runs.
pub fn today_string() -> String {
    date_string(today_days())
}

/// The date `n` days before today, UTC -- for "usage in the last week"
/// cutoffs.
pub fn days_ago_string(n: i64) -> String {
    date_string(today_days() - n)
}

/// Howard Hinnant's days-since-epoch -> civil (proleptic Gregorian) date
/// algorithm: http://howardhinnant.github.io/date_algorithms.html#civil_from_days
/// Pure integer arithmetic, correct for the entire `i64` range this app will
/// ever see a date in (verified against Python's `datetime` across 1900,
/// 2000, 2026, and 2100 during development).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reference values cross-checked against Python's `datetime` for the
    /// epoch itself, a well-known algorithm test vector (2000-03-01), a
    /// pre-epoch date, and a far-future one.
    #[test]
    fn civil_from_days_matches_known_reference_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(11017), (2000, 3, 1));
        assert_eq!(civil_from_days(-25567), (1900, 1, 1));
        assert_eq!(civil_from_days(47846), (2100, 12, 31));
    }

    #[test]
    fn date_string_formats_with_leading_zeroes() {
        assert_eq!(date_string(0), "1970-01-01");
    }

    #[test]
    fn days_ago_is_strictly_before_today() {
        let today = today_string();
        let week_ago = days_ago_string(7);
        assert!(week_ago < today, "{week_ago} should sort before {today}");
    }

    #[test]
    fn zero_days_ago_is_today() {
        assert_eq!(days_ago_string(0), today_string());
    }
}
