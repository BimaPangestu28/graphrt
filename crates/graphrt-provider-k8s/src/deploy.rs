//! Deploy-time driver (`graphrt deploy`): server-side applies the Deployment
//! and Service of one deployment version. Secrets are referenced, never
//! embedded: the Deployment points at Secret objects (`secretKeyRef`) that
//! [`K8sDeployDriver::apply_secret`] or the operator writes with deploy-time
//! credentials (conformance C10).
//!
//! A new version starts with `min_replicas` (0 = scale to zero); re-applying an
//! existing version keeps its current replica count, so a deploy never fights
//! the gateway's scaling.
//!
//! `SecretRef::reference` is `<secret-name>/<key>`, or `<secret-name>` for
//! the key named like the env var. `provider_settings` accepts `port`
//! (default 8080); the namespace is the driver's.

use base64::Engine as _;
use graphrt_provider_api::{BoxFuture, DeployDriver, DeployError, DeploymentRef, DeploymentSpec, ProviderError};
use reqwest::Method;
use serde_json::{Value, json};

use crate::auth::SecretString;
use crate::client::{KubeClient, RequestBody, http_status};
use crate::names::{is_label_value, object_name};
use crate::resources::{DEPLOYMENT_LABEL, MANAGED_BY_LABEL, VERSION_LABEL, spec_replicas};

/// Field manager of deploy-time applies.
pub const DEPLOY_FIELD_MANAGER: &str = "graphrt-deploy";
/// Annotation carrying the workflow fingerprint of a version.
pub const FINGERPRINT_ANNOTATION: &str = "graphrt.dev/workflow-fingerprint";
const DEFAULT_PORT: u16 = 8080;

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

    async fn server_side_apply(&self, collection: &str, name: &str, manifest: &Value) -> Result<(), ProviderError> {
        let path = format!("{collection}/{name}?fieldManager={DEPLOY_FIELD_MANAGER}&force=true");
        self.client.send(Method::PATCH, &path, Some(RequestBody::apply_patch(manifest))).await.map(drop)
    }

    /// Current replicas of an existing Deployment, `None` when it does not exist yet.
    async fn current_replicas(&self, name: &str) -> Result<Option<u32>, ProviderError> {
        let path = format!("/apis/apps/v1/namespaces/{}/deployments/{name}/scale", self.namespace);
        match self.client.get(&path).await {
            Ok(scale) => Ok(Some(spec_replicas(&scale))),
            Err(error) if http_status(&error) == Some(404) => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Write a Secret from deploy-time values (the gateway never calls this).
    pub async fn apply_secret(&self, name: &str, values: &[(String, SecretString)]) -> Result<(), DeployError> {
        if !is_dns_subdomain(name) {
            return Err(DeployError::invalid(format!("secret name {name:?} is not a DNS-1123 subdomain")));
        }
        let collection = format!("/api/v1/namespaces/{}/secrets", self.namespace);
        Ok(self.server_side_apply(&collection, name, &secret_manifest(&self.namespace, name, values)).await?)
    }

    /// Apply `spec` (the body of [`DeployDriver::apply`]).
    pub async fn apply_spec(&self, spec: &DeploymentSpec) -> Result<DeploymentRef, DeployError> {
        let port = validate(spec)?;
        let reference = DeploymentRef::new(&spec.name, &spec.version, "");
        let name = object_name(&reference);
        let replicas = self.current_replicas(&name).await?.unwrap_or(spec.min_replicas);
        let apps = format!("/apis/apps/v1/namespaces/{}/deployments", self.namespace);
        self.server_side_apply(&apps, &name, &deployment_manifest(spec, &self.namespace, replicas, port)).await?;
        let services = format!("/api/v1/namespaces/{}/services", self.namespace);
        self.server_side_apply(&services, &name, &service_manifest(spec, &self.namespace, port)).await?;
        Ok(DeploymentRef::new(&spec.name, &spec.version, format!("{}/{name}", self.namespace)))
    }
}

impl DeployDriver for K8sDeployDriver {
    fn apply<'a>(&'a self, spec: &'a DeploymentSpec) -> BoxFuture<'a, Result<DeploymentRef, DeployError>> {
        Box::pin(self.apply_spec(spec))
    }
}

/// Reject specs Kubernetes would refuse, before any API call.
///
/// @returns the container port
pub fn validate(spec: &DeploymentSpec) -> Result<u16, DeployError> {
    if !is_label_value(&spec.name) || !is_label_value(&spec.version) {
        return Err(DeployError::invalid(format!(
            "deployment {:?} / version {:?} must be label values",
            spec.name, spec.version
        )));
    }
    if spec.image.as_deref().is_none_or(|image| image.trim().is_empty()) {
        return Err(DeployError::invalid("kubernetes needs an image"));
    }
    if spec.command.is_empty() {
        return Err(DeployError::invalid("command is empty"));
    }
    if spec.min_replicas > spec.max_replicas {
        return Err(DeployError::invalid("min_replicas > max_replicas"));
    }
    let mut port = DEFAULT_PORT;
    for (key, value) in &spec.provider_settings {
        match key.as_str() {
            "port" => {
                port = value
                    .as_u64()
                    .and_then(|port| u16::try_from(port).ok())
                    .filter(|port| *port != 0)
                    .ok_or_else(|| DeployError::invalid(format!("port {value} is not a TCP port")))?;
            }
            other => return Err(DeployError::unsupported(format!("provider setting {other:?}"))),
        }
    }
    if let Some(name) = spec.env.keys().find(|name| !is_env_name(name)) {
        return Err(DeployError::invalid(format!("env var name {name:?}")));
    }
    for secret in &spec.secrets {
        if !is_env_name(&secret.env) {
            return Err(DeployError::invalid(format!("secret env var name {:?}", secret.env)));
        }
        let (name, key) = secret_key_ref(&secret.reference, &secret.env);
        if !is_dns_subdomain(name) || key.is_empty() {
            return Err(DeployError::invalid(format!(
                "secret reference {:?} is not <secret>[/<key>]",
                secret.reference
            )));
        }
    }
    Ok(port)
}

/// `<secret>/<key>` or `<secret>` (key = env var name).
fn secret_key_ref<'a>(reference: &'a str, env: &'a str) -> (&'a str, &'a str) {
    reference.split_once('/').unwrap_or((reference, env))
}

