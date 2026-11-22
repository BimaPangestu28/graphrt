//! The run-time [`Provider`] for Fly Machines.
//!
//! A deployment version is a **pool** of Machines in one Fly app, created
//! stopped by `graphrt deploy --target fly` and tagged with metadata
//! (`graphrt_pool`, `graphrt_deployment`, `graphrt_version`).
//!
//! - `wake(d, n)` is singleflight per version: it counts started Machines and
//!   starts the missing ones, preferring **suspended** (memory snapshot, fast
//!   resume) over stopped (cold boot) over never-booted. A start that fails for
//!   capacity moves on to the next Machine; when the pool is exhausted it
//!   clones a pool member's config into a new Machine (the placer picks
//!   another host) up to `max_machines_per_version`.
//! - `release(r, Suspend | ScaleIn)` suspends the Machine and falls back to
//!   `stop` (cold) when suspend is refused (guest > 2 GB, swap, unsupported);
//!   a Machine refused once goes straight to `stop` afterwards.
//! - Addresses are the Machines' 6PN IPs as the API reports them, never
//!   anything the agent claims (plan §3.2).
//!
//! The provider never sees deployment secrets: those are Fly app secrets set
//! at deploy time and injected by Fly into the Machine's environment.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, PoisonError};

use graphrt_provider_api::{
    Addressing, BoxFuture, Capabilities, DeploymentRef, Endpoint, EndpointAddress, Provider, ProviderError,
    ProviderErrorKind, ReleaseMode, ReplicaPhase, ReplicaRef, ReplicaState, Scaler,
};
use tokio::time::Instant;

use crate::client::{MAX_WAIT_SECS, MachinesClient};
use crate::config::FlyProviderConfig;
use crate::model::{
    CreateMachineRequest, METADATA_DEPLOYMENT, METADATA_POOL, METADATA_VERSION, Machine, MachineState,
    StopMachineRequest,
};

/// Wake p95 targets of plan §5.
const WAKE_P95_SUSPEND_MS: u64 = 800;
const WAKE_P95_STOP_MS: u64 = 2_500;
/// Attempts at finding a free Machine name when growing the pool.
const NAME_ATTEMPTS: u32 = 8;

/// Fly Machines provider (feature `provider-fly`).
#[derive(Debug)]
pub struct FlyProvider {
    config: FlyProviderConfig,
    clients: BTreeMap<String, MachinesClient>,
    wake_locks: Mutex<BTreeMap<DeploymentRef, Arc<tokio::sync::Mutex<()>>>>,
    suspend_refused: Mutex<BTreeSet<String>>,
}

impl FlyProvider {
    /// Builds the provider and one Machines client per configured app.
    ///
    /// @param config - operator config (FlyProviderConfig)
    /// @returns The provider
    /// @throws ProviderError(Fatal) - invalid config, app name or API URL
    pub fn new(config: FlyProviderConfig) -> Result<Self, ProviderError> {
        config.validate().map_err(ProviderError::fatal)?;
        let mut clients = BTreeMap::new();
        for (app_name, token_origin) in &config.apps {
            let client = MachinesClient::new(
                config.api_base_url.clone(),
                app_name.clone(),
                token_origin.clone(),
                config.client_policy.clone(),
            )?;
            clients.insert(app_name.clone(), client);
        }
        Ok(Self {
            config,
            clients,
            wake_locks: Mutex::new(BTreeMap::new()),
            suspend_refused: Mutex::new(BTreeSet::new()),
        })
    }

    fn client_for(&self, deployment: &DeploymentRef) -> Result<&MachinesClient, ProviderError> {
        self.clients.get(&deployment.target).ok_or_else(|| {
            ProviderError::fatal(format!("Fly app {:?} is not configured in the gateway", deployment.target))
        })
    }

    fn wake_lock(&self, deployment: &DeploymentRef) -> Arc<tokio::sync::Mutex<()>> {
        let mut wake_locks = self.wake_locks.lock().unwrap_or_else(PoisonError::into_inner);
        wake_locks.entry(deployment.clone()).or_default().clone()
    }

    /// Machines of the version's pool, filtered server-side and checked again here.
    async fn pool_machines(
        &self,
        client: &MachinesClient,
        deployment: &DeploymentRef,
    ) -> Result<Vec<Machine>, ProviderError> {
        let machines = client.list_machines(&pool_metadata(deployment)).await?;
        Ok(machines
            .into_iter()
            .filter(|machine| {
                machine.belongs_to(&deployment.name, &deployment.version) && machine.state != MachineState::Destroyed
            })
            .collect())
    }

