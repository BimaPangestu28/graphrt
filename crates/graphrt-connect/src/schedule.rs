//! When a source syncs by itself.
//!
//! ```yaml
//! schedule: manual                                   # only on request (default)
//! schedule: { every: 6h }                            # 30m, 6h, 1d (at least 5 minutes)
//! schedule: { daily: "06:00", timezone: Asia/Jakarta }
//! schedule: { cron: "0 */4 * * 1-5", timezone: UTC }
//! ```
//!
//! A source that never synced is due at once. A sync missed while nothing was
//! running (the computer slept, the server was down) runs once on the next
//! start, not once per missed slot.

use graphrt_core::cron::CronExpr;
use graphrt_core::cron::tz::Zone;
use serde::{Deserialize, Serialize};

use crate::error::{ConnectError, Result};

/// Shortest interval between two scheduled syncs.
pub const MIN_INTERVAL_SECS: u64 = 5 * 60;
/// Longest interval (one year).
pub const MAX_INTERVAL_SECS: u64 = 366 * 24 * 3600;

/// When a source syncs on its own.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "RawSchedule", into = "RawSchedule")]
pub enum SyncSchedule {
    /// Only when someone asks ("Sync now", the API, `graphrt kb sync`).
    #[default]
    Manual,
    /// Every `secs` seconds after the last sync.
    Every { secs: u64 },
    /// Every day at `time` (`HH:MM`) in `timezone`.
    Daily { time: String, timezone: String },
    /// A five field cron expression in `timezone`.
    Cron { expression: String, timezone: String },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged)]
enum RawSchedule {
    Name(String),
    Map(RawMap),
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawMap {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    every: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    daily: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cron: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    timezone: Option<String>,
}

impl TryFrom<RawSchedule> for SyncSchedule {
    type Error = String;

    fn try_from(raw: RawSchedule) -> std::result::Result<Self, String> {
        let schedule = match raw {
            RawSchedule::Name(name) if name.trim().eq_ignore_ascii_case("manual") => Self::Manual,
            RawSchedule::Name(name) => return Err(format!("schedule `{name}`: use manual, every, daily or cron")),
            RawSchedule::Map(map) => {
                let timezone = map.timezone.clone().unwrap_or_else(|| "UTC".to_owned());
                match (map.every, map.daily, map.cron) {
                    (Some(every), None, None) => Self::Every { secs: parse_duration(&every)? },
                    (None, Some(time), None) => Self::Daily { time, timezone },
                    (None, None, Some(expression)) => Self::Cron { expression, timezone },
                    (None, None, None) => Self::Manual,
                    _ => return Err("a schedule has one of every, daily or cron".to_owned()),
                }
            }
        };
        schedule.validate().map_err(|e| e.to_string())?;
        Ok(schedule)
    }
}

impl From<SyncSchedule> for RawSchedule {
    fn from(schedule: SyncSchedule) -> Self {
        match schedule {
            SyncSchedule::Manual => Self::Name("manual".to_owned()),
            SyncSchedule::Every { secs } => {
                Self::Map(RawMap { every: Some(format_duration(secs)), ..RawMap::default() })
            }
            SyncSchedule::Daily { time, timezone } => {
                Self::Map(RawMap { daily: Some(time), timezone: Some(timezone), ..RawMap::default() })
            }
            SyncSchedule::Cron { expression, timezone } => {
                Self::Map(RawMap { cron: Some(expression), timezone: Some(timezone), ..RawMap::default() })
            }
        }
    }
}

/// `30m`, `6h`, `1d`, `90s` or a number of seconds.
fn parse_duration(text: &str) -> std::result::Result<u64, String> {
    let text = text.trim();
    let (number, unit) = text.split_at(text.find(|c: char| !c.is_ascii_digit()).unwrap_or(text.len()));
    let value: u64 = number.parse().map_err(|_| format!("`{text}` is not a duration such as 30m, 6h or 1d"))?;
    let factor = match unit.trim() {
        "" | "s" => 1,
        "m" | "min" => 60,
        "h" => 3600,
        "d" => 86_400,
        _ => return Err(format!("`{text}` is not a duration such as 30m, 6h or 1d")),
    };
    Ok(value.saturating_mul(factor))
}

fn format_duration(secs: u64) -> String {
    if secs.is_multiple_of(86_400) {
        format!("{}d", secs / 86_400)
    } else if secs.is_multiple_of(3600) {
        format!("{}h", secs / 3600)
    } else if secs.is_multiple_of(60) {
        format!("{}m", secs / 60)
    } else {
        format!("{secs}s")
    }
}

/// `HH:MM` -> `(hour, minute)`.
fn parse_time(time: &str) -> Option<(u32, u32)> {
    let (hour, minute) = time.trim().split_once(':')?;
    let (hour, minute) = (hour.parse::<u32>().ok()?, minute.parse::<u32>().ok()?);
    (hour < 24 && minute < 60).then_some((hour, minute))
}

impl SyncSchedule {
    /// Every `hours` hours.
    pub fn every_hours(hours: u64) -> Self {
        Self::Every { secs: hours.saturating_mul(3600) }
    }

