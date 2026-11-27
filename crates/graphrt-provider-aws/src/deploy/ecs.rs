//! `deploy-ecs`: registers one immutable task definition revision per deployment version.
//!
//! No ECS service is created: the gateway is the scheduler (`RunTask` per
//! replica), so a version is just a task definition revision. The returned
//! `DeploymentRef::target` is its ARN, which [`crate::ecs::EcsProvider`] starts.
//!
//! Provider settings (`DeploymentSpec::provider_settings`):
//!
//! | Key | Required | Meaning |
//! | --- | --- | --- |
//! | `execution_role_arn` | yes | pulls the image, reads `secrets` from Secrets Manager / SSM, writes logs |
//! | `task_role_arn` | no | role the agent runs as |
//! | `family` | no | task definition family (default `graphrt-<name>`) |
//! | `container_port` | no | agent port (default 8080) |
//! | `cpu_architecture` | no | `X86_64` (default) or `ARM64` |
//! | `log_group` | no | CloudWatch Logs group (`awslogs` driver, stream prefix `graphrt`) |

use std::fmt;
use std::sync::Arc;

use serde_json::{Value, json};

use super::{Settings, is_env_name, validate_secret_refs};
use crate::api::{BoxFuture, DeployDriver, DeployError, DeploymentRef, DeploymentSpec, ProviderError};
use crate::auth::CredentialChain;
use crate::ecs::{FARGATE_MAX_STOP_TIMEOUT_SECS, validate_stop_timeout};
use crate::rest::{ApiLimits, AwsRestClient, ServiceEndpoint};

const TARGET: &str = "AmazonEC2ContainerServiceV20141113.RegisterTaskDefinition";
const CONTAINER_NAME: &str = "agent";
const DEFAULT_PORT: u64 = 8080;

/// Configuration of the ECS deploy driver.
#[derive(Clone, Debug)]
pub struct EcsDeployConfig {
    pub endpoint: ServiceEndpoint,
    pub limits: ApiLimits,
}

impl EcsDeployConfig {
    /// Public ECS endpoint of `region`.
    pub fn new(region: &str) -> Self {
        Self { endpoint: ServiceEndpoint::regional("ecs", region), limits: ApiLimits::default() }
    }
}

/// Registers task definitions (`graphrt deploy --target ecs-fargate`).
pub struct EcsDeployDriver {
    client: AwsRestClient,
}

impl fmt::Debug for EcsDeployDriver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EcsDeployDriver").field("client", &self.client).finish()
    }
}

/// Fargate CPU units (1024 = 1 vCPU) and the memory range (MiB) each allows.
const FARGATE_SIZES: &[(u32, u32, u32)] = &[
    (256, 512, 2048),
    (512, 1024, 4096),
    (1024, 2048, 8192),
    (2048, 4096, 16384),
    (4096, 8192, 30720),
    (8192, 16384, 61440),
    (16384, 32768, 122880),
];

/// Smallest valid Fargate size for the requested millicores / MiB (0 = default:
/// 0.5 vCPU, the smallest memory of the size).
///
/// Memory is rounded up to what the size accepts (512 / 1024 / 2048 MiB at 0.25
/// vCPU, whole GiB above), and CPU grows until the memory fits.
///
/// @throws `DeployError::invalid` when the request exceeds the largest Fargate size.
pub fn fargate_size(cpu_millis: u32, memory_mib: u32) -> Result<(u32, u32), DeployError> {
    let wanted_units = if cpu_millis == 0 { 512 } else { (u64::from(cpu_millis) * 1024).div_ceil(1000) };
    for &(units, min_memory, max_memory) in
        FARGATE_SIZES.iter().filter(|(units, _, _)| u64::from(*units) >= wanted_units)
    {
        let memory = memory_mib.max(min_memory);
        let memory = if units == 256 { memory.next_power_of_two() } else { memory.div_ceil(1024) * 1024 };
        if memory <= max_memory {
            return Ok((units, memory));
        }
    }
    Err(DeployError::invalid(format!("no Fargate size fits {cpu_millis} millicores and {memory_mib} MiB")))
}

