//! Provider selection from the **operator's** gateway configuration (plan §6).
//!
//! `gateway.yaml` names providers under `providers:` and each deployment picks
//! one by name (`provider: cluster`, default `local`). Endpoints, regions,
//! namespaces, allowed apps and credentials all come from here; a deployment
//! spec written by `graphrt deploy` only carries a target *name* that the
//! provider checks against this configuration, so a tenant can never point
//! the gateway at an arbitrary host (SSRF).
//!
//! Every kind is parsed in every build; building one whose cargo feature is
//! off fails with a message naming the feature.
//!
//! ```yaml
//! providers:
//!   cluster: { kind: k8s, namespace: agents }                  # in-cluster SA or kubeconfig
//!   run:     { kind: cloud_run, project: acme, region: europe-west1 }
//!   tasks:   { kind: ecs, region: eu-west-1, deployments: { shop: { cluster: agents, task_definition_family: shop, subnets: [subnet-1], security_groups: [sg-1] } } }
//!   fn:      { kind: lambda, region: eu-west-1, functions: { faq: graphrt-faq } }
//!   machines: { kind: fly, apps: { graphrt-chat: { token_env: FLY_API_TOKEN } } }
//! ```

use std::collections::BTreeMap;
use std::sync::Arc;

use graphrt_provider_api::Provider;
use serde::Deserialize;

/// Where a secret (token) is read from at run time.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
pub struct TokenSourceConfig {
    /// Environment variable.
    #[serde(default)]
    pub token_env: Option<String>,
    /// File (a mounted secret), re-read after a 401.
    #[serde(default)]
    pub token_file: Option<std::path::PathBuf>,
}

/// Kubernetes settings.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct K8sConfig {
    /// Namespace of the agents' Deployments.
    pub namespace: String,
    /// Kubeconfig file when the gateway runs outside the cluster (default: in-cluster, then `$KUBECONFIG`).
    #[serde(default)]
    pub kubeconfig: Option<std::path::PathBuf>,
    /// Kubeconfig context.
    #[serde(default)]
    pub context: Option<String>,
    /// Seconds `wake` waits for ready pods (default 15).
    #[serde(default)]
    pub wake_timeout_secs: Option<u64>,
    /// `terminationGracePeriodSeconds` (default 120).
    #[serde(default)]
    pub grace_secs: Option<u64>,
}

/// Cloud Run settings.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct CloudRunProviderConfig {
    /// GCP project.
    pub project: String,
    /// Region.
    pub region: String,
    /// Admin API base URL override (private endpoint).
    #[serde(default)]
    pub admin_endpoint: Option<String>,
    /// Service id prefix (default `graphrt-`).
    #[serde(default)]
    pub service_prefix: Option<String>,
    /// `id_token` (default) or `none` (private network, HMAC only).
    #[serde(default)]
    pub invoke_auth: Option<String>,
    /// Revision request timeout (≤ 3600).
    #[serde(default)]
    pub request_timeout_secs: Option<u64>,
}

/// One ECS task template.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct EcsTaskConfig {
    /// ECS cluster.
    pub cluster: String,
    /// Task definition family.
    pub task_definition_family: String,
    /// Subnets.
    #[serde(default)]
    pub subnets: Vec<String>,
    /// Security groups.
    #[serde(default)]
    pub security_groups: Vec<String>,
    /// Public IP (default false: the gateway runs in the same VPC).
    #[serde(default)]
    pub assign_public_ip: bool,
    /// Agent port in the task (default 8080).
    #[serde(default = "default_agent_port")]
    pub container_port: u16,
    /// Capacity provider (`FARGATE_SPOT`).
    #[serde(default)]
    pub capacity_provider: Option<String>,
}

fn default_agent_port() -> u16 {
    8080
}

/// ECS settings.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct EcsProviderConfig {
    /// Region.
    pub region: String,
    /// ECS endpoint override (VPC endpoint).
    #[serde(default)]
    pub endpoint: Option<String>,
    /// Deployment → task template.
    #[serde(default)]
    pub deployments: BTreeMap<String, EcsTaskConfig>,
    /// Seconds `wake` waits for RUNNING tasks (default 180).
    #[serde(default)]
    pub wake_timeout_secs: Option<u64>,
}