    fn endpoint_of(&self, deployment: &DeploymentRef, machine: &Machine) -> Option<Endpoint> {
        let address = six_pn_address(&machine.private_ip, self.config.agent_port)?;
        Some(Endpoint { replica: ReplicaRef { deployment: deployment.clone(), id: machine.id.clone() }, address })
    }

    fn ready_endpoints(&self, deployment: &DeploymentRef, machines: &[Machine]) -> Vec<Endpoint> {
        machines
            .iter()
            .filter(|machine| machine.state == MachineState::Started)
            .filter_map(|machine| self.endpoint_of(deployment, machine))
            .collect()
    }

    async fn wake_pool(&self, deployment: &DeploymentRef, replicas: u32) -> Result<Vec<Endpoint>, ProviderError> {
        let client = self.client_for(deployment)?;
        let wake_lock = self.wake_lock(deployment);
        let _singleflight = wake_lock.lock().await;
        let machines = self.pool_machines(client, deployment).await?;
        let ready_count = self.ready_endpoints(deployment, &machines).len();
        let wanted = replicas as usize;
        if ready_count >= wanted {
            return Ok(self.ready_endpoints(deployment, &machines));
        }
        let booting = self.boot_missing(client, deployment, &machines, wanted - ready_count).await?;
        let started = self.wait_until_started(client, booting).await;
        // 6PN addresses are fixed at creation, so the first listing (or the
        // create response) already has them: no second list call.
        let mut endpoints = self.ready_endpoints(deployment, &machines);
        endpoints.extend(started.iter().filter_map(|machine| self.endpoint_of(deployment, machine)));
        if endpoints.len() < wanted {
            return Err(ProviderError::transient(format!(
                "Fly app {}: {} of {wanted} Machines started within {:?}",
                deployment.target,
                endpoints.len(),
                self.config.start_timeout
            )));
        }
        Ok(endpoints)
    }

    /// Starts (or creates) `missing` Machines; returns the Machines now booting.
    async fn boot_missing(
        &self,
        client: &MachinesClient,
        deployment: &DeploymentRef,
        machines: &[Machine],
        missing: usize,
    ) -> Result<Vec<Machine>, ProviderError> {
        let mut booting: Vec<Machine> =
            machines.iter().filter(|machine| machine.state == MachineState::Starting).take(missing).cloned().collect();
        let mut last_capacity_error = None;
        for machine in start_order(machines) {
            if booting.len() >= missing {
                break;
            }
            match client.start_machine(&machine.id).await {
                Ok(()) => booting.push(machine.clone()),
                Err(error) if matches!(error.kind, ProviderErrorKind::Capacity | ProviderErrorKind::NotFound) => {
                    last_capacity_error = Some(error)
                }
                Err(error) => return Err(error),
            }
        }
        if booting.len() < missing {
            let created =
                self.grow_pool(client, deployment, machines, missing - booting.len(), &mut last_capacity_error).await?;
            booting.extend(created);
        }
        if booting.is_empty() {
            return Err(last_capacity_error.unwrap_or_else(|| {
                ProviderError::capacity(format!(
                    "Fly app {}: no startable Machine left in the pool of {}",
                    deployment.target, deployment.version
                ))
            }));
        }
        Ok(booting)
    }

    /// Creates up to `count` started Machines cloned from a pool member.
    async fn grow_pool(
        &self,
        client: &MachinesClient,
        deployment: &DeploymentRef,
        machines: &[Machine],
        count: usize,
        last_capacity_error: &mut Option<ProviderError>,
    ) -> Result<Vec<Machine>, ProviderError> {
        let room = (self.config.max_machines_per_version as usize).saturating_sub(machines.len());
        let Some(template) = machines.iter().find(|machine| machine.config.is_some()) else {
            return Err(ProviderError::not_found(format!(
                "Fly app {}: deployment {} {} has no Machines; run `graphrt deploy --target fly` first",
                deployment.target, deployment.name, deployment.version
            )));
        };
        let mut created = Vec::new();
        let mut next_index = machines.len() as u32;
        for _ in 0..count.min(room) {
            match self.create_clone(client, deployment, template, &mut next_index).await {
                Ok(machine) => created.push(machine),
                Err(error) if error.kind == ProviderErrorKind::Capacity => {
                    *last_capacity_error = Some(error);
                    break;
                }
                Err(error) => return Err(error),
            }
        }
        Ok(created)
    }

