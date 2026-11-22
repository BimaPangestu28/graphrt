//! Provider abstraction of the graphrt gateway (plan §6).
//!
//! This module mirrors the trait and types that `graphrt-provider-api` will
//! publish (branch `feat/provider-api`). Until that crate lands the Kubernetes
//! provider codes against this copy; once it exists the types here become
//! re-exports and the provider implements the shared trait unchanged.

use std::fmt;
use std::future::Future;
use std::net::SocketAddr;
use std::time::Duration;

/// How the gateway addresses replicas of a deployment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Addressing {
    /// The gateway knows every replica's address and picks one itself.
    Replica,
    /// The platform exposes one URL and routes requests itself.
    Url,
}

/// Who decides how many replicas run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scaler {
    /// The gateway scales (`wake` / `release`).
    Gateway,
    /// The platform scales on incoming requests.
    Platform,
}

/// Static facts about a provider the gateway plans around.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capabilities {
    pub addressing: Addressing,
    pub scaler: Scaler,
    /// Hard cap on one request / SSE stream (`None` = unbounded).
    pub max_request_secs: Option<u64>,
    /// Idle timeout of the platform ingress (`None` = none in the path).
    pub idle_timeout_secs: Option<u64>,
    /// Longest shutdown grace the platform honours.
    pub max_grace_secs: u64,
    /// Whether a replica keeps CPU between requests.
    pub background_cpu: bool,
    /// Whether `ReleaseMode::Suspend` is a real suspend (memory kept).
    pub supports_suspend: bool,
    /// Wake latency target (p95) from plan §5, checked by conformance C2.
    pub wake_p95_target_ms: u64,
}

/// One immutable version of a deployment (decision 6: versions are immutable).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DeploymentRef {
    pub deployment: String,
    pub version: String,
}

impl DeploymentRef {
    /// Build a reference from a deployment name and a version.
    pub fn new(deployment: impl Into<String>, version: impl Into<String>) -> Self {
        Self { deployment: deployment.into(), version: version.into() }
    }
}

/// One replica of a deployment version, named by the provider.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ReplicaRef {
    pub deployment: DeploymentRef,
    /// Provider-side id (the pod name on Kubernetes).
    pub id: String,
}

/// A ready replica the gateway can dispatch runs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    pub replica: ReplicaRef,
    /// Address taken from the provider API, never from the agent (plan §3.2).
    pub address: SocketAddr,
}

/// How to give a replica back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReleaseMode {
    Suspend,
    Stop,
    ScaleIn,
}

/// Lifecycle phase of a replica as the provider reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplicaPhase {
    /// Scheduled or starting, not ready for runs yet.
    Starting,
    /// Ready (readiness probe passing).
    Ready,
    /// Being shut down (deletion requested, grace running).
    Terminating,
}

/// Reconciliation view of one replica.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplicaState {
    pub replica: ReplicaRef,
    pub phase: ReplicaPhase,
    pub address: Option<SocketAddr>,
}

/// Errors of run-time provider operations, classified for the gateway.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProviderError {
    /// No room for the requested replicas (unschedulable, quota).
    #[error("capacity: {0}")]
    Capacity(String),
    /// The provider API throttled the gateway.
    #[error("rate limited: {message}")]
    RateLimited { message: String, retry_after: Option<Duration> },
    /// Credentials missing, expired or lacking permission.
    #[error("auth: {0}")]
    Auth(String),
    /// The deployment or replica does not exist.
    #[error("not found: {0}")]
    NotFound(String),
    /// Worth retrying (timeouts, 5xx, conflicts).
    #[error("transient: {0}")]
    Transient(String),
    /// Will not succeed on retry (bad spec, invalid config).
    #[error("fatal: {0}")]
    Fatal(String),
}

/// Run-time operations only; creating deployments is the `DeployDriver`'s job.
pub trait Provider: Send + Sync {
    /// Static facts about this provider.
    fn capabilities(&self) -> Capabilities;
    /// Make at least `replicas` replicas ready and return their endpoints. Idempotent.
    fn wake(
        &self,
        deployment: &DeploymentRef,
        replicas: u32,
    ) -> impl Future<Output = Result<Vec<Endpoint>, ProviderError>> + Send;
    /// Give one replica back.
    fn release(
        &self,
        replica: &ReplicaRef,
        mode: ReleaseMode,
    ) -> impl Future<Output = Result<(), ProviderError>> + Send;
    /// Every replica the provider knows of, for reconciliation.
    fn list(
        &self,
        deployment: &DeploymentRef,
    ) -> impl Future<Output = Result<Vec<ReplicaState>, ProviderError>> + Send;
}

/// Reference to a secret held by the platform's secret store. Only the
/// reference travels in a spec, never the value (conformance C10).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretRef {
    /// Secret object name in the platform's store.
    pub name: String,
    /// One key of the secret, or `None` to expose every key as an env var.
    pub key: Option<String>,
    /// Env var name for a single `key` (defaults to the key).
    pub env: Option<String>,
}

/// What `graphrt deploy` asks a driver to create for one deployment version.
#[derive(Clone, PartialEq, Eq)]
pub struct DeploymentSpec {
    pub deployment: DeploymentRef,
    pub image: String,
    /// Container arguments (the image entrypoint is `graphrt`).
    pub args: Vec<String>,
    /// Port the agent listens on.
    pub port: u16,
    /// Plain (non-secret) environment variables.
    pub env: Vec<(String, String)>,
    pub secrets: Vec<SecretRef>,
    /// Shutdown grace given to a replica (plan §5: default 120 s).
    pub grace_secs: u64,
    pub cpu: Option<String>,
    pub memory: Option<String>,
}

impl fmt::Debug for DeploymentSpec {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Env values may still be sensitive by accident: only names are printed.
        let env_names: Vec<&str> = self.env.iter().map(|(name, _)| name.as_str()).collect();
        formatter
            .debug_struct("DeploymentSpec")
            .field("deployment", &self.deployment)
            .field("image", &self.image)
            .field("args", &self.args)
            .field("port", &self.port)
            .field("env", &env_names)
            .field("secrets", &self.secrets)
            .field("grace_secs", &self.grace_secs)
            .finish_non_exhaustive()
    }
}

/// Errors of `DeployDriver::apply`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DeployError {
    #[error("invalid deployment spec: {0}")]
    InvalidSpec(String),
    #[error(transparent)]
    Provider(#[from] ProviderError),
}

/// Deploy-time creation of a deployment version (`graphrt deploy`).
pub trait DeployDriver: Send + Sync {
    /// Create or update everything one deployment version needs. Idempotent.
    fn apply(
        &self,
        spec: &DeploymentSpec,
    ) -> impl Future<Output = Result<DeploymentRef, DeployError>> + Send;
}