/// Lambda settings.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct LambdaProviderConfig {
    /// Region.
    pub region: String,
    /// Lambda endpoint override (VPC endpoint).
    #[serde(default)]
    pub endpoint: Option<String>,
    /// Deployment → function name or ARN (when the version's target is empty).
    #[serde(default)]
    pub functions: BTreeMap<String, String>,
    /// Gateway-side cap of one run (≤ 840).
    #[serde(default)]
    pub max_run_secs: Option<u64>,
}

/// Fly settings.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct FlyConfig {
    /// Machines API base URL (default public; `http://_api.internal:4280` on Fly).
    #[serde(default)]
    pub api_base_url: Option<String>,
    /// Apps the gateway may wake, each with its own token.
    pub apps: BTreeMap<String, TokenSourceConfig>,
    /// Agent port in each Machine (default 8080).
    #[serde(default)]
    pub agent_port: Option<u16>,
    /// `suspend` (default) or `stop`.
    #[serde(default)]
    pub release: Option<String>,
    /// Upper bound of Machines per version (default 10).
    #[serde(default)]
    pub max_machines_per_version: Option<u32>,
}

/// Azure Container Apps settings.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct AcaProviderConfig {
    /// Subscription id.
    pub subscription_id: String,
    /// Resource group of the container apps.
    pub resource_group: String,
    /// ARM endpoint (default `https://management.azure.com`; sovereign clouds differ).
    #[serde(default)]
    pub arm_endpoint: Option<String>,
    /// `standard` (default: 240 s, preempt planned), `standard_idle_verified`, or `premium`.
    #[serde(default)]
    pub ingress: Option<String>,
    /// Premium ingress `requestIdleTimeout` in minutes (4-30).
    #[serde(default)]
    pub request_idle_timeout_minutes: Option<u32>,
    /// Seconds `wake` waits for the revision (default 60).
    #[serde(default)]
    pub wake_timeout_secs: Option<u64>,
}

/// One named provider.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProviderConfig {
    /// local-process (always built with `gw-local`).
    Local,
    /// Kubernetes (`gw-k8s`).
    K8s(K8sConfig),
    /// Cloud Run (`gw-cloud-run`).
    CloudRun(CloudRunProviderConfig),
    /// ECS Fargate (`gw-ecs`).
    Ecs(EcsProviderConfig),
    /// Lambda (`gw-lambda`).
    Lambda(LambdaProviderConfig),
    /// Fly Machines (`gw-fly`).
    Fly(FlyConfig),
    /// Azure Container Apps (`gw-aca`).
    Aca(AcaProviderConfig),
}

impl ProviderConfig {
    /// The `kind:` value.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::K8s(_) => "k8s",
            Self::CloudRun(_) => "cloud_run",
            Self::Ecs(_) => "ecs",
            Self::Lambda(_) => "lambda",
            Self::Fly(_) => "fly",
            Self::Aca(_) => "aca",
        }
    }

    /// The cargo feature that builds this kind.
    pub fn feature(&self) -> &'static str {
        match self {
            Self::Local => "gw-local",
            Self::K8s(_) => "gw-k8s",
            Self::CloudRun(_) => "gw-cloud-run",
            Self::Ecs(_) => "gw-ecs",
            Self::Lambda(_) => "gw-lambda",
            Self::Fly(_) => "gw-fly",
            Self::Aca(_) => "gw-aca",
        }
    }
}

/// Kinds compiled into this build.
pub fn compiled_kinds() -> Vec<&'static str> {
    let mut kinds = Vec::new();
    if cfg!(feature = "gw-local") {
        kinds.push("local");
    }
    if cfg!(feature = "gw-k8s") {
        kinds.push("k8s");
    }
    if cfg!(feature = "gw-cloud-run") {
        kinds.push("cloud_run");
    }
    if cfg!(feature = "gw-ecs") {
        kinds.push("ecs");
    }
    if cfg!(feature = "gw-lambda") {
        kinds.push("lambda");
    }
    if cfg!(feature = "gw-fly") {
        kinds.push("fly");
    }
    if cfg!(feature = "gw-aca") {
        kinds.push("aca");
    }
    kinds
}

fn not_built(config: &ProviderConfig) -> String {
    format!("provider kind `{}` is not in this build (rebuild with the `{}` feature)", config.kind(), config.feature())
}

