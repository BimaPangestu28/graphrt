//! Deployment spec of one container app version, its validation, and the ARM
//! `Microsoft.App/containerApps` body it renders to.

use serde_json::{Value, json};

use crate::api::DeployError;
use crate::keyvault::{KeyVaultConfig, KeyVaultSecretRef};

/// Largest `terminationGracePeriodSeconds` Container Apps accepts.
pub const MAX_GRACE_SECS: u32 = 600;
/// Default grace (plan §5: set to 120 s by `graphrt deploy`).
pub const DEFAULT_GRACE_SECS: u32 = 120;

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

/// A private registry pulled with the app's identity (`AcrPull`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegistryRef {
    pub server: String,
    /// `"system"` or a user-assigned identity resource id.
    pub identity: String,
}

/// What `graphrt deploy --target aca` creates: one immutable version.
#[derive(Clone, Debug, PartialEq)]
pub struct AcaAppSpec {
    pub deployment: String,
    pub version: u64,
    /// Container app name: 2-32 of `[a-z0-9-]`, starts with a letter, no `--`.
    pub app_name: String,
    pub location: String,
    /// Resource id of the managed environment.
    pub environment_id: String,
    pub image: String,
    pub args: Vec<String>,
    pub target_port: u16,
    /// vCPU, e.g. 0.5.
    pub cpu: f64,
    /// e.g. `"1Gi"` (Container Apps wants 2 GiB per vCPU on consumption).
    pub memory: String,
    pub env: Vec<EnvVar>,
    pub secrets: Vec<KeyVaultSecretRef>,
    pub identity: AppIdentity,
    pub registry: Option<RegistryRef>,
    /// Agent `max_active_runs`: the HTTP scale rule's `concurrentRequests`.
    pub max_active_runs: u32,
    pub max_replicas: u32,
    pub grace_secs: u32,
    /// Readiness probe path of `graphrt serve`.
    pub health_path: String,
}

/// Env var names that must come from a secret reference, never a clear value.
const SECRET_NAME_MARKERS: [&str; 6] = ["SECRET", "TOKEN", "PASSWORD", "API_KEY", "APIKEY", "PRIVATE_KEY"];

impl AcaAppSpec {
    /// Revision suffix of this version (`v<N>`); the revision name is `<app>--v<N>`.
    pub fn revision_suffix(&self) -> String {
        version_label(self.version)
    }

    pub fn revision_name(&self) -> String {
        revision_name(&self.app_name, self.version)
    }

    /// @throws `DeployError::InvalidSpec` naming the first problem found.
    pub fn validate(&self, key_vault: &KeyVaultConfig) -> Result<(), DeployError> {
        let invalid = |message: String| Err(DeployError::InvalidSpec(message));
        if !is_app_name(&self.app_name) {
            return invalid(format!(
                "app name {:?} must be 2-32 lowercase letters, digits or '-', start with a letter, and not contain '--'",
                self.app_name
            ));
        }
        if self.version == 0 {
            return invalid("deployment version must be >= 1".into());
        }
        if self.image.is_empty() || self.location.is_empty() || self.environment_id.is_empty() {
            return invalid("image, location and environment_id are required".into());
        }
        if self.grace_secs > MAX_GRACE_SECS {
            return invalid(format!(
                "grace_secs {} exceeds the Container Apps maximum {MAX_GRACE_SECS}",
                self.grace_secs
            ));
        }
        if self.max_active_runs == 0 || !(1..=1000).contains(&self.max_replicas) {
            return invalid("max_active_runs must be >= 1 and max_replicas 1..=1000".into());
        }
        for secret in &self.secrets {
            secret.validate(key_vault).map_err(|error| DeployError::InvalidSpec(error.to_string()))?;
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
    pub fn to_arm(&self, traffic: Vec<Value>) -> Value {
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
        json!({
            "location": self.location,
            "identity": identity,
            "tags": { "graphrt-deployment": self.deployment, "graphrt-version": self.version.to_string() },
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
                    "revisionSuffix": self.revision_suffix(),
                    "terminationGracePeriodSeconds": self.grace_secs,
                    "containers": [{
                        "name": "agent",
                        "image": self.image,
                        "args": self.args,
                        "env": env,
                        "resources": { "cpu": self.cpu, "memory": self.memory },
                        "probes": [{
                            "type": "Readiness",
                            "httpGet": { "path": self.health_path, "port": self.target_port },
                            "periodSeconds": 5,
                        }],
                    }],
                    "scale": {
                        // Scale to zero; the first request wakes a replica.
                        "minReplicas": 0,
                        "maxReplicas": self.max_replicas,
                        "rules": [{
                            "name": "http-concurrency",
                            "http": { "metadata": { "concurrentRequests": self.max_active_runs.to_string() } },
                        }],
                    },
                },
            },
        })
    }
}

