//! Replica pool of one deployment version: activator, scaler, idle tracker
//! and reconciliation (plan §2).
//!
//! - **Activator**: a request that finds no free slot *holds*; one wake per
//!   pool is in flight at a time (**singleflight**), so 20 concurrent cold
//!   requests start one replica (C1). Waiters form a queue bounded in length
//!   (`max_queued`) and age (`queue_timeout`); beyond either the caller gets a
//!   `503 Retry-After`.
//! - **Scaler 0→N, least-loaded**: a lease goes to the replica with the fewest
//!   active runs that is below `max_active_runs`; when all are full and the pool is
//!   below `max_replicas`, one more replica is woken.
//! - **Idle tracker**: replicas with no run for `idle_timeout` are released
//!   (`Suspend` when the provider supports it, else `Stop`), down to `min_replicas`.
//! - **Reconcile** (≤ 1/min, plus on demand after a broken stream): `list`
//!   drops replicas the provider no longer reports ready (C5) and releases
//!   replicas the gateway does not know (orphans after a gateway crash, C12).
//!
//! All of it is in memory; nothing here touches the store (I7).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use graphrt_provider_api::{
    Addressing, Capabilities, DeploymentRef, Endpoint, Provider, ProviderError, ReleaseMode, ReplicaPhase, ReplicaRef,
};
use tokio::sync::Notify;
use tokio::time::Instant;

use crate::quota::{Quotas, WakeWindow};

/// Pool settings.
#[derive(Debug, Clone)]
pub struct PoolConfig {
    /// Concurrent runs per replica (the agent's `max_active_runs`).
    pub max_active_runs: u32,
    /// Replicas kept warm.
    pub min_replicas: u32,
    /// Idle time before a replica is released.
    pub idle_timeout: Duration,
    /// Longest a request waits for a replica.
    pub queue_timeout: Duration,
    /// Longest one wake may take before it counts as failed.
    pub wake_timeout: Duration,
    /// Quotas.
    pub quotas: Quotas,
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self {
            max_active_runs: 8,
            min_replicas: 0,
            idle_timeout: Duration::from_secs(300),
            queue_timeout: Duration::from_secs(30),
            wake_timeout: Duration::from_secs(60),
            quotas: Quotas::default(),
        }
    }
}

/// Why no replica could be handed out.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ActivateError {
    /// `max_queued` requests already wait.
    #[error("too many requests waiting for a replica")]
    QueueFull,
    /// Waited `queue_timeout` without a free slot.
    #[error("no replica became available in time")]
    QueueTimeout,
    /// `max_wakes_per_hour` reached and every replica is busy.
    #[error("wake quota exhausted")]
    WakeQuota,
    /// The provider failed to wake a replica.
    #[error("wake failed: {0}")]
    Wake(String),
    /// The pool was shut down (version retired).
    #[error("deployment version is not available")]
    Closed,
}

impl ActivateError {
    /// Seconds a client should wait before retrying.
    pub fn retry_after_secs(&self) -> u64 {
        match self {
            Self::QueueFull | Self::QueueTimeout => 2,
            Self::WakeQuota => 60,
            Self::Wake(_) => 5,
            Self::Closed => 0,
        }
    }
}

#[derive(Debug)]
struct Slot {
    endpoint: Endpoint,
    active_runs: u32,
    idle_since: Instant,
}

#[derive(Debug, Default)]
struct PoolState {
    slots: Vec<Slot>,
    waking: bool,
    waiters: u32,
    last_wake_error: Option<(Instant, String)>,
    wakes: WakeWindow,
    closed: bool,
}

/// Counters for `/metrics`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PoolStats {
    /// Replicas known.
    pub replicas: u32,
    /// Runs holding a lease.
    pub active_runs: u32,
    /// Requests waiting.
    pub queued: u32,
}

/// Replica pool of one deployment version.
pub struct Pool {
    deployment: DeploymentRef,
    provider: Arc<dyn Provider>,
    capabilities: Capabilities,
    config: PoolConfig,
    state: Mutex<PoolState>,
    changed: Notify,
}

impl std::fmt::Debug for Pool {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("Pool").field("deployment", &self.deployment).finish_non_exhaustive()
    }
}

/// A slot on one replica, released on drop.
#[derive(Debug)]
pub struct Lease {
    pool: Arc<Pool>,
    /// Where to send the run.
    pub endpoint: Endpoint,
    released: bool,
}

impl Lease {
    /// Gives the slot back and marks the replica broken (its stream failed):
    /// it gets no new runs until reconciliation confirms it.
    pub fn release_broken(mut self) {
        self.released = true;
        self.pool.give_back(&self.endpoint.replica, true);
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        if !self.released {
            self.pool.give_back(&self.endpoint.replica, false);
        }
    }
}

