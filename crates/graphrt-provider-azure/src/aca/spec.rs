//! The container app of one deployment version: built from the gateway's
//! generic `DeploymentSpec`, validated, and rendered as the ARM
//! `Microsoft.App/containerApps` body.

use graphrt_provider_api::{DeployError, DeploymentSpec};
use serde_json::{Value, json};

pub use super::names::{DEFAULT_GRACE_SECS, MAX_GRACE_SECS, revision_name, version_label};
use super::names::{MAX_LABEL_LEN, is_app_name};
use crate::keyvault::{KeyVaultConfig, KeyVaultSecretRef};

/// One environment variable of the agent container.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EnvVar {
    /// A non-secret value, stored in the app definition in clear text.
    Value { name: String, value: String },
    /// A reference to an app secret (a Key Vault reference).
    Secret { name: String, secret_name: String },
}

/// The identity the app uses for Key Vault and the registry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AppIdentity {
    System,
    /// Resource id of a user-assigned identity.
    UserAssigned(String),
}

impl AppIdentity {
    /// `"system"` or the resource id, as Key Vault and registry entries name it.
    pub fn reference(&self) -> String {
        match self {
            Self::System => "system".into(),
            Self::UserAssigned(id) => id.clone(),
        }
    }
}

/// A private registry pulled with the app's identity (`AcrPull`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegistryRef {
    pub server: String,
    /// `"system"` or a user-assigned identity resource id.
    pub identity: String,
}

/// One immutable container app version, ready to render.
#[derive(Clone, Debug, PartialEq)]
pub struct AcaAppSpec {
    pub deployment: String,
    /// Version as the gateway names it (`v7`, `7`, a short hash).
    pub version: String,
    /// Container app name: 2-32 of `[a-z0-9-]`, starts with a letter, no `--`.
    pub app_name: String,
    pub location: String,
    /// Resource id of the managed environment.
    pub environment_id: String,
    pub image: String,
    /// Entrypoint override and its arguments (`graphrt serve --trust-gateway …`).
    pub command: Vec<String>,
    pub target_port: u16,
    /// vCPU, e.g. 0.5.
    pub cpu: f64,
    /// e.g. `"1Gi"` (consumption wants 2 GiB per vCPU).
    pub memory: String,
    pub env: Vec<EnvVar>,
    pub secrets: Vec<KeyVaultSecretRef>,
    pub identity: AppIdentity,
    pub registry: Option<RegistryRef>,
    /// Agent `max_active_runs`: the HTTP scale rule's `concurrentRequests`.
    pub max_active_runs: u32,
    pub min_replicas: u32,
    pub max_replicas: u32,
    pub grace_secs: u64,
    /// Readiness probe path of `graphrt serve`.
    pub health_path: String,
    pub workflow_fingerprint: String,
}

/// Env var names that must come from a secret reference, never a clear value.
const SECRET_NAME_MARKERS: [&str; 6] = ["SECRET", "TOKEN", "PASSWORD", "API_KEY", "APIKEY", "PRIVATE_KEY"];