/// Builds a remote provider (every kind but `local`, which the app builds itself).
///
/// # Errors
/// The kind is not compiled in, or its credentials / settings are invalid.
pub fn build_remote(config: &ProviderConfig) -> Result<Arc<dyn Provider>, String> {
    match config {
        ProviderConfig::Local => Err("the local provider is built by the gateway app".to_owned()),
        ProviderConfig::K8s(settings) => build_k8s(settings).ok_or_else(|| not_built(config))?,
        ProviderConfig::CloudRun(settings) => build_cloud_run(settings).ok_or_else(|| not_built(config))?,
        ProviderConfig::Ecs(settings) => build_ecs(settings).ok_or_else(|| not_built(config))?,
        ProviderConfig::Lambda(settings) => build_lambda(settings).ok_or_else(|| not_built(config))?,
        ProviderConfig::Fly(settings) => build_fly(settings).ok_or_else(|| not_built(config))?,
        ProviderConfig::Aca(settings) => build_aca(settings).ok_or_else(|| not_built(config))?,
    }
}

type Built = Option<Result<Arc<dyn Provider>, String>>;

/// The K8s cluster settings: kubeconfig when configured (feature
/// `gw-k8s-kubeconfig`), else the in-cluster service account.
///
/// # Errors
/// Missing credentials, or a kubeconfig asked for without the feature.
#[cfg(feature = "gw-k8s")]
pub fn k8s_client(settings: &K8sConfig) -> Result<graphrt_provider_k8s::KubeClient, String> {
    let cluster = k8s_cluster(settings)?;
    graphrt_provider_k8s::KubeClient::new(&cluster, graphrt_provider_k8s::ClientOptions::default())
        .map_err(|error| error.to_string())
}

#[cfg(feature = "gw-k8s-kubeconfig")]
fn k8s_cluster(settings: &K8sConfig) -> Result<graphrt_provider_k8s::auth::ClusterConfig, String> {
    use graphrt_provider_k8s::auth::{KubeconfigOptions, infer_cluster_config, kubeconfig_from_env};
    match (&settings.kubeconfig, &settings.context) {
        (None, None) => infer_cluster_config(),
        (path, context) => kubeconfig_from_env(&KubeconfigOptions { path: path.clone(), context: context.clone() }),
    }
    .map_err(|error| format!("kubernetes credentials: {error}"))
}

#[cfg(all(feature = "gw-k8s", not(feature = "gw-k8s-kubeconfig")))]
fn k8s_cluster(settings: &K8sConfig) -> Result<graphrt_provider_k8s::auth::ClusterConfig, String> {
    if settings.kubeconfig.is_some() || settings.context.is_some() {
        return Err("kubeconfig / context need the `gw-k8s-kubeconfig` feature (this build only uses the in-cluster service account)".to_owned());
    }
    graphrt_provider_k8s::auth::in_cluster_config()
        .map_err(|error| format!("kubernetes in-cluster credentials: {error}"))
}

#[cfg(feature = "gw-k8s")]
fn build_k8s(settings: &K8sConfig) -> Built {
    let build = || -> Result<Arc<dyn Provider>, String> {
        let mut config = graphrt_provider_k8s::K8sProviderConfig::new(&settings.namespace);
        if let Some(secs) = settings.wake_timeout_secs {
            config.wake_timeout = std::time::Duration::from_secs(secs);
        }
        if let Some(secs) = settings.grace_secs {
            config.grace_secs = secs;
        }
        Ok(Arc::new(graphrt_provider_k8s::K8sProvider::new(k8s_client(settings)?, config)))
    };
    Some(build())
}

#[cfg(not(feature = "gw-k8s"))]
fn build_k8s(_settings: &K8sConfig) -> Built {
    None
}

/// Cloud Run config from the operator settings.
#[cfg(feature = "gw-cloud-run")]
pub fn cloud_run_config(settings: &CloudRunProviderConfig) -> Result<graphrt_gcp::cloud_run::CloudRunConfig, String> {
    use graphrt_gcp::cloud_run::{CloudRunConfig, InvokeAuth};
    let mut config = CloudRunConfig::new(&settings.project, &settings.region);
    if let Some(endpoint) = &settings.admin_endpoint {
        config.admin_endpoint.clone_from(endpoint);
    }
    if let Some(prefix) = &settings.service_prefix {
        config.service_prefix.clone_from(prefix);
    }
    if let Some(secs) = settings.request_timeout_secs {
        config.request_timeout_secs = secs;
    }
    config.invoke_auth = match settings.invoke_auth.as_deref() {
        None | Some("id_token") => InvokeAuth::IdToken,
        Some("none") => InvokeAuth::None,
        Some(other) => return Err(format!("cloud_run.invoke_auth must be id_token or none, not `{other}`")),
    };
    Ok(config)
}

