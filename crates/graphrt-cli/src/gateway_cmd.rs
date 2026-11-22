//! `graphrt gateway` and `graphrt deploy` (feature `gateway`).
//!
//! - `graphrt gateway --config gateway.yaml`: serve the gateway.
//! - `graphrt gateway --dev agent.yaml`: zero-config local gateway in front of
//!   one workflow (store in `.graphrt/gateway-dev.db`, random keys, no client
//!   auth, local-process agents of this binary).
//! - `graphrt gateway key new` / `key hmac`: a client API key (and the digest
//!   for `api_keys:`) / an agent HMAC key for `keys_env`.
//! - `graphrt gateway import --from <serve store dir> --store gateway.db --deployment <d>`.
//! - `graphrt deploy --target local --deployment <d> --version <v> <workflow>`:
//!   registers a new immutable version with a running gateway.

use std::collections::BTreeMap;
use std::path::PathBuf;

use clap::{Args, Subcommand, ValueEnum};
use graphrt_gateway::config::{DeploymentConfig, GatewayConfig, PoolFileConfig};
use graphrt_gateway::deployment::VersionInfo;
use graphrt_provider_api::{DeployDriver, DeploymentSpec, Resources};

use crate::error::CliError;

/// Exit code of success.
const EXIT_OK: u8 = 0;

/// Options of `graphrt gateway`.
#[derive(Debug, Args)]
pub struct GatewayArgs {
    #[command(subcommand)]
    pub command: Option<GatewayCommand>,
    /// Gateway configuration file (YAML); default `$GRAPHRT_GATEWAY_CONFIG`.
    #[arg(long, value_name = "FILE")]
    pub config: Option<PathBuf>,
    /// Local development: serve this workflow behind a gateway with no auth.
    #[arg(long, value_name = "WORKFLOW", conflicts_with = "config")]
    pub dev: Option<PathBuf>,
    /// Listen address (overrides the config).
    #[arg(long, value_name = "HOST:PORT")]
    pub listen: Option<String>,
}

/// Subcommands of `graphrt gateway`.
#[derive(Debug, Subcommand)]
pub enum GatewayCommand {
    /// Generate keys.
    Key {
        #[command(subcommand)]
        kind: KeyKind,
    },
    /// Import the runs of a `graphrt serve --store` directory into a gateway store.
    Import {
        /// The `graphrt serve` store directory.
        #[arg(long, value_name = "DIR")]
        from: PathBuf,
        /// The gateway's SQLite file.
        #[arg(long, value_name = "FILE")]
        store: PathBuf,
        /// Deployment the runs belong to.
        #[arg(long)]
        deployment: String,
    },
    /// Validate a configuration file and print a summary.
    Check {
        /// Configuration file.
        #[arg(value_name = "FILE")]
        config: PathBuf,
    },
}

/// Key kinds.
#[derive(Debug, Subcommand)]
pub enum KeyKind {
    /// A client API key and its SHA-256 digest (put the digest under `api_keys:`).
    New,
    /// An agent HMAC key `kid:hex` (the value of a deployment's `keys_env`).
    Hmac {
        /// Key id.
        #[arg(long, default_value = "k1")]
        kid: String,
    },
}

/// Deploy targets of this build.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum DeployTarget {
    /// local-process agents managed by the gateway.
    Local,
}

/// Options of `graphrt deploy`.
#[derive(Debug, Args)]
pub struct DeployArgs {
    /// Workflow file the agents serve.
    #[arg(value_name = "WORKFLOW")]
    pub workflow: PathBuf,
    /// Target platform.
    #[arg(long, value_enum, default_value = "local")]
    pub target: DeployTarget,
    /// Deployment name (configured in the gateway).
    #[arg(long)]
    pub deployment: String,
    /// New immutable version id (default: a timestamp).
    #[arg(long)]
    pub version: Option<String>,
    /// Gateway URL; default `$GRAPHRT_GATEWAY_URL` or `http://127.0.0.1:8700`.
    #[arg(long)]
    pub gateway: Option<String>,
    /// Environment variable holding the admin token.
    #[arg(long, default_value = "GRAPHRT_GATEWAY_ADMIN_TOKEN")]
    pub token_env: String,
    /// Extra arguments for `graphrt serve` (e.g. `--providers providers.yaml`).
    #[arg(last = true)]
    pub serve_args: Vec<String>,
}

