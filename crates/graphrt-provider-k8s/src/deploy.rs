//! Deploy-time driver (`graphrt deploy`): server-side applies the Deployment
//! and Service of one deployment version. Secrets are referenced, never
//! embedded: the Deployment points at Secret objects (`envFrom` /
//! `secretKeyRef`) that [`K8sDeployDriver::apply_secret`] or the operator
//! writes with deploy-time credentials (conformance C10).
//!
//! The Deployment is created with the replica count it already has (0 for a
//! new version), so re-applying a version never fights the gateway's scaling.

use reqwest::Method;
use serde_json::{Value, json};

use base64::Engine as _;

use crate::api::{DeployDriver, DeployError, DeploymentRef, DeploymentSpec, SecretRef};
use crate::auth::SecretString;
use crate::client::{KubeClient, RequestBody};
use crate::names::{is_label_value, object_name};
use crate::resources::{DEPLOYMENT_LABEL, MANAGED_BY_LABEL, Scale, VERSION_LABEL};

/// Field manager of deploy-time applies.
pub const DEPLOY_FIELD_MANAGER: &str = "graphrt-deploy";

/// Creates deployment versions in one namespace.
#[derive(Debug, Clone)]
pub struct K8sDeployDriver {
    client: KubeClient,
    namespace: String,
}

impl K8sDeployDriver {
    /// A driver applying into `namespace`.
    pub fn new(client: KubeClient, namespace: impl Into<String>) -> Self {
        Self { client, namespace: namespace.into() }
    }

    fn apply_path(&self, collection: &str, name: &str) -> String {
        format!("{collection}/{name}?fieldManager={DEPLOY_FIELD_MANAGER}&force=true")
    }

    async fn server_side_apply(&self, collection: &str, name: &str, manifest: &Value) -> Result<(), DeployError> {
        let path = self.apply_path(collection, name);
        self.client
            .send(Method::PATCH, &path, Some(RequestBody::apply_patch(manifest)))
            .await
            .map(drop)
            .map_err(|error| DeployError::Provider(error.into()))
    }

    /// Current replicas of an existing Deployment, 0 when it does not exist yet.
    async fn current_replicas(&self, name: &str) -> Result<u32, DeployError> {
        let path = format!("/apis/apps/v1/namespaces/{}/deployments/{name}/scale", self.namespace);
        match self.client.get::<Scale>(&path).await {
            Ok(scale) => Ok(scale.spec.replicas),
            Err(error) if error.status_code() == Some(404) => Ok(0),
            Err(error) => Err(DeployError::Provider(error.into())),
        }
    }

    /// Write a Secret from deploy-time values (the gateway never calls this).
    pub async fn apply_secret(&self, name: &str, values: &[(String, SecretString)]) -> Result<(), DeployError> {
        if !is_dns_subdomain(name) {
            return Err(DeployError::InvalidSpec(format!("secret name {name:?} is not a DNS-1123 subdomain")));
        }
        let collection = format!("/api/v1/namespaces/{}/secrets", self.namespace);
        self.server_side_apply(&collection, name, &secret_manifest(&self.namespace, name, values)).await
    }
}

impl DeployDriver for K8sDeployDriver {
    async fn apply(&self, spec: &DeploymentSpec) -> Result<DeploymentRef, DeployError> {
        validate(spec)?;
        let name = object_name(&spec.deployment);
        let replicas = self.current_replicas(&name).await?;
        let apps = format!("/apis/apps/v1/namespaces/{}/deployments", self.namespace);
        self.server_side_apply(&apps, &name, &deployment_manifest(spec, &self.namespace, replicas)).await?;
        let services = format!("/api/v1/namespaces/{}/services", self.namespace);
        self.server_side_apply(&services, &name, &service_manifest(spec, &self.namespace)).await?;
        Ok(spec.deployment.clone())
    }
}

