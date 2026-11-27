//! The operations of [`FlyDirect`]: deploy, status, versions, rollback,
//! pause, resume and remove.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use graphrt_provider_api::ProviderErrorKind;
use serde::Serialize;

use super::machine_config::{
    AgentSource, META_DIRECT, META_MODE, META_PAUSED, Role, STORE_MACHINE_NAME, agent_on_volume, holding_config,
    machine_config_for, machine_name_for, paused, store_volume_of, validate_request, volume_gb_for, with_role,
    workflow_name_of,
};
use super::serve_options::{STORE_VOLUME_GB, STORE_VOLUME_NAME, StoreSummary};
use super::status::{
    DeploymentStatus, VersionSummary, deployed_at_ms, deployment_status, has_role, is_direct, metadata,
    version_history, version_of,
};
use super::{
    API_KEY_SECRET, DeployOutcome, DeployRequest, DirectError, ErrorCode, FlyDirect, ServiceMode, Step, StepEvent,
    StepStatus,
};
use crate::client::MAX_WAIT_SECS;
use crate::model::{CreateMachineRequest, Machine, MachineState, StopMachineRequest, UpdateMachineRequest, Volume};

/// Receives progress events.
pub type Progress<'callback> = &'callback mut dyn FnMut(StepEvent);

/// A failed deploy. `api_key` is set when the deploy already stored a new API
/// key before failing: it is the only time the caller can learn it.
#[derive(Clone, PartialEq, Eq)]
pub struct DeployFailure {
    pub error: DirectError,
    pub api_key: Option<String>,
}

impl std::fmt::Debug for DeployFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DeployFailure")
            .field("error", &self.error)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

impl std::fmt::Display for DeployFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(formatter)
    }
}

impl std::error::Error for DeployFailure {}

/// Result of a rollback or resume: the version now serving.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ServingVersion {
    pub app: String,
    pub url: String,
    pub version: String,
    pub workflow: Option<String>,
}

/// The Machine a deploy is bringing up.
struct Candidate {
    id: String,
    /// An existing Machine of the same version was updated instead of a new one created.
    reused: bool,
    /// The config the Machine served with before this deploy updated it in
    /// place: put back when the new version does not get healthy.
    restore: Option<serde_json::Value>,
}

impl Candidate {
    /// A kept Machine (a standby version) brought back by a rollback.
    fn kept(id: &str) -> Self {
        Self { id: id.to_owned(), reused: true, restore: None }
    }
}

/// ` ; the agent (N MB) was uploaded onto the store volume` or `... was already there`.
fn upload_note(request: &DeployRequest, uploaded: bool) -> String {
    let size = format!("{:.1} MB", request.agent.contents.len() as f64 / 1_000_000.0);
    if uploaded {
        format!("; the agent ({size}) was uploaded onto the store volume")
    } else {
        format!("; the agent ({size}) was already on the store volume")
    }
}

/// Whether the Machine mounts the deployment's store volume.
pub(super) fn mounts_store(machine: &Machine) -> bool {
    store_volume_of(machine.config.as_ref()).is_some()
}

fn emit(progress: Progress<'_>, step: Step, status: StepStatus, message: impl Into<String>) {
    progress(StepEvent { step, status, message: message.into() });
}

/// Reports the end of a step: `done` with its message, or `failed` with the error.
fn finish<T>(progress: Progress<'_>, step: Step, result: Result<(T, String), DirectError>) -> Result<T, DirectError> {
    match result {
        Ok((value, message)) => {
            emit(progress, step, StepStatus::Done, message);
            Ok(value)
        }
        Err(error) => {
            emit(progress, step, StepStatus::Failed, error.message.clone());
            Err(error)
        }
    }
}

