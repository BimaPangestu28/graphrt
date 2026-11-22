//! RFC 3339 timestamps (`expirationTimestamp` of exec credentials) without a
//! date-time crate.

use std::time::{Duration, SystemTime};

/// Parse `YYYY-MM-DDTHH:MM:SS[.frac](Z|±HH:MM)` into a `SystemTime`.
///
/// @returns `None` for malformed input or dates before 1970.
pub fn parse_rfc3339(text: &str) -> Option<SystemTime> {
    let bytes = text.as_bytes();
    if bytes.len() < 20 || bytes.get(4) != Some(&b'-') || bytes.get(7) != Some(&b'-') {
        return None;
    }
    if !matches!(bytes.get(10), Some(b'T' | b't' | b' ')) || bytes.get(13) != Some(&b':') || bytes.get(16) != Some(&b':')
    {
        return None;
    }
    let year = parse_digits(text.get(0..4)?)?;
    let month = parse_digits(text.get(5..7)?)?;
    let day = parse_digits(text.get(8..10)?)?;
    let hour = parse_digits(text.get(11..13)?)?;
    let minute = parse_digits(text.get(14..16)?)?;
    let second = parse_digits(text.get(17..19)?)?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    let mut rest = text.get(19..)?;
    if let Some(fraction) = rest.strip_prefix('.') {
        let digits = fraction.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 {
            return None;
        }
        rest = fraction.get(digits..)?;
    }
    let offset_secs = parse_offset(rest)?;
    let days = days_from_civil(year, month, day);
    let local_secs = days * 86_400 + hour * 3600 + minute * 60 + second;
    let utc_secs = local_secs - offset_secs;
    let utc_secs = u64::try_from(utc_secs).ok()?;
    Some(SystemTime::UNIX_EPOCH + Duration::from_secs(utc_secs))
}

fn parse_digits(text: &str) -> Option<i64> {
    if text.bytes().all(|byte| byte.is_ascii_digit()) { text.parse().ok() } else { None }
}

/// Seconds east of UTC for `Z` or `±HH:MM`.
fn parse_offset(text: &str) -> Option<i64> {
    if text == "Z" || text == "z" {
        return Some(0);
    }
    let sign = match text.as_bytes().first()? {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    if text.len() != 6 || text.as_bytes().get(3) != Some(&b':') {
        return None;
    }
    let hours = parse_digits(text.get(1..3)?)?;
    let minutes = parse_digits(text.get(4..6)?)?;
    Some(sign * (hours * 3600 + minutes * 60))
}

/// Days since 1970-01-01 of a proleptic Gregorian date (Howard Hinnant's algorithm).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let month_index = (month + 9) % 12;
    let day_of_year = (153 * month_index + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unix(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
    }

    #[test]
    fn parses_utc_fractional_and_offsets() {
        assert_eq!(parse_rfc3339("1970-01-01T00:00:00Z"), Some(unix(0)));
        assert_eq!(parse_rfc3339("2026-11-22T10:15:30Z"), Some(unix(1_795_342_530)));
        assert_eq!(parse_rfc3339("2026-11-22T10:15:30.123456789Z"), Some(unix(1_795_342_530)));
        assert_eq!(parse_rfc3339("2026-11-22T17:15:30+07:00"), Some(unix(1_795_342_530)));
        assert_eq!(parse_rfc3339("2026-11-22T05:15:30-05:00"), Some(unix(1_795_342_530)));
        assert_eq!(parse_rfc3339("2024-02-29T00:00:00Z"), Some(unix(1_709_164_800)));
    }

    #[test]
    fn rejects_malformed_timestamps() {
        for text in ["", "2026-11-22", "2026-13-01T00:00:00Z", "2026-11-22T10:15:30", "2026-11-22T10:15:30+0700", "x026-11-22T10:15:30Z", "2026-11-22T10:15:30.Z", "1969-12-31T23:59:59Z"] {
            assert_eq!(parse_rfc3339(text), None, "{text}");
        }
    }
}
