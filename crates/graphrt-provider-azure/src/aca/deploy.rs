//! `graphrt deploy --target aca`: create or update a container app version.

use super::revisions::{labelled_versions, traffic_after_deploy, traffic_for_deploy};
use super::spec::AcaAppSpec;
use super::{AcaConfig, checked_app_name};
use crate::api::{DeployDriver, DeployError, DeploymentRef, ProviderError};
use crate::arm::ArmClient;
use crate::keyvault::KeyVaultConfig;

/// Deploy driver over ARM, with deploy-time credentials
/// (`Container Apps Contributor` on the resource group).
#[derive(Debug, Clone)]
pub struct AcaDeployDriver {
    arm: ArmClient,
    config: AcaConfig,
    key_vault: KeyVaultConfig,
}

impl AcaDeployDriver {
    /// @throws `ProviderError::Fatal` for an invalid configuration.
    pub fn new(arm: ArmClient, config: AcaConfig, key_vault: KeyVaultConfig) -> Result<Self, ProviderError> {
        config.validate()?;
        Ok(Self { arm, config, key_vault })
    }

    /// Retire versions past the retention window (run after each deploy, with
    /// the deploy-time role, so the gateway can stay read-only).
    ///
    /// @throws `ProviderError` from ARM.
    pub async fn retire_old_versions(&self, app: &str, now_unix: u64) -> Result<Vec<String>, ProviderError> {
        super::retire::retire_old_versions(&self.arm, &self.config, app, now_unix).await
    }

    /// Labelled versions of an existing app, or none when it does not exist yet.
    async fn existing_versions(&self, app: &str) -> Result<Vec<u64>, ProviderError> {
        match self.arm.get(&self.config.app_path(app, "")).await {
            Ok(response) => Ok(labelled_versions(&response.body)),
            Err(ProviderError::NotFound(_)) => Ok(Vec::new()),
            Err(error) => Err(error),
        }
    }
}

impl DeployDriver for AcaDeployDriver {
    type Spec = AcaAppSpec;

    /// Versions are immutable: applying an already labelled version is a no-op.
    /// Otherwise: `PUT` the app with the new template (revision `<app>--v<N>`,
    /// 100 % via `latestRevision`, retained versions at weight 0), wait for the
    /// operation, then `PATCH` the traffic so the new revision carries label `v<N>`.
    async fn apply(&self, spec: &AcaAppSpec) -> Result<DeploymentRef, DeployError> {
        spec.validate(&self.key_vault)?;
        let app = checked_app_name(&spec.app_name)?;
        let deployed =
            DeploymentRef { deployment: spec.deployment.clone(), version: spec.version, resource: app.to_string() };
        let mut retained = self.existing_versions(app).await?;
        if retained.contains(&spec.version) {
            return Ok(deployed);
        }
        if retained.iter().any(|version| *version > spec.version) {
            return Err(DeployError::InvalidSpec(format!(
                "version {} is older than a deployed version of {app}; versions must increase",
                spec.version
            )));
        }
        retained.sort_unstable();
        let path = self.config.app_path(app, "");
        let started = self.arm.put(&path, &spec.to_arm(traffic_for_deploy(app, &retained))).await?;
        self.arm.wait(&started).await?;
        let label = serde_json::json!({
            "properties": { "configuration": { "ingress": { "traffic": traffic_after_deploy(app, spec.version, &retained) } } }
        });
        let labelled = self.arm.patch(&path, &label).await?;
        self.arm.wait(&labelled).await?;
        Ok(deployed)
    }
}
