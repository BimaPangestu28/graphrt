//! Revisions as deployment versions.
//!
//! Every deployment version N is revision `<app>--vN` with traffic label `vN`
//! (weight 0 once superseded). A label has its own FQDN
//! (`<app>---vN.<env domain>`), so a run paused on version N-1 resumes on
//! N-1's code while new runs go to N. Version N-1 stays (min 0 replicas) for
//! the retention window counted from when N was created, then its label is
//! removed and the revision deactivated (plan §1 decision 6, conformance C11).

use crate::http::json_at;
use std::time::Duration;

use serde_json::Value;

use super::spec::{version_label, version_of_revision};

/// One revision of a container app, as ARM lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Revision {
    pub name: String,
    pub version: Option<u64>,
    pub active: bool,
    /// Unix seconds.
    pub created_at: Option<u64>,
    pub replicas: u64,
    pub traffic_weight: u64,
}

impl Revision {
    /// Parse one entry of `GET …/revisions`.
    pub fn from_arm(app_name: &str, value: &Value) -> Option<Self> {
        let name = json_at(value, "/name").as_str()?.to_string();
        let properties = json_at(value, "/properties");
        Some(Self {
            version: version_of_revision(app_name, &name),
            active: json_at(properties, "/active").as_bool().unwrap_or(false),
            created_at: json_at(properties, "/createdTime").as_str().and_then(parse_rfc3339),
            replicas: json_at(properties, "/replicas").as_u64().unwrap_or(0),
            traffic_weight: json_at(properties, "/trafficWeight").as_u64().unwrap_or(0),
            name,
        })
    }
}

/// Revisions to retire at `now`: every active graphrt revision whose successor
/// version was created more than `retention` ago. The newest version is never retired.
pub fn plan_retirement(revisions: &[Revision], now_unix: u64, retention: Duration) -> Vec<String> {
    let mut versioned: Vec<&Revision> = revisions.iter().filter(|revision| revision.version.is_some()).collect();
    versioned.sort_by_key(|revision| revision.version);
    versioned
        .windows(2)
        .filter_map(|pair| {
            let (older, successor) = (pair.first()?, pair.get(1)?);
            let superseded_at = successor.created_at?;
            let expired = now_unix >= superseded_at.saturating_add(retention.as_secs());
            (older.active && expired).then(|| older.name.clone())
        })
        .collect()
}

/// Traffic list for a deploy of `version`: 100 % to the new revision (by
/// `latestRevision`, since it does not exist yet), and weight-0 labelled
/// entries for retained versions.
pub fn traffic_for_deploy(app_name: &str, retained_versions: &[u64]) -> Vec<Value> {
    let mut traffic = vec![serde_json::json!({ "latestRevision": true, "weight": 100 })];
    traffic.extend(retained_versions.iter().map(|version| labelled_entry(app_name, *version, 0)));
    traffic
}

/// Traffic list once `version` exists: labelled at 100 %, retained versions at 0 %.
pub fn traffic_after_deploy(app_name: &str, version: u64, retained_versions: &[u64]) -> Vec<Value> {
    let mut traffic = vec![labelled_entry(app_name, version, 100)];
    traffic.extend(
        retained_versions.iter().filter(|kept| **kept != version).map(|kept| labelled_entry(app_name, *kept, 0)),
    );
    traffic
}

fn labelled_entry(app_name: &str, version: u64, weight: u64) -> Value {
    serde_json::json!({
        "revisionName": super::spec::revision_name(app_name, version),
        "weight": weight,
        "label": version_label(version),
    })
}

/// Versions that currently have a label in the app's traffic list.
pub fn labelled_versions(app: &Value) -> Vec<u64> {
    json_at(app, "/properties/configuration/ingress/traffic")
        .as_array()
        .map(|entries| {
            entries
                .iter()
                .filter_map(|entry| json_at(entry, "/label").as_str()?.strip_prefix('v')?.parse().ok())
                .collect()
        })
        .unwrap_or_default()
}