impl AcaAppSpec {
    /// Build the app from the gateway's generic spec. Azure settings come from
    /// `provider_settings`: `location`, `environment_id` (required),
    /// `app_name` (default: the deployment name), `identity` (`"system"` or a
    /// user-assigned identity resource id; default `"system"`),
    /// `registry_server`, `target_port` (8080), `health_path` (`/healthz`).
    /// Each `SecretRef.reference` is a Key Vault secret URL.
    ///
    /// @throws `DeployError::invalid` for a missing or malformed setting.
    pub fn from_deployment_spec(spec: &DeploymentSpec) -> Result<Self, DeployError> {
        let setting = |key: &str| spec.provider_settings.get(key).and_then(Value::as_str).map(str::to_string);
        let required = |key: &str| {
            setting(key).ok_or_else(|| {
                DeployError::invalid(format!("provider_settings.{key} is required for azure-container-apps"))
            })
        };
        let identity = match setting("identity").as_deref() {
            None | Some("system") => AppIdentity::System,
            Some(id) => AppIdentity::UserAssigned(id.to_string()),
        };
        let target_port = match spec.provider_settings.get("target_port") {
            None => 8080,
            Some(port) => port
                .as_u64()
                .and_then(|port| u16::try_from(port).ok())
                .ok_or_else(|| DeployError::invalid("provider_settings.target_port must be a port number"))?,
        };
        let mut env: Vec<EnvVar> =
            spec.env.iter().map(|(name, value)| EnvVar::Value { name: name.clone(), value: value.clone() }).collect();
        let mut secrets = Vec::new();
        for secret in &spec.secrets {
            let secret_name = secret.env.to_ascii_lowercase().replace('_', "-");
            secrets.push(KeyVaultSecretRef {
                name: secret_name.clone(),
                secret_url: secret.reference.clone(),
                identity: identity.reference(),
            });
            env.push(EnvVar::Secret { name: secret.env.clone(), secret_name });
        }
        let cpu = if spec.resources.cpu_millis == 0 { 0.5 } else { f64::from(spec.resources.cpu_millis) / 1000.0 };
        let memory_mib = if spec.resources.memory_mib == 0 { 1024 } else { spec.resources.memory_mib };
        Ok(Self {
            deployment: spec.name.clone(),
            version: spec.version.clone(),
            app_name: setting("app_name").unwrap_or_else(|| spec.name.clone()),
            location: required("location")?,
            environment_id: required("environment_id")?,
            image: spec.image.clone().ok_or_else(|| DeployError::invalid("azure-container-apps needs an image"))?,
            command: spec.command.clone(),
            target_port,
            cpu,
            memory: format_memory(memory_mib),
            env,
            secrets,
            registry: setting("registry_server").map(|server| RegistryRef { server, identity: identity.reference() }),
            identity,
            max_active_runs: spec.max_active_runs,
            min_replicas: spec.min_replicas,
            max_replicas: spec.max_replicas,
            grace_secs: if spec.grace_secs == 0 { DEFAULT_GRACE_SECS } else { spec.grace_secs },
            health_path: setting("health_path").unwrap_or_else(|| "/healthz".into()),
            workflow_fingerprint: spec.workflow_fingerprint.clone(),
        })
    }

    /// Revision suffix and traffic label of this version.
    ///
    /// @throws `DeployError::invalid` when the version cannot be a label.
    pub fn label(&self) -> Result<String, DeployError> {
        version_label(&self.version).ok_or_else(|| {
            DeployError::invalid(format!(
                "version {:?} must be 1-{MAX_LABEL_LEN} lowercase letters, digits or '-' to be a revision label",
                self.version
            ))
        })
    }

