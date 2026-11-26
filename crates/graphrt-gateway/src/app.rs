//! Builds a running gateway from a [`GatewayConfig`] and serves it
//! (`graphrt gateway`): store, deployments on the local-process provider,
//! version reload, reclaim after a restart, and the background loops (outbox,
//! backups, idle release, reconciliation, version retirement, TTL purge,
//! cron).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use graphrt_core::store::{RunStore, now_ms};

use crate::auth::DeploymentAuth;
use crate::config::{DeploymentConfig, GatewayConfig};
use crate::deployment::{Deployment, VersionInfo};
use crate::limiter::Gcra;
use crate::local::{LocalProvider, LocalVersion};
use crate::pool::PrewarmRequest;
use crate::scheduler::CronJob;
use crate::server::{App, VersionHook, load_versions, router};
use crate::signature::{parse_keys, unix_now};
use crate::store::SqliteStore;
use crate::supervisor::{Gateway, SupervisorSettings};
use crate::warm::{ArrivalModel, PrewarmSource, WakeGate};

/// A gateway assembled from its config, not yet listening.
#[derive(Debug)]
pub struct Built {
    /// HTTP state.
    pub app: Arc<App>,
    /// Cron jobs per deployment.
    pub crons: Vec<(Arc<Deployment>, CronJob)>,
    /// Callback secrets per deployment.
    pub callback_secrets: HashMap<String, Vec<u8>>,
    /// Backup policy.
    pub backup: Option<crate::store::backup::BackupPolicy>,
    /// TTL of finished runs.
    pub ttl: Duration,
}

fn env_value(name: &str) -> Result<String, String> {
    std::env::var(name).map_err(|_| format!("environment variable {name} is not set"))
}

fn keys_of(config: &DeploymentConfig) -> Result<String, String> {
    match &config.inline_keys {
        Some(keys) => Ok(keys.clone()),
        None => env_value(&config.keys_env),
    }
}

fn build_deployment(
    config: &DeploymentConfig,
    provider: Arc<dyn graphrt_provider_api::Provider>,
    client: &reqwest::Client,
    wake_gate: &Arc<WakeGate>,
) -> Result<Deployment, String> {
    let keys_text = keys_of(config)?;
    let keys = parse_keys(&keys_text).map_err(|error| format!("{}: {error}", config.name))?;
    let auth = DeploymentAuth::new(config.api_keys.clone(), config.jwt.clone(), client)
        .map_err(|error| format!("{}: {error}", config.name))?;
    let mut pool_config = config.pool_config();
    pool_config.wake_gate = Some(wake_gate.clone());
    let mut deployment = Deployment::new(&config.name, auth, keys, provider, pool_config);
    if config.warm.predictive {
        let model = ArrivalModel::new(unix_now() as i64, config.warm.alpha, config.warm.threshold);
        deployment.predictor = Some((std::sync::Mutex::new(model), config.warm.lead_secs));
    }
    deployment.principal_limiter = config.rate_limit.map(|limit| Gcra::new(limit.into(), 100_000));
    deployment.input_headers = config
        .input_headers
        .iter()
        .map(|mapping| crate::deployment::InputHeader {
            header: mapping.header.to_ascii_lowercase(),
            ..mapping.clone()
        })
        .collect();
    deployment.open = config.open;
    Ok(deployment)
}

/// The warm hint of `deployment`'s pools: `POST /internal/v1/warm` to each
/// replica, in the background. Best effort: a replica that is gone, or an
/// agent older than the hint (`404`), only misses the head start.
pub fn warm_hint(
    gateway: &Arc<Gateway>,
    deployment: &str,
    key: Option<&crate::signature::GatewayKey>,
) -> Option<crate::pool::WarmHint> {
    let key = key?.clone();
    let name = deployment.to_owned();
    let gateway = Arc::downgrade(gateway);
    Some(crate::pool::WarmHint(Arc::new(move |addresses| {
        let (gateway, key, name) = (gateway.clone(), key.clone(), name.clone());
        graphrt_core::task::spawn(async move {
            let Some(gateway) = gateway.upgrade() else { return };
            for address in addresses {
                match gateway.agents.warm(&address, &key, &name).await {
                    // An agent older than the hint: nothing to warm there.
                    Ok(()) | Err(crate::agent_client::AgentError::Refused { status: 404, .. }) => {}
                    Err(error) => eprintln!("gateway: warm hint to {name}: {error}"),
                }
            }
        });
    })))
}