fn labels(spec: &DeploymentSpec) -> Value {
    json!({ DEPLOYMENT_LABEL: spec.name, VERSION_LABEL: spec.version, MANAGED_BY_LABEL: "graphrt" })
}

fn selector(spec: &DeploymentSpec) -> Value {
    json!({ DEPLOYMENT_LABEL: spec.name, VERSION_LABEL: spec.version })
}

fn container_env(spec: &DeploymentSpec) -> Vec<Value> {
    let plain = spec.env.iter().map(|(name, value)| json!({ "name": name, "value": value }));
    let secrets = spec.secrets.iter().map(|secret| {
        let (name, key) = secret_key_ref(&secret.reference, &secret.env);
        json!({ "name": secret.env, "valueFrom": { "secretKeyRef": { "name": name, "key": key } } })
    });
    plain.chain(secrets).collect()
}

fn resources(spec: &DeploymentSpec) -> Value {
    let mut requests = serde_json::Map::new();
    let mut limits = serde_json::Map::new();
    if spec.resources.cpu_millis > 0 {
        requests.insert("cpu".into(), json!(format!("{}m", spec.resources.cpu_millis)));
    }
    if spec.resources.memory_mib > 0 {
        let memory = json!(format!("{}Mi", spec.resources.memory_mib));
        requests.insert("memory".into(), memory.clone());
        limits.insert("memory".into(), memory);
    }
    json!({ "requests": requests, "limits": limits })
}