/// `v<N>`: revision suffix and traffic label of deployment version N.
pub fn version_label(version: u64) -> String {
    format!("v{version}")
}

pub fn revision_name(app_name: &str, version: u64) -> String {
    format!("{app_name}--{}", version_label(version))
}

/// The version encoded in a revision name `<app>--v<N>`.
pub fn version_of_revision(app_name: &str, revision: &str) -> Option<u64> {
    revision.strip_prefix(app_name)?.strip_prefix("--v")?.parse().ok()
}

fn looks_secret(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    SECRET_NAME_MARKERS.iter().any(|marker| upper.contains(marker))
}

pub(crate) fn is_app_name(name: &str) -> bool {
    (2..=32).contains(&name.len())
        && name.starts_with(|c: char| c.is_ascii_lowercase())
        && !name.ends_with('-')
        && !name.contains("--")
        && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn sample_spec() -> AcaAppSpec {
        AcaAppSpec {
            deployment: "support-bot".into(),
            version: 3,
            app_name: "support-bot".into(),
            location: "westeurope".into(),
            environment_id: "/subscriptions/s/resourceGroups/rg/providers/Microsoft.App/managedEnvironments/env".into(),
            image: "ghcr.io/example/agent:3".into(),
            args: vec!["serve".into(), "agent.yaml".into(), "--trust-gateway".into()],
            target_port: 8080,
            cpu: 0.5,
            memory: "1Gi".into(),
            env: vec![
                EnvVar::Value { name: "RUST_LOG".into(), value: "info".into() },
                EnvVar::Secret { name: "OPENAI_API_KEY".into(), secret_name: "openai-key".into() },
            ],
            secrets: vec![KeyVaultSecretRef {
                name: "openai-key".into(),
                secret_url: "https://kv1.vault.azure.net/secrets/openai-key".into(),
                identity: "system".into(),
            }],
            identity: AppIdentity::System,
            registry: None,
            max_active_runs: 8,
            max_replicas: 10,
            grace_secs: DEFAULT_GRACE_SECS,
            health_path: "/healthz".into(),
        }
    }

    #[test]
    fn spec_renders_scale_to_zero_http_rule_grace_and_key_vault_refs() {
        let body = sample_spec().to_arm(vec![json!({"latestRevision": true, "weight": 100})]);
        let template = &body["properties"]["template"];
        assert_eq!(template["scale"]["minReplicas"], 0);
        assert_eq!(template["scale"]["rules"][0]["http"]["metadata"]["concurrentRequests"], "8");
        assert_eq!(template["terminationGracePeriodSeconds"], 120);
        assert_eq!(template["revisionSuffix"], "v3");
        let configuration = &body["properties"]["configuration"];
        assert_eq!(configuration["activeRevisionsMode"], "Multiple");
        assert_eq!(configuration["ingress"]["external"], false);
        assert_eq!(configuration["ingress"]["transport"], "http");
        assert_eq!(configuration["secrets"][0]["keyVaultUrl"], "https://kv1.vault.azure.net/secrets/openai-key");
        assert_eq!(template["containers"][0]["env"][1], json!({"name": "OPENAI_API_KEY", "secretRef": "openai-key"}));
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

    #[test]
    fn revision_names_round_trip_versions() {
        assert_eq!(revision_name("bot", 12), "bot--v12");
        assert_eq!(version_of_revision("bot", "bot--v12"), Some(12));
        assert_eq!(version_of_revision("bot", "bot--abc"), None);
        assert_eq!(version_of_revision("bot", "other--v1"), None);
    }
}
