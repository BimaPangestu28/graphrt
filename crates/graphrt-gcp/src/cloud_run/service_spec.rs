//! Renders a graphrt `DeploymentSpec` as a Cloud Run Admin API v2 `Service`.
//!
//! What every graphrt service gets:
//! - `scaling.minInstanceCount = min_replicas` (0 by default: scale to zero) and
//!   `maxInstanceCount = max_replicas`;
//! - `maxInstanceRequestConcurrency = max_active_runs` (one request = one run stream);
//! - `cpuIdle = true` (request-based billing: CPU only while a request is open,
//!   which is why the gateway holds every run's stream) and `startupCpuBoost = true`;
//! - `timeout = request_timeout_secs` (≤ 3600 s; the gateway preempts before it);
//! - secrets as `valueSource.secretKeyRef` (Secret Manager references, never values);
//! - one traffic tag per version (`v7`), so a run paused on version N can be
//!   resumed on revision N while version N+1 takes 100 % of new traffic.

use std::collections::BTreeMap;

use graphrt_provider_api::{DeployError, DeploymentSpec, SecretRef};
use serde_json::{Map, Value, json};

use super::config::CloudRunConfig;
use super::naming::label_value;

/// Port the agent listens on inside the container (Cloud Run's `$PORT`).
pub const CONTAINER_PORT: u16 = 8080;
/// Cloud Run sets these itself; a spec may not.
const RESERVED_ENV: [&str; 4] = ["PORT", "K_SERVICE", "K_REVISION", "K_CONFIGURATION"];

/// Names of one deployment version on Cloud Run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceNames {
    /// `projects/{p}/locations/{r}/services/{id}`.
    pub resource: String,
    /// Service id.
    pub service_id: String,
    /// Revision name of this version.
    pub revision: String,
    /// Traffic tag of this version.
    pub tag: String,
    /// Custom audience of the service's ID tokens (same for every tag URL).
    pub audience: String,
}

/// Validated `provider_settings` of a Cloud Run spec.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloudRunSettings {
    /// Runtime service account of the revision (needs `secretAccessor` on its secrets).
    pub service_account: Option<String>,
    /// `INGRESS_TRAFFIC_*`.
    pub ingress: &'static str,
    /// Startup CPU boost (default on).
    pub startup_cpu_boost: bool,
    /// `EXECUTION_ENVIRONMENT_GEN1|GEN2`, `None` = platform default.
    pub execution_environment: Option<&'static str>,
}

impl CloudRunSettings {
    /// Parses `provider_settings`; unknown keys are rejected so a typo never
    /// silently deploys a public or unboosted service. Endpoints are not settable.
    ///
    /// @throws `DeployError::invalid` on an unknown key or a bad value.
    pub fn parse(settings: &BTreeMap<String, Value>) -> Result<Self, DeployError> {
        let mut parsed = Self {
            service_account: None,
            ingress: "INGRESS_TRAFFIC_ALL",
            startup_cpu_boost: true,
            execution_environment: None,
        };
        for (key, value) in settings {
            match key.as_str() {
                "service_account" => {
                    let email =
                        value.as_str().filter(|email| email.contains('@') && !email.contains(char::is_whitespace));
                    parsed.service_account = Some(
                        email.ok_or_else(|| DeployError::invalid("service_account must be an e-mail"))?.to_owned(),
                    );
                }
                "ingress" => {
                    parsed.ingress = match value.as_str() {
                        Some("all") => "INGRESS_TRAFFIC_ALL",
                        Some("internal") => "INGRESS_TRAFFIC_INTERNAL_ONLY",
                        Some("internal-and-cloud-load-balancing") => "INGRESS_TRAFFIC_INTERNAL_LOAD_BALANCER",
                        _ => {
                            return Err(DeployError::invalid(
                                "ingress must be all, internal or internal-and-cloud-load-balancing",
                            ));
                        }
                    }
                }
                "startup_cpu_boost" => {
                    parsed.startup_cpu_boost =
                        value.as_bool().ok_or_else(|| DeployError::invalid("startup_cpu_boost must be a boolean"))?;
                }
                "execution_environment" => {
                    parsed.execution_environment = Some(match value.as_str() {
                        Some("gen1") => "EXECUTION_ENVIRONMENT_GEN1",
                        Some("gen2") => "EXECUTION_ENVIRONMENT_GEN2",
                        _ => return Err(DeployError::invalid("execution_environment must be gen1 or gen2")),
                    });
                }
                other => return Err(DeployError::invalid(format!("unknown Cloud Run setting `{other}`"))),
            }
        }
        Ok(parsed)
    }
}