/// Assembles the gateway. `keys_by_deployment` receives each deployment's
/// raw key list so the local provider can hand it to its agents.
///
/// # Errors
/// Missing environment variables, bad keys, store failures.
pub async fn build(config: &GatewayConfig) -> Result<Built, String> {
    build_on(config, open_store(config)?).await
}

/// Opens the configured store (`:memory:` or a file).
///
/// # Errors
/// When SQLite cannot open it.
pub fn open_store(config: &GatewayConfig) -> Result<SqliteStore, String> {
    if config.store == Path::new(":memory:") { SqliteStore::in_memory() } else { SqliteStore::open(&config.store) }
        .map_err(|error| error.to_string())
}

/// [`build`] on an already opened (possibly fenced) store.
///
/// # Errors
/// Missing environment variables, bad keys, store failures.
pub async fn build_on(config: &GatewayConfig, store: SqliteStore) -> Result<Built, String> {
    crate::install_crypto_provider();
    let settings = SupervisorSettings {
        reclaim_grace: Duration::from_secs(config.reclaim_grace_secs),
        ..SupervisorSettings::default()
    };
    let gateway = Gateway::new(store, settings);
    let provider = Arc::new(
        LocalProvider::new(&config.socket_dir, Duration::from_secs(config.local_grace_secs))
            .map_err(|error| format!("socket dir: {error}"))?,
    );
    if provider.socket_dir() != config.socket_dir.as_path() {
        eprintln!(
            "graphrt gateway: socket_dir {} is too long for Unix sockets; agents listen in {}",
            config.socket_dir.display(),
            provider.socket_dir().display()
        );
    }
    let client =
        reqwest::Client::builder().timeout(Duration::from_secs(10)).build().map_err(|error| error.to_string())?;
    let mut app = App::new(gateway.clone());
    let wake_gate = Arc::new(WakeGate::new(config.wake_storm));
    app.wake_gate = Some(wake_gate.clone());
    let mut crons = Vec::new();
    let mut callback_secrets = HashMap::new();
    let mut keys_by_deployment = HashMap::new();
    // Remote providers are built once per name and shared by their deployments.
    let mut remote: HashMap<String, Arc<dyn graphrt_provider_api::Provider>> = HashMap::new();
    let mut retirers: HashMap<String, Arc<dyn crate::retire::VersionRetirer>> = HashMap::new();
    for (name, provider_config) in &config.providers {
        if let Some(retirer) =
            crate::providers::build_retirer(provider_config).map_err(|error| format!("provider {name}: {error}"))?
        {
            retirers.insert(name.clone(), retirer);
        }
        if !matches!(provider_config, crate::providers::ProviderConfig::Local) {
            let built =
                crate::providers::build_remote(provider_config).map_err(|error| format!("provider {name}: {error}"))?;
            remote.insert(name.clone(), built);
        }
    }
    for deployment_config in &config.deployments {
        let provider_name = deployment_config.provider.as_deref().unwrap_or("local");
        let chosen: Arc<dyn graphrt_provider_api::Provider> = match remote.get(provider_name) {
            Some(remote_provider) => remote_provider.clone(),
            None => {
                // Only deployments on the local provider get local agents.
                keys_by_deployment.insert(deployment_config.name.clone(), keys_of(deployment_config)?);
                provider.clone()
            }
        };
        let mut deployment = build_deployment(deployment_config, chosen, &client, &wake_gate)?;
        deployment.pool_config.warm_hint = warm_hint(&gateway, &deployment.name, deployment.signing_key());
        deployment.retirer = retirers.get(provider_name).cloned();
        let deployment = Arc::new(deployment);
        if let Some(name) = &deployment_config.callback_secret_env {
            callback_secrets.insert(deployment.name.clone(), env_value(name)?.into_bytes());
        }
        for hook in &deployment_config.hooks {
            let secret = hook.secret_env.as_deref().map(env_value).transpose()?.map(String::into_bytes);
            app.hooks.insert((deployment.name.clone(), hook.id.clone()), (hook.clone(), secret));
        }
        for cron in &deployment_config.crons {
            let mut schedule = crate::cron::Schedule::new(&cron.id, &cron.schedule, &cron.timezone)?;
            schedule.catchup = cron.catchup;
            schedule.max_catchup = cron.max_catchup;
            schedule.jitter_secs = cron.jitter_secs;
            schedule.prewarm_secs = cron.prewarm_secs;
            let mut body = serde_json::Map::new();
            body.insert("inputs".into(), serde_json::Value::Object(cron.inputs.clone()));
            crons.push((deployment.clone(), CronJob { schedule, body }));
        }
        gateway.add_deployment(deployment);
    }
    app.ip_limiter = config.per_ip.map(|limit| Gcra::new(limit.into(), 200_000));
    let admin_token = match &config.inline_admin_token {
        Some(token) => Some(token.clone()),
        None => config.admin_token_env.as_deref().map(env_value).transpose()?,
    };
    app.admin_token_hash = admin_token.map(|token| crate::hex::sha256_hex(token.as_bytes()));
    let local = provider.clone();
    let hook: VersionHook = Arc::new(move |deployment: &Deployment, info: &VersionInfo| {
        let Some(keys) = keys_by_deployment.get(&deployment.name).cloned() else { return };
        local.register(
            &info.reference(&deployment.name),
            LocalVersion {
                command: agent_command(&info.command, deployment.pool_config.max_active_runs),
                keys,
                env: Vec::new(),
            },
        );
    });
    load_versions(&gateway, Some(&hook)).await.map_err(|error| error.to_string())?;
    app.on_version = Some(hook);
    let backup = config.backup.as_ref().map(|backup| crate::store::backup::BackupPolicy {
        dir: backup.dir.clone(),
        interval: Duration::from_secs(backup.interval_secs.max(10)),
        keep: backup.keep,
    });
    Ok(Built {
        app: Arc::new(app),
        crons,
        callback_secrets,
        backup,
        ttl: Duration::from_secs(config.store_ttl_days * 86_400),
    })
}

