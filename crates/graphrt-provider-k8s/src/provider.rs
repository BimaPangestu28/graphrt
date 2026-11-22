//! The run-time Kubernetes provider: scale a version's Deployment 0<->N
//! through the `scale` subresource, pick scale-in victims with
//! `controller.kubernetes.io/pod-deletion-cost`, and report ready replicas from
//! the Service's EndpointSlices.
//!
//! RBAC the gateway's service account needs in the agents' namespace:
//! `deployments/scale` get+patch, `pods` get+list+patch, `endpointslices`
//! list+watch. Nothing else: Deployments, Services and Secrets are created by
//! `graphrt deploy` with deploy-time credentials (see [`crate::deploy`]).

use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use reqwest::Method;
use tokio::sync::Mutex;
use tokio::time::Instant;

use crate::api::{
    Addressing, Capabilities, DeploymentRef, Endpoint, Provider, ProviderError, ReleaseMode, ReplicaPhase, ReplicaRef,
    ReplicaState, Scaler,
};
use crate::client::{KubeClient, KubeError, RequestBody};
use crate::names::{object_name, pod_selector};
use crate::readiness::ReadinessTracker;
use crate::resources::{ObjectList, POD_DELETION_COST, Pod, ReadyAddress, Scale};

/// Field manager of the gateway's scale patches (deploy uses `graphrt-deploy`).
pub const GATEWAY_FIELD_MANAGER: &str = "graphrt-gateway";
/// Deletion cost given to a scale-in victim: lowest possible, deleted first.
const VICTIM_COST: &str = "-2147483648";
/// CAS attempts on the scale subresource before giving up (409 Conflict).
const SCALE_CAS_ATTEMPTS: u32 = 8;

/// Operator configuration of the provider (never taken from a deployment spec).
#[derive(Debug, Clone)]
pub struct K8sProviderConfig {
    /// Namespace holding the agents' Deployments.
    pub namespace: String,
    /// How long `wake` waits for readiness (plan D10: per target; 15 s covers an uncached pull).
    pub wake_timeout: Duration,
    /// Upper bound on replicas one `wake` may ask for.
    pub max_replicas: u32,
    /// `terminationGracePeriodSeconds` the deploy driver sets, reported as `max_grace_secs`.
    pub grace_secs: u64,
}

impl K8sProviderConfig {
    /// Defaults for `namespace`: 15 s wake timeout, 64 replicas, 120 s grace.
    pub fn new(namespace: impl Into<String>) -> Self {
        Self { namespace: namespace.into(), wake_timeout: Duration::from_secs(15), max_replicas: 64, grace_secs: 120 }
    }
}

/// Kubernetes provider (self-hosted, GKE, EKS, AKS: only cluster auth differs).
#[derive(Debug, Clone)]
pub struct K8sProvider {
    client: KubeClient,
    config: K8sProviderConfig,
    /// One lock per Deployment: concurrent wakes coalesce (singleflight, C1)
    /// and scale-in never races scale-up.
    locks: Arc<StdMutex<HashMap<String, Arc<Mutex<()>>>>>,
}

impl K8sProvider {
    /// Build a provider over an API client.
    pub fn new(client: KubeClient, config: K8sProviderConfig) -> Self {
        Self { client, config, locks: Arc::default() }
    }

    /// The underlying API client.
    pub fn client(&self) -> &KubeClient {
        &self.client
    }

    fn lock_for(&self, name: &str) -> Arc<Mutex<()>> {
        let mut locks = self.locks.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        locks.entry(name.to_owned()).or_default().clone()
    }

    fn deployment_path(&self, name: &str) -> String {
        format!("/apis/apps/v1/namespaces/{}/deployments/{name}", self.config.namespace)
    }

    fn pods_path(&self) -> String {
        format!("/api/v1/namespaces/{}/pods", self.config.namespace)
    }

    /// A tracker of the version's ready endpoints (for the gateway's watch loop, C5).
    pub fn readiness(&self, deployment: &DeploymentRef) -> ReadinessTracker {
        ReadinessTracker::new(self.client.clone(), &self.config.namespace, &object_name(deployment))
    }

