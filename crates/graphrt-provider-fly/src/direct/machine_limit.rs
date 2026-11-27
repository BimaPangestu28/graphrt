//! Creating a version's Machine within the organization's Machine limit.
//!
//! Fly caps how many Machines an organization may have (three on Fly's Free
//! plan) and refuses a new one with `422 "Your organization has reached its
//! machine limit"`. Every version of a direct deployment keeps a stopped
//! standby Machine for rollback, so without this a deployment on a new
//! account stops deploying at its third version. When Fly refuses for that
//! reason, the oldest standby version of this deployment is destroyed and the
//! Machine is created again, until it fits or no standby version is left.
//! The serving version and the Machine with the store volume are never
//! touched.

use graphrt_provider_api::{ProviderError, ProviderErrorKind};

use super::lifecycle::mounts_store;
use super::machine_config::Role;
use super::status::{deployed_at_ms, has_role, version_of};
use super::{DirectError, ErrorCode, FlyDirect};
use crate::error::is_name_taken;
use crate::model::{CreateMachineRequest, Machine};

/// Whether Fly refused a Machine because the organization has as many as it may have.
pub(super) fn is_machine_limit(error: &ProviderError) -> bool {
    error.message.to_ascii_lowercase().contains("machine limit")
}

/// A Machine created for a version, and the standby versions destroyed to make room for it.
pub(super) struct CreatedMachine {
    pub(super) machine: Machine,
    pub(super) freed_versions: Vec<String>,
}

impl CreatedMachine {
    /// `"; removed standby version(s) v1 to stay within Fly's Machine limit"`, or nothing.
    pub(super) fn note(&self) -> String {
        if self.freed_versions.is_empty() {
            return String::new();
        }
        format!(
            "; removed standby version(s) {} to stay within the Fly organization's Machine limit",
            self.freed_versions.join(", ")
        )
    }
}

/// Standby Machines that may go to make room, oldest version first.
fn removable_standby(machines: &[Machine]) -> Vec<&Machine> {
    let mut standby: Vec<&Machine> =
        machines.iter().filter(|machine| has_role(machine, Role::Standby) && !mounts_store(machine)).collect();
    standby.sort_by_key(|machine| deployed_at_ms(machine));
    standby
}

impl FlyDirect {
    /// Creates a version's Machine. When the organization is at its Machine
    /// limit, destroys this deployment's oldest standby version and tries
    /// again, one standby at a time.
    ///
    /// @param create - the Machine to create (&CreateMachineRequest)
    /// @returns The Machine and the labels of the standby versions destroyed for it
    /// @throws DirectError - `invalid_input` when a Machine of that name exists but
    ///   is not graphrt's; `fly_api` when the limit is reached with no standby
    ///   version left to remove; any other refusal as Fly reported it
    pub(super) async fn create_version_machine(
        &self,
        create: &CreateMachineRequest<'_>,
    ) -> Result<CreatedMachine, DirectError> {
        let mut freed_versions = Vec::new();
        loop {
            let error = match self.client.create_machine(create).await {
                Ok(machine) => return Ok(CreatedMachine { machine, freed_versions }),
                Err(error) => error,
            };
            if is_name_taken(&error) {
                return Err(DirectError::invalid(format!(
                    "a Machine named {} exists in {} but is not managed by graphrt",
                    create.name,
                    self.app_name()
                )));
            }
            if !is_machine_limit(&error) {
                return Err(error.into());
            }
            let machines = self.direct_machines().await?;
            let Some(oldest) = removable_standby(&machines).first().copied() else {
                return Err(self.limit_reached(&error, &freed_versions));
            };
            match self.client.destroy_machine(&oldest.id).await {
                Ok(()) => {}
                Err(destroy_error) if destroy_error.kind == ProviderErrorKind::NotFound => {}
                Err(destroy_error) => return Err(destroy_error.into()),
            }
            freed_versions.push(version_of(oldest).unwrap_or(oldest.name.as_str()).to_owned());
        }
    }

    /// The error for a Machine limit that removing standby versions could not get under.
    fn limit_reached(&self, error: &ProviderError, freed_versions: &[String]) -> DirectError {
        let freed = if freed_versions.is_empty() {
            "no standby version of this deployment was left to remove".to_owned()
        } else {
            format!("removing standby version(s) {} was not enough", freed_versions.join(", "))
        };
        DirectError::new(
            ErrorCode::FlyApi,
            format!(
                "the Fly organization has as many Machines as its plan allows (Fly's Free plan allows 3) and {freed}: \
                 upgrade the organization to Pay As You Go on its Fly billing page, or remove apps it no longer needs \
                 (Fly said: {})",
                error.message.trim()
            ),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_machine_limit_refusal_is_recognised_whatever_its_case() {
        let refusal = ProviderError::fatal(
            "Fly POST /v1/apps/a/machines: HTTP 422: {\"error\":\"Your organization has reached its machine limit. \
             Please contact billing@fly.io\"}",
        );
        assert!(is_machine_limit(&refusal));
        assert!(is_machine_limit(&ProviderError::fatal("Machine Limit reached")));
        assert!(!is_machine_limit(&ProviderError::fatal("HTTP 422: a machine with this name already exists")));
    }
}
