//! The run-time Cloud Run provider of the gateway.
//!
//! Cloud Run scales itself on incoming requests, so this is an
//! `Addressing::Url` / `Scaler::Platform` provider:
//! - `wake` resolves the version's tag URL from the Admin API (never from the
//!   agent) and sends one authenticated `GET /readyz`, which is what makes
//!   Cloud Run start an instance from zero. Concurrent wakes share that probe.
//!   Not `/healthz`: Cloud Run's front end reserves that path and answers it
//!   with its own 404 before the request reaches the container.
//! - `release` is a no-op: Cloud Run scales in by itself once no request is open.
//! - `list` reports the one pseudo-replica `url` for reconciliation.
//!
//! Request-based billing (`cpuIdle = true`): an instance only has CPU while a
//! request is open. The gateway holds every run's stream for the whole run, so
//! runs keep their CPU; anything an agent would do after the stream closed is
//! throttled to near zero. Shutdown grace is a fixed 10 s, so drain always
//! preempts (decision 1), and runs reaching `request_timeout_secs - 60 s` are
//! preempted and re-dispatched by the gateway.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use futures::FutureExt as _;
use futures::future::Shared;
use graphrt_provider_api::http::{JitterRng, RetryPolicy, SecretString};
use graphrt_provider_api::{
    Addressing, BoxFuture, Capabilities, DeploymentRef, Endpoint, EndpointAddress, Provider, ProviderError,
    ProviderErrorKind, ReleaseMode, ReplicaPhase, ReplicaRef, ReplicaState, Scaler,
};
use tokio::time::Instant;

use super::admin::AdminApi;
use super::config::{CloudRunConfig, InvokeAuth, is_allowed_endpoint};
use super::naming::{service_id, version_tag};
use super::status::CloudRunServiceStatus;
use crate::auth::GcpAuth;
use crate::http_client::{ClientOptions, build_client};

/// Header carrying the invoker ID token. Cloud Run validates it and removes its
/// signature before the request reaches the container.
pub const INVOKE_AUTH_HEADER: &str = "x-serverless-authorization";
/// Id of the single pseudo-replica of a URL-addressed deployment.
pub const URL_REPLICA_ID: &str = "url";
/// Cloud Run sends SIGTERM and kills the instance 10 s later.
pub const SHUTDOWN_GRACE_SECS: u64 = 10;
/// Wake p95 target of plan §5 (startup CPU boost, image cached by the platform).
pub const WAKE_P95_TARGET_MS: u64 = 2_000;
/// Path of the wake probe. Cloud Run's front end keeps `/healthz` for itself
/// (it answers `404` without starting an instance), so the probe asks the
/// agent's readiness endpoint, which also says `503` while it drains.
pub const WAKE_PROBE_PATH: &str = "/readyz";

type SharedWake = Shared<BoxFuture<'static, Result<Endpoint, ProviderError>>>;

/// Cloud Run provider. Cheap to clone.
#[derive(Clone)]
pub struct CloudRunProvider {
    inner: Arc<ProviderInner>,
}

struct ProviderInner {
    config: CloudRunConfig,
    admin: AdminApi,
    auth: GcpAuth,
    invoke: reqwest::Client,
    policy: RetryPolicy,
    rng: JitterRng,
    services: Mutex<HashMap<String, (CloudRunServiceStatus, Instant)>>,
    wakes: Mutex<HashMap<String, SharedWake>>,
    warm: Mutex<HashMap<String, Instant>>,
}

impl std::fmt::Debug for CloudRunProvider {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CloudRunProvider")
            .field("parent", &self.inner.config.parent())
            .field("principal", &self.inner.auth.principal())
            .finish_non_exhaustive()
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl CloudRunProvider {
    /// A provider for `config.project` / `config.region` using `auth` (the
    /// gateway's run-time identity: `roles/run.viewer` + `roles/run.invoker`).
    ///
    /// @throws `ProviderError` (fatal) on an invalid configuration.
    pub fn new(config: CloudRunConfig, auth: GcpAuth) -> Result<Self, ProviderError> {
        let admin = AdminApi::new(&config, auth.access_tokens())?;
        // No whole-request timeout: run streams last up to 60 minutes.
        let invoke = build_client(ClientOptions { request_timeout: None, bypass_proxy: false })?;
        let policy =
            RetryPolicy { max_attempts: 4, base_delay: Duration::from_millis(100), max_delay: Duration::from_secs(2) };
        Ok(Self {
            inner: Arc::new(ProviderInner {
                config,
                admin,
                auth,
                invoke,
                policy,
                rng: JitterRng::from_entropy(),
                services: Mutex::new(HashMap::new()),
                wakes: Mutex::new(HashMap::new()),
                warm: Mutex::new(HashMap::new()),
            }),
        })
    }

    /// The service resource of a deployment: its `target` when set (it must be
    /// in the configured project/region), else derived from its name.
    ///
    /// @throws `ProviderError` (fatal) for a target outside the configured parent.
    pub fn service_resource(&self, deployment: &DeploymentRef) -> Result<String, ProviderError> {
        let parent = self.inner.config.parent();
        if deployment.target.is_empty() {
            return Ok(format!(
                "{parent}/services/{}",
                service_id(&self.inner.config.service_prefix, &deployment.name)
            ));
        }
        let id = deployment.target.strip_prefix(&format!("{parent}/services/")).unwrap_or_default();
        if id.is_empty() || !id.bytes().all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-') {
            return Err(ProviderError::fatal(format!("target `{}` is not a service in {parent}", deployment.target)));
        }
        Ok(deployment.target.clone())
    }

