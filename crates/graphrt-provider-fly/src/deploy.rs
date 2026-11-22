//! `graphrt deploy --target fly`: the deploy-time [`DeployDriver`].
//!
//! Runs with a deploy-time token (CLI or CI), never inside the gateway:
//!
//! 1. ensures the Fly app exists (creates it in `org` when asked);
//! 2. writes secrets to **Fly app secrets** (`env:VAR` references are read
//!    from the deploying shell; `fly:NAME` references must already exist).
//!    Fly injects them as environment variables at boot, so neither the spec,
//!    the Machine config nor the gateway ever holds a value (C10);
//! 3. creates the version's pool: `pool_size` Machines with `skip_launch`
//!    (stopped, only the rootfs is billed), no public services (6PN only),
//!    `restart.policy = no` and `stop_config` = SIGTERM + `grace_secs`
//!    (Fly's `kill_timeout`, at most 300 s) so the agent can drain.
//!
//! `apply` is idempotent: Machine names are deterministic and existing ones are
//! kept, so a retried deploy never doubles the pool.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use graphrt_provider_api::{BoxFuture, DeployDriver, DeployError, DeploymentRef, DeploymentSpec, ProviderErrorKind};

use crate::client::{ClientPolicy, MachinesClient};
use crate::config::{MAX_KILL_TIMEOUT_SECS, MAX_SUSPEND_MEMORY_MB};
use crate::error::is_name_taken;
use crate::model::{
    CreateMachineRequest, GuestConfig, InitConfig, METADATA_DEPLOYMENT, METADATA_POOL, METADATA_VERSION, MachineConfig,
    RestartConfig, StopConfig,
};
use crate::provider::{pool_machine_name, pool_metadata};
use crate::token::TokenOrigin;

/// Metadata key with the workflow fingerprint, so resumes go to the right version.
pub const METADATA_FINGERPRINT: &str = "graphrt_fingerprint";
/// Default guest memory: `graphrt serve` idles at a few MiB.
const DEFAULT_MEMORY_MB: u32 = 256;
/// Largest pool `apply` creates in one go.
const MAX_POOL_SIZE: u32 = 100;

/// Looks up a deploy-time environment variable (`std::env::var` in the CLI).
pub type EnvLookup = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// Deploy-time settings of the driver.
#[derive(Clone)]
pub struct FlyDeployConfig {
    /// Machines API base URL (operator / CLI flag, never from the spec).
    pub api_base_url: String,
    /// Deploy-time token (may create apps and set secrets).
    pub token_origin: TokenOrigin,
    /// Retries and rate limit.
    pub client_policy: ClientPolicy,
    /// Source of `env:VAR` secret values.
    pub env_lookup: EnvLookup,
}

impl std::fmt::Debug for FlyDeployConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("FlyDeployConfig").field("api_base_url", &self.api_base_url).finish_non_exhaustive()
    }
}

/// The Fly [`DeployDriver`].
#[derive(Debug)]
pub struct FlyDeployDriver {
    config: FlyDeployConfig,
}

/// What `apply` will do, derived from a spec and validated before any call.
#[derive(Debug, Clone)]
struct FlyDeployPlan {
    app_name: String,
    org_slug: Option<String>,
    network: String,
    region: String,
    pool_size: u32,
    machine_config: MachineConfig,
    secrets_to_write: BTreeMap<String, String>,
    secrets_expected: BTreeSet<String>,
}

impl FlyDeployDriver {
    /// A driver with the given deploy-time config.
    pub fn new(config: FlyDeployConfig) -> Self {
        Self { config }
    }

    async fn apply_spec(&self, spec: &DeploymentSpec) -> Result<DeploymentRef, DeployError> {
        let plan = plan_deploy(spec, &self.config.env_lookup)?;
        let client = MachinesClient::new(
            self.config.api_base_url.clone(),
            plan.app_name.clone(),
            self.config.token_origin.clone(),
            self.config.client_policy.clone(),
        )?;
        ensure_app(&client, &plan).await?;
        write_secrets(&client, &plan).await?;
        let deployment = DeploymentRef::new(spec.name.clone(), spec.version.clone(), plan.app_name.clone());
        create_pool(&client, &deployment, &plan).await?;
        Ok(deployment)
    }