    async fn get_scale(&self, name: &str) -> Result<Scale, KubeError> {
        self.client.get(&format!("{}/scale", self.deployment_path(name))).await
    }

    /// CAS the scale subresource to `next(current)`; `None` means leave it.
    async fn update_scale<F>(&self, name: &str, mut next: F) -> Result<u32, ProviderError>
    where
        F: FnMut(u32) -> Option<u32>,
    {
        let path = format!("{}/scale?fieldManager={GATEWAY_FIELD_MANAGER}", self.deployment_path(name));
        for _ in 0..SCALE_CAS_ATTEMPTS {
            let scale = self.get_scale(name).await?;
            let Some(replicas) = next(scale.spec.replicas) else { return Ok(scale.spec.replicas) };
            // resourceVersion makes the patch a compare-and-swap: a concurrent writer gets 409.
            let patch = serde_json::json!({
                "metadata": { "resourceVersion": scale.metadata.resource_version },
                "spec": { "replicas": replicas },
            });
            match self.client.send(Method::PATCH, &path, Some(RequestBody::merge_patch(&patch))).await {
                Ok(_) => return Ok(replicas),
                Err(error) if error.status_code() == Some(409) => continue,
                Err(error) => return Err(error.into()),
            }
        }
        Err(ProviderError::Transient(format!("scale of {name}: too many concurrent updates")))
    }

    async fn list_pods(&self, deployment: &DeploymentRef) -> Result<Vec<Pod>, KubeError> {
        let path = format!("{}?labelSelector={}", self.pods_path(), pod_selector(deployment));
        let list: ObjectList<Pod> = self.client.get(&path).await?;
        Ok(list.items)
    }

    /// Why a wake timed out: unschedulable pods are `Capacity`, bad images `Fatal`.
    async fn diagnose_wake_timeout(&self, deployment: &DeploymentRef, wanted: u32, ready: usize) -> ProviderError {
        let pods = self.list_pods(deployment).await.unwrap_or_default();
        let active = pods.iter().filter(|pod| !pod.is_terminating());
        let mut diagnosis = None;
        for pod in active {
            if let Some(message) = pod.unschedulable_message() {
                return ProviderError::Capacity(format!("{}: {message}", pod.metadata.name));
            }
            if diagnosis.is_none() {
                diagnosis = pod.fatal_waiting_reason().map(|reason| format!("{}: {reason}", pod.metadata.name));
            }
        }
        match diagnosis {
            Some(reason) => ProviderError::Fatal(reason),
            None => ProviderError::Transient(format!(
                "{ready}/{wanted} replicas of {} ready after {:?}",
                object_name(deployment),
                self.config.wake_timeout
            )),
        }
    }

    fn endpoints(deployment: &DeploymentRef, ready: &[ReadyAddress]) -> Vec<Endpoint> {
        ready
            .iter()
            .map(|address| Endpoint {
                replica: ReplicaRef { deployment: deployment.clone(), id: address.pod.clone() },
                address: address.address,
            })
            .collect()
    }

    async fn wake_locked(&self, deployment: &DeploymentRef, replicas: u32) -> Result<Vec<Endpoint>, ProviderError> {
        let name = object_name(deployment);
        let deadline = Instant::now() + self.config.wake_timeout;
        let mut tracker = self.readiness(deployment);
        tracker.relist().await?;
        let wanted = usize::try_from(replicas).unwrap_or(usize::MAX);
        if tracker.ready().len() < wanted {
            self.update_scale(&name, |current| (current < replicas).then_some(replicas)).await?;
        }
        let reached = tracker.wait_until(deadline, |ready| ready.len() >= wanted).await?;
        let ready = tracker.ready();
        if !reached {
            return Err(self.diagnose_wake_timeout(deployment, replicas, ready.len()).await);
        }
        Ok(Self::endpoints(deployment, &ready))
    }