fn random_hex(bytes: usize) -> Result<String, CliError> {
    use ring::rand::SecureRandom as _;
    let mut buffer = vec![0u8; bytes];
    ring::rand::SystemRandom::new().fill(&mut buffer).map_err(|_| CliError::failed("system random number generator unavailable"))?;
    Ok(graphrt_gateway::hex::encode(&buffer))
}

fn print(text: &str) -> Result<(), CliError> {
    use std::io::Write as _;
    writeln!(std::io::stdout().lock(), "{text}").map_err(|error| CliError::failed(error.to_string()))
}

fn runtime() -> Result<tokio::runtime::Runtime, CliError> {
    tokio::runtime::Builder::new_multi_thread().enable_all().build().map_err(|error| CliError::failed(format!("cannot start runtime: {error}")))
}

fn agent_command(workflow: &std::path::Path, extra: &[String]) -> Result<Vec<String>, CliError> {
    let exe = std::env::current_exe().map_err(|error| CliError::failed(format!("cannot find this binary: {error}")))?;
    let workflow = std::fs::canonicalize(workflow).map_err(|error| CliError::invalid(format!("{}: {error}", workflow.display())))?;
    let mut command = vec![exe.to_string_lossy().into_owned(), "serve".to_owned(), workflow.to_string_lossy().into_owned()];
    command.extend(extra.iter().cloned());
    Ok(command)
}

fn workflow_name(workflow: &std::path::Path) -> String {
    workflow.file_stem().and_then(|stem| stem.to_str()).unwrap_or("workflow").to_owned()
}

/// `graphrt gateway`.
///
/// # Errors
/// Configuration, store and bind failures.
pub fn gateway(args: GatewayArgs) -> Result<u8, CliError> {
    match args.command {
        Some(GatewayCommand::Key { kind: KeyKind::New }) => {
            let key = format!("grk_{}", random_hex(24)?);
            print(&format!("api key:  {key}\nsha256:   {}", graphrt_gateway::hex::sha256_hex(key.as_bytes())))?;
            return Ok(EXIT_OK);
        }
        Some(GatewayCommand::Key { kind: KeyKind::Hmac { kid } }) => {
            print(&format!("{kid}:{}", random_hex(32)?))?;
            return Ok(EXIT_OK);
        }
        Some(GatewayCommand::Check { config }) => {
            let text = std::fs::read_to_string(&config).map_err(|error| CliError::invalid(format!("{}: {error}", config.display())))?;
            let parsed = GatewayConfig::from_yaml(&text).map_err(CliError::invalid)?;
            print(&format!("ok: {} deployments, store {}", parsed.deployments.len(), parsed.store.display()))?;
            return Ok(EXIT_OK);
        }
        Some(GatewayCommand::Import { from, store, deployment }) => return import(&from, &store, &deployment),
        None => {}
    }
    let config_path = args.config.clone().or_else(|| std::env::var_os("GRAPHRT_GATEWAY_CONFIG").filter(|value| !value.is_empty()).map(PathBuf::from));
    let (mut config, versions) = match (&config_path, &args.dev) {
        (Some(path), None) => {
            let text = std::fs::read_to_string(path).map_err(|error| CliError::invalid(format!("{}: {error}", path.display())))?;
            (GatewayConfig::from_yaml(&text).map_err(CliError::invalid)?, Vec::new())
        }
        (None, Some(workflow)) => dev_config(workflow)?,
        _ => return Err(CliError::usage("graphrt gateway needs --config <file> or --dev <workflow>")),
    };
    if let Some(listen) = args.listen {
        config.listen = listen;
    }
    runtime()?.block_on(graphrt_gateway::app::serve_with(config, versions)).map_err(CliError::failed)?;
    Ok(EXIT_OK)
}