/// Checks the parts of a spec Cloud Run constrains.
///
/// @throws `DeployError::invalid` with the first problem found.
pub fn validate_spec(spec: &DeploymentSpec) -> Result<(), DeployError> {
    if spec.image.as_deref().is_none_or(str::is_empty) {
        return Err(DeployError::invalid("Cloud Run needs a container image"));
    }
    if spec.max_active_runs == 0 || spec.max_active_runs > 1_000 {
        return Err(DeployError::invalid("max_active_runs must be within 1..=1000 (Cloud Run concurrency)"));
    }
    if spec.max_replicas == 0 || spec.min_replicas > spec.max_replicas {
        return Err(DeployError::invalid("need 0 <= min_replicas <= max_replicas and max_replicas >= 1"));
    }
    let cpu = spec.resources.cpu_millis;
    if cpu != 0 && cpu < 1_000 && spec.max_active_runs > 1 {
        return Err(DeployError::invalid("Cloud Run allows less than 1 vCPU only with max_active_runs = 1"));
    }
    if cpu != 0 && cpu > 1_000 && !cpu.is_multiple_of(1_000) {
        return Err(DeployError::invalid("above 1 vCPU, Cloud Run takes whole vCPUs (2000, 4000, 8000 millicores)"));
    }
    for name in spec.env.keys().chain(spec.secrets.iter().map(|secret| &secret.env)) {
        if !is_valid_env_name(name) {
            return Err(DeployError::invalid(format!("invalid environment variable name `{name}`")));
        }
    }
    for secret in &spec.secrets {
        if spec.env.contains_key(&secret.env) {
            return Err(DeployError::invalid(format!("`{}` is both a plain env var and a secret", secret.env)));
        }
        parse_secret_reference(&secret.reference)?;
    }
    Ok(())
}

fn is_valid_env_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    let starts_well = bytes.next().is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_');
    starts_well
        && name.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        && !RESERVED_ENV.contains(&name)
        && !name.starts_with("X_GOOGLE_")
}

/// Splits `projects/{p}/secrets/{s}[/versions/{v}]` into `(secret, version)`;
/// the version defaults to `latest`.
///
/// @throws `DeployError::invalid` for anything else (a plaintext value, a URL...).
pub fn parse_secret_reference(reference: &str) -> Result<(String, String), DeployError> {
    let invalid = || {
        DeployError::invalid("Cloud Run secrets must reference Secret Manager: projects/<p>/secrets/<s>[/versions/<v>]")
    };
    let parts: Vec<&str> = reference.split('/').collect();
    match parts.as_slice() {
        ["projects", project, "secrets", secret] if [*project, *secret].into_iter().all(is_valid_segment) => {
            Ok((format!("projects/{project}/secrets/{secret}"), "latest".to_owned()))
        }
        ["projects", project, "secrets", secret, "versions", version]
            if [*project, *secret, *version].into_iter().all(is_valid_segment) =>
        {
            Ok((format!("projects/{project}/secrets/{secret}"), (*version).to_owned()))
        }
        _ => Err(invalid()),
    }
}

/// A resource-name segment: starts alphanumeric, then `[A-Za-z0-9._-]`.
fn is_valid_segment(segment: &str) -> bool {
    segment.bytes().next().is_some_and(|byte| byte.is_ascii_alphanumeric())
        && segment.bytes().all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte))
}

/// The full `Service` body for `PATCH ...?allowMissing=true`.
///
/// @param existing - The service as it is now (for keeping older version tags)
pub fn render_service(
    config: &CloudRunConfig,
    spec: &DeploymentSpec,
    names: &ServiceNames,
    settings: &CloudRunSettings,
    existing: Option<&Value>,
) -> Result<Value, DeployError> {
    validate_spec(spec)?;
    let mut template = json!({
        "revision": names.revision,
        "labels": {
            "managed-by": "graphrt",
            "graphrt-deployment": label_value(&spec.name),
            "graphrt-version": names.tag,
            "graphrt-fingerprint": label_value(&spec.workflow_fingerprint),
        },
        "scaling": { "minInstanceCount": spec.min_replicas, "maxInstanceCount": spec.max_replicas },
        "timeout": format!("{}s", config.request_timeout_secs),
        "maxInstanceRequestConcurrency": spec.max_active_runs,
        "containers": [render_container(spec, settings)?],
    });
    if let Some(object) = template.as_object_mut() {
        if let Some(account) = &settings.service_account {
            object.insert("serviceAccount".to_owned(), Value::String(account.clone()));
        }
        if let Some(environment) = settings.execution_environment {
            object.insert("executionEnvironment".to_owned(), Value::String(environment.to_owned()));
        }
    }
    Ok(json!({
        "labels": { "managed-by": "graphrt", "graphrt-deployment": label_value(&spec.name) },
        "ingress": settings.ingress,
        "customAudiences": [names.audience],
        "template": template,
        "traffic": render_traffic(&names.tag, existing, &[]),
    }))
}

