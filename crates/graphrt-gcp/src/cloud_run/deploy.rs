//! `graphrt deploy --target cloud-run`: creates or updates the Cloud Run service
//! of a deployment version with deploy-time credentials (`roles/run.developer`
//! + `roles/iam.serviceAccountUser` on the runtime service account).

use std::sync::Arc;

use graphrt_provider_api::{BoxFuture, DeployDriver, DeployError, DeploymentRef, DeploymentSpec, ProviderError};
use serde_json::Value;

use super::admin::AdminApi;
use super::config::CloudRunConfig;
use super::naming::{revision_name, service_id, version_tag};
use super::service_spec::{CloudRunSettings, ServiceNames, render_service, render_traffic};
use super::status::CloudRunServiceStatus;
use crate::auth::GcpAuth;

/// Deploy driver over the Cloud Run Admin API v2.
#[derive(Clone)]
pub struct CloudRunDeployDriver {
    config: Arc<CloudRunConfig>,
    admin: AdminApi,
}

impl std::fmt::Debug for CloudRunDeployDriver {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("CloudRunDeployDriver").field("parent", &self.config.parent()).finish_non_exhaustive()
    }
}

impl CloudRunDeployDriver {
    /// A driver for `config.project` / `config.region`.
    ///
    /// @throws `ProviderError` (fatal) on an invalid configuration.
    pub fn new(config: CloudRunConfig, auth: &GcpAuth) -> Result<Self, ProviderError> {
        let admin = AdminApi::new(&config, auth.access_tokens())?;
        Ok(Self { config: Arc::new(config), admin })
    }

    /// Names of `spec`'s version on Cloud Run.
    pub fn names(&self, spec_name: &str, version: &str) -> ServiceNames {
        let service = service_id(&self.config.service_prefix, spec_name);
        let tag = version_tag(version);
        ServiceNames {
            resource: format!("{}/services/{service}", self.config.parent()),
            revision: revision_name(&service, &tag),
            audience: format!("graphrt/{}/{}/{service}", self.config.project, self.config.region),
            service_id: service,
            tag,
        }
    }

    /// The `Service` body `apply` would send, for `graphrt deploy --dry-run`.
    ///
    /// @throws `DeployError` when the spec is invalid for Cloud Run.
    pub async fn render(&self, spec: &DeploymentSpec) -> Result<Value, DeployError> {
        let names = self.names(&spec.name, &spec.version);
        let settings = CloudRunSettings::parse(&spec.provider_settings)?;
        let existing = self.admin.get_service(&names.resource).await?;
        render_service(&self.config, spec, &names, &settings, existing.as_ref())
    }

    /// Removes the traffic tags of `versions` (decision 6: old versions stay
    /// wakeable for 7 days, then the gateway retires them and resumes get
    /// `410 version_retired`). The revisions stay until Cloud Run garbage-collects them.
    ///
    /// @param deployment - Any version of the deployment (names the service)
    /// @throws `DeployError` when the service does not exist or the update fails.
    pub async fn retire_versions(&self, deployment: &DeploymentRef, versions: &[String]) -> Result<(), DeployError> {
        let names = self.names(&deployment.name, &deployment.version);
        let existing = self
            .admin
            .get_service(&names.resource)
            .await?
            .ok_or_else(|| DeployError::invalid(format!("{} does not exist", names.resource)))?;
        let status = CloudRunServiceStatus::from_service(&existing);
        let latest_tag = status.latest_tag.clone().unwrap_or_else(|| names.tag.clone());
        let retired: Vec<String> = versions.iter().map(|version| version_tag(version)).collect();
        if retired.contains(&latest_tag) {
            return Err(DeployError::invalid("cannot retire the version that serves new traffic"));
        }
        let body = serde_json::json!({ "traffic": render_traffic(&latest_tag, Some(&existing), &retired) });
        let resource = format!("{}?updateMask=traffic", names.resource);
        let operation = self.admin.patch_service(&resource, &body, false).await?;
        self.admin.wait_operation(operation, self.config.deploy_timeout, self.config.operation_poll_interval).await?;
        Ok(())
    }

    async fn apply_spec(&self, spec: &DeploymentSpec) -> Result<DeploymentRef, DeployError> {
        let names = self.names(&spec.name, &spec.version);
        let settings = CloudRunSettings::parse(&spec.provider_settings)?;
        let existing = self.admin.get_service(&names.resource).await?;
        let reference = DeploymentRef::new(&spec.name, &spec.version, &names.resource);
        if let Some(service) = &existing {
            // Versions are immutable: re-applying a version that is already the
            // ready latest revision is a no-op (idempotent `graphrt deploy`).
            let status = CloudRunServiceStatus::from_service(service);
            let current_revision = service.pointer("/template/revision").and_then(Value::as_str);
            if status.ready
                && current_revision == Some(names.revision.as_str())
                && status.latest_ready_revision.as_deref() == Some(names.revision.as_str())
            {
                return Ok(reference);
            }
        }
        let body = render_service(&self.config, spec, &names, &settings, existing.as_ref())?;
        let operation = self.admin.patch_service(&names.resource, &body, false).await?;
        let service = self
            .admin
            .wait_operation(operation, self.config.deploy_timeout, self.config.operation_poll_interval)
            .await?;
        let service = if service.is_null() {
            self.admin.get_service(&names.resource).await?.unwrap_or(Value::Null)
        } else {
            service
        };
        let status = CloudRunServiceStatus::from_service(&service);
        if status.latest_ready_revision.as_deref() != Some(names.revision.as_str()) {
            return Err(DeployError::invalid(format!(
                "revision {} did not become ready (latest ready: {})",
                names.revision,
                status.latest_ready_revision.as_deref().unwrap_or("none")
            )));
        }
        Ok(reference)
    }
}

impl DeployDriver for CloudRunDeployDriver {
    fn apply<'a>(&'a self, spec: &'a DeploymentSpec) -> BoxFuture<'a, Result<DeploymentRef, DeployError>> {
        Box::pin(self.apply_spec(spec))
    }
}