impl FlyDirect {
    /// Deploys `request.version` and makes it current once `/readyz` passes.
    ///
    /// Idempotent: the app, IPs and the version's Machine are reused when they
    /// exist. On failure the previous version keeps serving.
    ///
    /// @param request - what to deploy (&DeployRequest)
    /// @param progress - receives one `running` and one `done`/`failed` event per step
    /// @returns The URL, version, workflow name and (first deploy or rotation only) the API key
    /// @throws DeployFailure - with a stable [`ErrorCode`]; carries the API key if it was already stored
    pub async fn deploy(
        &self,
        request: &DeployRequest,
        progress: Progress<'_>,
    ) -> Result<DeployOutcome, DeployFailure> {
        let fail = |error| DeployFailure { error, api_key: None };
        emit(progress, Step::Preparing, StepStatus::Running, format!("Preparing Fly app {}", self.app_name()));
        finish(progress, Step::Preparing, self.prepare(request).await).map_err(fail)?;
        emit(progress, Step::Secrets, StepStatus::Running, "Writing Fly secrets");
        let (secrets_version, api_key) =
            finish(progress, Step::Secrets, self.write_secrets(request).await).map_err(fail)?;
        let fail = |error| DeployFailure { error, api_key: api_key.clone() };
        emit(progress, Step::Starting, StepStatus::Running, format!("Starting version {}", request.version));
        let workflow = workflow_name_of(&request.agent.file_name);
        let started = self.start_candidate(request, &workflow, secrets_version, progress).await;
        let candidate = finish(progress, Step::Starting, started).map_err(fail)?;
        emit(progress, Step::Checking, StepStatus::Running, "Waiting for GET /readyz to pass");
        let promoted = self.promote_and_tidy(&candidate, request).await;
        let store = finish(progress, Step::Checking, promoted).map_err(fail)?;
        Ok(DeployOutcome {
            app: self.app_name().to_owned(),
            url: self.url(),
            version: request.version.clone(),
            workflow,
            api_key,
            serve: request.serve.summary(),
            store,
        })
    }

    /// Promotes the candidate, deletes the store when the deploy asked for
    /// that, and reports the store the deployment has afterwards.
    async fn promote_and_tidy(
        &self,
        candidate: &Candidate,
        request: &DeployRequest,
    ) -> Result<(Option<StoreSummary>, String), DirectError> {
        let ((), mut message) = self.check_and_promote(candidate, &request.version).await?;
        if agent_on_volume(&request.agent) {
            let workflow = workflow_name_of(&request.agent.file_name);
            let kept = super::upload::volume_agent(request, &workflow);
            // Only the serving agent stays; older ones are deleted. Best effort: a failure
            // only leaves an old file on the volume.
            match self.prune_volume_agents(&candidate.id, &[kept.name.as_str()]).await {
                Ok(_) => {}
                Err(error) => message.push_str(&format!("; older agents stay on the volume ({})", error.message)),
            }
        }
        if request.drop_store {
            let dropped = self.drop_store().await?;
            message.push_str(if dropped { "; the store volume was deleted" } else { "; there was no store volume" });
        }
        // The volume is in use when the deploy keeps state on it or serves its agent from it.
        let store = self.store_summary(request.serve.needs_store() || agent_on_volume(&request.agent)).await?;
        if let Some(kept) = store.as_ref().filter(|store| !store.in_use) {
            message.push_str(&format!(
                "; store volume {} is kept and still billed (deploy with --drop-store to delete it)",
                kept.volume
            ));
        }
        Ok((store, message))
    }

    /// The deployment's store volume, if it has one.
    ///
    /// @param in_use - whether the serving version mounts it (bool)
    async fn store_summary(&self, in_use: bool) -> Result<Option<StoreSummary>, DirectError> {
        Ok(self.store_volume().await?.map(|volume| StoreSummary {
            volume: volume.id,
            size_gb: volume.size_gb,
            region: volume.region,
            in_use,
        }))
    }

    async fn store_volume(&self) -> Result<Option<Volume>, DirectError> {
        Ok(self.client.list_volumes().await?.into_iter().find(|volume| volume.name == STORE_VOLUME_NAME))
    }