/// The command of a local agent: `command` with `--max-active-runs` set to the
/// pool's limit when it does not set one itself. Otherwise an agent below the
/// pool's limit refuses the runs above its own (`429`) that the pool hands it.
pub fn agent_command(command: &[String], max_active_runs: u32) -> Vec<String> {
    let mut command = command.to_vec();
    let sets_limit =
        command.iter().any(|argument| argument == "--max-active-runs" || argument.starts_with("--max-active-runs="));
    if !sets_limit && !command.is_empty() {
        command.push("--max-active-runs".to_owned());
        command.push(max_active_runs.max(1).to_string());
    }
    command
}

/// Retires versions whose 7-day pool expired: closes their pool (replicas
/// released, new resumes get `410`) and frees their platform objects with the
/// deployment's retirer, retrying failed retirements on the next call.
pub async fn retire_expired(deployment: &Deployment, now: u64) {
    let mut versions =
        std::mem::take(&mut *deployment.pending_retirement.lock().unwrap_or_else(std::sync::PoisonError::into_inner));
    for retired in deployment.take_retired(now) {
        eprintln!(
            "{{\"audit\":\"version_retired\",\"deployment\":\"{}\",\"version\":\"{}\"}}",
            deployment.name, retired.info.version
        );
        retired.pool.close().await;
        versions.push(retired.info.reference(&deployment.name));
    }
    let Some(retirer) = &deployment.retirer else { return };
    if versions.is_empty() {
        return;
    }
    if let Err(error) = retirer.retire(&versions).await {
        eprintln!(
            "gateway: retiring old versions of {} on the platform failed (retried next hour): {error}",
            deployment.name
        );
        deployment.pending_retirement.lock().unwrap_or_else(std::sync::PoisonError::into_inner).extend(versions);
    } else {
        let names: Vec<&str> = versions.iter().map(|version| version.version.as_str()).collect();
        eprintln!(
            "{{\"audit\":\"version_retired_on_platform\",\"deployment\":\"{}\",\"versions\":{names:?}}}",
            deployment.name
        );
    }
}

/// Starts a predictive prewarm of `deployment` when its learned pattern
/// expects traffic within the lead (once per predicted slot), after a random
/// delay below the gateway's prewarm jitter and half the lead. Returns whether
/// one was started.
pub fn prewarm_predicted(deployment: &Deployment, unix_secs: u64) -> bool {
    let Some(lead) = deployment.predicts_traffic(unix_secs) else { return false };
    let Ok(current) = deployment.current() else { return false };
    current.pool.spawn_prewarm(PrewarmRequest {
        hold: ArrivalModel::hold(lead.as_secs()),
        ..PrewarmRequest::one(PrewarmSource::Predictive, lead / 2)
    });
    true
}