    /// @throws `DeployError::invalid` naming the first problem found.
    pub fn validate(&self, key_vault: &KeyVaultConfig) -> Result<(), DeployError> {
        let invalid = |message: String| Err(DeployError::invalid(message));
        if !is_app_name(&self.app_name) {
            return invalid(format!(
                "app name {:?} must be 2-32 lowercase letters, digits or '-', start with a letter, and not contain '--'",
                self.app_name
            ));
        }
        self.label()?;
        if self.image.is_empty() || self.location.is_empty() || self.environment_id.is_empty() {
            return invalid("image, location and environment_id are required".into());
        }
        if self.grace_secs > MAX_GRACE_SECS {
            return invalid(format!(
                "grace_secs {} exceeds the Container Apps maximum {MAX_GRACE_SECS}",
                self.grace_secs
            ));
        }
        if self.max_active_runs == 0
            || !(1..=1000).contains(&self.max_replicas)
            || self.min_replicas > self.max_replicas
        {
            return invalid(
                "max_active_runs must be >= 1, max_replicas 1..=1000 and min_replicas <= max_replicas".into(),
            );
        }
        for secret in &self.secrets {
            secret.validate(key_vault).map_err(|error| DeployError::invalid(error.message))?;
        }
        for variable in &self.env {
            match variable {
                EnvVar::Value { name, .. } if looks_secret(name) => {
                    return invalid(format!(
                        "env {name} looks like a secret: reference a Key Vault secret instead of a clear value"
                    ));
                }
                EnvVar::Secret { name, secret_name } if !self.secrets.iter().any(|s| &s.name == secret_name) => {
                    return invalid(format!("env {name} references unknown secret {secret_name:?}"));
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// The ARM body of `containerApps createOrUpdate`. `traffic` is the
    /// ingress traffic list (current version plus retained old versions).
    ///
    /// @throws `DeployError::invalid` when the version is not a valid label.
    pub fn to_arm(&self, traffic: Vec<Value>) -> Result<Value, DeployError> {
        let env: Vec<Value> = self
            .env
            .iter()
            .map(|variable| match variable {
                EnvVar::Value { name, value } => json!({ "name": name, "value": value }),
                EnvVar::Secret { name, secret_name } => json!({ "name": name, "secretRef": secret_name }),
            })
            .collect();
        let identity = match &self.identity {
            AppIdentity::System => json!({ "type": "SystemAssigned" }),
            AppIdentity::UserAssigned(id) => {
                json!({ "type": "UserAssigned", "userAssignedIdentities": { id.clone(): {} } })
            }
        };
        let registries: Vec<Value> = self
            .registry
            .iter()
            .map(|registry| json!({ "server": registry.server, "identity": registry.identity }))
            .collect();
        let mut container = json!({
            "name": "agent",
            "image": self.image,
            "env": env,
            "resources": { "cpu": self.cpu, "memory": self.memory },
            "probes": [{
                "type": "Readiness",
                "httpGet": { "path": self.health_path, "port": self.target_port },
                "periodSeconds": 5,
            }],
        });
        if let Some((entrypoint, args)) = self.command.split_first()
            && let Some(fields) = container.as_object_mut()
        {
            fields.insert("command".into(), json!([entrypoint]));
            fields.insert("args".into(), json!(args));
        }
        Ok(json!({
            "location": self.location,
            "identity": identity,
            "tags": {
                "graphrt-deployment": self.deployment,
                "graphrt-version": self.version,
                "graphrt-workflow": self.workflow_fingerprint,
            },
            "properties": {
                "environmentId": self.environment_id,
                "configuration": {
                    // Old versions stay addressable through revision labels (plan §1 decision 6).
                    "activeRevisionsMode": "Multiple",
                    "ingress": {
                        // Reached by the gateway only: internal ingress, HTTP/1.1 (the agent serves http1).
                        "external": false,
                        "targetPort": self.target_port,
                        "transport": "http",
                        "allowInsecure": false,
                        "traffic": traffic,
                    },
                    "secrets": self.secrets.iter().map(KeyVaultSecretRef::to_arm).collect::<Vec<_>>(),
                    "registries": registries,
                },
                "template": {
                    "revisionSuffix": self.label()?,
                    "terminationGracePeriodSeconds": self.grace_secs,
                    "containers": [container],
                    "scale": {
                        // Scale to zero by default; the first request wakes a replica.
                        "minReplicas": self.min_replicas,
                        "maxReplicas": self.max_replicas,
                        "rules": [{
                            "name": "http-concurrency",
                            "http": { "metadata": { "concurrentRequests": self.max_active_runs.to_string() } },
                        }],
                    },
                },
            },
        }))
    }
}

/// `1536` MiB → `"1.5Gi"`, `1024` → `"1Gi"`.
fn format_memory(mebibytes: u32) -> String {
    let gibibytes = f64::from(mebibytes) / 1024.0;
    let text = format!("{gibibytes:.2}");
    format!("{}Gi", text.trim_end_matches('0').trim_end_matches('.'))
}

fn looks_secret(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    SECRET_NAME_MARKERS.iter().any(|marker| upper.contains(marker))
}

#[cfg(test)]
pub(crate) mod tests {
    use std::collections::BTreeMap;

    use graphrt_provider_api::{Resources, SecretRef};

    use super::*;

    pub(crate) fn generic_spec(version: &str) -> DeploymentSpec {
        let mut provider_settings = BTreeMap::new();
        provider_settings.insert("location".into(), json!("westeurope"));
        provider_settings.insert(
            "environment_id".into(),
            json!("/subscriptions/s/resourceGroups/rg/providers/Microsoft.App/managedEnvironments/env"),
        );
        DeploymentSpec {
            name: "support-bot".into(),
            version: version.into(),
            image: Some("ghcr.io/example/agent:3".into()),
            command: vec!["graphrt".into(), "serve".into(), "agent.yaml".into(), "--trust-gateway".into()],
            env: BTreeMap::from([("RUST_LOG".to_string(), "info".to_string())]),
            secrets: vec![SecretRef {
                env: "OPENAI_API_KEY".into(),
                reference: "https://kv1.vault.azure.net/secrets/openai-key".into(),
            }],
            max_active_runs: 8,
            min_replicas: 0,
            max_replicas: 10,
            grace_secs: 0,
            resources: Resources { cpu_millis: 500, memory_mib: 1024 },
            workflow_fingerprint: "sha256:abc".into(),
            provider_settings,
        }
    }

    pub(crate) fn sample_spec() -> AcaAppSpec {
        AcaAppSpec::from_deployment_spec(&generic_spec("3")).unwrap()
    }

    #[test]
    fn spec_renders_scale_to_zero_http_rule_grace_and_key_vault_refs() {
        let body = sample_spec().to_arm(vec![json!({"latestRevision": true, "weight": 100})]).unwrap();
        let template = &body["properties"]["template"];
        assert_eq!(template["scale"]["minReplicas"], 0);
        assert_eq!(template["scale"]["rules"][0]["http"]["metadata"]["concurrentRequests"], "8");
        assert_eq!(template["terminationGracePeriodSeconds"], 120);
        assert_eq!(template["revisionSuffix"], "v3");
        assert_eq!(template["containers"][0]["command"], json!(["graphrt"]));
        assert_eq!(template["containers"][0]["args"], json!(["serve", "agent.yaml", "--trust-gateway"]));
        assert_eq!(template["containers"][0]["resources"], json!({"cpu": 0.5, "memory": "1Gi"}));
        let configuration = &body["properties"]["configuration"];
        assert_eq!(configuration["activeRevisionsMode"], "Multiple");
        assert_eq!(configuration["ingress"]["external"], false);
        assert_eq!(configuration["ingress"]["transport"], "http");
        assert_eq!(
            configuration["secrets"][0],
            json!({"name": "openai-api-key", "keyVaultUrl": "https://kv1.vault.azure.net/secrets/openai-key", "identity": "system"})
        );
        assert_eq!(
            template["containers"][0]["env"][1],
            json!({"name": "OPENAI_API_KEY", "secretRef": "openai-api-key"})
        );
        assert_eq!(body["tags"]["graphrt-workflow"], "sha256:abc");
    }

    #[test]
    fn missing_settings_and_bad_versions_are_rejected() {
        let mut spec = generic_spec("3");
        spec.provider_settings.remove("environment_id");
        assert!(AcaAppSpec::from_deployment_spec(&spec).is_err());
        let config = KeyVaultConfig::default();
        for version in ["", "v--1", "v1-", "Ab_c", "this-version-label-is-far-too-long-for-azure"] {
            let mut app = sample_spec();
            app.version = version.into();
            assert!(app.validate(&config).is_err(), "{version:?} must be rejected");
        }
        assert_eq!(version_label("V7").as_deref(), Some("v7"));
        assert_eq!(version_label("20261122").as_deref(), Some("v20261122"));
        assert_eq!(format_memory(1536), "1.5Gi");
    }

    #[test]
    fn clear_secret_values_and_dangling_references_are_rejected() {
        let config = KeyVaultConfig::default();
        assert!(sample_spec().validate(&config).is_ok());

        let mut clear = sample_spec();
        clear.env.push(EnvVar::Value { name: "ANTHROPIC_API_KEY".into(), value: "sk-ant-123".into() });
        let error = clear.validate(&config).unwrap_err().to_string();
        assert!(error.contains("ANTHROPIC_API_KEY") && !error.contains("sk-ant-123"));

        let mut dangling = sample_spec();
        dangling.env.push(EnvVar::Secret { name: "X".into(), secret_name: "missing".into() });
        assert!(dangling.validate(&config).is_err());

        let mut long_grace = sample_spec();
        long_grace.grace_secs = 601;
        assert!(long_grace.validate(&config).is_err());

        for name in ["A", "-ab", "ab-", "a--b", "Upper", "a_b"] {
            let mut bad = sample_spec();
            bad.app_name = name.into();
            assert!(bad.validate(&config).is_err(), "{name} must be rejected");
        }
    }
}