fn is_family(name: &str) -> bool {
    !name.is_empty() && name.len() <= 255 && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// The `RegisterTaskDefinition` request for `spec`.
///
/// @throws `DeployError::invalid` when the spec cannot run on Fargate.
pub fn task_definition_request(spec: &DeploymentSpec, region: &str) -> Result<Value, DeployError> {
    let settings = Settings(&spec.provider_settings);
    let image =
        spec.image.as_deref().filter(|i| !i.is_empty()).ok_or_else(|| DeployError::invalid("ECS needs an image"))?;
    let grace = if spec.grace_secs == 0 { FARGATE_MAX_STOP_TIMEOUT_SECS } else { spec.grace_secs };
    validate_stop_timeout(grace, grace.saturating_sub(2)).map_err(|e| DeployError::invalid(e.message))?;
    validate_secret_refs(&spec.secrets)?;
    if let Some(key) = spec.env.keys().find(|k| !is_env_name(k)) {
        return Err(DeployError::invalid(format!("env name {key:?} is not a valid variable name")));
    }
    let family = settings.string("family")?.map_or_else(|| format!("graphrt-{}", spec.name), str::to_owned);
    if !is_family(&family) {
        return Err(DeployError::invalid(format!(
            "task definition family {family:?} must match [A-Za-z0-9_-]{{1,255}}"
        )));
    }
    let port = settings.u64("container_port")?.unwrap_or(DEFAULT_PORT);
    if !(1..=65535).contains(&port) {
        return Err(DeployError::invalid(format!("container_port {port} out of range")));
    }
    let architecture = settings.string("cpu_architecture")?.unwrap_or("X86_64");
    if !matches!(architecture, "X86_64" | "ARM64") {
        return Err(DeployError::invalid("cpu_architecture must be X86_64 or ARM64"));
    }
    let (cpu, memory) = fargate_size(spec.resources.cpu_millis, spec.resources.memory_mib)?;
    let environment: Vec<Value> = spec.env.iter().map(|(k, v)| json!({ "name": k, "value": v })).collect();
    let secrets: Vec<Value> = spec.secrets.iter().map(|s| json!({ "name": s.env, "valueFrom": s.reference })).collect();
    let mut container = json!({
        "name": CONTAINER_NAME,
        "image": image,
        "essential": true,
        "portMappings": [{ "containerPort": port, "protocol": "tcp" }],
        "environment": environment,
        "secrets": secrets,
        "stopTimeout": grace,
    });
    // `spec.command` is the whole argv (`graphrt serve ...`), as on Kubernetes and
    // Cloud Run: the program replaces the image's ENTRYPOINT. ECS `command` alone
    // only replaces CMD, so the graphrt image would run `graphrt graphrt serve`.
    if let Some((program, arguments)) = spec.command.split_first()
        && let Some(object) = container.as_object_mut()
    {
        object.insert("entryPoint".into(), json!([program]));
        object.insert("command".into(), json!(arguments));
    }
    if let Some(group) = settings.string("log_group")?
        && let Some(object) = container.as_object_mut()
    {
        object.insert(
            "logConfiguration".into(),
            json!({ "logDriver": "awslogs", "options": {
                "awslogs-group": group, "awslogs-region": region, "awslogs-stream-prefix": "graphrt" } }),
        );
    }
    let mut request = json!({
        "family": family,
        "networkMode": "awsvpc",
        "requiresCompatibilities": ["FARGATE"],
        "cpu": cpu.to_string(),
        "memory": memory.to_string(),
        "executionRoleArn": settings.required("execution_role_arn")?,
        "runtimePlatform": { "cpuArchitecture": architecture, "operatingSystemFamily": "LINUX" },
        "containerDefinitions": [container],
        "tags": [
            { "key": "graphrt:deployment", "value": spec.name },
            { "key": "graphrt:version", "value": spec.version },
            { "key": "graphrt:workflow", "value": spec.workflow_fingerprint },
        ],
    });
    if let Some(role) = settings.string("task_role_arn")?
        && let Some(object) = request.as_object_mut()
    {
        object.insert("taskRoleArn".into(), json!(role));
    }
    Ok(request)
}

impl EcsDeployDriver {
    /// A driver signing with deploy-time credentials from `chain`.
    ///
    /// @throws `ProviderError::fatal` when the HTTP client cannot be built.
    pub fn new(chain: Arc<CredentialChain>, config: EcsDeployConfig) -> Result<Self, ProviderError> {
        Ok(Self { client: AwsRestClient::new(chain, config.endpoint, config.limits)? })
    }

    async fn register(&self, spec: &DeploymentSpec) -> Result<DeploymentRef, DeployError> {
        let request = task_definition_request(spec, &self.client.endpoint().region)?;
        let response = self.client.json_11(TARGET, &request).await?;
        let arn = response
            .get("taskDefinition")
            .and_then(|t| t.get("taskDefinitionArn"))
            .and_then(Value::as_str)
            .ok_or_else(|| DeployError::from(ProviderError::transient("RegisterTaskDefinition returned no ARN")))?;
        tracing::info!(deployment = %spec.name, version = %spec.version, task_definition = arn, "ecs task definition registered");
        Ok(DeploymentRef::new(spec.name.clone(), spec.version.clone(), arn))
    }
}

impl DeployDriver for EcsDeployDriver {
    fn apply<'a>(&'a self, spec: &'a DeploymentSpec) -> BoxFuture<'a, Result<DeploymentRef, DeployError>> {
        Box::pin(self.register(spec))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fargate_sizes() {
        assert_eq!(fargate_size(0, 0).unwrap(), (512, 1024));
        assert_eq!(fargate_size(250, 512).unwrap(), (256, 512));
        assert_eq!(fargate_size(250, 1500).unwrap(), (256, 2048));
        assert_eq!(fargate_size(250, 3000).unwrap(), (512, 3072));
        assert_eq!(fargate_size(1000, 0).unwrap(), (1024, 2048));
        assert_eq!(fargate_size(1000, 3000).unwrap(), (1024, 3072));
        assert_eq!(fargate_size(500, 9000).unwrap(), (2048, 9216));
        assert_eq!(fargate_size(4000, 30720).unwrap(), (4096, 30720));
        assert!(fargate_size(16_000, 200_000).is_err());
    }

    #[test]
    fn family_names() {
        assert!(is_family("graphrt-chat_1"));
        assert!(!is_family("graphrt chat") && !is_family(""));
    }
}