impl Pool {
    /// A pool for `deployment` on `provider`.
    pub fn new(deployment: DeploymentRef, provider: Arc<dyn Provider>, config: PoolConfig) -> Arc<Self> {
        let capabilities = provider.capabilities();
        Arc::new(Self {
            deployment,
            provider,
            capabilities,
            config,
            state: Mutex::new(PoolState::default()),
            changed: Notify::new(),
        })
    }

    /// The deployment version.
    pub fn deployment(&self) -> &DeploymentRef {
        &self.deployment
    }

    /// The provider's capabilities.
    pub fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, PoolState> {
        self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn slot_capacity(&self) -> u32 {
        match self.capabilities.addressing {
            // The platform scales behind one URL: the gateway only caps the total.
            Addressing::Url => self.config.max_active_runs.saturating_mul(self.config.quotas.max_replicas.max(1)),
            Addressing::Replica => self.config.max_active_runs.max(1),
        }
    }

    fn max_slots(&self) -> usize {
        match self.capabilities.addressing {
            Addressing::Url => 1,
            Addressing::Replica => self.config.quotas.max_replicas.max(1) as usize,
        }
    }

    /// Takes the least-loaded free slot, if any.
    fn try_take(&self, state: &mut PoolState) -> Option<Endpoint> {
        let capacity = self.slot_capacity();
        let slot =
            state.slots.iter_mut().filter(|slot| slot.active_runs < capacity).min_by_key(|slot| slot.active_runs)?;
        slot.active_runs += 1;
        Some(slot.endpoint.clone())
    }

    /// Hands out a slot, waking a replica when needed (hold + singleflight).
    ///
    /// # Errors
    /// See [`ActivateError`].
    pub async fn acquire(self: &Arc<Self>) -> Result<Lease, ActivateError> {
        let deadline = Instant::now() + self.config.queue_timeout;
        let mut queued = false;
        let result = loop {
            let notified = self.changed.notified();
            {
                let mut state = self.lock();
                if state.closed {
                    break Err(ActivateError::Closed);
                }
                if let Some(endpoint) = self.try_take(&mut state) {
                    break Ok(endpoint);
                }
                if !queued {
                    if state.waiters >= self.config.quotas.max_queued {
                        break Err(ActivateError::QueueFull);
                    }
                    state.waiters += 1;
                    queued = true;
                }
                // A wake that just failed is reported instead of retried at once.
                if let Some((at, message)) = &state.last_wake_error
                    && state.slots.is_empty()
                    && !state.waking
                    && at.elapsed() < Duration::from_secs(1)
                {
                    break Err(ActivateError::Wake(message.clone()));
                }
                if !state.waking && state.slots.len() < self.max_slots() {
                    if !state.wakes.try_record(Instant::now(), self.config.quotas.max_wakes_per_hour) {
                        if state.slots.is_empty() {
                            break Err(ActivateError::WakeQuota);
                        }
                    } else {
                        state.waking = true;
                        let target = state.slots.len() as u32 + 1;
                        tokio::spawn(Arc::clone(self).wake(target));
                    }
                }
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                break Err(ActivateError::QueueTimeout);
            }
        };
        if queued {
            self.lock().waiters -= 1;
        }
        result.map(|endpoint| Lease { pool: Arc::clone(self), endpoint, released: false })
    }

    /// The single in-flight wake of this pool.
    async fn wake(self: Arc<Self>, target: u32) {
        let outcome =
            tokio::time::timeout(self.config.wake_timeout, self.provider.wake(&self.deployment, target)).await;
        let mut state = self.lock();
        state.waking = false;
        match outcome {
            Ok(Ok(endpoints)) => {
                let before = state.slots.len();
                for endpoint in endpoints {
                    if !state.slots.iter().any(|slot| slot.endpoint.replica == endpoint.replica) {
                        state.slots.push(Slot { endpoint, active_runs: 0, idle_since: Instant::now() });
                    }
                }
                if state.slots.len() == before {
                    // Nothing new was started: the wake does not count against the quota.
                    state.wakes.refund();
                }
                state.last_wake_error = None;
            }
            Ok(Err(error)) => state.last_wake_error = Some((Instant::now(), error.to_string())),
            Err(_) => {
                state.last_wake_error =
                    Some((Instant::now(), format!("wake timed out after {:?}", self.config.wake_timeout)))
            }
        }
        drop(state);
        self.changed.notify_waiters();
    }

    fn give_back(&self, replica: &ReplicaRef, broken: bool) {
        let mut state = self.lock();
        if broken {
            state.slots.retain(|slot| slot.endpoint.replica != *replica);
        } else if let Some(slot) = state.slots.iter_mut().find(|slot| slot.endpoint.replica == *replica) {
            slot.active_runs = slot.active_runs.saturating_sub(1);
            if slot.active_runs == 0 {
                slot.idle_since = Instant::now();
            }
        }
        drop(state);
        self.changed.notify_waiters();
    }

    /// Wakes `min_replicas` (or one, for a cron prewarm) without taking a slot.
    ///
    /// # Errors
    /// The provider's error.
    pub async fn prewarm(self: &Arc<Self>, replicas: u32) -> Result<(), ProviderError> {
        let wanted = replicas.min(self.max_slots() as u32);
        if self.lock().slots.len() as u32 >= wanted {
            return Ok(());
        }
        let endpoints = self.provider.wake(&self.deployment, wanted).await?;
        let mut state = self.lock();
        for endpoint in endpoints {
            if !state.slots.iter().any(|slot| slot.endpoint.replica == endpoint.replica) {
                state.slots.push(Slot { endpoint, active_runs: 0, idle_since: Instant::now() });
            }
        }
        drop(state);
        self.changed.notify_waiters();
        Ok(())
    }

    /// Releases replicas idle for `idle_timeout`, keeping `min_replicas`.
    /// Returns how many were released.
    pub async fn release_idle(&self) -> usize {
        if self.capabilities.addressing == Addressing::Url {
            return 0;
        }
        let victims: Vec<ReplicaRef> = {
            let mut state = self.lock();
            let mut keep = self.config.min_replicas as usize;
            let mut victims = Vec::new();
            state.slots.sort_by_key(|slot| std::cmp::Reverse(slot.idle_since));
            state.slots.retain(|slot| {
                let idle = slot.active_runs == 0 && slot.idle_since.elapsed() >= self.config.idle_timeout;
                if !idle || keep > 0 {
                    keep = keep.saturating_sub(1);
                    return true;
                }
                victims.push(slot.endpoint.replica.clone());
                false
            });
            victims
        };
        let mode = if self.capabilities.supports_suspend { ReleaseMode::Suspend } else { ReleaseMode::Stop };
        for replica in &victims {
            if let Err(error) = self.provider.release(replica, mode).await {
                eprintln!("gateway: releasing idle replica {} of {}: {error}", replica.id, self.deployment.name);
            }
        }
        victims.len()
    }

    /// Compares the pool with `list`: forgets replicas that are no longer
    /// ready and releases ready ones the pool does not know (orphans).
    /// Returns `(dropped, orphans_released)`.
    ///
    /// # Errors
    /// The provider's `list` error.
    pub async fn reconcile(&self) -> Result<(usize, usize), ProviderError> {
        if self.capabilities.addressing == Addressing::Url {
            return Ok((0, 0));
        }
        let listed = self.provider.list(&self.deployment).await?;
        let (dropped, orphans) = {
            let mut state = self.lock();
            let before = state.slots.len();
            state.slots.retain(|slot| {
                listed.iter().any(|row| row.replica == slot.endpoint.replica && row.phase == ReplicaPhase::Ready)
            });
            let dropped = before - state.slots.len();
            let orphans: Vec<ReplicaRef> = listed
                .iter()
                .filter(|row| matches!(row.phase, ReplicaPhase::Ready | ReplicaPhase::Starting) && !state.waking)
                .filter(|row| !state.slots.iter().any(|slot| slot.endpoint.replica == row.replica))
                .map(|row| row.replica.clone())
                .collect();
            (dropped, orphans)
        };
        for orphan in &orphans {
            if let Err(error) = self.provider.release(orphan, ReleaseMode::Stop).await {
                eprintln!("gateway: releasing orphan replica {} of {}: {error}", orphan.id, self.deployment.name);
            }
        }
        if dropped > 0 {
            self.changed.notify_waiters();
        }
        Ok((dropped, orphans.len()))
    }

    /// Releases every replica and refuses new leases (version retired, shutdown).
    pub async fn close(&self) {
        let replicas: Vec<ReplicaRef> = {
            let mut state = self.lock();
            state.closed = true;
            state.slots.drain(..).map(|slot| slot.endpoint.replica).collect()
        };
        self.changed.notify_waiters();
        if self.capabilities.addressing == Addressing::Url {
            return;
        }
        for replica in &replicas {
            let _best_effort = self.provider.release(replica, ReleaseMode::Stop).await;
        }
    }

    /// Current counters.
    pub fn stats(&self) -> PoolStats {
        let state = self.lock();
        PoolStats {
            replicas: state.slots.len() as u32,
            active_runs: state.slots.iter().map(|slot| slot.active_runs).sum(),
            queued: state.waiters,
        }
    }
}

#[cfg(test)]
mod tests {
    use graphrt_provider_api::conformance::MemoryProvider;