    /// Destroys every Machine of a retired version (the gateway calls this once
    /// the 7-day window of decision 6 has passed).
    ///
    /// @param deployment - the retired version (&DeploymentRef)
    /// @returns How many Machines were destroyed
    pub async fn retire_version(&self, deployment: &DeploymentRef) -> Result<usize, DeployError> {
        let client = MachinesClient::new(
            self.config.api_base_url.clone(),
            deployment.target.clone(),
            self.config.token_origin.clone(),
            self.config.client_policy.clone(),
        )?;
        let machines = client.list_machines(&pool_metadata(deployment)).await?;
        let mut destroyed = 0;
        for machine in machines.iter().filter(|machine| machine.belongs_to(&deployment.name, &deployment.version)) {
            match client.destroy_machine(&machine.id).await {
                Ok(()) => destroyed += 1,
                Err(error) if error.kind == ProviderErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(destroyed)
    }
}

impl DeployDriver for FlyDeployDriver {
    fn apply<'a>(&'a self, spec: &'a DeploymentSpec) -> BoxFuture<'a, Result<DeploymentRef, DeployError>> {
        Box::pin(self.apply_spec(spec))
    }
}

fn plan_deploy(spec: &DeploymentSpec, env_lookup: &EnvLookup) -> Result<FlyDeployPlan, DeployError> {
    let image = spec
        .image
        .clone()
        .filter(|image| !image.is_empty())
        .ok_or_else(|| DeployError::invalid("Fly needs a container image (`image`)"))?;
    let grace_secs = if spec.grace_secs == 0 { MAX_KILL_TIMEOUT_SECS } else { spec.grace_secs };
    if grace_secs > MAX_KILL_TIMEOUT_SECS {
        return Err(DeployError::invalid(format!(
            "grace_secs {grace_secs} exceeds Fly's kill_timeout limit of {MAX_KILL_TIMEOUT_SECS} s"
        )));
    }
    let pool_size = setting_u32(spec, "pool_size")?.unwrap_or(spec.max_replicas.max(1));
    if pool_size == 0 || pool_size > MAX_POOL_SIZE {
        return Err(DeployError::invalid(format!("pool_size must be 1..={MAX_POOL_SIZE}, got {pool_size}")));
    }
    let (secrets_to_write, secrets_expected) = resolve_secrets(spec, env_lookup)?;
    let machine_config = MachineConfig {
        image,
        env: machine_env(spec, &secrets_to_write, &secrets_expected)?,
        metadata: machine_metadata(spec),
        guest: Some(guest_config(spec)?),
        init: (!spec.command.is_empty()).then(|| InitConfig { cmd: spec.command.clone() }),
        restart: Some(RestartConfig { policy: "no".to_owned() }),
        stop_config: Some(StopConfig { signal: "SIGTERM".to_owned(), timeout: format!("{grace_secs}s") }),
        auto_destroy: false,
    };
    Ok(FlyDeployPlan {
        app_name: setting_str(spec, "app")?.unwrap_or_else(|| spec.name.clone()),
        org_slug: setting_str(spec, "org")?,
        network: setting_str(spec, "network")?.unwrap_or_default(),
        region: setting_str(spec, "region")?.unwrap_or_default(),
        pool_size,
        machine_config,
        secrets_to_write,
        secrets_expected,
    })
}

/// Splits secret references into values to write (`env:VAR`) and names that
/// must already exist in the app (`fly:NAME`).
fn resolve_secrets(
    spec: &DeploymentSpec,
    env_lookup: &EnvLookup,
) -> Result<(BTreeMap<String, String>, BTreeSet<String>), DeployError> {
    let mut secrets_to_write = BTreeMap::new();
    let mut secrets_expected = BTreeSet::new();
    for secret in &spec.secrets {
        if !is_env_name(&secret.env) {
            return Err(DeployError::invalid(format!("secret env name {:?} is not a valid variable name", secret.env)));
        }
        if let Some(variable) = secret.reference.strip_prefix("env:") {
            let value = env_lookup(variable).ok_or_else(|| {
                DeployError::invalid(format!("secret {}: deploy-time variable {variable} is unset", secret.env))
            })?;
            secrets_to_write.insert(secret.env.clone(), value);
        } else if let Some(name) = secret.reference.strip_prefix("fly:") {
            if name != secret.env {
                return Err(DeployError::unsupported(format!(
                    "secret {}: Fly injects app secrets under their own name, so `fly:{name}` must be `fly:{}`",
                    secret.env, secret.env
                )));
            }
            secrets_expected.insert(secret.env.clone());
        } else {
            return Err(DeployError::unsupported(format!(
                "secret {}: Fly supports `env:VAR` and `fly:NAME` references",
                secret.env
            )));
        }
    }
    Ok((secrets_to_write, secrets_expected))
}

fn machine_env(
    spec: &DeploymentSpec,
    secrets_to_write: &BTreeMap<String, String>,
    secrets_expected: &BTreeSet<String>,
) -> Result<BTreeMap<String, String>, DeployError> {
    if let Some(clash) =
        spec.env.keys().find(|name| secrets_to_write.contains_key(*name) || secrets_expected.contains(*name))
    {
        return Err(DeployError::invalid(format!("{clash} is both a plain env variable and a secret")));
    }
    let mut env = spec.env.clone();
    env.insert("GRAPHRT_DEPLOYMENT".to_owned(), spec.name.clone());
    env.insert("GRAPHRT_DEPLOYMENT_VERSION".to_owned(), spec.version.clone());
    Ok(env)
}

fn machine_metadata(spec: &DeploymentSpec) -> BTreeMap<String, String> {
    BTreeMap::from([
        (METADATA_POOL.to_owned(), "1".to_owned()),
        (METADATA_DEPLOYMENT.to_owned(), spec.name.clone()),
        (METADATA_VERSION.to_owned(), spec.version.clone()),
        (METADATA_FINGERPRINT.to_owned(), spec.workflow_fingerprint.clone()),
    ])
}

/// Fly guests: whole CPUs, memory in 256 MB steps.
fn guest_config(spec: &DeploymentSpec) -> Result<GuestConfig, DeployError> {
    let cpu_kind = setting_str(spec, "cpu_kind")?.unwrap_or_else(|| "shared".to_owned());
    if cpu_kind != "shared" && cpu_kind != "performance" {
        return Err(DeployError::invalid(format!("cpu_kind must be shared or performance, got {cpu_kind:?}")));
    }
    let cpus = spec.resources.cpu_millis.div_ceil(1000).max(1);
    let requested_memory = if spec.resources.memory_mib == 0 { DEFAULT_MEMORY_MB } else { spec.resources.memory_mib };
    Ok(GuestConfig { cpu_kind, cpus, memory_mb: requested_memory.div_ceil(256) * 256 })
}

async fn ensure_app(client: &MachinesClient, plan: &FlyDeployPlan) -> Result<(), DeployError> {
    if client.app_exists().await? {
        return Ok(());
    }
    let Some(org_slug) = &plan.org_slug else {
        return Err(DeployError::invalid(format!(
            "Fly app {} does not exist; set provider setting `org` to create it",
            plan.app_name
        )));
    };
    client.create_app(org_slug, &plan.network).await?;
    Ok(())
}

async fn write_secrets(client: &MachinesClient, plan: &FlyDeployPlan) -> Result<(), DeployError> {
    if !plan.secrets_to_write.is_empty() {
        let values = plan.secrets_to_write.iter().map(|(name, value)| (name.as_str(), Some(value.as_str()))).collect();
        client.update_secrets(values).await?;
    }
    if plan.secrets_expected.is_empty() {
        return Ok(());
    }
    let existing: BTreeSet<String> = client.list_secret_names().await?.into_iter().collect();
    match plan.secrets_expected.iter().find(|name| !existing.contains(*name)) {
        Some(missing) => Err(DeployError::invalid(format!(
            "Fly secret {missing} is not set on app {} (`fly secrets set {missing}=...`)",
            plan.app_name
        ))),
        None => Ok(()),
    }
}

async fn create_pool(
    client: &MachinesClient,
    deployment: &DeploymentRef,
    plan: &FlyDeployPlan,
) -> Result<(), DeployError> {
    let existing_names: BTreeSet<String> = client
        .list_machines(&pool_metadata(deployment))
        .await?
        .into_iter()
        .filter(|machine| machine.belongs_to(&deployment.name, &deployment.version))
        .map(|machine| machine.name)
        .collect();
    let config = serde_json::to_value(&plan.machine_config)
        .map_err(|error| DeployError::invalid(format!("encoding the Machine config: {error}")))?;
    for index in 0..plan.pool_size {
        let name = pool_machine_name(&deployment.name, &deployment.version, index);
        if existing_names.contains(&name) {
            continue;
        }
        let request =
            CreateMachineRequest { name, region: plan.region.clone(), config: config.clone(), skip_launch: true };
        match client.create_machine(&request).await {
            Err(error) if !is_name_taken(&error) => return Err(error.into()),
            _ => {}
        }
    }
    Ok(())
}

/// Whether suspend can work for this guest (Fly snapshots guests ≤ 2 GB).
pub fn suspend_supported(memory_mb: u32) -> bool {
    memory_mb <= MAX_SUSPEND_MEMORY_MB
}

fn setting_str(spec: &DeploymentSpec, key: &str) -> Result<Option<String>, DeployError> {
    match spec.provider_settings.get(key) {
        None => Ok(None),
        Some(serde_json::Value::String(value)) if !value.is_empty() => Ok(Some(value.clone())),
        Some(other) => {
            Err(DeployError::invalid(format!("provider setting {key} must be a non-empty string, got {other}")))
        }
    }
}

fn setting_u32(spec: &DeploymentSpec, key: &str) -> Result<Option<u32>, DeployError> {
    match spec.provider_settings.get(key) {
        None => Ok(None),
        Some(value) => value.as_u64().and_then(|number| u32::try_from(number).ok()).map(Some).ok_or_else(|| {
            DeployError::invalid(format!("provider setting {key} must be a positive integer, got {value}"))
        }),
    }
}

fn is_env_name(name: &str) -> bool {
    let mut characters = name.chars();
    characters.next().is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && characters.all(|character| character.is_ascii_alphanumeric() || character == '_')
}

#[cfg(test)]
mod tests {
    use graphrt_provider_api::{Resources, SecretRef};

    use super::*;

    fn spec() -> DeploymentSpec {
        DeploymentSpec {
            name: "chat".into(),
            version: "v7".into(),
            image: Some("ghcr.io/example/chat:1".into()),
            command: vec!["serve".into(), "/app/workflows".into(), "--addr".into(), "[::]:8080".into()],
            env: BTreeMap::from([("RUST_LOG".into(), "info".into())]),
            secrets: vec![
                SecretRef { env: "OPENAI_API_KEY".into(), reference: "env:LOCAL_OPENAI".into() },
                SecretRef { env: "SUPABASE_KEY".into(), reference: "fly:SUPABASE_KEY".into() },
            ],
            max_active_runs: 8,
            min_replicas: 0,
            max_replicas: 3,
            grace_secs: 0,
            resources: Resources { cpu_millis: 1500, memory_mib: 300 },
            workflow_fingerprint: "fp1".into(),
            provider_settings: BTreeMap::from([("region".into(), serde_json::json!("ams"))]),
        }
    }

    fn lookup() -> EnvLookup {
        Arc::new(|name| (name == "LOCAL_OPENAI").then(|| "sk-test-value".to_owned()))
    }

    #[test]
    fn plan_keeps_secret_values_out_of_the_machine_config() {
        let plan = plan_deploy(&spec(), &lookup()).unwrap();
        let encoded = serde_json::to_string(&plan.machine_config).unwrap();
        assert!(!encoded.contains("sk-test-value"));
        assert_eq!(plan.secrets_to_write.get("OPENAI_API_KEY").map(String::as_str), Some("sk-test-value"));
        assert!(plan.secrets_expected.contains("SUPABASE_KEY"));
    }

    #[test]
    fn plan_sets_kill_timeout_guest_and_pool() {
        let plan = plan_deploy(&spec(), &lookup()).unwrap();
        let stop_config = plan.machine_config.stop_config.unwrap();
        assert_eq!(stop_config, StopConfig { signal: "SIGTERM".into(), timeout: "300s".into() });
        assert_eq!(
            plan.machine_config.guest.unwrap(),
            GuestConfig { cpu_kind: "shared".into(), cpus: 2, memory_mb: 512 }
        );
        assert_eq!(plan.pool_size, 3);
        assert_eq!(plan.app_name, "chat");
        assert_eq!(plan.machine_config.restart.unwrap().policy, "no");
    }

    #[test]
    fn rejects_grace_above_fly_limit_and_unknown_references() {
        let mut too_long = spec();
        too_long.grace_secs = 600;
        assert!(plan_deploy(&too_long, &lookup()).is_err());
        let mut renamed = spec();
        renamed.secrets = vec![SecretRef { env: "A".into(), reference: "fly:B".into() }];
        assert!(plan_deploy(&renamed, &lookup()).is_err());
        let mut vault = spec();
        vault.secrets = vec![SecretRef { env: "A".into(), reference: "projects/p/secrets/s".into() }];
        assert!(plan_deploy(&vault, &lookup()).is_err());
    }

    #[test]
    fn missing_deploy_time_variable_is_an_error_without_value() {
        let error = plan_deploy(&spec(), &(Arc::new(|_: &str| None) as EnvLookup)).unwrap_err();
        assert!(error.message.contains("LOCAL_OPENAI"));
    }

    #[test]
    fn suspend_limit() {
        assert!(suspend_supported(2048));
        assert!(!suspend_supported(4096));
    }
}