/// Reject specs Kubernetes would refuse, before any API call.
pub fn validate(spec: &DeploymentSpec) -> Result<(), DeployError> {
    let invalid = |message: String| Err(DeployError::InvalidSpec(message));
    if !is_label_value(&spec.deployment.deployment) || !is_label_value(&spec.deployment.version) {
        return invalid(format!("deployment {:?} / version {:?} must be label values", spec.deployment.deployment, spec.deployment.version));
    }
    if spec.image.trim().is_empty() {
        return invalid("image is empty".into());
    }
    if spec.port == 0 {
        return invalid("port is 0".into());
    }
    for (name, _) in &spec.env {
        if !is_env_name(name) {
            return invalid(format!("env var name {name:?}"));
        }
    }
    for secret in &spec.secrets {
        if !is_dns_subdomain(&secret.name) {
            return invalid(format!("secret name {:?}", secret.name));
        }
        if secret.key.is_none() && secret.env.is_some() {
            return invalid(format!("secret {:?}: env needs a key", secret.name));
        }
        if let Some(env) = secret.env.as_deref().or(secret.key.as_deref())
            && !is_env_name(env)
        {
            return invalid(format!("secret {:?}: env var name {env:?}", secret.name));
        }
    }
    Ok(())
}

fn labels(deployment: &DeploymentRef) -> Value {
    json!({
        DEPLOYMENT_LABEL: deployment.deployment,
        VERSION_LABEL: deployment.version,
        MANAGED_BY_LABEL: "graphrt",
    })
}

fn selector(deployment: &DeploymentRef) -> Value {
    json!({ DEPLOYMENT_LABEL: deployment.deployment, VERSION_LABEL: deployment.version })
}

fn container_env(spec: &DeploymentSpec) -> (Vec<Value>, Vec<Value>) {
    let mut env: Vec<Value> = spec.env.iter().map(|(name, value)| json!({ "name": name, "value": value })).collect();
    let mut env_from = Vec::new();
    for SecretRef { name, key, env: env_name } in &spec.secrets {
        match key {
            Some(key) => env.push(json!({
                "name": env_name.as_deref().unwrap_or(key),
                "valueFrom": { "secretKeyRef": { "name": name, "key": key } },
            })),
            None => env_from.push(json!({ "secretRef": { "name": name } })),
        }
    }
    (env, env_from)
}

/// The `apps/v1` Deployment of one version.
pub fn deployment_manifest(spec: &DeploymentSpec, namespace: &str, replicas: u32) -> Value {
    let name = object_name(&spec.deployment);
    let (env, env_from) = container_env(spec);
    let mut requests = serde_json::Map::new();
    let mut limits = serde_json::Map::new();
    if let Some(cpu) = &spec.cpu {
        requests.insert("cpu".into(), json!(cpu));
    }
    if let Some(memory) = &spec.memory {
        requests.insert("memory".into(), json!(memory));
        limits.insert("memory".into(), json!(memory));
    }
    let resources = json!({ "requests": requests, "limits": limits });
    json!({
        "apiVersion": "apps/v1",
        "kind": "Deployment",
        "metadata": { "name": name, "namespace": namespace, "labels": labels(&spec.deployment) },
        "spec": {
            "replicas": replicas,
            "revisionHistoryLimit": 1,
            "selector": { "matchLabels": selector(&spec.deployment) },
            "template": {
                "metadata": { "labels": labels(&spec.deployment) },
                "spec": {
                    "terminationGracePeriodSeconds": spec.grace_secs,
                    // The agent never talks to the API server (decision 2: gateway is the only writer).
                    "automountServiceAccountToken": false,
                    "enableServiceLinks": false,
                    "containers": [{
                        "name": "agent",
                        "image": spec.image,
                        "args": spec.args,
                        "ports": [{ "name": "http", "containerPort": spec.port, "protocol": "TCP" }],
                        "env": env,
                        "envFrom": env_from,
                        "resources": resources,
                        // A 1 s probe period keeps cold wake inside the 3 s target (plan §5).
                        "readinessProbe": {
                            "httpGet": { "path": "/healthz", "port": "http" },
                            "periodSeconds": 1,
                            "timeoutSeconds": 1,
                            "failureThreshold": 2,
                        },
                        "securityContext": { "allowPrivilegeEscalation": false },
                    }],
                },
            },
        },
    })
}

/// The ClusterIP Service whose EndpointSlices the provider watches.
pub fn service_manifest(spec: &DeploymentSpec, namespace: &str) -> Value {
    json!({
        "apiVersion": "v1",
        "kind": "Service",
        "metadata": { "name": object_name(&spec.deployment), "namespace": namespace, "labels": labels(&spec.deployment) },
        "spec": {
            "type": "ClusterIP",
            "selector": selector(&spec.deployment),
            "ports": [{ "name": "http", "port": spec.port, "targetPort": "http", "protocol": "TCP" }],
        },
    })
}

