//! Azure Container Apps (feature `provider-aca`).
//!
//! - [`AcaProvider`]: run-time [`Provider`] with `Addressing::Url` and a
//!   platform scaler. The HTTP scale rule (`minReplicas = 0`,
//!   `concurrentRequests = max_active_runs`) wakes a replica on the first
//!   request, so `wake` resolves the version's label URL and sends one warm-up
//!   request (single-flight per URL); `release` is a no-op; `list` reports
//!   replicas for reconciliation. Gateway RBAC: read + revision deactivate only.
//! - [`AcaDeployDriver`]: `graphrt deploy` side, `PUT containerApps` with the
//!   spec, then labels the new revision `v<N>` (deploy-time credentials).
//! - Ingress timeouts ([`IngressMode`]): see `docs/deploy-azure.md`.

mod deploy;
mod retire;
pub mod revisions;
pub mod spec;

use crate::http::json_at;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub use deploy::AcaDeployDriver;
pub use spec::{AcaAppSpec, AppIdentity, EnvVar, RegistryRef};

use crate::api::{
    Addressing, Capabilities, DeploymentRef, Endpoint, Provider, ProviderError, ReleaseMode, ReplicaRef, ReplicaState,
    Scaler,
};
use crate::arm::ArmClient;
use revisions::{Revision, label_fqdn, labelled_versions};
use spec::version_label;

/// Default `Microsoft.App` api-version (GA).
pub const DEFAULT_API_VERSION: &str = "2025-01-01";
/// Standard ingress request timeout ("Request time out is 240 seconds").
pub const STANDARD_INGRESS_TIMEOUT_SECS: u64 = 240;
/// How long a successful warm-up counts, so a burst of wakes sends one probe.
const WARM_TTL: Duration = Duration::from_secs(5);
const PROBE_INTERVAL: Duration = Duration::from_millis(250);

/// How the environment's ingress limits a long SSE stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IngressMode {
    /// Standard ingress. Microsoft documents a 240 s request timeout without
    /// saying whether it is idle or total, so the gateway is told both: the
    /// 15 s heartbeat covers the idle reading, and a planned preempt at
    /// 240 - 60 s covers the total reading. Each node of a run must then
    /// finish within 180 s; use `Premium` for longer nodes.
    Standard,
    /// Standard ingress after the real-Azure C6 test proved the 240 s limit is
    /// idle-only: heartbeats keep streams open, no request cap.
    StandardIdleVerified,
    /// Premium ingress (`ingressConfiguration.requestIdleTimeout`, 4-30 min,
    /// documented as idle-based): no request cap.
    Premium { request_idle_timeout_minutes: u32 },
}

/// Provider settings from the gateway operator's configuration.
#[derive(Clone, Debug)]
pub struct AcaConfig {
    pub subscription_id: String,
    pub resource_group: String,
    pub api_version: String,
    pub ingress: IngressMode,
    /// `terminationGracePeriodSeconds` the deploy driver sets (≤ 600).
    pub grace_secs: u32,
    /// Agent health path probed on wake.
    pub health_path: String,
    pub wake_timeout: Duration,
    pub wake_p95_target_ms: u64,
    /// How long version N-1 stays wakeable after N was deployed (plan: 7 days).
    pub old_version_retention: Duration,
    /// `https` in Azure; `http` only for fake servers in tests.
    pub invoke_scheme: String,
    /// DNS overrides for the warm-up client (`host` → address), for private
    /// DNS setups the gateway host cannot resolve, and for fake servers.
    pub resolve_overrides: Vec<(String, std::net::SocketAddr)>,
}

impl AcaConfig {
    pub fn new(subscription_id: impl Into<String>, resource_group: impl Into<String>) -> Self {
        Self {
            subscription_id: subscription_id.into(),
            resource_group: resource_group.into(),
            api_version: DEFAULT_API_VERSION.into(),
            ingress: IngressMode::Standard,
            grace_secs: spec::DEFAULT_GRACE_SECS,
            health_path: "/healthz".into(),
            wake_timeout: Duration::from_secs(60),
            wake_p95_target_ms: 10_000,
            old_version_retention: Duration::from_secs(7 * 86_400),
            invoke_scheme: "https".into(),
            resolve_overrides: Vec::new(),
        }
    }