    async fn create_clone(
        &self,
        client: &MachinesClient,
        deployment: &DeploymentRef,
        template: &Machine,
        next_index: &mut u32,
    ) -> Result<Machine, ProviderError> {
        let config = template.config.clone().unwrap_or_default();
        // The template's config is reused verbatim (same image, guest, stop_config, metadata).
        for _ in 0..NAME_ATTEMPTS {
            let request = CreateMachineRequest {
                name: pool_machine_name(&deployment.name, &deployment.version, *next_index),
                region: template.region.clone(),
                config: config.clone(),
                skip_launch: false,
            };
            *next_index += 1;
            match client.create_machine(&request).await {
                Err(error) if crate::error::is_name_taken(&error) => continue,
                other => return other,
            }
        }
        Err(ProviderError::transient(format!(
            "Fly app {}: no free Machine name after {NAME_ATTEMPTS} attempts",
            deployment.target
        )))
    }

    /// Waits for every booting Machine to report `started` until `start_timeout`;
    /// returns the ones that did. The Machines boot in parallel on Fly, so
    /// waiting on them one after the other costs no more than the slowest boot.
    async fn wait_until_started(&self, client: &MachinesClient, booting: Vec<Machine>) -> Vec<Machine> {
        let deadline = Instant::now() + self.config.start_timeout;
        let mut started = Vec::with_capacity(booting.len());
        for machine in booting {
            if wait_started_until(client, &machine.id, deadline).await {
                started.push(machine);
            }
        }
        started
    }

    async fn release_machine(&self, replica: &ReplicaRef, mode: ReleaseMode) -> Result<(), ProviderError> {
        let client = self.client_for(&replica.deployment)?;
        let machine = match client.get_machine(&replica.id).await {
            Ok(machine) => machine,
            Err(error) if error.kind == ProviderErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        };
        if !matches!(machine.state, MachineState::Started | MachineState::Starting) {
            return Ok(());
        }
        if self.should_suspend(&replica.id, mode) {
            match client.suspend_machine(&replica.id).await {
                Ok(()) => return Ok(()),
                Err(error) if error.kind == ProviderErrorKind::Auth => return Err(error),
                Err(error) if error.kind == ProviderErrorKind::Fatal => self.remember_suspend_refused(&replica.id),
                Err(_) => {}
            }
        }
        self.stop_machine(client, &replica.id).await
    }

    fn should_suspend(&self, machine_id: &str, mode: ReleaseMode) -> bool {
        let refused = self.suspend_refused.lock().unwrap_or_else(PoisonError::into_inner).contains(machine_id);
        mode != ReleaseMode::Stop && self.config.preferred_release == ReleaseMode::Suspend && !refused
    }

    fn remember_suspend_refused(&self, machine_id: &str) {
        self.suspend_refused.lock().unwrap_or_else(PoisonError::into_inner).insert(machine_id.to_owned());
    }

    async fn stop_machine(&self, client: &MachinesClient, machine_id: &str) -> Result<(), ProviderError> {
        let request = StopMachineRequest {
            signal: "SIGTERM".to_owned(),
            timeout: format!("{}s", self.config.stop_timeout.as_secs()),
        };
        match client.stop_machine(machine_id, &request).await {
            Err(error) if error.kind == ProviderErrorKind::NotFound => Ok(()),
            other => other,
        }
    }

    async fn list_pool(&self, deployment: &DeploymentRef) -> Result<Vec<ReplicaState>, ProviderError> {
        let client = self.client_for(deployment)?;
        let machines = self.pool_machines(client, deployment).await?;
        Ok(machines
            .iter()
            .map(|machine| ReplicaState {
                replica: ReplicaRef { deployment: deployment.clone(), id: machine.id.clone() },
                phase: replica_phase(machine.state),
                address: six_pn_address(&machine.private_ip, self.config.agent_port),
                started_at_ms: None,
            })
            .collect())
    }
}

impl Provider for FlyProvider {
    fn capabilities(&self) -> Capabilities {
        let suspend_preferred = self.config.preferred_release == ReleaseMode::Suspend;
        Capabilities {
            addressing: Addressing::Replica,
            scaler: Scaler::Gateway,
            max_request_secs: None,
            idle_timeout_secs: None,
            max_grace_secs: self.config.stop_timeout.as_secs(),
            background_cpu: true,
            supports_suspend: suspend_preferred,
            wake_p95_target_ms: if suspend_preferred { WAKE_P95_SUSPEND_MS } else { WAKE_P95_STOP_MS },
        }
    }