fn secret_manifest(namespace: &str, name: &str, values: &[(String, SecretString)]) -> Value {
    let data: serde_json::Map<String, Value> = values
        .iter()
        .map(|(key, value)| {
            (key.clone(), json!(base64::engine::general_purpose::STANDARD.encode(value.expose())))
        })
        .collect();
    json!({
        "apiVersion": "v1",
        "kind": "Secret",
        "type": "Opaque",
        "metadata": { "name": name, "namespace": namespace, "labels": { MANAGED_BY_LABEL: "graphrt" } },
        "data": data,
    })
}

fn is_env_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes.next().is_some_and(|first| first.is_ascii_alphabetic() || first == b'_')
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-'))
}

fn is_dns_subdomain(name: &str) -> bool {
    name.len() <= 253
        && name.split('.').all(|label| {
            let bytes = label.as_bytes();
            !bytes.is_empty()
                && bytes.len() <= 63
                && bytes.first().is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
                && bytes.last().is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
                && bytes.iter().all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-')
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> DeploymentSpec {
        DeploymentSpec {
            deployment: DeploymentRef::new("support-bot", "v3"),
            image: "ghcr.io/bimapangestu28/graphrt:0.5.0".into(),
            args: vec!["serve".into(), "--trust-gateway".into(), "/app/workflow.yaml".into()],
            port: 8080,
            env: vec![("RUST_LOG".into(), "info".into())],
            secrets: vec![
                SecretRef { name: "support-bot-llm".into(), key: None, env: None },
                SecretRef { name: "shared".into(), key: Some("openai".into()), env: Some("OPENAI_API_KEY".into()) },
            ],
            grace_secs: 120,
            cpu: Some("250m".into()),
            memory: Some("128Mi".into()),
        }
    }

    #[test]
    fn deployment_references_secrets_without_values() {
        let manifest = deployment_manifest(&spec(), "agents", 0);
        let container = &manifest["spec"]["template"]["spec"]["containers"][0];
        assert_eq!(manifest["metadata"]["name"], "support-bot-v3");
        assert_eq!(manifest["spec"]["replicas"], 0);
        assert_eq!(manifest["spec"]["template"]["spec"]["terminationGracePeriodSeconds"], 120);
        assert_eq!(manifest["spec"]["template"]["spec"]["automountServiceAccountToken"], false);
        assert_eq!(container["envFrom"][0]["secretRef"]["name"], "support-bot-llm");
        assert_eq!(container["env"][1]["name"], "OPENAI_API_KEY");
        assert_eq!(container["env"][1]["valueFrom"]["secretKeyRef"], json!({"name": "shared", "key": "openai"}));
        assert_eq!(container["readinessProbe"]["httpGet"]["path"], "/healthz");
        assert_eq!(manifest["spec"]["selector"]["matchLabels"][VERSION_LABEL], "v3");
    }

    #[test]
    fn service_selects_the_version_pods() {
        let manifest = service_manifest(&spec(), "agents");
        assert_eq!(manifest["spec"]["selector"], selector(&spec().deployment));
        assert_eq!(manifest["spec"]["ports"][0]["targetPort"], "http");
    }

    #[test]
    fn secret_manifest_base64_encodes_values() {
        let manifest = secret_manifest("agents", "bot-llm", &[("OPENAI_API_KEY".into(), SecretString::new("sk-test"))]);
        assert_eq!(manifest["data"]["OPENAI_API_KEY"], "c2stdGVzdA==");
    }

    #[test]
    fn validation_rejects_what_kubernetes_would() {
        assert!(validate(&spec()).is_ok());
        let mut bad = spec();
        bad.deployment.version = "v3/x".into();
        assert!(validate(&bad).is_err());
        let mut bad = spec();
        bad.env.push(("1BAD".into(), "x".into()));
        assert!(validate(&bad).is_err());
        let mut bad = spec();
        bad.secrets.push(SecretRef { name: "Upper".into(), key: None, env: None });
        assert!(validate(&bad).is_err());
        let mut bad = spec();
        bad.secrets.push(SecretRef { name: "ok".into(), key: None, env: Some("X".into()) });
        assert!(validate(&bad).is_err());
        let mut bad = spec();
        bad.port = 0;
        assert!(validate(&bad).is_err());
    }
}
