//! Parses the IMF-fixdate of an HTTP `Date` header (`Sun, 22 Nov 2026 10:00:00 GMT`),
//! used to learn the token endpoint's clock when the local clock is skewed.

/// Unix seconds of an IMF-fixdate, or `None` when it is not one.
pub(crate) fn parse_imf_fixdate(value: &str) -> Option<u64> {
    let mut parts = value.split_whitespace();
    let _weekday = parts.next()?;
    let day: u32 = parts.next()?.parse().ok()?;
    let month = month_number(parts.next()?)?;
    let year: i64 = parts.next()?.parse().ok()?;
    let mut clock = parts.next()?.split(':');
    let hour: u64 = clock.next()?.parse().ok()?;
    let minute: u64 = clock.next()?.parse().ok()?;
    let second: u64 = clock.next()?.parse().ok()?;
    if parts.next()? != "GMT" || !(1..=31).contains(&day) || hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    let days = days_from_civil(year, month, day);
    u64::try_from(days).ok().map(|days| days * 86_400 + hour * 3_600 + minute * 60 + second)
}

fn month_number(name: &str) -> Option<u32> {
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    MONTHS.iter().position(|month| *month == name).and_then(|index| u32::try_from(index + 1).ok())
}

/// Days since 1970-01-01 of a proleptic Gregorian date (Howard Hinnant's algorithm).
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let month = i64::from(month);
    let shifted_month = if month > 2 { month - 3 } else { month + 9 };
    let day_of_year = (153 * shifted_month + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_known_dates() {
        assert_eq!(parse_imf_fixdate("Thu, 01 Jan 1970 00:00:00 GMT"), Some(0));
        assert_eq!(parse_imf_fixdate("Wed, 21 Oct 2015 07:28:00 GMT"), Some(1_445_412_480));
        assert_eq!(parse_imf_fixdate("Sun, 22 Nov 2026 10:00:00 GMT"), Some(1_795_341_600));
    }

    #[test]
    fn rejects_other_formats() {
        assert_eq!(parse_imf_fixdate("1445412480"), None);
        assert_eq!(parse_imf_fixdate("Wed, 21 Foo 2015 07:28:00 GMT"), None);
        assert_eq!(parse_imf_fixdate("Wed, 21 Oct 2015 07:28:00 PST"), None);
    }
}