/// FQDN of a label: `<app>---<label>.<rest of the app FQDN>`.
pub fn label_fqdn(app_fqdn: &str, label: &str) -> Option<String> {
    let (app, rest) = app_fqdn.split_once('.')?;
    Some(format!("{app}---{label}.{rest}"))
}

/// Minimal RFC 3339 (`2026-11-22T10:00:00[.fff](Z|±hh:mm)`) to Unix seconds.
pub fn parse_rfc3339(text: &str) -> Option<u64> {
    let number = |range: std::ops::Range<usize>| -> Option<i64> { text.get(range)?.parse().ok() };
    let (year, month, day) = (number(0..4)?, number(5..7)?, number(8..10)?);
    let (hour, minute, second) = (number(11..13)?, number(14..16)?, number(17..19)?);
    if text.get(4..5) != Some("-") || text.get(10..11).is_none_or(|t| t != "T" && t != "t") {
        return None;
    }
    let zone = text.get(19..)?.trim_start_matches(|c: char| c == '.' || c.is_ascii_digit());
    let offset_secs = match zone {
        "Z" | "z" | "" => 0,
        signed => {
            let sign = if signed.starts_with('-') { -1 } else { 1 };
            let hours: i64 = signed.get(1..3)?.parse().ok()?;
            let minutes: i64 = signed.get(4..6)?.parse().ok()?;
            sign * (hours * 3600 + minutes * 60)
        }
    };
    // Days from civil (Howard Hinnant's algorithm).
    let shifted_year = if month <= 2 { year - 1 } else { year };
    let era = shifted_year.div_euclid(400);
    let year_of_era = shifted_year - era * 400;
    let day_of_year = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146_097 + day_of_era - 719_468;
    u64::try_from(days * 86_400 + hour * 3600 + minute * 60 + second - offset_secs).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: u64 = 86_400;

    fn revision(version: u64, created_at: u64, active: bool) -> Revision {
        Revision {
            name: format!("bot--v{version}"),
            version: Some(version),
            active,
            created_at: Some(created_at),
            replicas: 0,
            traffic_weight: 0,
        }
    }

    #[test]
    fn old_version_is_kept_six_days_and_retired_on_day_eight() {
        let retention = Duration::from_secs(7 * DAY);
        let revisions = [revision(1, 0, true), revision(2, 10 * DAY, true)];
        assert!(plan_retirement(&revisions, 16 * DAY, retention).is_empty(), "day 6 keeps v1");
        assert_eq!(plan_retirement(&revisions, 18 * DAY, retention), vec!["bot--v1".to_string()], "day 8 retires v1");
        let already_inactive = [revision(1, 0, false), revision(2, 10 * DAY, true)];
        assert!(plan_retirement(&already_inactive, 30 * DAY, retention).is_empty());
        assert!(plan_retirement(&[revision(5, 0, true)], 100 * DAY, retention).is_empty(), "newest is never retired");
    }

    #[test]
    fn rfc3339_parses_arm_timestamps() {
        assert_eq!(parse_rfc3339("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_rfc3339("2026-11-22T10:00:00Z"), Some(1_795_341_600));
        assert_eq!(parse_rfc3339("2026-11-22T10:00:00.1234567+00:00"), Some(1_795_341_600));
        assert_eq!(parse_rfc3339("2026-11-22T12:00:00+02:00"), Some(1_795_341_600));
        assert_eq!(parse_rfc3339("not a date"), None);
    }

    #[test]
    fn label_fqdn_and_traffic_lists() {
        assert_eq!(
            label_fqdn("bot.internal.happy-sky.westeurope.azurecontainerapps.io", "v2").as_deref(),
            Some("bot---v2.internal.happy-sky.westeurope.azurecontainerapps.io")
        );
        let traffic = traffic_after_deploy("bot", 3, &[2, 3]);
        assert_eq!(traffic.len(), 2);
        assert_eq!(traffic[0]["label"], "v3");
        assert_eq!(traffic[1]["weight"], 0);
        let app = serde_json::json!({"properties": {"configuration": {"ingress": {"traffic": traffic}}}});
        assert_eq!(labelled_versions(&app), vec![3, 2]);
    }
}
