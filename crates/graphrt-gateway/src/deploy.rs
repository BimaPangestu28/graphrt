//! `graphrt deploy --target local`: a [`DeployDriver`] that registers a new
//! immutable version with a running gateway through its admin API
//! (`POST /admin/v1/deployments/{d}/versions`, bearer admin token). The
//! gateway's local-process provider then starts the version's agents on demand.
//!
//! `DeploymentSpec::provider_settings["workflow"]` names the served workflow
//! (the agent serves a file under its stem).

use std::time::Duration;

use graphrt_provider_api::{
    BoxFuture, DeployDriver, DeployError, DeployErrorKind, DeploymentRef, DeploymentSpec, ProviderErrorKind,
};

/// Driver for the local-process target.
#[derive(Debug, Clone)]
pub struct LocalDeployDriver {
    gateway_url: String,
    admin_token: String,
    client: reqwest::Client,
}

impl LocalDeployDriver {
    /// A driver for the gateway at `gateway_url`.
    pub fn new(gateway_url: &str, admin_token: &str) -> Self {
        let client = reqwest::Client::builder().timeout(Duration::from_secs(30)).build().unwrap_or_default();
        Self { gateway_url: gateway_url.trim_end_matches('/').to_owned(), admin_token: admin_token.to_owned(), client }
    }
}

/// Checks a spec for the local target.
///
/// # Errors
/// [`DeployError::invalid`] naming the problem.
pub fn validate_local_spec(spec: &DeploymentSpec) -> Result<String, DeployError> {
    if spec.command.is_empty() {
        return Err(DeployError::invalid("the local target needs the agent command"));
    }
    if spec.image.is_some() {
        return Err(DeployError::unsupported("the local target runs a command, not an image"));
    }
    if !spec.secrets.iter().all(|secret| secret.reference.starts_with("env:")) {
        return Err(DeployError::unsupported(
            "local secrets are references `env:NAME` to the gateway host environment",
        ));
    }
    let workflow =
        spec.provider_settings.get("workflow").and_then(serde_json::Value::as_str).filter(|name| !name.is_empty());
    workflow
        .map(str::to_owned)
        .ok_or_else(|| DeployError::invalid("provider_settings.workflow (the served workflow name) is required"))
}

/// What the gateway records about a new version.
#[derive(Debug, Clone, Copy)]
pub struct VersionRegistration<'a> {
    /// Deployment name.
    pub deployment: &'a str,
    /// New version.
    pub version: &'a str,
    /// Served workflow name.
    pub workflow: &'a str,
    /// Workflow fingerprint (may be empty).
    pub fingerprint: &'a str,
    /// Provider target the platform driver returned (`DeploymentRef::target`).
    pub target: &'a str,
    /// Agent command (local-process only).
    pub command: &'a [String],
}

impl LocalDeployDriver {
    /// Registers a version with the gateway (`POST /admin/v1/deployments/{d}/versions`).
    ///
    /// # Errors
    /// The gateway is unreachable or refuses.
    pub async fn register(&self, registration: VersionRegistration<'_>) -> Result<(), DeployError> {
        let body = serde_json::json!({
            "version": registration.version, "workflow": registration.workflow, "fingerprint": registration.fingerprint,
            "target": registration.target, "command": registration.command,
        });
        let url = format!("{}/admin/v1/deployments/{}/versions", self.gateway_url, registration.deployment);
        let response = self.client.post(url).bearer_auth(&self.admin_token).json_body(&body).send().await.map_err(|error| DeployError {
            kind: DeployErrorKind::Provider(ProviderErrorKind::Transient),
            message: format!("gateway unreachable: {error}"),
        })?;
        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
            let text = response.text().await.unwrap_or_default();
            return Err(DeployError {
                kind: DeployErrorKind::Provider(graphrt_provider_api::ProviderError::kind_for_status(status)),
                message: format!("gateway refused ({status}): {}", text.chars().take(300).collect::<String>()),
            });
        }
        Ok(())
    }
}

impl DeployDriver for LocalDeployDriver {
    fn apply<'a>(&'a self, spec: &'a DeploymentSpec) -> BoxFuture<'a, Result<DeploymentRef, DeployError>> {
        Box::pin(async move {
            let workflow = validate_local_spec(spec)?;
            self.register(VersionRegistration {
                deployment: &spec.name,
                version: &spec.version,
                workflow: &workflow,
                fingerprint: &spec.workflow_fingerprint,
                target: "local",
                command: &spec.command,
            })
            .await?;
            Ok(DeploymentRef::new(&spec.name, &spec.version, "local"))
        })
    }
}

/// A platform driver (K8s, Cloud Run, ECS, Lambda, Fly) followed by the
/// registration of the new version with the gateway, so the gateway only
/// ever learns the target name the platform returned.
pub struct RegisteringDriver {
    platform: Box<dyn DeployDriver>,
    gateway: LocalDeployDriver,
}

impl std::fmt::Debug for RegisteringDriver {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("RegisteringDriver").finish_non_exhaustive()
    }
}

impl RegisteringDriver {
    /// Wraps `platform`; versions are registered at `gateway`.
    pub fn new(platform: Box<dyn DeployDriver>, gateway: LocalDeployDriver) -> Self {
        Self { platform, gateway }
    }
}

impl DeployDriver for RegisteringDriver {
    fn apply<'a>(&'a self, spec: &'a DeploymentSpec) -> BoxFuture<'a, Result<DeploymentRef, DeployError>> {
        Box::pin(async move {
            let workflow = spec.provider_settings.get("workflow").and_then(serde_json::Value::as_str).filter(|name| !name.is_empty());
            let workflow = workflow.ok_or_else(|| DeployError::invalid("provider_settings.workflow (the served workflow name) is required"))?.to_owned();
            let deployed = self.platform.apply(spec).await?;
            self.gateway
                .register(VersionRegistration {
                    deployment: &spec.name,
                    version: &deployed.version,
                    workflow: &workflow,
                    fingerprint: &spec.workflow_fingerprint,
                    target: &deployed.target,
                    command: &[],
                })
                .await?;
            Ok(deployed)
        })
    }
}

trait JsonBody {
    fn json_body(self, value: &serde_json::Value) -> Self;
}

impl JsonBody for reqwest::RequestBuilder {
    fn json_body(self, value: &serde_json::Value) -> Self {
        self.header("content-type", "application/json").body(value.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> DeploymentSpec {
        serde_json::from_value(serde_json::json!({
            "name": "shop", "version": "v2", "image": null, "command": ["graphrt", "serve", "agent.yaml"],
            "max_active_runs": 8, "min_replicas": 0, "max_replicas": 4, "grace_secs": 30, "workflow_fingerprint": "",
            "provider_settings": { "workflow": "agent" }
        }))
        .unwrap()
    }

    #[test]
    fn validates_local_specs() {
        assert_eq!(validate_local_spec(&spec()).unwrap(), "agent");
        let mut no_command = spec();
        no_command.command.clear();
        assert_eq!(validate_local_spec(&no_command).unwrap_err().kind, DeployErrorKind::Invalid);
        let mut image = spec();
        image.image = Some("ghcr.io/x".into());
        assert_eq!(validate_local_spec(&image).unwrap_err().kind, DeployErrorKind::Unsupported);
    }
}