#[cfg(feature = "gw-cloud-run")]
fn build_cloud_run(settings: &CloudRunProviderConfig) -> Built {
    let build = || -> Result<Arc<dyn Provider>, String> {
        let auth = graphrt_gcp::auth::GcpAuth::from_env().map_err(|error| format!("gcp credentials: {error}"))?;
        let provider = graphrt_gcp::cloud_run::CloudRunProvider::new(cloud_run_config(settings)?, auth)
            .map_err(|error| error.to_string())?;
        Ok(Arc::new(crate::cloud_run::CloudRunGatewayProvider::new(provider).map_err(|error| error.to_string())?))
    };
    Some(build())
}

#[cfg(not(feature = "gw-cloud-run"))]
fn build_cloud_run(_settings: &CloudRunProviderConfig) -> Built {
    None
}

/// The AWS credential chain of the gateway process (env → container → web identity → IMDSv2).
#[cfg(any(feature = "gw-ecs", feature = "gw-lambda"))]
pub fn aws_chain(region: &str) -> Result<Arc<graphrt_provider_aws::auth::CredentialChain>, String> {
    let mut chain = graphrt_provider_aws::auth::ChainConfig::from_env();
    chain.region = region.to_owned();
    graphrt_provider_aws::auth::CredentialChain::new(chain, graphrt_provider_aws::auth::Clock::default())
        .map(Arc::new)
        .map_err(|error| format!("aws credentials: {error}"))
}

#[cfg(any(feature = "gw-ecs", feature = "gw-lambda"))]
fn aws_endpoint(service: &str, region: &str, base_url: Option<&String>) -> graphrt_provider_aws::rest::ServiceEndpoint {
    let mut endpoint = graphrt_provider_aws::rest::ServiceEndpoint::regional(service, region);
    if let Some(base_url) = base_url {
        endpoint.base_url.clone_from(base_url);
    }
    endpoint
}

/// ECS provider config from the operator settings.
#[cfg(feature = "gw-ecs")]
pub fn ecs_config(settings: &EcsProviderConfig) -> graphrt_provider_aws::ecs::EcsConfig {
    let mut config = graphrt_provider_aws::ecs::EcsConfig::new(&settings.region);
    config.endpoint = aws_endpoint("ecs", &settings.region, settings.endpoint.as_ref());
    if let Some(secs) = settings.wake_timeout_secs {
        config.wake_timeout = std::time::Duration::from_secs(secs);
    }
    for (deployment, task) in &settings.deployments {
        config.deployments.insert(
            deployment.clone(),
            graphrt_provider_aws::ecs::EcsTaskTemplate {
                cluster: task.cluster.clone(),
                task_definition_family: task.task_definition_family.clone(),
                subnets: task.subnets.clone(),
                security_groups: task.security_groups.clone(),
                assign_public_ip: task.assign_public_ip,
                container_port: task.container_port,
                capacity_provider: task.capacity_provider.clone(),
                platform_version: None,
            },
        );
    }
    config
}

#[cfg(feature = "gw-ecs")]
fn build_ecs(settings: &EcsProviderConfig) -> Built {
    let build = || -> Result<Arc<dyn Provider>, String> {
        let provider = graphrt_provider_aws::ecs::EcsProvider::new(aws_chain(&settings.region)?, ecs_config(settings))
            .map_err(|error| error.to_string())?;
        Ok(Arc::new(provider))
    };
    Some(build())
}

#[cfg(not(feature = "gw-ecs"))]
fn build_ecs(_settings: &EcsProviderConfig) -> Built {
    None
}

/// Lambda provider config from the operator settings.
#[cfg(feature = "gw-lambda")]
pub fn lambda_config(settings: &LambdaProviderConfig) -> graphrt_provider_aws::lambda::LambdaConfig {
    let mut config = graphrt_provider_aws::lambda::LambdaConfig::new(&settings.region);
    config.endpoint = aws_endpoint("lambda", &settings.region, settings.endpoint.as_ref());
    if let Some(secs) = settings.max_run_secs {
        config.max_run_secs = secs;
    }
    for (deployment, function) in &settings.functions {
        config.functions.insert(
            deployment.clone(),
            graphrt_provider_aws::lambda::LambdaFunction { function_name: function.clone() },
        );
    }
    config
}