    /// The header the gateway adds to every request it sends to this
    /// deployment (`X-Serverless-Authorization: Bearer <ID token>`), or `None`
    /// with [`InvokeAuth::None`]. The token is cached until `exp - 5 min`.
    ///
    /// @param force_refresh - after the service answered 401/403 once
    pub async fn invoke_authorization(
        &self,
        deployment: &DeploymentRef,
        force_refresh: bool,
    ) -> Result<Option<(&'static str, SecretString)>, ProviderError> {
        if self.inner.config.invoke_auth == InvokeAuth::None {
            return Ok(None);
        }
        let resource = self.service_resource(deployment)?;
        let status = self.inner.status(&resource, false).await?;
        let audience = status.audience.ok_or_else(|| ProviderError::not_found(format!("{resource} has no URL yet")))?;
        let token = self.inner.auth.id_tokens(&audience).token(force_refresh).await?;
        Ok(Some((INVOKE_AUTH_HEADER, SecretString::new(format!("Bearer {}", token.expose())))))
    }

    /// The service as Cloud Run reports it now (bypasses the URL cache), for
    /// `graphrt gateway status` and drift checks.
    pub async fn service_status(&self, deployment: &DeploymentRef) -> Result<CloudRunServiceStatus, ProviderError> {
        let resource = self.service_resource(deployment)?;
        self.inner.status(&resource, true).await
    }
}

impl ProviderInner {
    /// Service status, from the cache when younger than `url_cache_ttl`.
    async fn status(&self, resource: &str, force: bool) -> Result<CloudRunServiceStatus, ProviderError> {
        let cached = if force { None } else { lock(&self.services).get(resource).cloned() };
        if let Some((status, fetched_at)) = cached
            && fetched_at.elapsed() < self.config.url_cache_ttl
        {
            return Ok(status);
        }
        let service = self
            .admin
            .get_service(resource)
            .await?
            .ok_or_else(|| ProviderError::not_found(format!("Cloud Run service {resource} does not exist")))?;
        let status = CloudRunServiceStatus::from_service(&service);
        lock(&self.services).insert(resource.to_owned(), (status.clone(), Instant::now()));
        Ok(status)
    }

    /// URL of the deployment version's tag, refreshed once when the cached
    /// service does not know the tag (a deploy just happened).
    async fn version_url(&self, resource: &str, tag: &str) -> Result<(String, CloudRunServiceStatus), ProviderError> {
        for force in [false, true] {
            let status = self.status(resource, force).await?;
            if let Some(url) = status.url_for_tag(tag) {
                if !is_allowed_endpoint(url) {
                    return Err(ProviderError::fatal(format!("service URL `{url}` is not https")));
                }
                return Ok((url.trim_end_matches('/').to_owned(), status));
            }
        }
        Err(ProviderError::not_found(format!("version tag `{tag}` of {resource} is retired or not deployed")))
    }

    async fn wake_once(
        self: Arc<Self>,
        deployment: DeploymentRef,
        resource: String,
    ) -> Result<Endpoint, ProviderError> {
        let tag = version_tag(&deployment.version);
        let (url, status) = self.version_url(&resource, &tag).await?;
        let probe = self.probe(&url, status.audience.as_deref());
        tokio::time::timeout(self.config.wake_timeout, probe).await.map_err(|_| {
            ProviderError::transient(format!("{url} did not answer /readyz within {:?}", self.config.wake_timeout))
        })??;
        lock(&self.warm).insert(format!("{resource}#{tag}"), Instant::now());
        Ok(Endpoint {
            replica: ReplicaRef { deployment, id: URL_REPLICA_ID.to_owned() },
            address: EndpointAddress::Http(url),
        })
    }

    /// `GET {url}/readyz` with the invoker token; retries 5xx/network errors
    /// (a draining instance answers 503) with jitter and refreshes the token
    /// once after 401/403.
    async fn probe(&self, url: &str, audience: Option<&str>) -> Result<(), ProviderError> {
        let health_url = format!("{url}{WAKE_PROBE_PATH}");
        let mut force_refresh = false;
        loop {
            let result = self.policy.run(&self.rng, |_| self.probe_once(&health_url, audience, force_refresh)).await;
            match result {
                Err(error) if error.kind == ProviderErrorKind::Auth && !force_refresh => force_refresh = true,
                other => return other,
            }
        }
    }

