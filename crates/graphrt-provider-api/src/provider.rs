//! The run-time `Provider` and deploy-time `DeployDriver` traits (plan §6).
//!
//! Both are object safe (`Box<dyn Provider>`): async methods return a boxed
//! future instead of using `async fn` in the trait, so the gateway can pick a
//! provider at run time from its config without generics, and without the
//! `async-trait` crate. Implementations write `Box::pin(async move { ... })`.

use std::future::Future;
use std::pin::Pin;

use crate::error::{DeployError, ProviderError};
use crate::types::{Capabilities, DeploymentRef, DeploymentSpec, Endpoint, ReleaseMode, ReplicaRef, ReplicaState};

/// A boxed `Send` future, the return type of every async trait method here.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Run-time operations only: wake, release, list. Creating or updating a
/// deployment is `graphrt deploy`'s job with deploy-time credentials
/// ([`DeployDriver`]); the gateway's credentials can only wake / scale / stop / list.
///
/// Contract (checked by the conformance harness):
/// - `wake(d, n)` is **idempotent and absolute**: it makes at least `n`
///   replicas of `d` ready and returns the endpoints of all ready replicas
///   (≥ `n`). Twenty concurrent `wake(d, 1)` from zero end with one replica
///   (C1). For `Addressing::Url` it may be a no-op returning the one URL.
/// - `release(r, mode)` gives back exactly `r`; releasing a replica that is
///   already gone is `Ok`. For `Addressing::Url` it is a no-op.
/// - `list(d)` reports every replica the provider knows for `d`, including
///   ones the gateway did not start (reconciliation, C5 and C12).
/// - Errors are classified ([`crate::ProviderErrorKind`]); credentials never
///   appear in errors or logs (C8, C10).
pub trait Provider: Send + Sync {
    /// Static capabilities.
    fn capabilities(&self) -> Capabilities;

    /// Ensures at least `replicas` ready replicas and returns their endpoints.
    fn wake<'a>(
        &'a self,
        deployment: &'a DeploymentRef,
        replicas: u32,
    ) -> BoxFuture<'a, Result<Vec<Endpoint>, ProviderError>>;

    /// Releases one replica (`Suspend`, `Stop` or `ScaleIn`).
    fn release<'a>(&'a self, replica: &'a ReplicaRef, mode: ReleaseMode) -> BoxFuture<'a, Result<(), ProviderError>>;

    /// Lists the replicas of a deployment version for reconciliation.
    fn list<'a>(&'a self, deployment: &'a DeploymentRef) -> BoxFuture<'a, Result<Vec<ReplicaState>, ProviderError>>;
}

/// Creates or updates a deployment version (`graphrt deploy --target <t>`).
/// Writes secrets to the provider's secret store as references; the gateway
/// never receives them.
pub trait DeployDriver: Send + Sync {
    /// Applies `spec` and returns the reference the gateway will wake.
    fn apply<'a>(&'a self, spec: &'a DeploymentSpec) -> BoxFuture<'a, Result<DeploymentRef, DeployError>>;
}

impl<P: Provider + ?Sized> Provider for std::sync::Arc<P> {
    fn capabilities(&self) -> Capabilities {
        (**self).capabilities()
    }
    fn wake<'a>(
        &'a self,
        deployment: &'a DeploymentRef,
        replicas: u32,
    ) -> BoxFuture<'a, Result<Vec<Endpoint>, ProviderError>> {
        (**self).wake(deployment, replicas)
    }
    fn release<'a>(&'a self, replica: &'a ReplicaRef, mode: ReleaseMode) -> BoxFuture<'a, Result<(), ProviderError>> {
        (**self).release(replica, mode)
    }
    fn list<'a>(&'a self, deployment: &'a DeploymentRef) -> BoxFuture<'a, Result<Vec<ReplicaState>, ProviderError>> {
        (**self).list(deployment)
    }
}
