//! Time zones for cron without embedding a tz database: IANA zones are read
//! from the system's TZif files (`$TZDIR` or `/usr/share/zoneinfo`), with the
//! POSIX TZ footer rule for instants past the last transition. `UTC` and fixed
//! offsets (`+07:00`) need no file. The container image ships zoneinfo
//! (`tzdata`) for named zones.

use std::path::PathBuf;

/// Days since 1970-01-01 of a proleptic Gregorian date (Hinnant).
pub fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let month = i64::from(month);
    let day_of_year = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// `(year, month, day)` of days since 1970-01-01.
pub fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted - era * 146_097;
    let year_of_era = (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * month_index + 2) / 5 + 1) as u32;
    let month = if month_index < 10 { month_index + 3 } else { month_index - 9 } as u32;
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// Day of week of days since epoch, 0 = Sunday.
pub fn weekday(days: i64) -> u32 {
    (days + 4).rem_euclid(7) as u32
}

fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ if (year % 4 == 0 && year % 100 != 0) || year % 400 == 0 => 29,
        _ => 28,
    }
}

/// When a DST change happens: `Mm.w.d/time` (the only form IANA footers use for
/// zones with DST), `time` in seconds after local midnight.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RuleDate {
    month: u32,
    week: u32,
    weekday: u32,
    time_secs: i64,
}

impl RuleDate {
    /// Local seconds since epoch of this rule in `year` (wall clock of the offset in force before the change).
    fn local_secs(&self, year: i64) -> i64 {
        let first = days_from_civil(year, self.month, 1);
        let first_weekday = weekday(first);
        let mut day = 1 + (self.weekday + 7 - first_weekday) % 7 + (self.week.min(5) - 1) * 7;
        while day > days_in_month(year, self.month) {
            day -= 7;
        }
        (first + i64::from(day) - 1) * 86_400 + self.time_secs
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PosixRule {
    std_offset: i32,
    dst: Option<(i32, RuleDate, RuleDate)>,
}

impl PosixRule {
    fn offset_at(&self, utc: i64) -> i32 {
        let Some((dst_offset, start, end)) = self.dst else { return self.std_offset };
        let (year, _, _) = civil_from_days(utc.div_euclid(86_400));
        let start_utc = start.local_secs(year) - i64::from(self.std_offset);
        let end_utc = end.local_secs(year) - i64::from(dst_offset);
        let in_dst =
            if start_utc < end_utc { utc >= start_utc && utc < end_utc } else { utc >= start_utc || utc < end_utc };
        if in_dst { dst_offset } else { self.std_offset }
    }
}

/// A time zone: UTC offsets over time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Zone {
    name: String,
    transitions: Vec<(i64, i32)>,
    initial_offset: i32,
    rule: Option<PosixRule>,
}

impl Zone {
    /// UTC.
    pub fn utc() -> Self {
        Self::fixed("UTC", 0)
    }

    /// A fixed offset in seconds east of UTC.
    pub fn fixed(name: &str, offset_secs: i32) -> Self {
        Self { name: name.to_owned(), transitions: Vec::new(), initial_offset: offset_secs, rule: None }
    }

    /// The zone's name as configured.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Resolves `UTC`, `+HH:MM` / `-HH:MM`, or an IANA name from the system zoneinfo.
    ///
    /// # Errors
    /// Unknown name, unreadable or invalid TZif file.
    pub fn load(name: &str) -> Result<Self, String> {
        if name.eq_ignore_ascii_case("utc") || name == "Etc/UTC" || name == "Z" {
            return Ok(Self::utc());
        }
        if let Some(offset) = parse_fixed_offset(name) {
            return Ok(Self::fixed(name, offset));
        }
        if name.contains("..")
            || name.starts_with('/')
            || !name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '_' | '-' | '+'))
        {
            return Err(format!("invalid time zone name `{name}`"));
        }
        let dir = std::env::var_os("TZDIR").map_or_else(|| PathBuf::from("/usr/share/zoneinfo"), PathBuf::from);
        let bytes = std::fs::read(dir.join(name))
            .map_err(|error| format!("time zone `{name}`: {error} (install tzdata or set TZDIR)"))?;
        let mut zone = parse_tzif(&bytes).ok_or_else(|| format!("time zone `{name}`: not a valid TZif file"))?;
        zone.name = name.to_owned();
        Ok(zone)
    }

    /// Offset (seconds east of UTC) in force at `utc` (unix seconds).
    pub fn offset_at(&self, utc: i64) -> i32 {
        let position = self.transitions.partition_point(|(at, _)| *at <= utc);
        if position == self.transitions.len()
            && let Some(rule) = &self.rule
            && (self.transitions.is_empty() || position > 0)
        {
            return rule.offset_at(utc);
        }
        match position.checked_sub(1).and_then(|index| self.transitions.get(index)) {
            Some((_, offset)) => *offset,
            None => self.initial_offset,
        }
    }

    /// The UTC instants whose local wall time is `local` (seconds since epoch
    /// on the local clock): none in a DST gap, two in a DST overlap (earlier first).
    pub fn local_to_utc(&self, local: i64) -> Vec<i64> {
        let mut offsets = vec![self.offset_at(local - 86_400), self.offset_at(local), self.offset_at(local + 86_400)];
        offsets.sort_unstable();
        offsets.dedup();
        let mut instants: Vec<i64> = offsets
            .into_iter()
            .map(|offset| local - i64::from(offset))
            .filter(|utc| i64::from(self.offset_at(*utc)) == local - utc)
            .collect();
        instants.sort_unstable();
        instants.dedup();
        instants
    }
}

