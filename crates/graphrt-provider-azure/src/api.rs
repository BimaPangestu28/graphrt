//! Provider contract of plan §6, as this crate implements it.
//!
//! The gateway's shared crate (`graphrt-provider-api`, branch
//! `feat/provider-api`) owns these types. Until it lands they live here with
//! the exact shapes of §6, so the Azure code compiles and is tested against
//! them; on rebase this module becomes a re-export of that crate.

use std::fmt;
use std::future::Future;
use std::time::Duration;

/// How the gateway reaches a deployment's replicas.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Addressing {
    /// The gateway holds one address per replica and picks one.
    Replica,
    /// The platform load-balances behind one URL; the gateway only holds streams.
    Url,
}

/// Who decides the number of replicas.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scaler {
    Gateway,
    Platform,
}

/// What a provider can and cannot do; the gateway plans drains, preempts and
/// heartbeats from these numbers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Capabilities {
    pub addressing: Addressing,
    pub scaler: Scaler,
    /// Hard cap on one request (`None` = unlimited). The gateway preempts at `cap - 60 s`.
    pub max_request_secs: Option<u64>,
    /// Ingress idle timeout: a stream with no bytes for this long is cut.
    pub idle_timeout_secs: Option<u64>,
    /// Grace between SIGTERM and SIGKILL.
    pub max_grace_secs: u64,
    /// Whether a replica keeps CPU while no request is in flight.
    pub background_cpu: bool,
    pub supports_suspend: bool,
    pub wake_p95_target_ms: u64,
}

/// A deployed, immutable version the gateway can wake.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeploymentRef {
    /// Deployment name (the namespace of plan §1 decision 5).
    pub deployment: String,
    /// Monotonic deployment version.
    pub version: u64,
    /// Provider-specific resource name (ACA: the container app name).
    pub resource: String,
}

/// Where the gateway sends `/internal/v1/*` requests.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Endpoint {
    Url(String),
    Replica { id: String, address: String },
}

/// A replica the gateway can release.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReplicaRef {
    pub deployment: DeploymentRef,
    pub replica: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReleaseMode {
    Suspend,
    Stop,
    ScaleIn,
}

/// One replica as the provider reports it (reconciliation and observability).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReplicaState {
    pub id: String,
    /// Provider-side revision / version label of the replica.
    pub revision: String,
    pub running: bool,
}

/// Provider failures, split so the gateway can pick a reaction (§6).
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ProviderError {
    /// Quota or capacity exhausted: fall back (another region, cold start) or fail.
    #[error("provider capacity: {0}")]
    Capacity(String),
    /// Throttled after the retry budget; `retry_after` is the provider's hint.
    #[error("provider rate limited (retry after {retry_after:?})")]
    RateLimited { retry_after: Option<Duration> },
    #[error("provider auth: {0}")]
    Auth(String),
    #[error("provider resource not found: {0}")]
    NotFound(String),
    #[error("provider transient error: {0}")]
    Transient(String),
    #[error("provider error: {0}")]
    Fatal(String),
}

/// Run-time operations only; creating/updating a deployment is `DeployDriver`'s job.
pub trait Provider: Send + Sync {
    fn capabilities(&self) -> Capabilities;
    /// Idempotent. `Url` providers return the one URL (after a warm-up request).
    fn wake(
        &self,
        deployment: &DeploymentRef,
        replicas: u32,
    ) -> impl Future<Output = Result<Vec<Endpoint>, ProviderError>> + Send;
    fn release(
        &self,
        replica: &ReplicaRef,
        mode: ReleaseMode,
    ) -> impl Future<Output = Result<(), ProviderError>> + Send;
    fn list(&self, deployment: &DeploymentRef)
    -> impl Future<Output = Result<Vec<ReplicaState>, ProviderError>> + Send;
}

/// Deploy-time driver (`graphrt deploy`), with deploy-time credentials.
pub trait DeployDriver: Send + Sync {
    type Spec: Send + Sync;
    fn apply(&self, spec: &Self::Spec) -> impl Future<Output = Result<DeploymentRef, DeployError>> + Send;
}

/// Deploy failures: a rejected spec, or the provider failing underneath.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum DeployError {
    #[error("invalid deployment spec: {0}")]
    InvalidSpec(String),
    #[error(transparent)]
    Provider(#[from] ProviderError),
}

/// A string that must never reach logs: `Debug` and `Display` print `[redacted]`.
#[derive(Clone, PartialEq, Eq)]
pub struct SecretString(String);

impl SecretString {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The secret itself, for the one place that sends it.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("[redacted]")
    }
}

impl fmt::Display for SecretString {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("[redacted]")
    }
}