#[cfg(feature = "gw-lambda")]
fn build_lambda(settings: &LambdaProviderConfig) -> Built {
    let build = || -> Result<Arc<dyn Provider>, String> {
        let provider =
            graphrt_provider_aws::lambda::LambdaProvider::new(aws_chain(&settings.region)?, lambda_config(settings))
                .map_err(|error| error.to_string())?;
        Ok(Arc::new(provider))
    };
    Some(build())
}

#[cfg(not(feature = "gw-lambda"))]
fn build_lambda(_settings: &LambdaProviderConfig) -> Built {
    None
}

/// Where a Fly token comes from.
#[cfg(feature = "gw-fly")]
pub fn fly_token_origin(app: &str, source: &TokenSourceConfig) -> Result<graphrt_provider_fly::TokenOrigin, String> {
    match (&source.token_env, &source.token_file) {
        (Some(variable), None) => Ok(graphrt_provider_fly::TokenOrigin::Env(variable.clone())),
        (None, Some(path)) => Ok(graphrt_provider_fly::TokenOrigin::File(path.clone())),
        _ => Err(format!("fly app {app}: set exactly one of token_env or token_file")),
    }
}

/// Fly provider config from the operator settings.
///
/// # Errors
/// No app, or an app without exactly one token source.
#[cfg(feature = "gw-fly")]
pub fn fly_config(settings: &FlyConfig) -> Result<graphrt_provider_fly::FlyProviderConfig, String> {
    let mut apps = settings.apps.iter();
    let (first, first_token) = apps.next().ok_or("fly: list at least one app under apps:")?;
    let mut config = graphrt_provider_fly::FlyProviderConfig::single_app(first, fly_token_origin(first, first_token)?);
    for (app, token) in apps {
        config.apps.insert(app.clone(), fly_token_origin(app, token)?);
    }
    if let Some(url) = &settings.api_base_url {
        config.api_base_url.clone_from(url);
    }
    if let Some(port) = settings.agent_port {
        config.agent_port = port;
    }
    if let Some(limit) = settings.max_machines_per_version {
        config.max_machines_per_version = limit;
    }
    config.preferred_release = match settings.release.as_deref() {
        None | Some("suspend") => graphrt_provider_api::ReleaseMode::Suspend,
        Some("stop") => graphrt_provider_api::ReleaseMode::Stop,
        Some(other) => return Err(format!("fly.release must be suspend or stop, not `{other}`")),
    };
    config.validate()?;
    Ok(config)
}

#[cfg(feature = "gw-fly")]
fn build_fly(settings: &FlyConfig) -> Built {
    let build = || -> Result<Arc<dyn Provider>, String> {
        let provider =
            graphrt_provider_fly::FlyProvider::new(fly_config(settings)?).map_err(|error| error.to_string())?;
        Ok(Arc::new(provider))
    };
    Some(build())
}

#[cfg(not(feature = "gw-fly"))]
fn build_fly(_settings: &FlyConfig) -> Built {
    None
}

/// Builds the deploy-time driver of a provider (`graphrt deploy --target ...`),
/// with the deploy-time credentials of the process running the CLI.
///
/// `fly_token` names the variable holding the Fly deploy token (default `FLY_API_TOKEN`).
///
/// # Errors
/// The kind's deploy feature is not compiled in, or its settings / credentials are invalid.
pub fn build_deploy_driver(config: &ProviderConfig) -> Result<Box<dyn graphrt_provider_api::DeployDriver>, String> {
    let missing = || {
        format!(
            "deploying to `{}` is not in this build (rebuild with `deploy-{}`)",
            config.kind(),
            config.feature().trim_start_matches("gw-")
        )
    };
    match config {
        ProviderConfig::Local => Err("use LocalDeployDriver for the local target".to_owned()),
        ProviderConfig::K8s(settings) => deploy_k8s(settings).ok_or_else(missing)?,
        ProviderConfig::CloudRun(settings) => deploy_cloud_run(settings).ok_or_else(missing)?,
        ProviderConfig::Ecs(settings) => deploy_ecs(settings).ok_or_else(missing)?,
        ProviderConfig::Lambda(settings) => deploy_lambda(settings).ok_or_else(missing)?,
        ProviderConfig::Fly(settings) => deploy_fly(settings).ok_or_else(missing)?,
        ProviderConfig::Aca(settings) => deploy_aca(settings).ok_or_else(missing)?,
    }
}