fn parse_fixed_offset(text: &str) -> Option<i32> {
    let (sign, rest) = match text.as_bytes().first()? {
        b'+' => (1, text.get(1..)?),
        b'-' => (-1, text.get(1..)?),
        _ => return None,
    };
    let (hours, minutes) = rest.split_once(':').unwrap_or((rest, "0"));
    let hours: i32 = hours.parse().ok().filter(|hours| *hours <= 14)?;
    let minutes: i32 = minutes.parse().ok().filter(|minutes| *minutes < 60)?;
    Some(sign * (hours * 3600 + minutes * 60))
}

struct Cursor<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl Cursor<'_> {
    fn take(&mut self, count: usize) -> Option<&[u8]> {
        let slice = self.bytes.get(self.position..self.position.checked_add(count)?)?;
        self.position += count;
        Some(slice)
    }
    fn u32(&mut self) -> Option<u32> {
        self.take(4).and_then(|bytes| bytes.try_into().ok()).map(u32::from_be_bytes)
    }
    fn i32(&mut self) -> Option<i32> {
        self.take(4).and_then(|bytes| bytes.try_into().ok()).map(i32::from_be_bytes)
    }
    fn i64(&mut self) -> Option<i64> {
        self.take(8).and_then(|bytes| bytes.try_into().ok()).map(i64::from_be_bytes)
    }
    fn u8(&mut self) -> Option<u8> {
        self.take(1).and_then(|bytes| bytes.first().copied())
    }
}

struct Counts {
    isut: usize,
    isstd: usize,
    leap: usize,
    time: usize,
    types: usize,
    chars: usize,
}

fn read_header(cursor: &mut Cursor<'_>) -> Option<(u8, Counts)> {
    if cursor.take(4)? != b"TZif" {
        return None;
    }
    let version = cursor.u8()?;
    cursor.take(15)?;
    let mut next = || cursor.u32().map(|value| value as usize);
    Some((
        version,
        Counts { isut: next()?, isstd: next()?, leap: next()?, time: next()?, types: next()?, chars: next()? },
    ))
}

/// Parses a TZif v2+ file (64-bit section and footer rule).
fn parse_tzif(bytes: &[u8]) -> Option<Zone> {
    let mut cursor = Cursor { bytes, position: 0 };
    let (version, v1) = read_header(&mut cursor)?;
    if version < b'2' {
        return None;
    }
    cursor.take(v1.time * 5 + v1.types * 6 + v1.chars + v1.leap * 8 + v1.isstd + v1.isut)?;
    let (_, counts) = read_header(&mut cursor)?;
    let times: Vec<i64> = (0..counts.time).map(|_| cursor.i64()).collect::<Option<_>>()?;
    let indices: Vec<u8> = (0..counts.time).map(|_| cursor.u8()).collect::<Option<_>>()?;
    let mut types = Vec::with_capacity(counts.types);
    for _ in 0..counts.types {
        let offset = cursor.i32()?;
        cursor.take(2)?;
        types.push(offset);
    }
    cursor.take(counts.chars + counts.leap * 12 + counts.isstd + counts.isut)?;
    let footer =
        cursor.bytes.get(cursor.position..).and_then(|rest| std::str::from_utf8(rest).ok()).unwrap_or_default();
    let rule = footer.trim_matches('\n').lines().next().and_then(parse_posix_rule);
    let transitions = times
        .into_iter()
        .zip(indices)
        .map(|(at, index)| Some((at, *types.get(usize::from(index))?)))
        .collect::<Option<Vec<_>>>()?;
    Some(Zone { name: String::new(), transitions, initial_offset: types.first().copied().unwrap_or(0), rule })
}

/// Parses `STDoffset[DST[offset][,start[/time],end[/time]]]` (POSIX offsets are west-positive).
fn parse_posix_rule(text: &str) -> Option<PosixRule> {
    let mut rest = text;
    skip_name(&mut rest)?;
    let std_offset = -parse_posix_offset(&mut rest)?;
    if rest.is_empty() {
        return Some(PosixRule { std_offset, dst: None });
    }
    skip_name(&mut rest)?;
    let dst_offset = if rest.starts_with(',') { std_offset + 3600 } else { -parse_posix_offset(&mut rest)? };
    let rules = rest.strip_prefix(',')?;
    let (start, end) = rules.split_once(',')?;
    Some(PosixRule { std_offset, dst: Some((dst_offset, parse_rule_date(start)?, parse_rule_date(end)?)) })
}