    use super::*;

    fn deployment() -> DeploymentRef {
        DeploymentRef::new("shop", "v1", "mem")
    }

    fn pool(provider: Arc<MemoryProvider>, config: PoolConfig) -> Arc<Pool> {
        Pool::new(deployment(), provider, config)
    }

    #[tokio::test(start_paused = true)]
    async fn twenty_cold_requests_wake_one_replica() {
        let provider = Arc::new(MemoryProvider::new(Duration::from_millis(50)));
        let pool = pool(provider.clone(), PoolConfig { max_active_runs: 20, ..PoolConfig::default() });
        let leases = futures::future::join_all((0..20).map(|_| pool.acquire())).await;
        assert!(leases.iter().all(Result::is_ok));
        assert_eq!(provider.ready_count(&deployment()), 1);
        assert_eq!(pool.stats(), PoolStats { replicas: 1, active_runs: 20, queued: 0 });
    }

    #[tokio::test(start_paused = true)]
    async fn scales_out_least_loaded_up_to_max() {
        let provider = Arc::new(MemoryProvider::new(Duration::from_millis(10)));
        let config = PoolConfig {
            max_active_runs: 2,
            quotas: Quotas { max_replicas: 2, ..Quotas::default() },
            queue_timeout: Duration::from_millis(200),
            ..PoolConfig::default()
        };
        let pool = pool(provider.clone(), config);
        let mut leases = Vec::new();
        for _ in 0..4 {
            leases.push(pool.acquire().await.unwrap());
        }
        assert_eq!(provider.ready_count(&deployment()), 2);
        let per_replica = |id: &str| leases.iter().filter(|lease| lease.endpoint.replica.id == id).count();
        assert_eq!(per_replica("mem-1"), 2);
        assert_eq!(per_replica("mem-2"), 2);
        assert_eq!(pool.acquire().await.unwrap_err(), ActivateError::QueueTimeout);
        leases.pop();
        let lease = pool.acquire().await.unwrap();
        assert_eq!(lease.endpoint.replica.id, "mem-2");
    }