fn render_container(spec: &DeploymentSpec, settings: &CloudRunSettings) -> Result<Value, DeployError> {
    let mut env: Vec<Value> = spec.env.iter().map(|(name, value)| json!({ "name": name, "value": value })).collect();
    for SecretRef { env: name, reference } in &spec.secrets {
        let (secret, version) = parse_secret_reference(reference)?;
        env.push(json!({ "name": name, "valueSource": { "secretKeyRef": { "secret": secret, "version": version } } }));
    }
    let mut limits = Map::new();
    let cpu = spec.resources.cpu_millis;
    if cpu != 0 {
        let formatted = if cpu.is_multiple_of(1_000) { (cpu / 1_000).to_string() } else { format!("{cpu}m") };
        limits.insert("cpu".to_owned(), Value::String(formatted));
    }
    if spec.resources.memory_mib != 0 {
        limits.insert("memory".to_owned(), Value::String(format!("{}Mi", spec.resources.memory_mib)));
    }
    let mut container = json!({
        "name": "agent",
        "image": spec.image,
        "ports": [{ "name": "http1", "containerPort": CONTAINER_PORT }],
        "env": env,
        "resources": { "limits": limits, "cpuIdle": true, "startupCpuBoost": settings.startup_cpu_boost },
    });
    if let (Some((program, arguments)), Some(object)) = (spec.command.split_first(), container.as_object_mut()) {
        object.insert("command".to_owned(), json!([program]));
        object.insert("args".to_owned(), json!(arguments));
    }
    Ok(container)
}

/// New traffic: 100 % to the latest revision under `new_tag`; every other tag
/// already on the service is kept at 0 % (the previous latest is pinned to its
/// revision), except tags listed in `retired`.
pub fn render_traffic(new_tag: &str, existing: Option<&Value>, retired: &[String]) -> Value {
    let mut traffic = vec![json!({ "type": "TRAFFIC_TARGET_ALLOCATION_TYPE_LATEST", "percent": 100, "tag": new_tag })];
    let Some(service) = existing else { return Value::Array(traffic) };
    let latest_ready = service.get("latestReadyRevision").and_then(Value::as_str).map(short_revision_name);
    for target in service.get("traffic").and_then(Value::as_array).into_iter().flatten() {
        let Some(tag) = target.get("tag").and_then(Value::as_str).filter(|tag| !tag.is_empty()) else { continue };
        if tag == new_tag || retired.iter().any(|retired_tag| retired_tag == tag) {
            continue;
        }
        let revision = match target.get("type").and_then(Value::as_str) {
            Some("TRAFFIC_TARGET_ALLOCATION_TYPE_LATEST") => latest_ready.clone(),
            _ => target.get("revision").and_then(Value::as_str).map(short_revision_name),
        };
        if let Some(revision) = revision {
            traffic.push(json!({
                "type": "TRAFFIC_TARGET_ALLOCATION_TYPE_REVISION",
                "revision": revision,
                "percent": 0,
                "tag": tag,
            }));
        }
    }
    Value::Array(traffic)
}

/// `projects/p/locations/r/services/s/revisions/x` → `x` (traffic takes short names).
fn short_revision_name(revision: &str) -> String {
    revision.rsplit('/').next().unwrap_or(revision).to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_references_only() {
        assert_eq!(
            parse_secret_reference("projects/p/secrets/openai-key").unwrap(),
            ("projects/p/secrets/openai-key".to_owned(), "latest".to_owned())
        );
        assert_eq!(parse_secret_reference("projects/p/secrets/k/versions/3").unwrap().1, "3");
        for bad in ["sk-live-123", "https://evil/secret", "projects/p/secrets/../x", "projects/p/secrets/"] {
            assert!(parse_secret_reference(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn previous_latest_is_pinned_and_retired_tags_dropped() {
        let existing = json!({
            "latestReadyRevision": "projects/p/locations/r/services/s/revisions/s-v2",
            "traffic": [
                { "type": "TRAFFIC_TARGET_ALLOCATION_TYPE_LATEST", "percent": 100, "tag": "v2" },
                { "type": "TRAFFIC_TARGET_ALLOCATION_TYPE_REVISION", "revision": "s-v1", "percent": 0, "tag": "v1" },
                { "type": "TRAFFIC_TARGET_ALLOCATION_TYPE_REVISION", "revision": "s-v0", "percent": 0, "tag": "v0" },
            ],
        });
        let traffic = render_traffic("v3", Some(&existing), &["v0".to_owned()]);
        assert_eq!(
            traffic,
            json!([
                { "type": "TRAFFIC_TARGET_ALLOCATION_TYPE_LATEST", "percent": 100, "tag": "v3" },
                { "type": "TRAFFIC_TARGET_ALLOCATION_TYPE_REVISION", "revision": "s-v2", "percent": 0, "tag": "v2" },
                { "type": "TRAFFIC_TARGET_ALLOCATION_TYPE_REVISION", "revision": "s-v1", "percent": 0, "tag": "v1" },
            ])
        );
    }

    #[test]
    fn settings_are_strict() {
        let mut settings = BTreeMap::new();
        settings.insert("ingress".to_owned(), json!("internal"));
        assert_eq!(CloudRunSettings::parse(&settings).unwrap().ingress, "INGRESS_TRAFFIC_INTERNAL_ONLY");
        settings.insert("admin_endpoint".to_owned(), json!("http://169.254.169.254"));
        assert!(CloudRunSettings::parse(&settings).is_err());
    }
}