    /// @throws `ProviderError::Fatal` for ids that could escape the ARM path.
    pub fn validate(&self) -> Result<(), ProviderError> {
        let safe = |value: &str| {
            !value.is_empty()
                && value.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '(' | ')'))
        };
        if !safe(&self.subscription_id) || !safe(&self.resource_group) {
            return Err(ProviderError::Fatal("invalid subscription id or resource group".into()));
        }
        if let IngressMode::Premium { request_idle_timeout_minutes } = self.ingress
            && !(4..=30).contains(&request_idle_timeout_minutes)
        {
            return Err(ProviderError::Fatal("premium ingress requestIdleTimeout must be 4..=30 minutes".into()));
        }
        if self.grace_secs > spec::MAX_GRACE_SECS {
            return Err(ProviderError::Fatal("grace_secs must be <= 600".into()));
        }
        Ok(())
    }

    /// ARM path of `app` plus `suffix`, with the api-version.
    pub fn app_path(&self, app: &str, suffix: &str) -> String {
        format!(
            "/subscriptions/{}/resourceGroups/{}/providers/Microsoft.App/containerApps/{app}{suffix}?api-version={}",
            self.subscription_id, self.resource_group, self.api_version
        )
    }

    /// The provider's capabilities under this configuration.
    pub fn capabilities(&self) -> Capabilities {
        let (max_request_secs, idle_timeout_secs) = match self.ingress {
            IngressMode::Standard => (Some(STANDARD_INGRESS_TIMEOUT_SECS), STANDARD_INGRESS_TIMEOUT_SECS),
            IngressMode::StandardIdleVerified => (None, STANDARD_INGRESS_TIMEOUT_SECS),
            IngressMode::Premium { request_idle_timeout_minutes } => {
                (None, u64::from(request_idle_timeout_minutes) * 60)
            }
        };
        Capabilities {
            addressing: Addressing::Url,
            scaler: Scaler::Platform,
            max_request_secs,
            idle_timeout_secs: Some(idle_timeout_secs),
            max_grace_secs: u64::from(self.grace_secs),
            // Consumption replicas keep their vCPU while running (not request-billed).
            background_cpu: true,
            supports_suspend: false,
            wake_p95_target_ms: self.wake_p95_target_ms,
        }
    }
}

/// Whether a heartbeat every `interval` keeps a stream open under `capabilities`:
/// at least two heartbeats must fit in the idle timeout, so one delayed
/// heartbeat does not cut the stream.
pub fn heartbeat_keeps_stream_open(interval: Duration, capabilities: &Capabilities) -> bool {
    capabilities.idle_timeout_secs.is_none_or(|idle| interval.as_secs_f64() * 2.0 <= idle as f64)
}

/// When the gateway must preempt a run to stay under the request cap (`cap - 60 s`).
pub fn planned_preempt_after(capabilities: &Capabilities) -> Option<Duration> {
    capabilities.max_request_secs.map(|cap| Duration::from_secs(cap.saturating_sub(60)))
}

/// Single-flight warm-up state of one URL.
#[derive(Debug, Default)]
struct WarmState {
    warmed_at: Option<Instant>,
}

/// The run-time Container Apps provider.
#[derive(Debug)]
pub struct AcaProvider {
    arm: ArmClient,
    config: AcaConfig,
    probe_client: reqwest::Client,
    urls: RwLock<HashMap<(String, u64), String>>,
    /// Serialises cache misses, so a wake storm on a cold cache sends one ARM GET.
    resolving: tokio::sync::Mutex<()>,
    warm: Mutex<HashMap<String, Arc<tokio::sync::Mutex<WarmState>>>>,
}

impl AcaProvider {
    /// @throws `ProviderError::Fatal` for an invalid configuration.
    pub fn new(arm: ArmClient, config: AcaConfig) -> Result<Self, ProviderError> {
        config.validate()?;
        let probe_client = build_probe_client(&config.resolve_overrides)?;
        Ok(Self {
            arm,
            config,
            probe_client,
            urls: RwLock::new(HashMap::new()),
            resolving: tokio::sync::Mutex::new(()),
            warm: Mutex::new(HashMap::new()),
        })
    }