type BuiltDriver = Option<Result<Box<dyn graphrt_provider_api::DeployDriver>, String>>;

#[cfg(feature = "deploy-k8s")]
fn deploy_k8s(settings: &K8sConfig) -> BuiltDriver {
    Some(k8s_client(settings).map(|client| {
        Box::new(graphrt_provider_k8s::K8sDeployDriver::new(client, &settings.namespace))
            as Box<dyn graphrt_provider_api::DeployDriver>
    }))
}
#[cfg(not(feature = "deploy-k8s"))]
fn deploy_k8s(_settings: &K8sConfig) -> BuiltDriver {
    None
}

#[cfg(feature = "deploy-cloud-run")]
fn deploy_cloud_run(settings: &CloudRunProviderConfig) -> BuiltDriver {
    let build = || -> Result<Box<dyn graphrt_provider_api::DeployDriver>, String> {
        let auth = graphrt_gcp::auth::GcpAuth::from_env().map_err(|error| format!("gcp credentials: {error}"))?;
        let driver = graphrt_gcp::cloud_run::CloudRunDeployDriver::new(cloud_run_config(settings)?, &auth)
            .map_err(|error| error.to_string())?;
        Ok(Box::new(driver))
    };
    Some(build())
}
#[cfg(not(feature = "deploy-cloud-run"))]
fn deploy_cloud_run(_settings: &CloudRunProviderConfig) -> BuiltDriver {
    None
}

#[cfg(feature = "deploy-ecs")]
fn deploy_ecs(settings: &EcsProviderConfig) -> BuiltDriver {
    let build = || -> Result<Box<dyn graphrt_provider_api::DeployDriver>, String> {
        let mut config = graphrt_provider_aws::deploy::ecs::EcsDeployConfig::new(&settings.region);
        config.endpoint = aws_endpoint("ecs", &settings.region, settings.endpoint.as_ref());
        let driver = graphrt_provider_aws::deploy::ecs::EcsDeployDriver::new(aws_chain(&settings.region)?, config)
            .map_err(|error| error.to_string())?;
        Ok(Box::new(driver))
    };
    Some(build())
}
#[cfg(not(feature = "deploy-ecs"))]
fn deploy_ecs(_settings: &EcsProviderConfig) -> BuiltDriver {
    None
}

#[cfg(feature = "deploy-lambda")]
fn deploy_lambda(settings: &LambdaProviderConfig) -> BuiltDriver {
    let build = || -> Result<Box<dyn graphrt_provider_api::DeployDriver>, String> {
        let mut config = graphrt_provider_aws::deploy::lambda::LambdaDeployConfig::new(&settings.region);
        config.lambda = aws_endpoint("lambda", &settings.region, settings.endpoint.as_ref());
        let driver =
            graphrt_provider_aws::deploy::lambda::LambdaDeployDriver::new(aws_chain(&settings.region)?, config)
                .map_err(|error| error.to_string())?;
        Ok(Box::new(driver))
    };
    Some(build())
}
#[cfg(not(feature = "deploy-lambda"))]
fn deploy_lambda(_settings: &LambdaProviderConfig) -> BuiltDriver {
    None
}

#[cfg(feature = "deploy-fly")]
fn deploy_fly(settings: &FlyConfig) -> BuiltDriver {
    let config = graphrt_provider_fly::FlyDeployConfig {
        api_base_url: settings
            .api_base_url
            .clone()
            .unwrap_or_else(|| graphrt_provider_fly::PUBLIC_API_BASE_URL.to_owned()),
        // Deploy-time token (may create apps and set secrets), from the CLI's environment.
        token_origin: graphrt_provider_fly::TokenOrigin::Env(graphrt_provider_fly::FLY_API_TOKEN_ENV.to_owned()),
        client_policy: graphrt_provider_fly::ClientPolicy::default(),
        env_lookup: Arc::new(|name: &str| std::env::var(name).ok()),
    };
    Some(Ok(Box::new(graphrt_provider_fly::FlyDeployDriver::new(config))))
}
#[cfg(not(feature = "deploy-fly"))]
fn deploy_fly(_settings: &FlyConfig) -> BuiltDriver {
    None
}