fn dev_config(workflow: &std::path::Path) -> Result<(GatewayConfig, Vec<(String, VersionInfo)>), CliError> {
    let mut config = GatewayConfig::from_yaml("store: .graphrt/gateway-dev.db").map_err(CliError::invalid)?;
    config.inline_admin_token = Some(random_hex(16)?);
    let deployment: DeploymentConfig = serde_saphyr::from_str("name: dev\nkeys_env: unused\nopen: true\n").map_err(|error| CliError::failed(error.to_string()))?;
    let deployment = DeploymentConfig { inline_keys: Some(format!("dev:{}", random_hex(32)?)), pool: PoolFileConfig { idle_timeout_secs: 120, ..PoolFileConfig::default() }, ..deployment };
    config.deployments.push(deployment);
    let version = VersionInfo {
        version: format!("dev-{}", graphrt_core::store::now_ms()),
        workflow: workflow_name(workflow),
        fingerprint: String::new(),
        target: "local".to_owned(),
        command: agent_command(workflow, &[])?,
        activated_ms: 0,
        superseded_ms: None,
    };
    eprintln!("graphrt gateway --dev: POST http://{}/v1/deployments/dev/runs (no auth; dev only)", config.listen);
    Ok((config, vec![("dev".to_owned(), version)]))
}

fn import(from: &std::path::Path, store: &std::path::Path, deployment: &str) -> Result<u8, CliError> {
    let source = crate::store::FileStore::open(from)?;
    let target = graphrt_gateway::store::SqliteStore::open(store).map_err(|error| CliError::failed(error.to_string()))?;
    let summary = runtime()?.block_on(graphrt_gateway::store::import::import_runs(&source, &target, deployment)).map_err(|error| CliError::failed(error.to_string()))?;
    print(&format!("imported {} runs ({} events), skipped {} already present", summary.imported, summary.events, summary.skipped))?;
    Ok(EXIT_OK)
}

/// `graphrt deploy`.
///
/// # Errors
/// Missing token, unreachable gateway, refused spec.
pub fn deploy(args: DeployArgs) -> Result<u8, CliError> {
    let DeployTarget::Local = args.target;
    let token = std::env::var(&args.token_env).map_err(|_| CliError::usage(format!("set {} to the gateway's admin token", args.token_env)))?;
    let version = args.version.clone().unwrap_or_else(|| format!("v{}", graphrt_core::store::now_ms() / 1000));
    let mut provider_settings = BTreeMap::new();
    provider_settings.insert("workflow".to_owned(), serde_json::Value::from(workflow_name(&args.workflow)));
    let spec = DeploymentSpec {
        name: args.deployment.clone(),
        version: version.clone(),
        image: None,
        command: agent_command(&args.workflow, &args.serve_args)?,
        env: BTreeMap::new(),
        secrets: Vec::new(),
        max_active_runs: 8,
        min_replicas: 0,
        max_replicas: 4,
        grace_secs: 30,
        resources: Resources::default(),
        workflow_fingerprint: String::new(),
        provider_settings,
    };
    graphrt_gateway::install_crypto_provider();
    let gateway = args.gateway.clone().or_else(|| std::env::var("GRAPHRT_GATEWAY_URL").ok()).unwrap_or_else(|| "http://127.0.0.1:8700".to_owned());
    let driver = graphrt_gateway::deploy::LocalDeployDriver::new(&gateway, &token);
    let deployed = runtime()?.block_on(driver.apply(&spec)).map_err(|error| CliError::failed(error.to_string()))?;
    print(&format!("deployed {}@{} (target local)", deployed.name, deployed.version))?;
    Ok(EXIT_OK)
}