    async fn probe_once(&self, url: &str, audience: Option<&str>, force_refresh: bool) -> Result<(), ProviderError> {
        let mut request = self.invoke.get(url);
        if self.config.invoke_auth == InvokeAuth::IdToken {
            let audience = audience.ok_or_else(|| ProviderError::fatal("service has no audience for ID tokens"))?;
            let token = self.auth.id_tokens(audience).token(force_refresh).await?;
            request = request.header(INVOKE_AUTH_HEADER, format!("Bearer {}", token.expose()));
        }
        let response = request.send().await.map_err(|error| {
            ProviderError::transient(if error.is_timeout() {
                "wake probe timed out"
            } else {
                "wake probe connect error"
            })
        })?;
        match response.status().as_u16() {
            200..=299 => Ok(()),
            // Cloud Run answers 429 when max instances are busy or the region lacks capacity.
            429 => {
                Err(ProviderError::capacity("Cloud Run has no instance available (max instances or region capacity)"))
            }
            401 | 403 => Err(ProviderError::auth(format!(
                "Cloud Run refused the invoker token (HTTP {}): grant roles/run.invoker",
                response.status().as_u16()
            ))),
            404 => Err(ProviderError::not_found("service URL answered 404")),
            status @ 500..=599 => Err(ProviderError::transient(format!("wake probe HTTP {status}"))),
            status => Err(ProviderError::fatal(format!("wake probe HTTP {status}"))),
        }
    }
}

impl Provider for CloudRunProvider {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            addressing: Addressing::Url,
            scaler: Scaler::Platform,
            max_request_secs: Some(self.inner.config.request_timeout_secs),
            idle_timeout_secs: None,
            max_grace_secs: SHUTDOWN_GRACE_SECS,
            // Request-based billing: no CPU outside an open request.
            background_cpu: false,
            supports_suspend: false,
            wake_p95_target_ms: WAKE_P95_TARGET_MS,
        }
    }

    /// `replicas` is advisory: Cloud Run decides the instance count from
    /// `maxInstanceRequestConcurrency`; the result is always the one URL.
    fn wake<'a>(
        &'a self,
        deployment: &'a DeploymentRef,
        _replicas: u32,
    ) -> BoxFuture<'a, Result<Vec<Endpoint>, ProviderError>> {
        Box::pin(async move {
            let resource = self.service_resource(deployment)?;
            let key = format!("{resource}#{}", version_tag(&deployment.version));
            let recently_warm =
                lock(&self.inner.warm).get(&key).is_some_and(|at| at.elapsed() < self.inner.config.warm_window);
            if recently_warm {
                let (url, _) = self.inner.version_url(&resource, &version_tag(&deployment.version)).await?;
                let replica = ReplicaRef { deployment: deployment.clone(), id: URL_REPLICA_ID.to_owned() };
                return Ok(vec![Endpoint { replica, address: EndpointAddress::Http(url) }]);
            }
            let shared = {
                let mut wakes = lock(&self.inner.wakes);
                wakes
                    .entry(key.clone())
                    .or_insert_with(|| {
                        let future: BoxFuture<'static, _> =
                            Box::pin(self.inner.clone().wake_once(deployment.clone(), resource.clone()));
                        future.shared()
                    })
                    .clone()
            };
            let result = shared.clone().await;
            let mut wakes = lock(&self.inner.wakes);
            if wakes.get(&key).is_some_and(|current| current.ptr_eq(&shared)) {
                wakes.remove(&key);
            }
            drop(wakes);
            result.map(|endpoint| vec![endpoint])
        })
    }

    fn release<'a>(&'a self, _replica: &'a ReplicaRef, _mode: ReleaseMode) -> BoxFuture<'a, Result<(), ProviderError>> {
        // Cloud Run scales in by itself; the gateway never stops instances.
        Box::pin(async { Ok(()) })
    }

    fn list<'a>(&'a self, deployment: &'a DeploymentRef) -> BoxFuture<'a, Result<Vec<ReplicaState>, ProviderError>> {
        Box::pin(async move {
            let resource = self.service_resource(deployment)?;
            let status = match self.inner.status(&resource, true).await {
                Ok(status) => status,
                Err(error) if error.kind == ProviderErrorKind::NotFound => return Ok(Vec::new()),
                Err(error) => return Err(error),
            };
            let tag = version_tag(&deployment.version);
            let Some(url) = status.url_for_tag(&tag) else { return Ok(Vec::new()) };
            let phase = if status.ready { ReplicaPhase::Ready } else { ReplicaPhase::Starting };
            Ok(vec![ReplicaState {
                replica: ReplicaRef { deployment: deployment.clone(), id: URL_REPLICA_ID.to_owned() },
                phase,
                address: Some(EndpointAddress::Http(url.trim_end_matches('/').to_owned())),
                started_at_ms: None,
            }])
        })
    }
}
