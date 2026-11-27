//! Cron expressions and time zones (feature `cron`), shared by the gateway's
//! schedules and the sync schedules of knowledge sources.
//!
//! - [`CronExpr`]: five fields (`minute hour day-of-month month day-of-week`)
//!   or `@hourly` / `@daily` / `@weekly` / `@monthly` / `@yearly`.
//! - [`tz::Zone`]: `UTC`, fixed offsets (`+07:00`) or IANA names read from the
//!   system zoneinfo, without an embedded tz database.

pub mod tz;

use std::collections::BTreeSet;

use self::tz::{Zone, civil_from_days, weekday};

/// Upper bound on days searched for the next fire (`0 0 29 2 1` can be years away).
pub const SEARCH_DAYS: i64 = 366 * 8;

/// A parsed `minute hour day-of-month month day-of-week` expression.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CronExpr {
    minutes: BTreeSet<u32>,
    hours: BTreeSet<u32>,
    days_of_month: BTreeSet<u32>,
    months: BTreeSet<u32>,
    days_of_week: BTreeSet<u32>,
    day_of_month_any: bool,
    day_of_week_any: bool,
}

const MONTH_NAMES: [&str; 12] = ["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"];
const DAY_NAMES: [&str; 7] = ["sun", "mon", "tue", "wed", "thu", "fri", "sat"];

fn parse_value(text: &str, names: &[&str], base: u32) -> Option<u32> {
    let lower = text.to_ascii_lowercase();
    if let Some(position) = names.iter().position(|name| *name == lower) {
        return Some(position as u32 + base);
    }
    text.parse().ok()
}

fn parse_field(field: &str, min: u32, max: u32, names: &[&str], base: u32) -> Result<BTreeSet<u32>, String> {
    let mut values = BTreeSet::new();
    for part in field.split(',') {
        let (range, step) = match part.split_once('/') {
            Some((range, step)) => (
                range,
                step.parse::<u32>().ok().filter(|step| *step > 0).ok_or_else(|| format!("bad step in `{part}`"))?,
            ),
            None => (part, 1),
        };
        let (low, high) = if range == "*" {
            (min, max)
        } else if let Some((low, high)) = range.split_once('-') {
            let low = parse_value(low, names, base).ok_or_else(|| format!("bad value in `{part}`"))?;
            let high = parse_value(high, names, base).ok_or_else(|| format!("bad value in `{part}`"))?;
            (low, high)
        } else {
            let value = parse_value(range, names, base).ok_or_else(|| format!("bad value in `{part}`"))?;
            (value, if part.contains('/') { max } else { value })
        };
        if low < min || high > max || low > high {
            return Err(format!("`{part}` is outside {min}-{max}"));
        }
        values.extend((low..=high).step_by(step as usize));
    }
    Ok(values)
}

impl CronExpr {
    /// Parses five fields or `@hourly` / `@daily` / `@weekly` / `@monthly` / `@yearly`.
    ///
    /// # Errors
    /// A message naming the bad field.
    pub fn parse(expression: &str) -> Result<Self, String> {
        let expanded = match expression.trim() {
            "@hourly" => "0 * * * *",
            "@daily" | "@midnight" => "0 0 * * *",
            "@weekly" => "0 0 * * 0",
            "@monthly" => "0 0 1 * *",
            "@yearly" | "@annually" => "0 0 1 1 *",
            other => other,
        };
        let fields: Vec<&str> = expanded.split_whitespace().collect();
        let [minute, hour, day_of_month, month, day_of_week] = fields.as_slice() else {
            return Err(format!("cron `{expression}` needs 5 fields"));
        };
        let mut days_of_week = parse_field(day_of_week, 0, 7, &DAY_NAMES, 0)?;
        if days_of_week.remove(&7) {
            days_of_week.insert(0);
        }
        Ok(Self {
            minutes: parse_field(minute, 0, 59, &[], 0)?,
            hours: parse_field(hour, 0, 23, &[], 0)?,
            days_of_month: parse_field(day_of_month, 1, 31, &[], 0)?,
            months: parse_field(month, 1, 12, &MONTH_NAMES, 1)?,
            days_of_week,
            day_of_month_any: *day_of_month == "*",
            day_of_week_any: *day_of_week == "*",
        })
    }