    pub fn config(&self) -> &AcaConfig {
        &self.config
    }

    /// The label URL of `deployment`'s version, resolved once from ARM.
    ///
    /// @throws `ProviderError::NotFound` when the version has no label (retired or never deployed).
    pub async fn resolve_url(&self, deployment: &DeploymentRef) -> Result<String, ProviderError> {
        let key = (deployment.resource.clone(), deployment.version);
        if let Some(url) = read_lock(&self.urls).get(&key) {
            return Ok(url.clone());
        }
        let _resolving = self.resolving.lock().await;
        if let Some(url) = read_lock(&self.urls).get(&key) {
            return Ok(url.clone());
        }
        let app = checked_app_name(&deployment.resource)?;
        let response = self.arm.get(&self.config.app_path(app, "")).await?;
        let fqdn = json_at(&response.body, "/properties/configuration/ingress/fqdn")
            .as_str()
            .ok_or_else(|| ProviderError::Fatal(format!("container app {app} has no ingress FQDN")))?;
        if !labelled_versions(&response.body).contains(&deployment.version) {
            return Err(ProviderError::NotFound(format!(
                "version {} of {app} is retired or was never deployed",
                version_label(deployment.version)
            )));
        }
        let host = label_fqdn(fqdn, &version_label(deployment.version))
            .ok_or_else(|| ProviderError::Fatal(format!("unexpected ingress FQDN {fqdn:?}")))?;
        let url = format!("{}://{host}", self.config.invoke_scheme);
        write_lock(&self.urls).insert(key, url.clone());
        Ok(url)
    }

    /// Send one warm-up request to `url` (waking a replica from zero); concurrent
    /// callers for the same URL wait for that one request.
    ///
    /// @throws `ProviderError::Transient` when no replica answers within `wake_timeout`.
    pub async fn warm_up(&self, url: &str) -> Result<(), ProviderError> {
        let state = {
            let mut warm = match self.warm.lock() {
                Ok(warm) => warm,
                Err(poisoned) => poisoned.into_inner(),
            };
            warm.entry(url.to_string()).or_default().clone()
        };
        let mut state = state.lock().await;
        if state.warmed_at.is_some_and(|at| at.elapsed() < WARM_TTL) {
            return Ok(());
        }
        let probe_url = format!("{url}{}", self.config.health_path);
        let deadline = Instant::now() + self.config.wake_timeout;
        loop {
            let answered = self.probe_client.get(&probe_url).send().await.map(|response| response.status().as_u16());
            match answered {
                Ok(status) if (200..300).contains(&status) => {
                    state.warmed_at = Some(Instant::now());
                    return Ok(());
                }
                // The environment's ingress answers 404 for an unknown label.
                Ok(404) => return Err(ProviderError::NotFound(format!("{url} has no revision behind it"))),
                _ if Instant::now() >= deadline => {
                    return Err(ProviderError::Transient(format!("no replica answered {url} within the wake timeout")));
                }
                _ => tokio::time::sleep(PROBE_INTERVAL).await,
            }
        }
    }

    /// Revisions of `app` as ARM reports them.
    ///
    /// @throws `ProviderError` from ARM.
    pub async fn revisions(&self, app: &str) -> Result<Vec<Revision>, ProviderError> {
        retire::list_revisions(&self.arm, &self.config, app).await
    }

    /// Retire versions older than the retention window and deactivate stray
    /// unlabelled revisions (see [`retire::retire_old_versions`]). Needs the
    /// optional write role; the gateway answers `410 version_retired` from
    /// its own version records either way.
    ///
    /// @throws `ProviderError` from ARM.
    pub async fn retire_old_versions(&self, app: &str, now_unix: u64) -> Result<Vec<String>, ProviderError> {
        let retired = retire::retire_old_versions(&self.arm, &self.config, app, now_unix).await?;
        write_lock(&self.urls).retain(|(cached_app, version), _| {
            cached_app != app || !retired.contains(&spec::revision_name(app, *version))
        });
        Ok(retired)
    }
}

impl Provider for AcaProvider {
    fn capabilities(&self) -> Capabilities {
        self.config.capabilities()
    }

