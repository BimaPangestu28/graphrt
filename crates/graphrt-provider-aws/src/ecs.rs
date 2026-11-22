//! `provider-ecs`: ECS Fargate with the gateway as the scheduler.
//!
//! Each replica is one Fargate task started with `RunTask` and stopped with
//! `StopTask`; its private IP comes from `DescribeTasks` (never from the agent).
//! Tasks are tagged with `startedBy = gr-<hash(deployment, version)>` so
//! `ListTasks` finds every replica of a version, including orphans left by a
//! gateway that was killed (conformance C12).
//!
//! Fargate wakes take tens of seconds (image pull + ENI attach, ≈ 20–60 s), so
//! this provider suits async, webhook and cron runs (cron prewarms) rather than
//! interactive chat. Fargate's `stopTimeout` is at most 120 s: the agent's drain
//! deadline must fit inside it ([`validate_stop_timeout`]).

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;

use crate::api::{
    Addressing, BoxFuture, Capabilities, DeploymentRef, Endpoint, EndpointAddress, Provider, ProviderError,
    ProviderErrorKind, ReleaseMode, ReplicaPhase, ReplicaRef, ReplicaState, Scaler,
};
use crate::auth::CredentialChain;
use crate::auth::sigv4::hex;
use crate::rest::{ApiLimits, AwsRestClient, ServiceEndpoint};

const TARGET_PREFIX: &str = "AmazonEC2ContainerServiceV20141113.";
/// Fargate's hard limit for `stopTimeout` (SIGTERM → SIGKILL).
pub const FARGATE_MAX_STOP_TIMEOUT_SECS: u64 = 120;
/// `RunTask` starts at most this many tasks per call.
const RUN_TASK_MAX_COUNT: u32 = 10;
/// `DescribeTasks` accepts at most this many ARNs per call.
const DESCRIBE_MAX_TASKS: usize = 100;

/// How to start the tasks of one deployment (operator configuration, keyed by deployment name).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EcsTaskTemplate {
    pub cluster: String,
    /// Task definition family. The task definition started is `DeploymentRef::target` when set
    /// (an ARN or `family:revision` written by `graphrt deploy`), else `family:version`.
    pub task_definition_family: String,
    pub subnets: Vec<String>,
    pub security_groups: Vec<String>,
    pub assign_public_ip: bool,
    /// Port the agent listens on inside the task.
    pub container_port: u16,
    /// `FARGATE_SPOT` or another capacity provider; `None` uses `launchType: FARGATE`.
    pub capacity_provider: Option<String>,
    pub platform_version: Option<String>,
}

/// Configuration of the ECS provider.
#[derive(Clone, Debug)]
pub struct EcsConfig {
    pub endpoint: ServiceEndpoint,
    pub limits: ApiLimits,
    /// Deployment name → task template.
    pub deployments: HashMap<String, EcsTaskTemplate>,
    /// How long `wake` waits for tasks to reach RUNNING with an IP.
    pub wake_timeout: Duration,
    /// `DescribeTasks` polling interval while waking.
    pub poll_interval: Duration,
}

impl EcsConfig {
    /// Defaults for `region`: public endpoint, 3 min wake timeout, 1 s polling.
    pub fn new(region: &str) -> Self {
        Self {
            endpoint: ServiceEndpoint::regional("ecs", region),
            limits: ApiLimits::default(),
            deployments: HashMap::new(),
            wake_timeout: Duration::from_secs(180),
            poll_interval: Duration::from_secs(1),
        }
    }
}