/// ACA config and ARM client from the operator settings and the process's
/// Azure credential (managed identity, workload identity, client secret / certificate).
///
/// # Errors
/// Missing credentials or invalid settings.
#[cfg(feature = "gw-aca")]
pub fn aca_parts(
    settings: &AcaProviderConfig,
) -> Result<(graphrt_provider_azure::arm::ArmClient, graphrt_provider_azure::aca::AcaConfig), String> {
    use graphrt_provider_azure::aca::{AcaConfig, IngressMode};
    use graphrt_provider_azure::arm::{ArmClient, ArmConfig};
    use graphrt_provider_azure::auth::{AuthEndpoints, AzureCredential, TokenProvider};
    let lookup = |name: &str| std::env::var(name).ok();
    let credential = AzureCredential::from_env(&lookup, &AuthEndpoints::default())
        .map_err(|error| format!("azure credentials: {error}"))?;
    let tokens = Arc::new(TokenProvider::new(credential).map_err(|error| format!("azure credentials: {error}"))?);
    let mut arm = ArmConfig::default();
    if let Some(endpoint) = &settings.arm_endpoint {
        arm.endpoint.clone_from(endpoint);
    }
    let arm = ArmClient::new(arm, tokens).map_err(|error| error.to_string())?;
    let mut config = AcaConfig::new(&settings.subscription_id, &settings.resource_group);
    config.ingress = match (settings.ingress.as_deref(), settings.request_idle_timeout_minutes) {
        (None | Some("standard"), _) => IngressMode::Standard,
        (Some("standard_idle_verified"), _) => IngressMode::StandardIdleVerified,
        (Some("premium"), Some(minutes)) => IngressMode::Premium { request_idle_timeout_minutes: minutes },
        (Some("premium"), None) => return Err("aca: premium ingress needs request_idle_timeout_minutes".to_owned()),
        (Some(other), _) => {
            return Err(format!("aca.ingress must be standard, standard_idle_verified or premium, not `{other}`"));
        }
    };
    if let Some(secs) = settings.wake_timeout_secs {
        config.wake_timeout = std::time::Duration::from_secs(secs);
    }
    config.validate().map_err(|error| error.to_string())?;
    Ok((arm, config))
}

#[cfg(feature = "gw-aca")]
fn build_aca(settings: &AcaProviderConfig) -> Built {
    let build = || -> Result<Arc<dyn Provider>, String> {
        let (arm, config) = aca_parts(settings)?;
        Ok(Arc::new(graphrt_provider_azure::aca::AcaProvider::new(arm, config).map_err(|error| error.to_string())?))
    };
    Some(build())
}
#[cfg(not(feature = "gw-aca"))]
fn build_aca(_settings: &AcaProviderConfig) -> Built {
    None
}

#[cfg(feature = "deploy-aca")]
fn deploy_aca(settings: &AcaProviderConfig) -> BuiltDriver {
    let build = || -> Result<Box<dyn graphrt_provider_api::DeployDriver>, String> {
        let (arm, config) = aca_parts(settings)?;
        let driver = graphrt_provider_azure::aca::AcaDeployDriver::new(
            arm,
            config,
            graphrt_provider_azure::keyvault::KeyVaultConfig::default(),
        )
        .map_err(|error| error.to_string())?;
        Ok(Box::new(driver))
    };
    Some(build())
}
#[cfg(not(feature = "deploy-aca"))]
fn deploy_aca(_settings: &AcaProviderConfig) -> BuiltDriver {
    None
}

/// The retirer of a provider kind, when it has platform objects to free and
/// its deploy feature is built (see [`crate::retire`]). `Ok(None)` otherwise.
///
/// # Errors
/// Credentials or settings of a retirer that should exist are invalid.
pub fn build_retirer(config: &ProviderConfig) -> Result<Option<Arc<dyn crate::retire::VersionRetirer>>, String> {
    match config {
        ProviderConfig::CloudRun(settings) => cloud_run_retirer(settings),
        ProviderConfig::Aca(settings) => aca_retirer(settings),
        ProviderConfig::Fly(settings) => fly_retirer(settings),
        ProviderConfig::Local | ProviderConfig::K8s(_) | ProviderConfig::Ecs(_) | ProviderConfig::Lambda(_) => Ok(None),
    }
}

type BuiltRetirer = Result<Option<Arc<dyn crate::retire::VersionRetirer>>, String>;

