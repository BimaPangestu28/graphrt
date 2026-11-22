//! Revisions as deployment versions.
//!
//! Every deployment version is a revision `<app>--<label>` with a traffic
//! label `<label>` (see [`version_label`]: `v7` stays `v7`, `7` becomes `v7`),
//! at weight 0 once superseded. A label has its own FQDN
//! (`<app>---<label>.<env domain>`), so a run paused on version N-1 resumes on
//! N-1's code while new runs go to N. Version N-1 stays (min 0 replicas) for
//! the retention window counted from when its successor was created, then its
//! label is removed and the revision deactivated (plan §1 decision 6, C11).

use std::time::Duration;

use serde_json::Value;

use super::names::{revision_name, version_label};
use crate::http::json_at;

/// One revision of a container app, as ARM lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Revision {
    pub name: String,
    /// The revision suffix (version label) when the name is `<app>--<label>`.
    pub label: Option<String>,
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
            label: label_of_revision(app_name, &name),
            active: json_at(properties, "/active").as_bool().unwrap_or(false),
            created_at: json_at(properties, "/createdTime").as_str().and_then(parse_rfc3339),
            replicas: json_at(properties, "/replicas").as_u64().unwrap_or(0),
            traffic_weight: json_at(properties, "/trafficWeight").as_u64().unwrap_or(0),
            name,
        })
    }
}

/// The label of a revision named `<app>--<label>`.
pub fn label_of_revision(app_name: &str, revision: &str) -> Option<String> {
    let label = revision.strip_prefix(app_name)?.strip_prefix("--")?;
    version_label(label).filter(|normalised| normalised == label)
}

/// Revisions to retire at `now`: every active versioned revision whose
/// successor (the next one created) is older than `retention`. The newest
/// version is never retired.
pub fn plan_retirement(revisions: &[Revision], now_unix: u64, retention: Duration) -> Vec<String> {
    let mut versioned: Vec<&Revision> =
        revisions.iter().filter(|revision| revision.label.is_some() && revision.created_at.is_some()).collect();
    versioned.sort_by_key(|revision| (revision.created_at, revision.name.clone()));
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

/// Traffic list for a deploy: 100 % to the new revision (by
/// `latestRevision`, since it does not exist yet), and weight-0 labelled
/// entries for retained versions.
pub fn traffic_for_deploy(app_name: &str, retained_labels: &[String]) -> Vec<Value> {
    let mut traffic = vec![serde_json::json!({ "latestRevision": true, "weight": 100 })];
    traffic.extend(retained_labels.iter().map(|label| labelled_entry(app_name, label, 0)));
    traffic
}

/// Traffic list once `label`'s revision exists: labelled at 100 %, retained versions at 0 %.
pub fn traffic_after_deploy(app_name: &str, label: &str, retained_labels: &[String]) -> Vec<Value> {
    let mut traffic = vec![labelled_entry(app_name, label, 100)];
    traffic.extend(
        retained_labels.iter().filter(|kept| kept.as_str() != label).map(|kept| labelled_entry(app_name, kept, 0)),
    );
    traffic
}

fn labelled_entry(app_name: &str, label: &str, weight: u64) -> Value {
    serde_json::json!({ "revisionName": revision_name(app_name, label), "weight": weight, "label": label })
}

/// Labels present in the app's traffic list.
pub fn traffic_labels(app: &Value) -> Vec<String> {
    json_at(app, "/properties/configuration/ingress/traffic")
        .as_array()
        .map(|entries| {
            entries.iter().filter_map(|entry| json_at(entry, "/label").as_str().map(str::to_string)).collect()
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

    fn revision(label: &str, created_at: u64, active: bool) -> Revision {
        Revision {
            name: format!("bot--{label}"),
            label: Some(label.into()),
            active,
            created_at: Some(created_at),
            replicas: 0,
            traffic_weight: 0,
        }
    }

    #[test]
    fn old_version_is_kept_six_days_and_retired_on_day_eight() {
        let retention = Duration::from_secs(7 * DAY);
        let revisions = [revision("v2", 10 * DAY, true), revision("v1", 0, true)];
        assert!(plan_retirement(&revisions, 16 * DAY, retention).is_empty(), "day 6 keeps v1");
        assert_eq!(plan_retirement(&revisions, 18 * DAY, retention), vec!["bot--v1".to_string()], "day 8 retires v1");
        let already_inactive = [revision("v1", 0, false), revision("v2", 10 * DAY, true)];
        assert!(plan_retirement(&already_inactive, 30 * DAY, retention).is_empty());
        assert!(
            plan_retirement(&[revision("v5", 0, true)], 100 * DAY, retention).is_empty(),
            "newest is never retired"
        );
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
        let traffic = traffic_after_deploy("bot", "v3", &["v2".into(), "v3".into()]);
        assert_eq!(traffic.len(), 2);
        assert_eq!(traffic[0]["label"], "v3");
        assert_eq!(traffic[1]["weight"], 0);
        let app = serde_json::json!({"properties": {"configuration": {"ingress": {"traffic": traffic}}}});
        assert_eq!(traffic_labels(&app), vec!["v3".to_string(), "v2".to_string()]);
        assert_eq!(label_of_revision("bot", "bot--v12").as_deref(), Some("v12"));
        assert_eq!(label_of_revision("bot", "bot--Hotfix"), None);
        assert_eq!(label_of_revision("bot", "other--v1"), None);
    }
}