/// Reject a task definition whose drain cannot fit in Fargate's grace period.
///
/// `stop_timeout_secs` is the task definition's `stopTimeout`; `drain_timeout_secs`
/// the agent's `--drain-timeout`, which must leave 2 s for the process to exit.
///
/// @throws `ProviderError::Fatal` when either value does not fit.
pub fn validate_stop_timeout(stop_timeout_secs: u64, drain_timeout_secs: u64) -> Result<(), ProviderError> {
    if stop_timeout_secs > FARGATE_MAX_STOP_TIMEOUT_SECS {
        return Err(ProviderError::fatal(format!(
            "ECS Fargate stopTimeout is at most {FARGATE_MAX_STOP_TIMEOUT_SECS} s (got {stop_timeout_secs})"
        )));
    }
    if drain_timeout_secs + 2 > stop_timeout_secs {
        return Err(ProviderError::fatal(format!(
            "agent --drain-timeout {drain_timeout_secs} s must be at most stopTimeout − 2 s ({} s)",
            stop_timeout_secs.saturating_sub(2)
        )));
    }
    Ok(())
}

/// The ECS Fargate provider.
pub struct EcsProvider {
    client: AwsRestClient,
    config: EcsConfig,
    /// One wake at a time per deployment version (singleflight, conformance C1).
    wake_locks: Mutex<HashMap<DeploymentRef, Arc<Mutex<()>>>>,
    /// Distinguishes the `RunTask` client tokens of this process from a restarted one.
    instance_id: u128,
    wake_nonce: AtomicU64,
}

impl fmt::Debug for EcsProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EcsProvider").field("config", &self.config).finish_non_exhaustive()
    }
}

/// `startedBy` tag of a deployment version (≤ 36 characters, as ECS requires).
pub fn started_by(deployment: &DeploymentRef) -> String {
    let digest =
        Sha256::digest(format!("{}\0{}\0{}", deployment.name, deployment.version, deployment.target).as_bytes());
    let hash = hex(&digest);
    format!("gr-{}", hash.get(..32).unwrap_or(&hash))
}

fn task_ip(task: &Value) -> Option<String> {
    let from_attachment = task.get("attachments").and_then(Value::as_array).and_then(|attachments| {
        attachments
            .iter()
            .filter(|a| a.get("type").and_then(Value::as_str) == Some("ElasticNetworkInterface"))
            .flat_map(|a| a.get("details").and_then(Value::as_array).into_iter().flatten())
            .find(|d| d.get("name").and_then(Value::as_str) == Some("privateIPv4Address"))
            .and_then(|d| d.get("value").and_then(Value::as_str))
            .map(str::to_owned)
    });
    from_attachment.or_else(|| {
        let containers = task.get("containers")?.as_array()?;
        containers.iter().find_map(|c| {
            let nic = c.get("networkInterfaces")?.as_array()?.first()?;
            nic.get("privateIpv4Address").or_else(|| nic.get("ipv6Address"))?.as_str().map(str::to_owned)
        })
    })
}

fn task_phase(task: &Value) -> ReplicaPhase {
    let last = task.get("lastStatus").and_then(Value::as_str).unwrap_or_default();
    let desired = task.get("desiredStatus").and_then(Value::as_str).unwrap_or_default();
    match last {
        "STOPPED" | "DELETED" => ReplicaPhase::Stopped,
        "DEACTIVATING" | "STOPPING" | "DEPROVISIONING" => ReplicaPhase::Stopping,
        _ if desired == "STOPPED" => ReplicaPhase::Stopping,
        "RUNNING" => ReplicaPhase::Ready,
        _ => ReplicaPhase::Starting,
    }
}

/// A replica as reported by ECS, plus why it stopped.
#[derive(Debug)]
struct TaskView {
    state: ReplicaState,
    stopped_reason: Option<String>,
}

/// Insert `key` into a JSON object body (no-op on non-objects).
fn set_field(body: &mut Value, key: &str, value: Value) {
    if let Some(object) = body.as_object_mut() {
        object.insert(key.to_owned(), value);
    }
}

fn is_capacity_reason(reason: &str) -> bool {
    let reason = reason.to_ascii_lowercase();
    reason.contains("capacity") || reason.starts_with("resource:")
}