fn skip_name(rest: &mut &str) -> Option<()> {
    if let Some(quoted) = rest.strip_prefix('<') {
        let end = quoted.find('>')?;
        *rest = quoted.get(end + 1..)?;
        return Some(());
    }
    let length = rest.find(|c: char| !c.is_ascii_alphabetic()).unwrap_or(rest.len());
    if length < 3 {
        return None;
    }
    *rest = rest.get(length..)?;
    Some(())
}

/// `[+-]hh[:mm[:ss]]` in seconds.
fn parse_posix_offset(rest: &mut &str) -> Option<i32> {
    let length = rest.find(|c: char| !(c.is_ascii_digit() || matches!(c, ':' | '+' | '-'))).unwrap_or(rest.len());
    let token = rest.get(..length)?;
    *rest = rest.get(length..)?;
    parse_hms(token)
}

fn parse_hms(token: &str) -> Option<i32> {
    let (sign, digits) = match token.as_bytes().first()? {
        b'-' => (-1, token.get(1..)?),
        b'+' => (1, token.get(1..)?),
        _ => (1, token),
    };
    let mut parts = digits.split(':').map(|part| part.parse::<i32>().ok());
    let hours = parts.next()??;
    let minutes = parts.next().unwrap_or(Some(0))?;
    let seconds = parts.next().unwrap_or(Some(0))?;
    Some(sign * (hours * 3600 + minutes * 60 + seconds))
}

fn parse_rule_date(text: &str) -> Option<RuleDate> {
    let (date, time) = text.split_once('/').unwrap_or((text, "2"));
    let mut fields = date.strip_prefix('M')?.split('.').map(|field| field.parse::<u32>().ok());
    let month = fields.next()??;
    let week = fields.next()??;
    let weekday = fields.next()??;
    if !(1..=12).contains(&month) || !(1..=5).contains(&week) || weekday > 6 {
        return None;
    }
    Some(RuleDate { month, week, weekday, time_secs: i64::from(parse_hms(time)?) })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn berlin_rule() -> Zone {
        Zone {
            name: "rule".into(),
            transitions: Vec::new(),
            initial_offset: 3600,
            rule: parse_posix_rule("CET-1CEST,M3.5.0,M10.5.0/3"),
        }
    }

    #[test]
    fn civil_round_trip() {
        for days in [-1_000_000, -1, 0, 1, 19_000, 2_932_896] {
            let (year, month, day) = civil_from_days(days);
            assert_eq!(days_from_civil(year, month, day), days);
        }
        assert_eq!(civil_from_days(days_from_civil(2024, 2, 29)), (2024, 2, 29));
        assert_eq!(weekday(days_from_civil(2026, 11, 22)), 0, "2026-11-22 is a Sunday");
    }

    #[test]
    fn posix_rule_dst_switches() {
        let zone = berlin_rule();
        // 2026-03-29 01:00 UTC: CET → CEST.
        let switch = days_from_civil(2026, 3, 29) * 86_400 + 3600;
        assert_eq!(zone.offset_at(switch - 1), 3600);
        assert_eq!(zone.offset_at(switch), 7200);
        // 2026-10-25 01:00 UTC: CEST → CET.
        let back = days_from_civil(2026, 10, 25) * 86_400 + 3600;
        assert_eq!(zone.offset_at(back - 1), 7200);
        assert_eq!(zone.offset_at(back), 3600);
    }

    #[test]
    fn gaps_and_overlaps() {
        let zone = berlin_rule();
        let gap_local = days_from_civil(2026, 3, 29) * 86_400 + 2 * 3600 + 30 * 60;
        assert!(zone.local_to_utc(gap_local).is_empty(), "02:30 does not exist on 2026-03-29");
        let overlap_local = days_from_civil(2026, 10, 25) * 86_400 + 2 * 3600 + 30 * 60;
        let instants = zone.local_to_utc(overlap_local);
        assert_eq!(instants.len(), 2, "02:30 happens twice on 2026-10-25");
        assert_eq!(instants[1] - instants[0], 3600);
        assert_eq!(Zone::utc().local_to_utc(1000), vec![1000]);
    }

    #[test]
    fn fixed_offsets_and_names() {
        assert_eq!(Zone::load("+07:00").unwrap().offset_at(0), 25_200);
        assert_eq!(Zone::load("-03:30").unwrap().offset_at(0), -12_600);
        assert!(Zone::load("../etc/passwd").is_err());
        assert_eq!(Zone::load("UTC").unwrap().offset_at(0), 0);
    }

    #[test]
    fn system_zoneinfo_when_present() {
        let Ok(zone) = Zone::load("Europe/Berlin") else { return };
        let summer = days_from_civil(2030, 7, 1) * 86_400;
        let winter = days_from_civil(2030, 1, 1) * 86_400;
        assert_eq!(zone.offset_at(summer), 7200);
        assert_eq!(zone.offset_at(winter), 3600);
    }
}