/// Predictive prewarm check and idle release (charging the keep-warm budget) every 5 s, reconciliation every minute (I7: at most one
/// `list` per minute per pool), retirement of 7-day-old versions, TTL purge hourly.
pub async fn maintenance(gateway: Arc<Gateway>, ttl: Duration) {
    let mut tick: u64 = 0;
    loop {
        tokio::time::sleep(Duration::from_secs(5)).await;
        tick += 1;
        for deployment in gateway.deployments() {
            prewarm_predicted(&deployment, unix_now());
            for version in deployment.versions() {
                version.pool.release_idle().await;
                if tick.is_multiple_of(12)
                    && let Err(error) = version.pool.reconcile().await
                {
                    eprintln!("gateway: reconcile {}@{}: {error}", deployment.name, version.info.version);
                }
            }
            if tick.is_multiple_of(720) {
                retire_expired(&deployment, now_ms()).await;
            }
        }
        if tick % 720 == 1 {
            let cutoff = now_ms().saturating_sub(ttl.as_millis() as u64);
            if let Err(error) = gateway.store.purge_before(cutoff).await {
                eprintln!("gateway: purge: {error}");
            }
        }
    }
}

/// Starts the background loops of `built`.
pub fn spawn_background(built: &Built) {
    let gateway = built.app.gateway.clone();
    let callback_client = reqwest::Client::builder().timeout(Duration::from_secs(10)).build().unwrap_or_default();
    graphrt_core::task::spawn(crate::outbox::run_outbox(
        gateway.clone(),
        callback_client,
        built.callback_secrets.clone(),
    ));
    if let Some(policy) = built.backup.clone() {
        graphrt_core::task::spawn(crate::store::backup::run_backups(gateway.store.clone(), policy));
    }
    graphrt_core::task::spawn(maintenance(gateway.clone(), built.ttl));
    for (deployment, job) in &built.crons {
        graphrt_core::task::spawn(crate::scheduler::run_job(gateway.clone(), deployment.clone(), job.clone()));
    }
}

/// Serves until SIGTERM / SIGINT. Agents keep running and report their runs as
/// orphans, so a restarted gateway reclaims them (plan §3.4).
///
/// # Errors
/// Bind failures and build errors.
pub async fn serve(config: GatewayConfig) -> Result<(), String> {
    serve_with(config, Vec::new()).await
}

/// [`serve`], registering `versions` (deployment, version) first if they are new (`--dev`).
///
/// # Errors
/// Bind failures and build errors.
pub async fn serve_with(config: GatewayConfig, versions: Vec<(String, VersionInfo)>) -> Result<(), String> {
    if config.ha.is_some() {
        return crate::ha::run::serve_active_passive(config, versions).await;
    }
    let started = tokio::time::Instant::now();
    let built = build(&config).await?;
    register_versions(&built, versions);
    let (queued, running) = built.app.gateway.recover().await.map_err(|error| error.to_string())?;
    spawn_background(&built);
    let listener = tokio::net::TcpListener::bind(&config.listen)
        .await
        .map_err(|error| format!("binding {}: {error}", config.listen))?;
    let address = listener.local_addr().map_err(|error| error.to_string())?;
    eprintln!(
        "graphrt gateway listening on http://{address} ({} deployments, reclaiming {running} running and {queued} queued runs, ready in {} ms)",
        built.app.gateway.deployments().len(),
        started.elapsed().as_millis()
    );
    let service = router(built.app.clone()).into_make_service_with_connect_info::<SocketAddr>();
    axum::serve(public_listener(listener), service)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .map_err(|error| error.to_string())
}

/// The gateway's public listener with `TCP_NODELAY` on every accepted
/// connection. Without it, the second and later small writes of a streamed
/// (SSE) response wait for the client's delayed ACK: about 40 ms at the end of
/// every stream and in the first token of some, on Linux (`graphrt serve`
/// does the same on its listener).
pub fn public_listener(listener: tokio::net::TcpListener) -> PublicListener {
    use axum::serve::ListenerExt as _;
    listener.tap_io(set_nodelay)
}

/// See [`public_listener`].
pub type PublicListener = axum::serve::TapIo<tokio::net::TcpListener, fn(&mut tokio::net::TcpStream)>;