    fn wake<'a>(
        &'a self,
        deployment: &'a DeploymentRef,
        replicas: u32,
    ) -> BoxFuture<'a, Result<Vec<Endpoint>, ProviderError>> {
        Box::pin(self.wake_pool(deployment, replicas))
    }

    fn release<'a>(&'a self, replica: &'a ReplicaRef, mode: ReleaseMode) -> BoxFuture<'a, Result<(), ProviderError>> {
        Box::pin(self.release_machine(replica, mode))
    }

    fn list<'a>(&'a self, deployment: &'a DeploymentRef) -> BoxFuture<'a, Result<Vec<ReplicaState>, ProviderError>> {
        Box::pin(self.list_pool(deployment))
    }
}

/// `metadata.*` filter selecting one version's pool.
pub(crate) fn pool_metadata(deployment: &DeploymentRef) -> BTreeMap<String, String> {
    BTreeMap::from([
        (METADATA_POOL.to_owned(), "1".to_owned()),
        (METADATA_DEPLOYMENT.to_owned(), deployment.name.clone()),
        (METADATA_VERSION.to_owned(), deployment.version.clone()),
    ])
}

/// Deterministic Machine name `<deployment>-<version>-<index>` (Fly names: `[a-z0-9-]`, ≤ 63).
pub(crate) fn pool_machine_name(deployment: &str, version: &str, index: u32) -> String {
    let sanitize = |text: &str| -> String {
        text.chars()
            .map(|character| if character.is_ascii_alphanumeric() { character.to_ascii_lowercase() } else { '-' })
            .collect()
    };
    let mut prefix = format!("{}-{}", sanitize(deployment), sanitize(version));
    prefix.truncate(52);
    format!("{}-{index}", prefix.trim_end_matches('-'))
}

/// Start priority: suspended (snapshot resume), then stopped (cold), then never booted.
fn start_order(machines: &[Machine]) -> Vec<&Machine> {
    let priority = |state: MachineState| match state {
        MachineState::Suspended => 0,
        MachineState::Stopped => 1,
        _ => 2,
    };
    let mut candidates: Vec<&Machine> = machines.iter().filter(|machine| machine.state.is_startable()).collect();
    candidates.sort_by_key(|machine| priority(machine.state));
    candidates
}

/// Whether the Machine reached `started` before `deadline`.
async fn wait_started_until(client: &MachinesClient, machine_id: &str, deadline: Instant) -> bool {
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return false;
        }
        let timeout = remaining.min(std::time::Duration::from_secs(MAX_WAIT_SECS));
        match client.wait_machine(machine_id, MachineState::Started, timeout).await {
            Ok(()) => return true,
            Err(error) if error.kind.is_retryable() => continue,
            Err(_) => return false,
        }
    }
}

/// `http://[fdaa:…]:port` from the Machine's 6PN address; `None` until Fly assigned one.
pub(crate) fn six_pn_address(private_ip: &str, port: u16) -> Option<EndpointAddress> {
    let address: std::net::IpAddr = private_ip.parse().ok()?;
    Some(EndpointAddress::Http(match address {
        std::net::IpAddr::V6(v6) => format!("http://[{v6}]:{port}"),
        std::net::IpAddr::V4(v4) => format!("http://{v4}:{port}"),
    }))
}

fn replica_phase(state: MachineState) -> ReplicaPhase {
    match state {
        MachineState::Started => ReplicaPhase::Ready,
        MachineState::Starting => ReplicaPhase::Starting,
        MachineState::Suspended => ReplicaPhase::Suspended,
        MachineState::Stopping | MachineState::Suspending | MachineState::Destroying | MachineState::Replacing => {
            ReplicaPhase::Stopping
        }
        MachineState::Created | MachineState::Stopped | MachineState::Destroyed => ReplicaPhase::Stopped,
        MachineState::Failed | MachineState::Unknown => ReplicaPhase::Failed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn six_pn_addresses_are_bracketed() {
        assert_eq!(
            six_pn_address("fdaa:0:1:a7b:1f2:3:4:2", 8080),
            Some(EndpointAddress::Http("http://[fdaa:0:1:a7b:1f2:3:4:2]:8080".into()))
        );
        assert_eq!(six_pn_address("", 8080), None);
        assert_eq!(six_pn_address("evil.example.com", 8080), None);
    }

    #[test]
    fn machine_names_are_fly_safe_and_bounded() {
        assert_eq!(pool_machine_name("Chat_Bot", "v1.2", 3), "chat-bot-v1-2-3");
        assert!(pool_machine_name(&"x".repeat(80), "v", 12).len() <= 63);
    }

    #[test]
    fn phases_cover_suspend() {
        assert_eq!(replica_phase(MachineState::Suspended), ReplicaPhase::Suspended);
        assert_eq!(replica_phase(MachineState::Created), ReplicaPhase::Stopped);
        assert_eq!(replica_phase(MachineState::Suspending), ReplicaPhase::Stopping);
    }
}