    /// Whether the expression allows the day `days` (days since 1970-01-01).
    pub fn day_matches(&self, days: i64) -> bool {
        let (_, month, day) = civil_from_days(days);
        if !self.months.contains(&month) {
            return false;
        }
        let by_month_day = self.days_of_month.contains(&day);
        let by_week_day = self.days_of_week.contains(&weekday(days));
        // Vixie cron: when both are restricted, either may match.
        match (self.day_of_month_any, self.day_of_week_any) {
            (true, true) => true,
            (false, true) => by_month_day,
            (true, false) => by_week_day,
            (false, false) => by_month_day || by_week_day,
        }
    }

    /// Minutes the expression fires at, ascending.
    pub fn minutes(&self) -> &BTreeSet<u32> {
        &self.minutes
    }

    /// Hours the expression fires at, ascending.
    pub fn hours(&self) -> &BTreeSet<u32> {
        &self.hours
    }

    /// Next instant (unix seconds) strictly after `after` at which the
    /// expression fires in `zone`. A wall time repeated by a DST overlap fires
    /// once (its first occurrence), one that falls into a DST gap is skipped.
    /// `None` when nothing matches within [`SEARCH_DAYS`] (`0 0 30 2 *`).
    pub fn next_after(&self, after: i64, zone: &Zone) -> Option<i64> {
        let start_local = after + i64::from(zone.offset_at(after));
        let first_day = start_local.div_euclid(86_400);
        for days in first_day..first_day + SEARCH_DAYS {
            if !self.day_matches(days) {
                continue;
            }
            for hour in &self.hours {
                for minute in &self.minutes {
                    let local = days * 86_400 + i64::from(*hour) * 3600 + i64::from(*minute) * 60;
                    if local < start_local - 86_400 {
                        continue;
                    }
                    if let Some(first) = zone.local_to_utc(local).first().filter(|first| **first > after) {
                        return Some(*first);
                    }
                }
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::tz::days_from_civil;
    use super::*;

    fn utc(year: i64, month: u32, day: u32, hour: u32, minute: u32) -> i64 {
        days_from_civil(year, month, day) * 86_400 + i64::from(hour) * 3600 + i64::from(minute) * 60
    }

    #[test]
    fn parses_fields() {
        let expr = CronExpr::parse("*/15 9-17 * jan-mar mon-fri").unwrap();
        assert_eq!(expr.minutes, BTreeSet::from([0, 15, 30, 45]));
        assert_eq!(expr.hours.len(), 9);
        assert_eq!(expr.months, BTreeSet::from([1, 2, 3]));
        assert_eq!(expr.days_of_week, BTreeSet::from([1, 2, 3, 4, 5]));
        assert!(CronExpr::parse("60 * * * *").is_err());
        assert!(CronExpr::parse("* * *").is_err());
        assert_eq!(CronExpr::parse("0 0 * * 7").unwrap().days_of_week, BTreeSet::from([0]));
    }

    #[test]
    fn next_after_in_a_fixed_zone() {
        let expr = CronExpr::parse("0 6 * * *").unwrap();
        let jakarta = Zone::load("+07:00").unwrap();
        // 06:00 in +07:00 is 23:00 UTC of the day before.
        assert_eq!(expr.next_after(utc(2026, 11, 27, 12, 0), &jakarta), Some(utc(2026, 11, 27, 23, 0)));
        assert_eq!(expr.next_after(utc(2026, 11, 27, 23, 0), &jakarta), Some(utc(2026, 11, 28, 23, 0)));
        assert_eq!(CronExpr::parse("0 0 30 2 *").unwrap().next_after(0, &Zone::utc()), None);
    }
}
