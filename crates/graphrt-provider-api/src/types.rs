//! Data types shared by every provider (plan §6).

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// How the gateway reaches replicas of a deployment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Addressing {
    /// The gateway holds one address per replica and picks the replica
    /// (local-process, Kubernetes, ECS Fargate, Fly).
    Replica,
    /// One URL per deployment; the platform routes and scales
    /// (Cloud Run, Azure Container Apps, Lambda). `release` is a no-op.
    Url,
}

/// Who decides the number of replicas.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scaler {
    /// The gateway scales 0→N (least-loaded) and releases idle replicas.
    Gateway,
    /// The platform scales on incoming requests.
    Platform,
}

/// Static facts about a provider the gateway plans around (drain policy,
/// planned preemption before a request cap, wake timeouts).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    /// Per-replica addresses or one URL.
    pub addressing: Addressing,
    /// Who scales.
    pub scaler: Scaler,
    /// Hard cap on one request / SSE stream (Cloud Run 3600, Lambda 900).
    /// `None` = unbounded. The gateway preempts at `cap - 60 s`.
    pub max_request_secs: Option<u64>,
    /// Ingress idle timeout the 15 s heartbeat must beat (ACA 240). `None` = none.
    pub idle_timeout_secs: Option<u64>,
    /// Time between SIGTERM and SIGKILL (Cloud Run 10, ECS 120). `0` means
    /// the platform gives no grace at all (Lambda): no planned preemption is
    /// possible, the gateway holds the stream and a run past
    /// `max_request_secs` fails explicitly with
    /// [`crate::ProviderErrorKind::PlatformTimeout`] (`failed{platform_timeout}`).
    pub max_grace_secs: u64,
    /// Whether the replica keeps CPU between requests (false on Lambda).
    pub background_cpu: bool,
    /// Whether `ReleaseMode::Suspend` is meaningful (Fly).
    pub supports_suspend: bool,
    /// Wake p95 target from plan §5, checked by conformance case C2.
    pub wake_p95_target_ms: u64,
}

impl Capabilities {
    /// Whether the gateway can preempt a run before a platform limit
    /// (`max_grace_secs > 0`).
    pub fn can_preempt(&self) -> bool {
        self.max_grace_secs > 0
    }
}

/// How a replica is given back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReleaseMode {
    /// Freeze memory, keep the machine (Fly suspend). Providers without
    /// suspend fall back to `Stop`.
    Suspend,
    /// Stop the replica (process exit, `StopTask`, Fly stop).
    Stop,
    /// Lower the replica count, preferring this replica as the victim
    /// (Kubernetes `pod-deletion-cost`).
    ScaleIn,
}

/// A deployment version known to a provider. Versions are immutable (decision 6).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct DeploymentRef {
    /// Deployment name, the namespace of run ids, HMAC keys and API keys.
    pub name: String,
    /// Immutable version (e.g. `v7` or a content hash).
    pub version: String,
    /// Provider resource the version lives in (Kubernetes `ns/deployment`,
    /// Cloud Run service name, ECS task definition ARN, Fly app...). Opaque to
    /// the gateway.
    pub target: String,
}

impl DeploymentRef {
    /// Builds a reference.
    pub fn new(name: impl Into<String>, version: impl Into<String>, target: impl Into<String>) -> Self {
        Self { name: name.into(), version: version.into(), target: target.into() }
    }
}

/// One replica of a deployment version.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ReplicaRef {
    /// The deployment version the replica runs.
    pub deployment: DeploymentRef,
    /// Provider id (pid, pod name, task ARN, machine id). For `Addressing::Url`
    /// the single pseudo-replica id is `url`.
    pub id: String,
}

/// Where a replica listens. Always obtained from the provider API or a socket
/// path the gateway chose, never from a claim made by the agent (plan §3.2).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndpointAddress {
    /// Unix domain socket (local-process).
    Unix(PathBuf),
    /// Base URL without trailing slash, e.g. `http://10.0.3.7:8080`.
    Http(String),
}

/// A reachable replica returned by `wake`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Endpoint {
    /// Which replica.
    pub replica: ReplicaRef,
    /// Where to reach it.
    pub address: EndpointAddress,
}

/// Lifecycle phase of a replica as the provider reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplicaPhase {
    /// Requested, not reachable yet.
    Starting,
    /// Reachable.
    Ready,
    /// Frozen (Fly suspend).
    Suspended,
    /// SIGTERM sent / draining.
    Stopping,
    /// Gone or stopped.
    Stopped,
    /// Crashed or failed to start.
    Failed,
}

/// One row of `Provider::list`, used for reconciliation (C5, C12).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplicaState {
    /// Which replica.
    pub replica: ReplicaRef,
    /// Its phase.
    pub phase: ReplicaPhase,
    /// Its address once known.
    pub address: Option<EndpointAddress>,
    /// Start time in unix milliseconds when the provider reports it.
    pub started_at_ms: Option<u64>,
}

/// A secret the platform injects as an environment variable. The spec carries
/// only the reference; the value lives in the provider's secret store and the
/// gateway never sees it (plan §5, C10).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecretRef {
    /// Environment variable name in the replica.
    pub env: String,
    /// Provider reference (`projects/p/secrets/s/versions/3`, a Key Vault URI,
    /// an ARN, `k8s-secret/key`, or `env:NAME` for local-process).
    pub reference: String,
}

/// Resource requests for one replica.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Resources {
    /// CPU in millicores (0 = provider default).
    pub cpu_millis: u32,
    /// Memory in MiB (0 = provider default).
    pub memory_mib: u32,
}

/// What `graphrt deploy` asks a `DeployDriver` to create or update.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeploymentSpec {
    /// Deployment name.
    pub name: String,
    /// New immutable version.
    pub version: String,
    /// Container image; `None` for local-process (it runs `command`).
    pub image: Option<String>,
    /// Command and arguments of the agent (`graphrt serve --trust-gateway ...`).
    pub command: Vec<String>,
    /// Plain environment (never secrets).
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Secrets as references.
    #[serde(default)]
    pub secrets: Vec<SecretRef>,
    /// Concurrent runs per replica (Cloud Run `containerConcurrency`).
    pub max_active_runs: u32,
    /// Replicas kept warm (0 = scale to zero).
    pub min_replicas: u32,
    /// Upper bound of the scaler and of the `max_replicas` quota.
    pub max_replicas: u32,
    /// Grace between SIGTERM and SIGKILL the driver sets on the platform.
    pub grace_secs: u64,
    /// Resources per replica.
    #[serde(default)]
    pub resources: Resources,
    /// Workflow fingerprint, so the gateway can route resumes to the version a
    /// paused run was snapshotted on.
    pub workflow_fingerprint: String,
    /// Provider-specific settings (region, namespace, VPC...), validated by the
    /// driver. Provider *endpoints* never come from here (SSRF, plan §6).
    #[serde(default)]
    pub provider_settings: BTreeMap<String, serde_json::Value>,
}