/// The `apps/v1` Deployment of one version.
pub fn deployment_manifest(spec: &DeploymentSpec, namespace: &str, replicas: u32, port: u16) -> Value {
    let name = object_name(&DeploymentRef::new(&spec.name, &spec.version, ""));
    json!({
        "apiVersion": "apps/v1",
        "kind": "Deployment",
        "metadata": {
            "name": name,
            "namespace": namespace,
            "labels": labels(spec),
            "annotations": { FINGERPRINT_ANNOTATION: spec.workflow_fingerprint },
        },
        "spec": {
            "replicas": replicas,
            "revisionHistoryLimit": 1,
            "selector": { "matchLabels": selector(spec) },
            "template": {
                "metadata": { "labels": labels(spec) },
                "spec": {
                    "terminationGracePeriodSeconds": spec.grace_secs,
                    // The agent never talks to the API server (decision 2: gateway is the only writer).
                    "automountServiceAccountToken": false,
                    "enableServiceLinks": false,
                    "containers": [{
                        "name": "agent",
                        "image": spec.image,
                        "command": spec.command,
                        "ports": [{ "name": "http", "containerPort": port, "protocol": "TCP" }],
                        "env": container_env(spec),
                        "resources": resources(spec),
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
pub fn service_manifest(spec: &DeploymentSpec, namespace: &str, port: u16) -> Value {
    json!({
        "apiVersion": "v1",
        "kind": "Service",
        "metadata": {
            "name": object_name(&DeploymentRef::new(&spec.name, &spec.version, "")),
            "namespace": namespace,
            "labels": labels(spec),
        },
        "spec": {
            "type": "ClusterIP",
            "selector": selector(spec),
            "ports": [{ "name": "http", "port": port, "targetPort": "http", "protocol": "TCP" }],
        },
    })
}

fn secret_manifest(namespace: &str, name: &str, values: &[(String, SecretString)]) -> Value {
    let data: serde_json::Map<String, Value> = values
        .iter()
        .map(|(key, value)| (key.clone(), json!(base64::engine::general_purpose::STANDARD.encode(value.expose()))))
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
    use graphrt_provider_api::{DeployErrorKind, Resources, SecretRef};

    use super::*;

    fn spec() -> DeploymentSpec {
        DeploymentSpec {
            name: "support-bot".into(),
            version: "v3".into(),
            image: Some("ghcr.io/graphrt/graphrt:0.5.0".into()),
            command: vec!["graphrt".into(), "serve".into(), "--trust-gateway".into(), "/app/workflow.yaml".into()],
            env: [("RUST_LOG".to_owned(), "info".to_owned())].into(),
            secrets: vec![
                SecretRef { env: "OPENAI_API_KEY".into(), reference: "shared/openai".into() },
                SecretRef { env: "ANTHROPIC_API_KEY".into(), reference: "support-bot-llm".into() },
            ],
            max_active_runs: 8,
            min_replicas: 0,
            max_replicas: 10,
            grace_secs: 120,
            resources: Resources { cpu_millis: 250, memory_mib: 128 },
            workflow_fingerprint: "sha256:abc".into(),
            provider_settings: Default::default(),
        }
    }

    #[test]
    fn deployment_references_secrets_without_values() {
        let manifest = deployment_manifest(&spec(), "agents", 0, 8080);
        let container = &manifest["spec"]["template"]["spec"]["containers"][0];
        assert_eq!(manifest["metadata"]["name"], "support-bot-v3");
        assert_eq!(manifest["metadata"]["annotations"][FINGERPRINT_ANNOTATION], "sha256:abc");
        assert_eq!(manifest["spec"]["replicas"], 0);
        assert_eq!(manifest["spec"]["template"]["spec"]["terminationGracePeriodSeconds"], 120);
        assert_eq!(manifest["spec"]["template"]["spec"]["automountServiceAccountToken"], false);
        assert_eq!(container["env"][1]["valueFrom"]["secretKeyRef"], json!({"name": "shared", "key": "openai"}));
        assert_eq!(
            container["env"][2]["valueFrom"]["secretKeyRef"],
            json!({"name": "support-bot-llm", "key": "ANTHROPIC_API_KEY"})
        );
        assert_eq!(container["resources"]["requests"]["cpu"], "250m");
        assert_eq!(container["resources"]["limits"]["memory"], "128Mi");
        assert_eq!(container["readinessProbe"]["httpGet"]["path"], "/healthz");
        assert_eq!(manifest["spec"]["selector"]["matchLabels"][VERSION_LABEL], "v3");
    }

    #[test]
    fn service_selects_the_version_pods() {
        let manifest = service_manifest(&spec(), "agents", 9000);
        assert_eq!(manifest["spec"]["selector"], selector(&spec()));
        assert_eq!(manifest["spec"]["ports"][0]["port"], 9000);
    }

    #[test]
    fn secret_manifest_base64_encodes_values() {
        let manifest = secret_manifest("agents", "bot-llm", &[("OPENAI_API_KEY".into(), SecretString::new("sk-test"))]);
        assert_eq!(manifest["data"]["OPENAI_API_KEY"], "c2stdGVzdA==");
    }

    #[test]
    fn validation_rejects_what_kubernetes_would() {
        assert_eq!(validate(&spec()).unwrap(), 8080);
        let mut with_port = spec();
        with_port.provider_settings.insert("port".into(), json!(9000));
        assert_eq!(validate(&with_port).unwrap(), 9000);
        type Mutation = Box<dyn Fn(&mut DeploymentSpec)>;
        let invalid: Vec<Mutation> = vec![
            Box::new(|spec| spec.version = "v3/x".into()),
            Box::new(|spec| spec.image = None),
            Box::new(|spec| spec.command.clear()),
            Box::new(|spec| {
                spec.env.insert("1BAD".into(), "x".into());
            }),
            Box::new(|spec| spec.secrets.push(SecretRef { env: "X".into(), reference: "Upper/key".into() })),
            Box::new(|spec| spec.secrets.push(SecretRef { env: "X".into(), reference: "ok/".into() })),
            Box::new(|spec| spec.min_replicas = 11),
            Box::new(|spec| {
                spec.provider_settings.insert("port".into(), json!(0));
            }),
        ];
        for (index, mutate) in invalid.iter().enumerate() {
            let mut bad = spec();
            mutate(&mut bad);
            assert_eq!(validate(&bad).unwrap_err().kind, DeployErrorKind::Invalid, "case {index}");
        }
        let mut unknown = spec();
        unknown.provider_settings.insert("endpoint".into(), json!("http://169.254.169.254"));
        assert_eq!(validate(&unknown).unwrap_err().kind, DeployErrorKind::Unsupported);
    }
}
