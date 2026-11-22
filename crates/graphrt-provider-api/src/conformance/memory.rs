//! `MemoryProvider`: an in-memory reference implementation of [`Provider`]
//! with `Addressing::Replica`. It validates the harness itself and is the
//! provider of the gateway's deterministic simulation.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use crate::error::ProviderError;
use crate::provider::{BoxFuture, Provider};
use crate::types::{
    Addressing, Capabilities, DeploymentRef, Endpoint, EndpointAddress, ReleaseMode, ReplicaPhase, ReplicaRef,
    ReplicaState, Scaler,
};

#[derive(Debug, Default)]
struct MemoryState {
    replicas: BTreeMap<ReplicaRef, ReplicaPhase>,
    pending_errors: Vec<ProviderError>,
}

/// In-memory provider. `wake` sleeps `wake_latency` (tokio clock) when it has
/// to start replicas.
#[derive(Debug)]
pub struct MemoryProvider {
    wake_latency: Duration,
    next_id: AtomicU64,
    api_calls: AtomicU64,
    state: Mutex<MemoryState>,
    wake_lock: tokio::sync::Mutex<()>,
}

impl MemoryProvider {
    /// A provider whose cold wake takes `wake_latency`.
    pub fn new(wake_latency: Duration) -> Self {
        Self {
            wake_latency,
            next_id: AtomicU64::new(1),
            api_calls: AtomicU64::new(0),
            state: Mutex::new(MemoryState::default()),
            wake_lock: tokio::sync::Mutex::new(()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, MemoryState> {
        self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Kills a replica behind the gateway's back (C5).
    pub fn kill_externally(&self, replica: &ReplicaRef) {
        if let Some(phase) = self.lock().replicas.get_mut(replica) {
            *phase = ReplicaPhase::Failed;
        }
    }

    /// Starts a replica the gateway did not ask for (orphan after `kill -9`, C12).
    pub fn spawn_orphan(&self, deployment: &DeploymentRef) -> ReplicaRef {
        let replica = self.new_replica(deployment);
        self.lock().replicas.insert(replica.clone(), ReplicaPhase::Ready);
        replica
    }

    /// Makes the next call fail with `error`.
    pub fn fail_next(&self, error: ProviderError) {
        self.lock().pending_errors.push(error);
    }

    /// Number of `wake` / `release` / `list` calls so far (I7).
    pub fn api_calls(&self) -> u64 {
        self.api_calls.load(Ordering::SeqCst)
    }

    /// Number of ready replicas of `deployment`.
    pub fn ready_count(&self, deployment: &DeploymentRef) -> usize {
        self.lock()
            .replicas
            .iter()
            .filter(|(replica, phase)| replica.deployment == *deployment && **phase == ReplicaPhase::Ready)
            .count()
    }

    fn new_replica(&self, deployment: &DeploymentRef) -> ReplicaRef {
        let number = self.next_id.fetch_add(1, Ordering::SeqCst);
        ReplicaRef { deployment: deployment.clone(), id: format!("mem-{number}") }
    }

    fn begin_call(&self) -> Result<(), ProviderError> {
        self.api_calls.fetch_add(1, Ordering::SeqCst);
        let mut state = self.lock();
        if state.pending_errors.is_empty() { Ok(()) } else { Err(state.pending_errors.remove(0)) }
    }

    fn endpoints(&self, deployment: &DeploymentRef) -> Vec<Endpoint> {
        self.lock()
            .replicas
            .iter()
            .filter(|(replica, phase)| replica.deployment == *deployment && **phase == ReplicaPhase::Ready)
            .map(|(replica, _)| Endpoint {
                replica: replica.clone(),
                address: EndpointAddress::Http(format!("mem://{}", replica.id)),
            })
            .collect()
    }
}

impl Provider for MemoryProvider {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            addressing: Addressing::Replica,
            scaler: Scaler::Gateway,
            max_request_secs: None,
            idle_timeout_secs: None,
            max_grace_secs: 30,
            background_cpu: true,
            supports_suspend: false,
            // Latency plus scheduler slack, so loaded CI machines do not flake C2.
            wake_p95_target_ms: (self.wake_latency.as_millis() as u64).saturating_add(100),
        }
    }

    fn wake<'a>(
        &'a self,
        deployment: &'a DeploymentRef,
        replicas: u32,
    ) -> BoxFuture<'a, Result<Vec<Endpoint>, ProviderError>> {
        Box::pin(async move {
            self.begin_call()?;
            let _serialised = self.wake_lock.lock().await;
            let missing = (replicas as usize).saturating_sub(self.ready_count(deployment));
            if missing > 0 {
                tokio::time::sleep(self.wake_latency).await;
                let mut state = self.lock();
                for _ in 0..missing {
                    let replica = self.new_replica(deployment);
                    state.replicas.insert(replica, ReplicaPhase::Ready);
                }
            }
            Ok(self.endpoints(deployment))
        })
    }

    fn release<'a>(&'a self, replica: &'a ReplicaRef, _mode: ReleaseMode) -> BoxFuture<'a, Result<(), ProviderError>> {
        Box::pin(async move {
            self.begin_call()?;
            self.lock().replicas.remove(replica);
            Ok(())
        })
    }

    fn list<'a>(&'a self, deployment: &'a DeploymentRef) -> BoxFuture<'a, Result<Vec<ReplicaState>, ProviderError>> {
        Box::pin(async move {
            self.begin_call()?;
            Ok(self
                .lock()
                .replicas
                .iter()
                .filter(|(replica, _)| replica.deployment == *deployment)
                .map(|(replica, phase)| ReplicaState {
                    replica: replica.clone(),
                    phase: *phase,
                    address: Some(EndpointAddress::Http(format!("mem://{}", replica.id))),
                    started_at_ms: None,
                })
                .collect())
        })
    }
}
