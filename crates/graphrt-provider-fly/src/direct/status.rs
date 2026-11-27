//! Status and version history of a direct deployment, derived from its
//! Machines' metadata and states (Fly is the only source of truth: nothing is
//! stored on the deploying machine).

use std::collections::BTreeMap;

use serde::Serialize;

use super::machine_config::{
    META_DEPLOYED_AT, META_DIRECT, META_MODE, META_PAUSED, META_ROLE, META_VERSION, META_WORKFLOW, Role,
};
use crate::model::{CheckStatus, Machine, MachineState};

/// A candidate older than this is a deploy that died half-way, not one in progress.
const STALE_CANDIDATE_MS: u64 = 15 * 60 * 1000;

/// Overall state of a deployment.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeploymentState {
    /// The current version serves (scale-to-zero: possibly stopped until the next request).
    Online,
    /// Paused: no public services, Machine stopped.
    Paused,
    /// A deploy or rollback is starting a version.
    Deploying,
    /// The current version is down (crashed, failing `/readyz`, or always-on but stopped).
    Failed,
    /// Nothing serves: the app exists but has no current version (e.g. its first deploy failed).
    Offline,
}

/// One Machine in a status report.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct MachineSummary {
    pub id: String,
    pub name: String,
    /// Fly Machine state (`started`, `stopped`, `suspended`, `starting`, `failed`...).
    pub state: String,
    pub region: String,
    pub version: Option<String>,
    /// `current`, `candidate` or `standby`.
    pub role: Option<String>,
    pub checks: Vec<CheckStatus>,
}

/// `graphrt deploy status --direct`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DeploymentStatus {
    pub app: String,
    pub url: String,
    pub status: DeploymentState,
    /// Current version label.
    pub current_version: Option<String>,
    /// Workflow name of the current version.
    pub workflow: Option<String>,
    /// `always-on` or `scale-to-zero`.
    pub mode: Option<String>,
    /// When the current version was deployed (RFC 3339, UTC).
    pub last_deployed_at: Option<String>,
    pub machines: Vec<MachineSummary>,
    /// The deployment's store volume, if it has one (billed by Fly while it exists).
    pub store: Option<super::serve_options::StoreSummary>,
}

/// One entry of `graphrt deploy list-versions --direct`, newest first.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct VersionSummary {
    /// Version label.
    pub label: String,
    pub deployed_at: Option<String>,
    pub current: bool,
    /// `current`, `candidate` or `standby`.
    pub role: String,
    pub mode: Option<String>,
    pub workflow: Option<String>,
    /// Fly state of the version's Machine.
    pub state: String,
}

/// The metadata view of one direct-deploy Machine.
pub(super) fn metadata<'machine>(machine: &'machine Machine, key: &str) -> Option<&'machine str> {
    machine.metadata_value(key)
}

/// Whether the Machine belongs to a direct deployment and is not being destroyed.
/// A Machine that is `replacing` is only taking a new config (Fly reports it so for a
/// moment after every update): it is still the deployment's.
pub(super) fn is_direct(machine: &Machine) -> bool {
    metadata(machine, META_DIRECT) == Some("1")
        && !matches!(machine.state, MachineState::Destroying | MachineState::Destroyed)
}

pub(super) fn has_role(machine: &Machine, role: Role) -> bool {
    metadata(machine, META_ROLE) == Some(role.as_str())
}

pub(super) fn version_of(machine: &Machine) -> Option<&str> {
    metadata(machine, META_VERSION)
}

pub(super) fn deployed_at_ms(machine: &Machine) -> u64 {
    metadata(machine, META_DEPLOYED_AT).and_then(|text| text.parse().ok()).unwrap_or(0)
}

/// Builds the status report from the app's direct-deploy Machines.
///
/// @param app - app name (&str)
/// @param url - public URL (String)
/// @param machines - Machines with `graphrt_direct=1` (&[Machine])
/// @param now_ms - wall clock, to tell a running deploy from an abandoned one (u64)
pub(super) fn deployment_status(app: &str, url: String, machines: &[Machine], now_ms: u64) -> DeploymentStatus {
    let current: Vec<&Machine> = machines.iter().filter(|machine| has_role(machine, Role::Current)).collect();
    let newest_current = current.iter().max_by_key(|machine| deployed_at_ms(machine)).copied();
    DeploymentStatus {
        app: app.to_owned(),
        url,
        status: derive_state(machines, &current, now_ms),
        current_version: newest_current.and_then(version_of).map(str::to_owned),
        workflow: newest_current.and_then(|machine| metadata(machine, META_WORKFLOW)).map(str::to_owned),
        mode: newest_current.and_then(|machine| metadata(machine, META_MODE)).map(str::to_owned),
        last_deployed_at: newest_current.map(|machine| format_rfc3339_ms(deployed_at_ms(machine))),
        machines: machines.iter().map(summarize).collect(),
        store: None,
    }
}

fn derive_state(machines: &[Machine], current: &[&Machine], now_ms: u64) -> DeploymentState {
    let deploying = machines.iter().any(|machine| {
        has_role(machine, Role::Candidate) && now_ms.saturating_sub(deployed_at_ms(machine)) < STALE_CANDIDATE_MS
    });
    if deploying {
        return DeploymentState::Deploying;
    }
    if current.is_empty() {
        return DeploymentState::Offline;
    }
    if current.iter().all(|machine| metadata(machine, META_PAUSED) == Some("1")) {
        return DeploymentState::Paused;
    }
    let broken = current.iter().any(|machine| {
        machine.state == MachineState::Failed
            || (machine.state == MachineState::Started && machine.checks.iter().any(|check| check.status == "critical"))
    });
    let always_on = current.iter().any(|machine| metadata(machine, META_MODE) == Some("always-on"));
    let none_running =
        !current.iter().any(|machine| matches!(machine.state, MachineState::Started | MachineState::Starting));
    if broken || (always_on && none_running) { DeploymentState::Failed } else { DeploymentState::Online }
}