impl EcsProvider {
    /// A provider signing with `chain`.
    ///
    /// @throws `ProviderError::Fatal` when the HTTP client cannot be built.
    pub fn new(chain: Arc<CredentialChain>, config: EcsConfig) -> Result<Self, ProviderError> {
        let client = AwsRestClient::new(chain, config.endpoint.clone(), config.limits)?;
        let instance_id = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos());
        Ok(Self { client, config, wake_locks: Mutex::new(HashMap::new()), instance_id, wake_nonce: AtomicU64::new(0) })
    }

    fn template(&self, deployment: &DeploymentRef) -> Result<&EcsTaskTemplate, ProviderError> {
        self.config
            .deployments
            .get(&deployment.name)
            .ok_or_else(|| ProviderError::fatal(format!("no ECS task template for deployment {}", deployment.name)))
    }

    fn task_definition(template: &EcsTaskTemplate, deployment: &DeploymentRef) -> String {
        if !deployment.target.is_empty() {
            deployment.target.clone()
        } else if deployment.version.is_empty() {
            template.task_definition_family.clone()
        } else {
            format!("{}:{}", template.task_definition_family, deployment.version)
        }
    }

    fn endpoint_url(template: &EcsTaskTemplate, ip: &str) -> String {
        if ip.contains(':') {
            format!("http://[{ip}]:{}", template.container_port)
        } else {
            format!("http://{ip}:{}", template.container_port)
        }
    }

    fn task_view(deployment: &DeploymentRef, template: &EcsTaskTemplate, task: &Value) -> Option<TaskView> {
        let arn = task.get("taskArn")?.as_str()?.to_owned();
        let phase = task_phase(task);
        let url =
            (phase == ReplicaPhase::Ready).then(|| task_ip(task)).flatten().map(|ip| Self::endpoint_url(template, &ip));
        let stopped_reason = task.get("stoppedReason").and_then(Value::as_str).map(str::to_owned);
        // `startedAt` is epoch seconds with a fraction.
        let started_at_ms = task.get("startedAt").and_then(Value::as_f64).map(|secs| (secs * 1000.0) as u64);
        Some(TaskView {
            state: ReplicaState {
                replica: ReplicaRef { deployment: deployment.clone(), id: arn },
                phase,
                address: url.map(EndpointAddress::Http),
                started_at_ms,
            },
            stopped_reason,
        })
    }

    async fn call(&self, operation: &str, body: Value) -> Result<Value, ProviderError> {
        self.client.json_11(&format!("{TARGET_PREFIX}{operation}"), &body).await
    }

    /// ARNs of tasks with desired status RUNNING for `deployment`.
    async fn list_task_arns(&self, deployment: &DeploymentRef, cluster: &str) -> Result<Vec<String>, ProviderError> {
        let started_by = started_by(deployment);
        let mut arns = Vec::new();
        let mut next_token: Option<String> = None;
        loop {
            let mut body =
                json!({ "cluster": cluster, "startedBy": started_by, "desiredStatus": "RUNNING", "maxResults": 100 });
            if let Some(token) = &next_token {
                set_field(&mut body, "nextToken", json!(token));
            }
            let page = self.call("ListTasks", body).await?;
            arns.extend(
                page.get("taskArns")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(str::to_owned),
            );
            next_token = page.get("nextToken").and_then(Value::as_str).map(str::to_owned);
            if next_token.is_none() {
                return Ok(arns);
            }
        }
    }

    async fn describe(&self, cluster: &str, arns: &[String]) -> Result<Vec<Value>, ProviderError> {
        let mut tasks = Vec::with_capacity(arns.len());
        for chunk in arns.chunks(DESCRIBE_MAX_TASKS) {
            let page = self.call("DescribeTasks", json!({ "cluster": cluster, "tasks": chunk })).await?;
            tasks.extend(page.get("tasks").and_then(Value::as_array).into_iter().flatten().cloned());
        }
        Ok(tasks)
    }

    fn run_task_body(
        &self,
        template: &EcsTaskTemplate,
        deployment: &DeploymentRef,
        count: u32,
        client_token: &str,
    ) -> Value {
        let mut body = json!({
            "cluster": template.cluster,
            "taskDefinition": Self::task_definition(template, deployment),
            "count": count,
            "startedBy": started_by(deployment),
            "group": format!("graphrt:{}", deployment.name),
            "clientToken": client_token,
            "enableECSManagedTags": true,
            "tags": [
                { "key": "graphrt:deployment", "value": deployment.name },
                { "key": "graphrt:version", "value": deployment.version },
            ],
            "networkConfiguration": { "awsvpcConfiguration": {
                "subnets": template.subnets,
                "securityGroups": template.security_groups,
                "assignPublicIp": if template.assign_public_ip { "ENABLED" } else { "DISABLED" },
            }},
        });
        match &template.capacity_provider {
            Some(provider) => {
                set_field(&mut body, "capacityProviderStrategy", json!([{ "capacityProvider": provider, "weight": 1 }]))
            }
            None => set_field(&mut body, "launchType", json!("FARGATE")),
        }
        if let Some(version) = &template.platform_version {
            set_field(&mut body, "platformVersion", json!(version));
        }
        body
    }

    /// Start `count` tasks; returns the new ARNs.
    ///
    /// Each call gets a fresh client token; the HTTP-level retries of one
    /// `RunTask` resend the same body, so a lost response never doubles the tasks.
    async fn run_tasks(
        &self,
        template: &EcsTaskTemplate,
        deployment: &DeploymentRef,
        count: u32,
    ) -> Result<Vec<String>, ProviderError> {
        let nonce = self.wake_nonce.fetch_add(1, Ordering::Relaxed);
        let mut launched = Vec::new();
        let mut failure: Option<String> = None;
        let mut remaining = count;
        let mut chunk_index = 0u32;
        while remaining > 0 {
            let batch = remaining.min(RUN_TASK_MAX_COUNT);
            let seed = format!("{}|{}|{nonce}|{chunk_index}", started_by(deployment), self.instance_id);
            let client_token = hex(&Sha256::digest(seed.as_bytes()));
            let response = self.call("RunTask", self.run_task_body(template, deployment, batch, &client_token)).await;
            let response = response.map_err(|e| match e {
                error if is_capacity_reason(&error.message) => ProviderError::capacity(error.message),
                other => other,
            })?;
            launched.extend(
                response
                    .get("tasks")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|t| t.get("taskArn").and_then(Value::as_str))
                    .map(str::to_owned),
            );
            if let Some(reason) = response
                .get("failures")
                .and_then(Value::as_array)
                .and_then(|f| f.first())
                .and_then(|f| f.get("reason").and_then(Value::as_str))
            {
                failure = Some(reason.to_owned());
            }
            remaining -= batch;
            chunk_index += 1;
        }
        match failure {
            Some(reason) if launched.is_empty() && is_capacity_reason(&reason) => Err(ProviderError::capacity(reason)),
            Some(reason) if launched.is_empty() => Err(ProviderError::transient(format!("RunTask failed: {reason}"))),
            Some(reason) => {
                tracing::warn!(deployment = %deployment.name, %reason, launched = launched.len(), "RunTask partially failed");
                Ok(launched)
            }
            None => Ok(launched),
        }
    }

    async fn wake_locked(&self, deployment: &DeploymentRef, replicas: u32) -> Result<Vec<Endpoint>, ProviderError> {
        let template = self.template(deployment)?;
        let started = Instant::now();
        let listed = self.list_task_arns(deployment, &template.cluster).await?;
        let states = self.describe(&template.cluster, &listed).await?;
        let mut live: Vec<String> = states
            .iter()
            .filter(|t| matches!(task_phase(t), ReplicaPhase::Starting | ReplicaPhase::Ready))
            .filter_map(|t| t.get("taskArn").and_then(Value::as_str).map(str::to_owned))
            .collect();
        let missing = replicas.saturating_sub(u32::try_from(live.len()).unwrap_or(u32::MAX));
        if missing > 0 {
            let launched = self.run_tasks(template, deployment, missing).await?;
            tracing::info!(deployment = %deployment.name, launched = launched.len(), "ecs tasks started");
            live.extend(launched);
        }
        loop {
            let tasks = self.describe(&template.cluster, &live).await?;
            let views: Vec<TaskView> = tasks.iter().filter_map(|t| Self::task_view(deployment, template, t)).collect();
            let ready: Vec<Endpoint> = views
                .iter()
                .filter_map(|v| {
                    v.state.address.clone().map(|address| Endpoint { replica: v.state.replica.clone(), address })
                })
                .collect();
            if u32::try_from(ready.len()).unwrap_or(u32::MAX) >= replicas {
                tracing::info!(deployment = %deployment.name, wake_ms = started.elapsed().as_millis() as u64, "ecs wake complete");
                return Ok(ready);
            }
            let starting = views
                .iter()
                .filter(|v| {
                    v.state.address.is_none() && matches!(v.state.phase, ReplicaPhase::Starting | ReplicaPhase::Ready)
                })
                .count();
            let stopped =
                views.iter().find(|v| matches!(v.state.phase, ReplicaPhase::Stopped | ReplicaPhase::Stopping));
            if let Some(stopped) =
                stopped.filter(|_| ready.len() + starting < usize::try_from(replicas).unwrap_or(usize::MAX))
            {
                let reason = stopped.stopped_reason.clone().unwrap_or_else(|| "task stopped while starting".into());
                return Err(if is_capacity_reason(&reason) {
                    ProviderError::capacity(reason)
                } else {
                    ProviderError::transient(format!(
                        "task {} stopped before RUNNING: {reason}",
                        stopped.state.replica.id
                    ))
                });
            }
            if started.elapsed() >= self.config.wake_timeout {
                return Err(ProviderError::transient(format!(
                    "{} of {replicas} tasks RUNNING after {} s",
                    ready.len(),
                    self.config.wake_timeout.as_secs()
                )));
            }
            tokio::time::sleep(self.config.poll_interval).await;
        }
    }
}