fn set_nodelay(stream: &mut tokio::net::TcpStream) {
    if let Err(error) = stream.set_nodelay(true) {
        eprintln!("gateway: TCP_NODELAY on a client connection: {error}");
    }
}

/// Registers `versions` (deployment, version) that are new (`--dev`).
pub fn register_versions(built: &Built, versions: Vec<(String, VersionInfo)>) {
    for (name, info) in versions {
        let Some(deployment) = built.app.gateway.deployment(&name) else { continue };
        if deployment.versions().iter().any(|entry| entry.info.version == info.version) {
            continue;
        }
        if let Some(hook) = &built.app.on_version {
            hook(&deployment, &info);
        }
        deployment.add_version(info, now_ms());
    }
}

/// Resolves on SIGTERM or SIGINT (Ctrl+C, Ctrl+Break, console close or
/// system shutdown on Windows).
pub async fn shutdown_signal() {
    crate::os::shutdown_signal().await;
}

#[cfg(test)]
mod tests {
    use graphrt_provider_api::conformance::MemoryProvider;

    use super::*;
    use crate::warm::ArrivalModel;

    #[test]
    fn local_agents_get_the_pool_limit_unless_their_command_sets_one() {
        let command = |items: &[&str]| items.iter().map(|item| (*item).to_owned()).collect::<Vec<_>>();
        assert_eq!(
            agent_command(&command(&["graphrt", "serve", "a.yaml"]), 1000),
            command(&["graphrt", "serve", "a.yaml", "--max-active-runs", "1000"])
        );
        let own = command(&["graphrt", "serve", "a.yaml", "--max-active-runs", "4"]);
        assert_eq!(agent_command(&own, 1000), own);
        let own = command(&["graphrt", "serve", "--max-active-runs=4", "a.yaml"]);
        assert_eq!(agent_command(&own, 1000), own);
        assert!(agent_command(&[], 8).is_empty());
    }

    #[tokio::test]
    async fn public_connections_have_tcp_nodelay() {
        use axum::serve::Listener as _;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let mut listener = public_listener(listener);
        let _client = tokio::net::TcpStream::connect(address).await.unwrap();
        let (accepted, _) = listener.accept().await;
        assert!(accepted.nodelay().unwrap(), "small SSE writes must not wait for the client's delayed ACK");
    }

    #[tokio::test(start_paused = true)]
    async fn learned_traffic_is_prewarmed_once_per_slot() {
        let provider = Arc::new(MemoryProvider::new(Duration::from_millis(10)));
        let key = crate::signature::GatewayKey::new("k", &[7; 32]).unwrap();
        let mut deployment =
            Deployment::new("shop", DeploymentAuth::default(), vec![key], provider.clone(), Default::default());
        // 1970-01-05 was a Monday: the four days below are weekdays.
        let nine = 4 * 86_400 + 9 * 3600;
        deployment.predictor = Some((std::sync::Mutex::new(ArrivalModel::new(0, 0.3, 0.6)), 60));
        let info = VersionInfo {
            version: "v1".into(),
            workflow: "wf".into(),
            fingerprint: String::new(),
            target: "mem".into(),
            command: Vec::new(),
            secret_inputs: None,
            activated_ms: 0,
            superseded_ms: None,
        };
        deployment.add_version(info, 0);
        for day in 0..3 {
            deployment.record_arrival(day * 86_400 + nine + 10);
        }
        assert!(!prewarm_predicted(&deployment, 3 * 86_400 + nine - 120), "not yet within the lead");
        assert!(prewarm_predicted(&deployment, 3 * 86_400 + nine - 60));
        assert!(!prewarm_predicted(&deployment, 3 * 86_400 + nine - 55), "once per slot");
        tokio::time::sleep(Duration::from_secs(1)).await;
        let pool = deployment.current().unwrap().pool;
        assert_eq!(pool.stats().replicas, 1);
        assert_eq!(pool.stats().prewarm.started, [1, 0, 0, 0]);
        let mut cold = Deployment::new(
            "cold",
            DeploymentAuth::default(),
            vec![crate::signature::GatewayKey::new("k", &[7; 32]).unwrap()],
            provider,
            Default::default(),
        );
        cold.predictor = None;
        cold.record_arrival(nine);
        assert!(!prewarm_predicted(&cold, nine), "predictive prewarm is off without a model");
    }
}