    async fn wake(&self, deployment: &DeploymentRef, replicas: u32) -> Result<Vec<Endpoint>, ProviderError> {
        let url = self.resolve_url(deployment).await?;
        if replicas > 0
            && let Err(error) = self.warm_up(&url).await
        {
            if matches!(error, ProviderError::NotFound(_)) {
                write_lock(&self.urls).remove(&(deployment.resource.clone(), deployment.version));
            }
            return Err(error);
        }
        Ok(vec![Endpoint::Url(url)])
    }

    /// The platform scales in after the HTTP rule's cooldown; nothing to do.
    async fn release(&self, _replica: &ReplicaRef, _mode: ReleaseMode) -> Result<(), ProviderError> {
        Ok(())
    }

    async fn list(&self, deployment: &DeploymentRef) -> Result<Vec<ReplicaState>, ProviderError> {
        let app = checked_app_name(&deployment.resource)?;
        let mut replicas = Vec::new();
        for revision in self.revisions(app).await?.into_iter().filter(|revision| revision.active) {
            let path = self.config.app_path(app, &format!("/revisions/{}/replicas", revision.name));
            let response = match self.arm.get(&path).await {
                Ok(response) => response,
                // A revision deactivated between the two calls.
                Err(ProviderError::NotFound(_)) => continue,
                Err(error) => return Err(error),
            };
            for replica in json_at(&response.body, "/value").as_array().into_iter().flatten() {
                let Some(id) = json_at(replica, "/name").as_str() else { continue };
                let state = json_at(replica, "/properties/runningState").as_str().unwrap_or("Unknown");
                replicas.push(ReplicaState {
                    id: id.to_string(),
                    revision: revision.name.clone(),
                    running: state.eq_ignore_ascii_case("Running"),
                });
            }
        }
        Ok(replicas)
    }
}

/// The warm-up client: no redirects, short timeouts, optional DNS overrides.
fn build_probe_client(overrides: &[(String, std::net::SocketAddr)]) -> Result<reqwest::Client, ProviderError> {
    let _already_installed = rustls::crypto::ring::default_provider().install_default();
    let mut builder = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none());
    for (host, address) in overrides {
        builder = builder.resolve(host, *address);
    }
    builder.build().map_err(|error| ProviderError::Fatal(format!("cannot build HTTP client: {error}")))
}

/// Unix seconds now (for `retire_old_versions` callers).
pub fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|elapsed| elapsed.as_secs()).unwrap_or(0)
}

/// `app` if it is a valid container app name (it is spliced into ARM paths).
fn checked_app_name(app: &str) -> Result<&str, ProviderError> {
    if spec::is_app_name(app) {
        Ok(app)
    } else {
        Err(ProviderError::Fatal(format!("invalid container app name {app:?}")))
    }
}

fn read_lock<T>(lock: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    match lock.read() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn write_lock<T>(lock: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    match lock.write() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capabilities_follow_the_ingress_mode() {
        let mut config = AcaConfig::new("sub", "rg");
        let standard = config.capabilities();
        assert_eq!(standard.addressing, Addressing::Url);
        assert_eq!(standard.scaler, Scaler::Platform);
        assert_eq!(standard.max_request_secs, Some(240));
        assert_eq!(planned_preempt_after(&standard), Some(Duration::from_secs(180)));
        assert!(heartbeat_keeps_stream_open(Duration::from_secs(15), &standard));
        assert!(!heartbeat_keeps_stream_open(Duration::from_secs(150), &standard));

        config.ingress = IngressMode::Premium { request_idle_timeout_minutes: 30 };
        let premium = config.capabilities();
        assert_eq!((premium.max_request_secs, premium.idle_timeout_secs), (None, Some(1800)));
        assert_eq!(planned_preempt_after(&premium), None);

        config.ingress = IngressMode::Premium { request_idle_timeout_minutes: 31 };
        assert!(config.validate().is_err());
    }

    #[test]
    fn config_rejects_path_injection() {
        assert!(AcaConfig::new("sub/../x", "rg").validate().is_err());
        assert!(AcaConfig::new("sub", "rg?x=1").validate().is_err());
        assert!(checked_app_name("bot/revisions").is_err());
    }
}