    /// The store volume, created in `region` (with `size_gb`) when the deployment has none.
    async fn ensure_store_volume(&self, region: &str, size_gb: u32) -> Result<Volume, DirectError> {
        if let Some(volume) = self.store_volume().await? {
            return match &volume.attached_machine_id {
                Some(machine_id) => Err(DirectError::new(
                    ErrorCode::FlyApi,
                    format!(
                        "store volume {} is attached to Machine {machine_id}, which graphrt does not manage; \
                         destroy that Machine (the volume and its data stay), then deploy again",
                        volume.id
                    ),
                )),
                None => {
                    self.ensure_volume_room(&volume.id, volume.size_gb, size_gb, None).await?;
                    Ok(volume)
                }
            };
        }
        if region.is_empty() {
            return Err(DirectError::invalid(
                "--region is required with --monitor, --devices, --push or --conversations, and for agents larger \
                 than 1 MiB: they are kept on a Fly volume, which lives in one region",
            ));
        }
        Ok(self.client.create_volume(STORE_VOLUME_NAME, region, size_gb).await?)
    }

    /// Destroys the Machine that mounts the store (never the serving one: a
    /// deploy that needs the store cannot drop it) and deletes the volume.
    ///
    /// @returns `false` when the deployment had no store volume
    async fn drop_store(&self) -> Result<bool, DirectError> {
        for machine in self.direct_machines().await?.iter().filter(|machine| mounts_store(machine)) {
            if has_role(machine, Role::Current) {
                return Err(DirectError::new(
                    ErrorCode::FlyApi,
                    format!("Machine {} serves with the store volume; it was not deleted", machine.id),
                ));
            }
            match self.client.destroy_machine(&machine.id).await {
                Ok(()) => {}
                Err(error) if error.kind == ProviderErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        let Some(volume) = self.store_volume().await? else { return Ok(false) };
        self.client.delete_volume(&volume.id).await?;
        Ok(true)
    }

    /// The deployment's state, current version and Machines.
    ///
    /// @param now_ms - wall clock in Unix ms (u64)
    /// @throws DirectError(NotFound) - the app does not exist
    pub async fn status(&self, now_ms: u64) -> Result<DeploymentStatus, DirectError> {
        let machines = self.direct_machines().await?;
        let store_in_use = machines.iter().any(|machine| has_role(machine, Role::Current) && mounts_store(machine));
        let mut status = deployment_status(self.app_name(), self.url(), &machines, now_ms);
        status.store = self.store_summary(store_in_use).await?;
        Ok(status)
    }

    /// Every version still kept (current and standby), newest deploy first.
    pub async fn list_versions(&self) -> Result<Vec<VersionSummary>, DirectError> {
        Ok(version_history(&self.direct_machines().await?))
    }

    /// Makes a kept version current again (its own agent file and config).
    ///
    /// @param version - a label from [`FlyDirect::list_versions`] (&str)
    /// @throws DirectError(NotFound) - the version is not kept; HealthCheckFailed - it did not get healthy (the current version keeps serving)
    pub async fn rollback(&self, version: &str, progress: Progress<'_>) -> Result<ServingVersion, DirectError> {
        let machines = self.direct_machines().await?;
        let target = machines
            .iter()
            .filter(|machine| version_of(machine) == Some(version))
            .max_by_key(|machine| deployed_at_ms(machine))
            .ok_or_else(|| DirectError::new(ErrorCode::NotFound, format!("version {version} is not kept on Fly")))?;
        if has_role(target, Role::Current) {
            return Ok(self.serving(target));
        }
        let mode = machines
            .iter()
            .filter(|machine| has_role(machine, Role::Current))
            .chain(std::iter::once(target))
            .find_map(|machine| metadata(machine, META_MODE).and_then(ServiceMode::parse))
            .unwrap_or(ServiceMode::ScaleToZero);
        emit(progress, Step::Starting, StepStatus::Running, format!("Starting version {version}"));
        let started = match self.relaunch(&target.id, Role::Candidate, mode).await {
            Ok(()) => Ok(((), "Machine started".to_owned())),
            Err(error) => Err(self.discard_candidate(&Candidate::kept(&target.id), error).await),
        };
        finish(progress, Step::Starting, started)?;
        emit(progress, Step::Checking, StepStatus::Running, "Waiting for GET /readyz to pass");
        finish(progress, Step::Checking, self.check_and_promote(&Candidate::kept(&target.id), version).await)?;
        Ok(self.serving(target))
    }

    /// Stops the current version and takes it off the Fly proxy (no autostart).
    ///
    /// @returns `false` when it was already paused
    /// @throws DirectError(NotFound) - nothing is deployed
    pub async fn pause(&self) -> Result<bool, DirectError> {
        let current = self.current_machines().await?;
        let running: Vec<&Machine> =
            current.iter().filter(|machine| metadata(machine, META_PAUSED) != Some("1")).collect();
        for machine in &running {
            self.halt(&machine.id).await?;
            let fresh = self.client.get_machine(&machine.id).await?;
            let config = paused(fresh.config.as_ref().unwrap_or(&serde_json::Value::Null));
            let update = UpdateMachineRequest { config: &config, skip_launch: true, min_secrets_version: None };
            self.update_stopped(&machine.id, &update).await?;
        }
        Ok(!running.is_empty())
    }

    /// Puts a paused deployment back on the Fly proxy and waits for `/readyz`.
    ///
    /// @returns The serving version, or `None` when it was not paused
    pub async fn resume(&self, progress: Progress<'_>) -> Result<Option<ServingVersion>, DirectError> {
        let current = self.current_machines().await?;
        let paused_machines: Vec<&Machine> =
            current.iter().filter(|machine| metadata(machine, META_PAUSED) == Some("1")).collect();
        let Some(first) = paused_machines.first() else { return Ok(None) };
        emit(progress, Step::Starting, StepStatus::Running, "Starting the paused version");
        let mut started = Ok(((), "Machine started".to_owned()));
        for machine in &paused_machines {
            let mode = metadata(machine, META_MODE).and_then(ServiceMode::parse).unwrap_or(ServiceMode::ScaleToZero);
            if let Err(error) = self.relaunch(&machine.id, Role::Current, mode).await {
                started = Err(error);
                break;
            }
        }
        finish(progress, Step::Starting, started)?;
        emit(progress, Step::Checking, StepStatus::Running, "Waiting for GET /readyz to pass");
        let mut healthy = Ok(((), "Online".to_owned()));
        for machine in &paused_machines {
            if let Err(error) = self.wait_healthy(&machine.id).await {
                healthy = Err(error);
                break;
            }
        }
        finish(progress, Step::Checking, healthy)?;
        Ok(Some(self.serving(first)))
    }

    /// Deletes the app with its Machines, secrets and IPs.
    ///
    /// @returns `false` when the app did not exist
    pub async fn remove(&self) -> Result<bool, DirectError> {
        match self.client.delete_app().await {
            Ok(()) => Ok(true),
            Err(error) if error.kind == ProviderErrorKind::NotFound => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    fn serving(&self, machine: &Machine) -> ServingVersion {
        ServingVersion {
            app: self.app_name().to_owned(),
            url: self.url(),
            version: version_of(machine).unwrap_or_default().to_owned(),
            workflow: metadata(machine, super::machine_config::META_WORKFLOW).map(str::to_owned),
        }
    }

    pub(super) async fn direct_machines(&self) -> Result<Vec<Machine>, DirectError> {
        match self.client.list_machines_with_metadata(META_DIRECT, "1").await {
            Ok(machines) => Ok(machines.into_iter().filter(is_direct).collect()),
            Err(error) if error.kind == ProviderErrorKind::NotFound => Err(DirectError::new(
                ErrorCode::NotFound,
                format!("Fly app {} does not exist (nothing deployed)", self.app_name()),
            )),
            Err(error) => Err(error.into()),
        }
    }

    async fn current_machines(&self) -> Result<Vec<Machine>, DirectError> {
        let current: Vec<Machine> =
            self.direct_machines().await?.into_iter().filter(|machine| has_role(machine, Role::Current)).collect();
        if current.is_empty() {
            return Err(DirectError::new(ErrorCode::NotFound, format!("nothing is deployed on {}", self.app_name())));
        }
        Ok(current)
    }

    /// Validates the request, creates the app when missing and gives it public IPs.
    async fn prepare(&self, request: &DeployRequest) -> Result<((), String), DirectError> {
        validate_request(request)?;
        let created = self.ensure_app(&request.org_slug).await?;
        let allocated = self.ensure_public_ips().await?;
        let app_note = if created { format!("created app {}", self.app_name()) } else { "app exists".to_owned() };
        Ok(((), format!("{app_note}; {allocated} public IP(s) allocated")))
    }

    async fn ensure_app(&self, org_slug: &str) -> Result<bool, DirectError> {
        if self.client.app_exists().await? {
            return Ok(false);
        }
        match self.client.create_app(org_slug, "").await {
            Ok(()) => Ok(true),
            Err(error) if error.message.contains("taken") || error.message.contains("already exists") => {
                Err(DirectError::new(
                    ErrorCode::AppNameTaken,
                    format!(
                        "the Fly app name {} belongs to someone else; pick another deployment name",
                        self.app_name()
                    ),
                ))
            }
            Err(error) => Err(error.into()),
        }
    }

    /// A dedicated IPv6 and a shared IPv4 (both free), so `<app>.fly.dev` resolves.
    async fn ensure_public_ips(&self) -> Result<usize, DirectError> {
        let ips = self.client.list_ip_assignments().await?;
        let mut allocated = 0;
        if !ips.iter().any(|ip| ip.is_public() && ip.is_v6()) {
            self.client.assign_ip("v6").await?;
            allocated += 1;
        }
        if !ips.iter().any(|ip| ip.is_public() && !ip.is_v6()) {
            self.client.assign_ip("shared_v4").await?;
            allocated += 1;
        }
        Ok(allocated)
    }

    /// Writes the user's secrets and, when the app has none (or on rotation), the API key.
    async fn write_secrets(
        &self,
        request: &DeployRequest,
    ) -> Result<((Option<u64>, Option<String>), String), DirectError> {
        let existing = self.client.list_secret_names().await?;
        let store_key = request.rotate_api_key || !existing.iter().any(|name| name == API_KEY_SECRET);
        let mut values: BTreeMap<&str, Option<&str>> =
            request.secrets.iter().map(|(name, value)| (name.as_str(), Some(value.as_str()))).collect();
        if store_key {
            if request.api_key_candidate.len() < 32 {
                return Err(DirectError::invalid("the generated API key is too short"));
            }
            values.insert(API_KEY_SECRET, Some(request.api_key_candidate.as_str()));
        }
        if values.is_empty() {
            return Ok(((None, None), "no secrets to write".to_owned()));
        }
        let names: Vec<&str> = values.keys().copied().collect();
        let version = self.client.update_secrets(values).await?;
        let api_key = store_key.then(|| request.api_key_candidate.clone());
        Ok(((version, api_key), format!("set {}", names.join(", "))))
    }

    /// Creates the version's Machine (or updates the one it already has) and waits for it to boot.
    /// An agent too large for the Machine config is first uploaded onto the store volume.
    async fn start_candidate(
        &self,
        request: &DeployRequest,
        workflow: &str,
        secrets_version: Option<u64>,
        progress: Progress<'_>,
    ) -> Result<(Candidate, String), DirectError> {
        let on_volume = agent_on_volume(&request.agent);
        let wants_store = request.serve.needs_store() || on_volume;
        let volume_gb = if on_volume { volume_gb_for(request.agent.contents.len()) } else { STORE_VOLUME_GB };
        // The store lives on one volume, and only a new Machine can mount a
        // volume: a deploy that needs the store updates the Machine that has it.
        let existing = self.direct_machines().await?.into_iter().find(|machine| {
            if wants_store {
                mounts_store(machine)
            } else {
                !mounts_store(machine) && version_of(machine) == Some(request.version.as_str())
            }
        });
        let mut note = String::new();
        let mut report = |message: String| emit(&mut *progress, Step::Starting, StepStatus::Running, message);
        let candidate = match existing {
            Some(machine) => {
                let volume = store_volume_of(machine.config.as_ref()).map(str::to_owned);
                let serving = has_role(&machine, Role::Current) && metadata(&machine, META_PAUSED) != Some("1");
                let restore = machine.config.clone().filter(|_| serving);
                let placed = match (on_volume, volume.as_deref()) {
                    (true, Some(volume_id)) => {
                        let size = self.store_volume().await?.map_or(volume_gb, |found| found.size_gb);
                        self.ensure_volume_room(volume_id, size, volume_gb, Some(&machine.id)).await?;
                        let (placed, uploaded) = self.upload_agent(&machine.id, request, workflow, &mut report).await?;
                        note = upload_note(request, uploaded);
                        Some(placed)
                    }
                    _ => None,
                };
                let agent = match &placed {
                    Some(placed) => AgentSource::OnVolume { dir: &placed.dir },
                    None => AgentSource::Inline(&request.agent),
                };
                let config = machine_config_for(request, workflow, &self.url(), volume.as_deref(), agent);
                let fresh = self.client.get_machine(&machine.id).await?;
                if !fresh.state.is_startable() && fresh.state != MachineState::Started {
                    self.halt(&machine.id).await?;
                }
                let update =
                    UpdateMachineRequest { config: &config, skip_launch: false, min_secrets_version: secrets_version };
                self.update_and_launch(&machine.id, &update).await?;
                Candidate { id: machine.id, reused: true, restore }
            }
            None => {
                let volume =
                    if wants_store { Some(self.ensure_store_volume(&request.region, volume_gb).await?) } else { None };
                let volume_id = volume.as_ref().map(|volume| volume.id.as_str());
                // A new Machine with an agent on the volume boots without services first,
                // receives the agent, then gets its real config.
                let config = match (on_volume, volume_id) {
                    (true, Some(volume_id)) => holding_config(request, workflow, &self.url(), volume_id),
                    _ => machine_config_for(
                        request,
                        workflow,
                        &self.url(),
                        volume_id,
                        AgentSource::Inline(&request.agent),
                    ),
                };
                let create = CreateMachineRequest {
                    name: if volume.is_some() {
                        STORE_MACHINE_NAME.to_owned()
                    } else {
                        machine_name_for(&request.version)
                    },
                    // A Machine runs where its volume is.
                    region: volume.as_ref().map_or(request.region.as_str(), |volume| volume.region.as_str()),
                    config: &config,
                    skip_launch: false,
                    min_secrets_version: secrets_version,
                };
                let created = self.create_version_machine(&create).await?;
                note = created.note();
                let candidate = Candidate { id: created.machine.id, reused: false, restore: None };
                if let (true, Some(volume_id)) = (on_volume, volume_id) {
                    if let Err(error) =
                        self.place_on_new_machine(&candidate.id, volume_id, request, workflow, &mut report).await
                    {
                        return Err(self.discard_candidate(&candidate, error).await);
                    }
                    note.push_str(&upload_note(request, true));
                }
                candidate
            }
        };
        if let Err(error) = self.wait_started(&candidate.id).await {
            return Err(if candidate.restore.is_some() {
                self.discard_candidate(&candidate, error).await
            } else {
                error
            });
        }
        self.wait_started(&candidate.id).await?;
        let verb = if candidate.reused { "updated" } else { "created" };
        let message = format!("{verb} Machine {} and booted it{note}", candidate.id);
        Ok((candidate, message))
    }

    /// Uploads the agent into a new (holding) store Machine, then gives it its real config.
    async fn place_on_new_machine(
        &self,
        machine_id: &str,
        volume_id: &str,
        request: &DeployRequest,
        workflow: &str,
        report: &mut dyn FnMut(String),
    ) -> Result<(), DirectError> {
        self.wait_started(machine_id).await?;
        let (placed, _) = self.upload_agent(machine_id, request, workflow, report).await?;
        let agent = AgentSource::OnVolume { dir: &placed.dir };
        let config = machine_config_for(request, workflow, &self.url(), Some(volume_id), agent);
        self.halt(machine_id).await?;
        let update = UpdateMachineRequest { config: &config, skip_launch: false, min_secrets_version: None };
        self.update_and_launch(machine_id, &update).await
    }

    /// Waits for health, then makes the candidate current and the rest standby.
    /// An unhealthy candidate is discarded and the previous version keeps serving.
    async fn check_and_promote(&self, candidate: &Candidate, version: &str) -> Result<((), String), DirectError> {
        if let Err(error) = self.wait_healthy(&candidate.id).await {
            return Err(self.discard_candidate(candidate, error).await);
        }
        self.client
            .set_machine_metadata(&candidate.id, super::machine_config::META_ROLE, Role::Current.as_str())
            .await?;
        let machines = self.direct_machines().await?;
        let previous: Vec<&Machine> = machines
            .iter()
            .filter(|machine| machine.id != candidate.id)
            .filter(|machine| has_role(machine, Role::Current) || has_role(machine, Role::Candidate))
            .collect();
        for machine in &previous {
            self.demote(&machine.id).await?;
        }
        let after_demotion = self.direct_machines().await?;
        let pruned = self.prune_standby(&after_demotion, &candidate.id).await?;
        Ok(((), format!("version {version} is live; {} version(s) demoted, {pruned} pruned", previous.len())))
    }

    /// Removes a candidate that failed its checks: destroyed when new, kept as
    /// standby when reused, and put back as it was when the deploy updated the
    /// serving Machine in place.
    async fn discard_candidate(&self, candidate: &Candidate, cause: DirectError) -> DirectError {
        if let Some(previous) = &candidate.restore {
            return match self.restore(&candidate.id, previous).await {
                Ok(()) => DirectError::new(
                    cause.code,
                    format!("{}; the version that served before was started again", cause.message),
                ),
                Err(restore_error) => DirectError::new(
                    cause.code,
                    format!(
                        "{}; starting the version that served before also failed: {}",
                        cause.message, restore_error.message
                    ),
                ),
            };
        }
        let cleanup = if candidate.reused {
            self.demote(&candidate.id).await
        } else {
            self.client.destroy_machine(&candidate.id).await.map_err(DirectError::from)
        };
        match cleanup {
            Ok(()) => cause,
            Err(cleanup_error) => DirectError::new(
                cause.code,
                format!(
                    "{}; cleaning up Machine {} also failed: {}",
                    cause.message, candidate.id, cleanup_error.message
                ),
            ),
        }
    }

    /// Gives a Machine the config it served with before an in-place deploy and boots it.
    async fn restore(&self, machine_id: &str, previous: &serde_json::Value) -> Result<(), DirectError> {
        let update = UpdateMachineRequest { config: previous, skip_launch: false, min_secrets_version: None };
        self.update_and_launch(machine_id, &update).await?;
        self.wait_started(machine_id).await
    }

    /// Stops a Machine and removes its services so nothing routes to or autostarts it.
    async fn demote(&self, machine_id: &str) -> Result<(), DirectError> {
        self.halt(machine_id).await?;
        let fresh = self.client.get_machine(machine_id).await?;
        let config = with_role(fresh.config.as_ref().unwrap_or(&serde_json::Value::Null), Role::Standby, None);
        let update = UpdateMachineRequest { config: &config, skip_launch: true, min_secrets_version: None };
        self.update_stopped(machine_id, &update).await
    }

    /// Re-adds services to a kept Machine with `role` and boots it.
    async fn relaunch(&self, machine_id: &str, role: Role, mode: ServiceMode) -> Result<(), DirectError> {
        let fresh = self.client.get_machine(machine_id).await?;
        let config = with_role(fresh.config.as_ref().unwrap_or(&serde_json::Value::Null), role, Some(mode));
        let update = UpdateMachineRequest { config: &config, skip_launch: false, min_secrets_version: None };
        self.update_and_launch(machine_id, &update).await?;
        self.wait_started(machine_id).await
    }

    /// Destroys standby versions beyond `keep_versions`, oldest first.
    async fn prune_standby(&self, machines: &[Machine], keep_id: &str) -> Result<usize, DirectError> {
        let mut standby: Vec<&Machine> = machines
            .iter()
            .filter(|machine| machine.id != keep_id && !has_role(machine, Role::Current))
            // The Machine with the store is the only one that can ever mount it again.
            .filter(|machine| !mounts_store(machine))
            .filter(|machine| has_role(machine, Role::Standby) || has_role(machine, Role::Candidate))
            .collect();
        standby.sort_by_key(|machine| std::cmp::Reverse(deployed_at_ms(machine)));
        let mut pruned = 0;
        for machine in standby.iter().skip(self.settings.keep_versions) {
            match self.client.destroy_machine(&machine.id).await {
                Ok(()) => pruned += 1,
                Err(error) if error.kind == ProviderErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(pruned)
    }

    /// Stops a Machine (SIGTERM, then Fly's grace) unless it is already down.
    pub(super) async fn halt(&self, machine_id: &str) -> Result<(), DirectError> {
        let machine = self.client.get_machine(machine_id).await?;
        if matches!(machine.state, MachineState::Stopped | MachineState::Created | MachineState::Suspended) {
            return Ok(());
        }
        let stop = StopMachineRequest { signal: "SIGTERM".to_owned(), timeout: "30s".to_owned() };
        self.client.stop_machine(machine_id, &stop).await?;
        self.client.wait_machine(machine_id, MachineState::Stopped, Duration::from_secs(MAX_WAIT_SECS)).await?;
        Ok(())
    }

    /// Waits until the Machine reports `started`, in `/wait` calls of at most 60 s.
    pub(super) async fn wait_started(&self, machine_id: &str) -> Result<(), DirectError> {
        let deadline = Instant::now() + self.settings.start_timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(DirectError::new(
                    ErrorCode::StartTimeout,
                    format!("Machine {machine_id} did not start within {} s", self.settings.start_timeout.as_secs()),
                ));
            }
            match self.client.wait_machine(machine_id, MachineState::Started, remaining).await {
                Ok(()) => return Ok(()),
                Err(error) if error.kind == ProviderErrorKind::Transient => {
                    let machine = self.client.get_machine(machine_id).await?;
                    // Still down after the update: Fly does not start a stopped Machine by itself.
                    if machine.state.is_startable() {
                        self.client.start_machine(machine_id).await?;
                    }
                    if machine.state == MachineState::Failed {
                        return Err(DirectError::new(
                            ErrorCode::StartTimeout,
                            format!("Machine {machine_id} failed to boot (see `fly logs -a {}`)", self.app_name()),
                        ));
                    }
                }
                Err(error) => return Err(error.into()),
            }
        }
    }

    /// Polls the Machine until every health check passes.
    async fn wait_healthy(&self, machine_id: &str) -> Result<(), DirectError> {
        let deadline = Instant::now() + self.settings.health_timeout;
        let mut last_seen = "no check result yet".to_owned();
        loop {
            let machine = self.client.get_machine(machine_id).await?;
            if !matches!(machine.state, MachineState::Started | MachineState::Starting) {
                return Err(DirectError::new(
                    ErrorCode::HealthCheckFailed,
                    format!(
                        "Machine {machine_id} is {} instead of running (see `fly logs -a {}`)",
                        machine.state.as_api_str(),
                        self.app_name()
                    ),
                ));
            }
            if !machine.checks.is_empty() && machine.checks.iter().all(|check| check.is_passing()) {
                return Ok(());
            }
            if let Some(check) = machine.checks.iter().find(|check| !check.is_passing()) {
                last_seen = format!("check {} is {}: {}", check.name, check.status, check.output);
            }
            if Instant::now() >= deadline {
                return Err(DirectError::new(
                    ErrorCode::HealthCheckFailed,
                    format!(
                        "GET /readyz did not pass within {} s ({last_seen})",
                        self.settings.health_timeout.as_secs()
                    ),
                ));
            }
            tokio::time::sleep(self.settings.poll_interval).await;
        }
    }
}