    /// Check the interval, time, expression and time zone.
    ///
    /// @throws `ConnectError::Config` naming the problem.
    pub fn validate(&self) -> Result<()> {
        match self {
            Self::Manual => Ok(()),
            Self::Every { secs } => {
                if (MIN_INTERVAL_SECS..=MAX_INTERVAL_SECS).contains(secs) {
                    Ok(())
                } else {
                    Err(ConnectError::Config("a sync interval is between 5 minutes and one year".into()))
                }
            }
            Self::Daily { time, timezone } => {
                parse_time(time).ok_or_else(|| ConnectError::Config(format!("`{time}` is not a time (HH:MM)")))?;
                Zone::load(timezone).map_err(ConnectError::Config)?;
                Ok(())
            }
            Self::Cron { expression, timezone } => {
                CronExpr::parse(expression).map_err(ConnectError::Config)?;
                Zone::load(timezone).map_err(ConnectError::Config)?;
                Ok(())
            }
        }
    }

    /// When the next sync is due (unix seconds), given the end of the last
    /// sync. `None` for manual sources. A due time in the past means "now".
    pub fn next_due(&self, last_sync: Option<i64>, now: i64) -> Option<i64> {
        if *self == Self::Manual {
            return None;
        }
        let Some(last) = last_sync else { return Some(now) };
        match self {
            Self::Manual => None,
            Self::Every { secs } => Some(last.saturating_add(i64::try_from(*secs).unwrap_or(i64::MAX))),
            Self::Daily { time, timezone } => {
                let (hour, minute) = parse_time(time)?;
                let expr = CronExpr::parse(&format!("{minute} {hour} * * *")).ok()?;
                expr.next_after(last, &Zone::load(timezone).ok()?)
            }
            Self::Cron { expression, timezone } => {
                CronExpr::parse(expression).ok()?.next_after(last, &Zone::load(timezone).ok()?)
            }
        }
    }

    /// Whether a sync is due at `now`.
    pub fn is_due(&self, last_sync: Option<i64>, now: i64) -> bool {
        self.next_due(last_sync, now).is_some_and(|due| due <= now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(yaml_like: serde_json::Value) -> std::result::Result<SyncSchedule, String> {
        serde_json::from_value(yaml_like).map_err(|e| e.to_string())
    }

    #[test]
    fn forms() {
        assert_eq!(parse(serde_json::json!("manual")).unwrap(), SyncSchedule::Manual);
        assert_eq!(parse(serde_json::json!({ "every": "6h" })).unwrap(), SyncSchedule::Every { secs: 21_600 });
        assert!(parse(serde_json::json!({ "every": "1m" })).is_err(), "too often");
        assert!(parse(serde_json::json!({ "every": "6h", "cron": "* * * * *" })).is_err());
        assert!(parse(serde_json::json!({ "daily": "25:00" })).is_err());
        assert!(parse(serde_json::json!({ "cron": "0 * * *" })).is_err());
        let daily = parse(serde_json::json!({ "daily": "06:00", "timezone": "+07:00" })).unwrap();
        assert_eq!(
            serde_json::to_value(&daily).unwrap(),
            serde_json::json!({ "daily": "06:00", "timezone": "+07:00" })
        );
        assert_eq!(serde_json::to_value(SyncSchedule::every_hours(24)).unwrap(), serde_json::json!({ "every": "1d" }));
    }

    #[test]
    fn due_times() {
        let now = 1_000_000;
        assert_eq!(SyncSchedule::Manual.next_due(None, now), None);
        let every = SyncSchedule::every_hours(1);
        assert!(every.is_due(None, now), "never synced: due at once");
        assert!(!every.is_due(Some(now - 60), now));
        assert!(every.is_due(Some(now - 3600), now));
        // 06:00 in +07:00 is 23:00 UTC.
        let daily = SyncSchedule::Daily { time: "06:00".into(), timezone: "+07:00".into() };
        let last = 82_800 - 10; // 1970-01-01 22:59:50 UTC
        assert_eq!(daily.next_due(Some(last), now), Some(82_800));
        assert!(daily.is_due(Some(last), now), "missed while down: due now, once");
    }
}
