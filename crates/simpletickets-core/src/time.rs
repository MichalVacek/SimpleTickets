//! UTC date and time formatting without a calendar dependency.

use std::time::{SystemTime, UNIX_EPOCH};

fn unix_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// Converts days since 1970-01-01 to a (year, month, day) civil date.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

fn format_date(seconds: i64) -> String {
    let (y, m, d) = civil_from_days(seconds.div_euclid(86_400));
    format!("{y:04}-{m:02}-{d:02}")
}

fn format_timestamp(seconds: i64) -> String {
    let rem = seconds.rem_euclid(86_400);
    format!(
        "{}T{:02}:{:02}:{:02}Z",
        format_date(seconds),
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// The current UTC time as `YYYY-MM-DDTHH:MM:SSZ`.
pub fn now_timestamp() -> String {
    format_timestamp(unix_seconds())
}

/// The current UTC date as `YYYY-MM-DD`.
pub fn today() -> String {
    format_date(unix_seconds())
}

/// True when `text` has the shape `YYYY-MM-DD` with a plausible month and day.
pub fn is_date(text: &str) -> bool {
    let b = text.as_bytes();
    if b.len() != 10 || b[4] != b'-' || b[7] != b'-' {
        return false;
    }
    let digits = |r: std::ops::Range<usize>| b[r].iter().all(u8::is_ascii_digit);
    if !(digits(0..4) && digits(5..7) && digits(8..10)) {
        return false;
    }
    let month: u32 = text[5..7].parse().unwrap_or(0);
    let day: u32 = text[8..10].parse().unwrap_or(0);
    (1..=12).contains(&month) && (1..=31).contains(&day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_known_instants() {
        assert_eq!(format_timestamp(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_timestamp(951_782_400 + 3661), "2000-02-29T01:01:01Z");
        assert_eq!(format_date(1_798_675_200), "2026-12-31");
    }

    #[test]
    fn validates_dates() {
        assert!(is_date("2026-11-05"));
        assert!(!is_date("2026-13-05"));
        assert!(!is_date("2026-1-05"));
        assert!(!is_date("tomorrow"));
        assert!(is_date(&today()));
    }
}