impl EcsProvider {
    async fn wake_singleflight(
        &self,
        deployment: &DeploymentRef,
        replicas: u32,
    ) -> Result<Vec<Endpoint>, ProviderError> {
        if replicas == 0 {
            return Ok(Vec::new());
        }
        let lock = {
            let mut locks = self.wake_locks.lock().await;
            Arc::clone(locks.entry(deployment.clone()).or_default())
        };
        let _guard = lock.lock().await;
        self.wake_locked(deployment, replicas).await
    }

    async fn stop_task(&self, replica: &ReplicaRef, mode: ReleaseMode) -> Result<(), ProviderError> {
        let template = self.template(&replica.deployment)?;
        let reason = match mode {
            ReleaseMode::ScaleIn => "graphrt gateway: scale-in",
            // Fargate has no suspend: fall back to stop.
            ReleaseMode::Stop | ReleaseMode::Suspend => "graphrt gateway: stop",
        };
        let body = json!({ "cluster": template.cluster, "task": replica.id, "reason": reason });
        match self.call("StopTask", body).await {
            Ok(_) => Ok(()),
            Err(error) if error.kind == ProviderErrorKind::NotFound => Ok(()),
            // ECS reports an unknown task as InvalidParameterException; stopping is idempotent.
            Err(error) if error.kind == ProviderErrorKind::Fatal && error.message.contains("not found") => Ok(()),
            Err(other) => Err(other),
        }
    }