    async fn release_locked(&self, replica: &ReplicaRef) -> Result<(), ProviderError> {
        let name = object_name(&replica.deployment);
        let pod_path = format!("{}/{}", self.pods_path(), replica.id);
        let pod: Pod = match self.client.get(&pod_path).await {
            Ok(pod) => pod,
            Err(error) if error.status_code() == Some(404) => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        if pod.is_terminating() {
            return Ok(());
        }
        let already_marked = pod.metadata.annotations.get(POD_DELETION_COST).map(String::as_str) == Some(VICTIM_COST);
        if !already_marked {
            self.set_deletion_cost(&pod_path, Some(VICTIM_COST)).await?;
        }
        let active = self.list_pods(&replica.deployment).await?.iter().filter(|pod| !pod.is_terminating()).count();
        let active = u32::try_from(active).unwrap_or(u32::MAX);
        let outcome = self
            .update_scale(&name, |current| {
                // A marked victim with more live pods than wanted: its scale-in is
                // already in flight (repeat call or controller lag).
                if already_marked && active > current {
                    return None;
                }
                Some(current.min(active).saturating_sub(1))
            })
            .await;
        if let Err(error) = outcome {
            // Unmark so a later scale-in elsewhere does not take this pod by surprise.
            if let Err(revert_error) = self.set_deletion_cost(&pod_path, None).await {
                tracing::warn!(pod = %replica.id, %revert_error, "could not clear pod-deletion-cost after a failed scale-in");
            }
            return Err(error);
        }
        Ok(())
    }

    async fn set_deletion_cost(&self, pod_path: &str, cost: Option<&str>) -> Result<(), ProviderError> {
        let patch = serde_json::json!({ "metadata": { "annotations": { POD_DELETION_COST: cost } } });
        match self.client.send(Method::PATCH, pod_path, Some(RequestBody::merge_patch(&patch))).await {
            Ok(_) => Ok(()),
            Err(error) if error.status_code() == Some(404) => Ok(()),
            Err(error) => Err(error.into()),
        }
    }
}

impl Provider for K8sProvider {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            addressing: Addressing::Replica,
            scaler: Scaler::Gateway,
            max_request_secs: None,
            idle_timeout_secs: None,
            max_grace_secs: self.config.grace_secs,
            background_cpu: true,
            supports_suspend: false,
            wake_p95_target_ms: 3_000,
        }
    }

    /// Scale to at least `replicas` and wait until that many are ready.
    /// `replicas == 0` only reports the ready endpoints.
    async fn wake(&self, deployment: &DeploymentRef, replicas: u32) -> Result<Vec<Endpoint>, ProviderError> {
        if replicas > self.config.max_replicas {
            return Err(ProviderError::Capacity(format!(
                "{replicas} replicas requested, provider limit is {}",
                self.config.max_replicas
            )));
        }
        let lock = self.lock_for(&object_name(deployment));
        let _guard = lock.lock().await;
        self.wake_locked(deployment, replicas).await
    }

    /// Scale in by one, making `replica` the victim. Every mode is a scale-in on
    /// Kubernetes (no suspend). Idempotent: a gone, terminating or already
    /// released pod is a no-op.
    async fn release(&self, replica: &ReplicaRef, _mode: ReleaseMode) -> Result<(), ProviderError> {
        let lock = self.lock_for(&object_name(&replica.deployment));
        let _guard = lock.lock().await;
        self.release_locked(replica).await
    }

    async fn list(&self, deployment: &DeploymentRef) -> Result<Vec<ReplicaState>, ProviderError> {
        let pods = self.list_pods(deployment).await?;
        Ok(pods
            .into_iter()
            .map(|pod| {
                let phase = if pod.is_terminating() {
                    ReplicaPhase::Terminating
                } else if pod.is_ready() {
                    ReplicaPhase::Ready
                } else {
                    ReplicaPhase::Starting
                };
                ReplicaState {
                    address: pod.address(),
                    replica: ReplicaRef { deployment: deployment.clone(), id: pod.metadata.name },
                    phase,
                }
            })
            .collect())
    }
}
