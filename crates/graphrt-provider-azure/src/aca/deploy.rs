//! `graphrt deploy --target azure-container-apps`: create or update a container app version.

use graphrt_provider_api::{
    BoxFuture, DeployDriver, DeployError, DeploymentRef, DeploymentSpec, ProviderError, ProviderErrorKind,
};

use super::revisions::{traffic_after_deploy, traffic_for_deploy, traffic_labels};
use super::spec::AcaAppSpec;
use super::{AcaConfig, checked_app_name};
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
    /// @throws `ProviderError` (fatal) for an invalid configuration.
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

    /// Apply one container app version. Versions are immutable: applying an
    /// already labelled version is a no-op. Otherwise `PUT` the app with the new
    /// template (revision `<app>--<label>`, 100 % via `latestRevision`, retained
    /// versions at weight 0), wait for the operation, then `PATCH` the traffic
    /// so the new revision carries its label.
    ///
    /// @throws `DeployError` invalid for a bad spec, or the ARM failure.
    pub async fn apply_app(&self, app_spec: &AcaAppSpec) -> Result<DeploymentRef, DeployError> {
        app_spec.validate(&self.key_vault)?;
        let app = checked_app_name(&app_spec.app_name)?;
        let label = app_spec.label()?;
        let deployed = DeploymentRef::new(app_spec.deployment.clone(), app_spec.version.clone(), app);
        let retained = self.existing_labels(app).await?;
        if retained.contains(&label) {
            return Ok(deployed);
        }
        let path = self.config.app_path(app, "");
        let started = self.arm.put(&path, &app_spec.to_arm(traffic_for_deploy(app, &retained))?).await?;
        self.arm.wait(&started).await?;
        let traffic = traffic_after_deploy(app, &label, &retained);
        let patch = serde_json::json!({ "properties": { "configuration": { "ingress": { "traffic": traffic } } } });
        let labelled = self.arm.patch(&path, &patch).await?;
        self.arm.wait(&labelled).await?;
        Ok(deployed)
    }

    /// Labels of an existing app, or none when it does not exist yet.
    async fn existing_labels(&self, app: &str) -> Result<Vec<String>, ProviderError> {
        match self.arm.get(&self.config.app_path(app, "")).await {
            Ok(response) => Ok(traffic_labels(&response.body)),
            Err(error) if error.kind == ProviderErrorKind::NotFound => Ok(Vec::new()),
            Err(error) => Err(error),
        }
    }
}

impl DeployDriver for AcaDeployDriver {
    fn apply<'a>(&'a self, spec: &'a DeploymentSpec) -> BoxFuture<'a, Result<DeploymentRef, DeployError>> {
        Box::pin(async move { self.apply_app(&AcaAppSpec::from_deployment_spec(spec)?).await })
    }
}