    #[tokio::test(start_paused = true)]
    async fn queue_is_bounded() {
        let provider = Arc::new(MemoryProvider::new(Duration::from_secs(5)));
        let config = PoolConfig { quotas: Quotas { max_queued: 1, ..Quotas::default() }, ..PoolConfig::default() };
        let pool = pool(provider, config);
        let first = tokio::spawn({
            let pool = pool.clone();
            async move { pool.acquire().await.map(|_| ()) }
        });
        tokio::time::sleep(Duration::from_millis(1)).await;
        assert_eq!(pool.acquire().await.unwrap_err(), ActivateError::QueueFull);
        assert!(first.await.unwrap().is_ok());
    }

    #[tokio::test(start_paused = true)]
    async fn idle_replicas_are_released_and_orphans_reconciled() {
        let provider = Arc::new(MemoryProvider::new(Duration::from_millis(1)));
        let config = PoolConfig { idle_timeout: Duration::from_secs(60), ..PoolConfig::default() };
        let pool = pool(provider.clone(), config);
        drop(pool.acquire().await.unwrap());
        assert_eq!(pool.release_idle().await, 0);
        tokio::time::advance(Duration::from_secs(61)).await;
        assert_eq!(pool.release_idle().await, 1);
        assert_eq!(provider.ready_count(&deployment()), 0);

        provider.spawn_orphan(&deployment());
        assert_eq!(pool.reconcile().await.unwrap(), (0, 1));
        assert_eq!(provider.ready_count(&deployment()), 0);

        let lease = pool.acquire().await.unwrap();
        provider.kill_externally(&lease.endpoint.replica);
        drop(lease);
        assert_eq!(pool.reconcile().await.unwrap().0, 1);
        assert_eq!(pool.stats().replicas, 0);
    }

    #[tokio::test(start_paused = true)]
    async fn wake_quota_and_errors_surface() {
        let provider = Arc::new(MemoryProvider::new(Duration::from_millis(1)));
        provider.fail_next(ProviderError::capacity("region full"));
        let pool = pool(provider, PoolConfig { queue_timeout: Duration::from_millis(100), ..PoolConfig::default() });
        assert!(matches!(pool.acquire().await, Err(ActivateError::Wake(_))));
        let quota = Quotas { max_wakes_per_hour: 0, ..Quotas::default() };
        let blocked = Pool::new(
            deployment(),
            Arc::new(MemoryProvider::new(Duration::ZERO)),
            PoolConfig { quotas: quota, ..PoolConfig::default() },
        );
        assert_eq!(blocked.acquire().await.unwrap_err(), ActivateError::WakeQuota);
    }
}