    async fn list_replicas(&self, deployment: &DeploymentRef) -> Result<Vec<ReplicaState>, ProviderError> {
        let template = self.template(deployment)?;
        let arns = self.list_task_arns(deployment, &template.cluster).await?;
        let tasks = self.describe(&template.cluster, &arns).await?;
        Ok(tasks.iter().filter_map(|t| Self::task_view(deployment, template, t)).map(|v| v.state).collect())
    }
}

impl Provider for EcsProvider {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            addressing: Addressing::Replica,
            scaler: Scaler::Gateway,
            max_request_secs: None,
            idle_timeout_secs: None,
            max_grace_secs: FARGATE_MAX_STOP_TIMEOUT_SECS,
            background_cpu: true,
            supports_suspend: false,
            wake_p95_target_ms: 45_000,
        }
    }

    fn wake<'a>(
        &'a self,
        deployment: &'a DeploymentRef,
        replicas: u32,
    ) -> BoxFuture<'a, Result<Vec<Endpoint>, ProviderError>> {
        Box::pin(self.wake_singleflight(deployment, replicas))
    }

    fn release<'a>(&'a self, replica: &'a ReplicaRef, mode: ReleaseMode) -> BoxFuture<'a, Result<(), ProviderError>> {
        Box::pin(self.stop_task(replica, mode))
    }

    fn list<'a>(&'a self, deployment: &'a DeploymentRef) -> BoxFuture<'a, Result<Vec<ReplicaState>, ProviderError>> {
        Box::pin(self.list_replicas(deployment))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn started_by_fits_ecs_limit_and_is_stable() {
        let d = DeploymentRef::new("a-very-long-deployment-name-for-support-chat", "17", "");
        let tag = started_by(&d);
        assert!(tag.len() <= 36, "{tag}");
        assert_eq!(tag, started_by(&d));
        assert_ne!(tag, started_by(&DeploymentRef { version: "18".into(), ..d }));
    }

    #[test]
    fn ip_from_eni_attachment_or_container() {
        let task = json!({"attachments":[{"type":"ElasticNetworkInterface","details":[
            {"name":"subnetId","value":"subnet-1"},{"name":"privateIPv4Address","value":"10.0.1.7"}]}]});
        assert_eq!(task_ip(&task).as_deref(), Some("10.0.1.7"));
        let task = json!({"containers":[{"networkInterfaces":[{"privateIpv4Address":"10.0.2.9"}]}]});
        assert_eq!(task_ip(&task).as_deref(), Some("10.0.2.9"));
        assert_eq!(task_ip(&json!({})), None);
    }

    #[test]
    fn phases() {
        assert_eq!(task_phase(&json!({"lastStatus":"PROVISIONING","desiredStatus":"RUNNING"})), ReplicaPhase::Starting);
        assert_eq!(task_phase(&json!({"lastStatus":"RUNNING","desiredStatus":"RUNNING"})), ReplicaPhase::Ready);
        assert_eq!(task_phase(&json!({"lastStatus":"RUNNING","desiredStatus":"STOPPED"})), ReplicaPhase::Stopping);
        assert_eq!(task_phase(&json!({"lastStatus":"DEPROVISIONING"})), ReplicaPhase::Stopping);
        assert_eq!(task_phase(&json!({"lastStatus":"STOPPED"})), ReplicaPhase::Stopped);
    }

    #[test]
    fn stop_timeout_validation() {
        assert!(validate_stop_timeout(120, 118).is_ok());
        assert!(validate_stop_timeout(121, 60).is_err());
        assert!(validate_stop_timeout(120, 119).is_err());
        assert!(validate_stop_timeout(30, 20).is_ok());
    }

    #[test]
    fn capacity_reasons() {
        assert!(is_capacity_reason("Capacity is unavailable at this time. Please try again later"));
        assert!(is_capacity_reason("RESOURCE:ENI"));
        assert!(!is_capacity_reason("CannotPullContainerError"));
    }
}