fn summarize(machine: &Machine) -> MachineSummary {
    MachineSummary {
        id: machine.id.clone(),
        name: machine.name.clone(),
        state: machine.state.as_api_str().to_owned(),
        region: machine.region.clone(),
        version: version_of(machine).map(str::to_owned),
        role: metadata(machine, META_ROLE).map(str::to_owned),
        checks: machine.checks.clone(),
    }
}

/// The versions of a deployment, newest deploy first (one entry per version label).
pub(super) fn version_history(machines: &[Machine]) -> Vec<VersionSummary> {
    let mut newest_per_version: BTreeMap<&str, &Machine> = BTreeMap::new();
    for machine in machines {
        let Some(version) = version_of(machine) else { continue };
        let keep_existing =
            newest_per_version.get(version).is_some_and(|existing| deployed_at_ms(existing) >= deployed_at_ms(machine));
        if !keep_existing {
            newest_per_version.insert(version, machine);
        }
    }
    let mut versions: Vec<(&str, &Machine)> = newest_per_version.into_iter().collect();
    versions.sort_by_key(|(_, machine)| std::cmp::Reverse(deployed_at_ms(machine)));
    versions
        .into_iter()
        .map(|(version, machine)| VersionSummary {
            label: version.to_owned(),
            deployed_at: Some(deployed_at_ms(machine)).filter(|ms| *ms > 0).map(format_rfc3339_ms),
            current: has_role(machine, Role::Current),
            role: metadata(machine, META_ROLE).unwrap_or("unknown").to_owned(),
            mode: metadata(machine, META_MODE).map(str::to_owned),
            workflow: metadata(machine, META_WORKFLOW).map(str::to_owned),
            state: machine.state.as_api_str().to_owned(),
        })
        .collect()
}

/// `YYYY-MM-DDTHH:MM:SS.mmmZ` of a Unix timestamp in milliseconds.
///
/// @example
/// assert_eq!(graphrt_provider_fly::direct::format_rfc3339_ms(0), "1970-01-01T00:00:00.000Z");
pub fn format_rfc3339_ms(unix_ms: u64) -> String {
    let seconds = unix_ms / 1000;
    let days = i64::try_from(seconds / 86_400).unwrap_or(0);
    let second_of_day = seconds % 86_400;
    // Howard Hinnant's `civil_from_days`.
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era = (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 { month_index + 3 } else { month_index - 9 };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        second_of_day / 3600,
        second_of_day / 60 % 60,
        second_of_day % 60,
        unix_ms % 1000
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn machine(role: &str, version: &str, state: &str, deployed_at: u64, extra: &str) -> Machine {
        serde_json::from_str(&format!(
            r#"{{"id":"m{version}{role}","name":"n","state":"{state}","region":"ams",{extra}
                "config":{{"metadata":{{"graphrt_direct":"1","graphrt_role":"{role}","graphrt_version":"{version}",
                "graphrt_mode":"scale-to-zero","graphrt_deployed_at":"{deployed_at}"}}}}}}"#
        ))
        .unwrap()
    }

    #[test]
    fn formats_rfc3339() {
        assert_eq!(format_rfc3339_ms(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(format_rfc3339_ms(1_764_000_000_123), "2025-11-24T16:00:00.123Z");
    }

    #[test]
    fn scale_to_zero_stopped_is_online_and_history_is_newest_first() {
        let machines = [machine("standby", "v1", "stopped", 1_000, ""), machine("current", "v2", "stopped", 2_000, "")];
        let status = deployment_status("app", "https://app.fly.dev".into(), &machines, 3_000);
        assert_eq!(status.status, DeploymentState::Online);
        assert_eq!(status.current_version.as_deref(), Some("v2"));
        let history = version_history(&machines);
        assert_eq!(history.iter().map(|entry| entry.label.as_str()).collect::<Vec<_>>(), ["v2", "v1"]);
        assert!(history[0].current && !history[1].current);
    }

    #[test]
    fn derives_deploying_failed_and_offline() {
        let fresh_candidate = [machine("candidate", "v3", "starting", 10_000, "")];
        assert_eq!(derive_state(&fresh_candidate, &[], 20_000), DeploymentState::Deploying);
        assert_eq!(derive_state(&fresh_candidate, &[], 10_000 + STALE_CANDIDATE_MS), DeploymentState::Offline);
        let crashed = machine("current", "v2", "failed", 1, "");
        assert_eq!(derive_state(&[], &[&crashed], 2), DeploymentState::Failed);
        let unhealthy = machine("current", "v2", "started", 1, r#""checks":[{"name":"readyz","status":"critical"}],"#);
        assert_eq!(derive_state(&[], &[&unhealthy], 2), DeploymentState::Failed);
    }

    #[test]
    fn a_machine_taking_a_new_config_still_belongs_to_the_deployment() {
        assert!(is_direct(&machine("current", "v1", "replacing", 1, "")));
        assert!(is_direct(&machine("standby", "v1", "stopped", 1, "")));
        assert!(!is_direct(&machine("current", "v1", "destroying", 1, "")));
        assert!(!is_direct(&machine("current", "v1", "destroyed", 1, "")));
    }
}