#[cfg(feature = "deploy-cloud-run")]
fn cloud_run_retirer(settings: &CloudRunProviderConfig) -> BuiltRetirer {
    let auth = graphrt_gcp::auth::GcpAuth::from_env().map_err(|error| format!("gcp credentials: {error}"))?;
    let driver = graphrt_gcp::cloud_run::CloudRunDeployDriver::new(cloud_run_config(settings)?, &auth)
        .map_err(|error| error.to_string())?;
    Ok(Some(Arc::new(crate::retire::CloudRunRetirer(driver))))
}
#[cfg(not(feature = "deploy-cloud-run"))]
fn cloud_run_retirer(_settings: &CloudRunProviderConfig) -> BuiltRetirer {
    eprintln!("gateway: cloud run tags of retired versions stay (build with deploy-cloud-run to drop them)");
    Ok(None)
}

#[cfg(feature = "deploy-aca")]
fn aca_retirer(settings: &AcaProviderConfig) -> BuiltRetirer {
    let (arm, config) = aca_parts(settings)?;
    let provider = graphrt_provider_azure::aca::AcaProvider::new(arm, config).map_err(|error| error.to_string())?;
    Ok(Some(Arc::new(crate::retire::AcaRetirer(provider))))
}
#[cfg(not(feature = "deploy-aca"))]
fn aca_retirer(_settings: &AcaProviderConfig) -> BuiltRetirer {
    eprintln!(
        "gateway: container app revisions of retired versions stay active (build with deploy-aca to deactivate them)"
    );
    Ok(None)
}

#[cfg(feature = "deploy-fly")]
fn fly_retirer(settings: &FlyConfig) -> BuiltRetirer {
    let mut drivers = BTreeMap::new();
    for (app, token) in &settings.apps {
        let config = graphrt_provider_fly::FlyDeployConfig {
            api_base_url: settings
                .api_base_url
                .clone()
                .unwrap_or_else(|| graphrt_provider_fly::PUBLIC_API_BASE_URL.to_owned()),
            token_origin: fly_token_origin(app, token)?,
            client_policy: graphrt_provider_fly::ClientPolicy::default(),
            env_lookup: Arc::new(|_: &str| None),
        };
        drivers.insert(app.clone(), graphrt_provider_fly::FlyDeployDriver::new(config));
    }
    Ok(Some(Arc::new(crate::retire::FlyRetirer(drivers))))
}
#[cfg(not(feature = "deploy-fly"))]
fn fly_retirer(_settings: &FlyConfig) -> BuiltRetirer {
    eprintln!("gateway: fly machines of retired versions stay (build with deploy-fly to destroy them)");
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_kind_parses_and_disabled_kinds_name_their_feature() {
        let yaml = r#"
cluster: { kind: k8s, namespace: agents }
run: { kind: cloud_run, project: acme, region: europe-west1 }
tasks: { kind: ecs, region: eu-west-1, deployments: { shop: { cluster: agents, task_definition_family: shop } } }
fn: { kind: lambda, region: eu-west-1, functions: { faq: graphrt-faq } }
machines: { kind: fly, apps: { graphrt-chat: { token_env: FLY_API_TOKEN } } }
apps: { kind: aca, subscription_id: sub, resource_group: rg }
dev: { kind: local }
"#;
        let providers: BTreeMap<String, ProviderConfig> = graphrt_core::yaml::from_str(yaml, false).unwrap();
        assert_eq!(providers.len(), 7);
        assert_eq!(
            providers["tasks"],
            ProviderConfig::Ecs(EcsProviderConfig {
                region: "eu-west-1".into(),
                endpoint: None,
                deployments: BTreeMap::from([(
                    "shop".into(),
                    EcsTaskConfig {
                        cluster: "agents".into(),
                        task_definition_family: "shop".into(),
                        subnets: vec![],
                        security_groups: vec![],
                        assign_public_ip: false,
                        container_port: 8080,
                        capacity_provider: None
                    }
                )]),
                wake_timeout_secs: None,
            })
        );
        for config in providers.values().filter(|config| !matches!(config, ProviderConfig::Local)) {
            if !compiled_kinds().contains(&config.kind())
                && let Err(error) = build_remote(config)
            {
                assert!(error.contains(config.feature()), "{error}");
            }
        }
        let unknown = graphrt_core::yaml::from_str::<ProviderConfig>("{ kind: app_runner }", false);
        assert!(unknown.is_err(), "App Runner is not supported");
    }
}
