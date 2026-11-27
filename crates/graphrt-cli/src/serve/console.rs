//! The admin console of `graphrt serve --console FILE` (cargo feature
//! `console`): the glue between the server and the `graphrt-console` crate.
//!
//! - At startup the console's secrets join the served environment (like
//!   `--env-file` entries) and the registry versions of this server's
//!   environment replace or add to the workflow files.
//! - [`wrap`] puts the console's gate in front of every route and adds its routes.
//! - The [`graphrt_console::Host`] here tells the console what is served,
//!   compiles uploaded versions, and restarts the server: a drain like
//!   `SIGTERM`, then the same command again, in the same process on Unix.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};

use axum::Router;
use futures::future::BoxFuture;
use graphrt_console::oidc::{Claims, IdTokenVerifier, VerifierSource};
use graphrt_console::{Console, ConsoleConfig, Format, Host, Options, ServedWorkflow};
use indexmap::IndexMap;

use super::startup::{lookup_env, store_dir};
use super::state::AppState;
use crate::args::ServeArgs;
use crate::error::CliError;
use crate::setup::ServiceOptions;
use crate::user_auth::jwks::{MIN_REFETCH_INTERVAL, RemoteKeySet};
use crate::user_auth::token::{self, ClaimRules};

/// The variable that names the console config when `--console` is not given.
pub const CONSOLE_ENV: &str = "GRAPHRT_CONSOLE";

/// Workflow files to serve: `(name, path)`, as [`super::discover`] lists them.
type WorkflowFiles = Vec<(String, PathBuf)>;

/// Set by [`CliHost::restart`]: once the drain ends, `graphrt serve` starts again.
static RESTART: AtomicBool = AtomicBool::new(false);

impl AppState {
    /// Serve the admin console (`--console`).
    pub fn with_console(mut self, console: Option<Arc<Console>>) -> Self {
        self.console = console;
        self
    }
}

/// Open the console of `--console` (else `$GRAPHRT_CONSOLE`): its secrets go
/// into `options`, its registry versions into `files`.
///
/// @returns The console, and the workflow files to serve.
/// @throws CliError (usage) - Without an API key, on an unusable config, key or data directory.
pub(super) fn open(
    args: &ServeArgs,
    options: &mut ServiceOptions,
    mut files: WorkflowFiles,
    api_key: Option<&str>,
) -> Result<(Option<Arc<Console>>, WorkflowFiles), CliError> {
    let path =
        args.console.clone().or_else(|| lookup_env(options, CONSOLE_ENV).filter(|p| !p.is_empty()).map(PathBuf::from));
    let Some(path) = path else { return Ok((None, files)) };
    let usage = |error: graphrt_console::ConsoleError| CliError::usage(format!("--console: {error}"));
    let Some(api_key) = api_key else {
        return Err(CliError::usage(format!(
            "--console needs an API key (${}): automation keeps using it, and signed in people act through it",
            args.api_key_env
        )));
    };
    let config = ConsoleConfig::load(&path).map_err(usage)?;
    let data_dir = graphrt_console::data_dir(&config, store_dir(args).as_deref()).map_err(usage)?;
    let secret_key = secret_key(&config, options)?;
    let client_secret = match &config.oidc.client_secret_env {
        Some(name) => {
            Some(lookup_env(options, name).map(|s| s.trim().to_owned()).filter(|s| !s.is_empty()).ok_or_else(|| {
                CliError::usage(format!("--console: the OIDC client secret variable ${name} is not set"))
            })?)
        }
        None => None,
    };
    let console = Console::open(Options {
        config,
        data_dir,
        api_key: api_key.to_owned(),
        secret_key,
        client_secret,
        verifiers: Arc::new(JwksVerifiers),
    })
    .map_err(usage)?;
    // Like `--env-file` entries, which win over the process environment.
    options.env_file.extend(console.secret_values().map_err(usage)?);
    for (name, version, file) in console.served_files().map_err(usage)? {
        eprintln!("  workflow `{name}`: registry version {} ({})", version.version, short(&version.hash));
        files.retain(|(served, _)| *served != name);
        files.push((name, file));
    }
    eprintln!("  {}", console.describe());
    let console = Arc::new(console);
    console.spawn_retention();
    Ok((Some(console), files))
}

fn short(hash: &str) -> &str {
    hash.get(..12).unwrap_or(hash)
}

/// The key of the console's secrets: `secrets.key_env`, else `secrets.key_file`.
///
/// @throws CliError (usage) - When the key is given but not 32 bytes, or the file cannot be read.
fn secret_key(
    config: &ConsoleConfig,
    options: &ServiceOptions,
) -> Result<Option<graphrt_console::secrets::SecretKey>, CliError> {
    let text = match lookup_env(options, &config.secrets.key_env).filter(|key| !key.trim().is_empty()) {
        Some(text) => text,
        None => match &config.secrets.key_file {
            Some(file) => std::fs::read_to_string(file).map_err(|error| {
                CliError::usage(format!("--console: cannot read the key file {}: {error}", file.display()))
            })?,
            None => return Ok(None),
        },
    };
    graphrt_console::secrets::SecretKey::parse(&text)
        .map(Some)
        .map_err(|error| CliError::usage(format!("--console: {error}")))
}

/// Put the console's gate in front of `routes` and add its own.
pub(super) fn wrap(
    routes: Router<Arc<AppState>>,
    state: &Arc<AppState>,
    max_upload_bytes: usize,
) -> Router<Arc<AppState>> {
    let Some(console) = &state.console else { return routes };
    routes
        .merge(graphrt_console::http::routes(console, max_upload_bytes))
        .layer(axum::middleware::from_fn_with_state(Arc::clone(console), graphrt_console::http::gate))
}

/// Tell the console what this server serves.
pub(super) fn attach(
    console: Option<&Arc<Console>>,
    state: &Arc<AppState>,
    files: &[(String, PathBuf)],
    options: &ServiceOptions,
) {
    let Some(console) = console else { return };
    let versions: IndexMap<String, u32> = console
        .served_files()
        .map(|served| served.into_iter().map(|(name, version, _)| (name, version.version)).collect())
        .unwrap_or_default();
    let served = files
        .iter()
        .filter_map(|(name, path)| {
            let workflow = state.workflows.get(name)?;
            let hash = std::fs::read(path).ok().map(|bytes| file_hash(&bytes));
            Some(ServedWorkflow {
                name: name.clone(),
                hash,
                version: versions.get(name).copied(),
                env: workflow.engine.graph().workflow().env.iter().map(|decl| decl.name.clone()).collect(),
            })
        })
        .collect();
    console.attach(Arc::new(CliHost {
        state: Arc::downgrade(state),
        served,
        registry: state.extensions.registry.clone(),
        options: options.clone(),
        trust: options.trust_keys.clone(),
    }));
}

/// SHA-256 of a file, hex: the registry's version hash.
fn file_hash(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    crate::hex::to_hex(&Sha256::digest(bytes))
}

/// Whether an owner asked for a restart (`POST /v1/console/apply`).
pub(super) fn restart_requested() -> bool {
    RESTART.load(Ordering::SeqCst)
}

/// Start `graphrt serve` again with the same arguments, after the drain.
///
/// @throws CliError - When the executable cannot be started again.
pub(super) fn restart() -> Result<u8, CliError> {
    let exe = std::env::current_exe().map_err(|error| CliError::failed(format!("cannot restart: {error}")))?;
    let mut command = std::process::Command::new(exe);
    command.args(std::env::args_os().skip(1));
    eprintln!("console: starting again to apply the changes");
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        // Replaces this process: same PID, so a supervisor or a container sees no exit.
        let error = command.exec();
        Err(CliError::failed(format!("cannot restart: {error}")))
    }
    #[cfg(not(unix))]
    {
        command.spawn().map_err(|error| CliError::failed(format!("cannot restart: {error}")))?;
        Ok(crate::error::EXIT_OK)
    }
}

/// The server, as the console sees it.
#[derive(Debug)]
struct CliHost {
    state: Weak<AppState>,
    served: Vec<ServedWorkflow>,
    registry: graphrt_core::Registry,
    options: ServiceOptions,
    trust: Vec<String>,
}

impl Host for CliHost {
    fn served(&self) -> Vec<ServedWorkflow> {
        self.served.clone()
    }

    fn validate<'a>(
        &'a self,
        name: &'a str,
        format: Format,
        bytes: &'a [u8],
    ) -> BoxFuture<'a, Result<Vec<String>, String>> {
        let registry = self.registry.clone();
        let options = self.options.clone();
        let trust = self.trust.clone();
        let name = name.to_owned();
        let bytes = bytes.to_vec();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || compile(&name, format, &bytes, registry, options, &trust))
                .await
                .map_err(|error| format!("the check stopped: {error}"))?
        })
    }

    fn restart(&self) -> Result<(), String> {
        let state = self.state.upgrade().ok_or("the server is stopping")?;
        if state.is_draining() {
            return Err("the server is stopping already".into());
        }
        RESTART.store(true, Ordering::SeqCst);
        state.request_stop();
        Ok(())
    }
}

/// Compile an uploaded version as `graphrt serve` would.
fn compile(
    name: &str,
    format: Format,
    bytes: &[u8],
    mut registry: graphrt_core::Registry,
    mut options: ServiceOptions,
    trust: &[String],
) -> Result<Vec<String>, String> {
    let graph = match format {
        Format::Yaml | Format::Json => {
            let text = std::str::from_utf8(bytes).map_err(|_| "the file is not UTF-8 text".to_owned())?;
            let workflow = graphrt_core::Workflow::parse(text).map_err(|error| error.to_string())?;
            graphrt_core::Graph::build(workflow, &registry)
                .map(Arc::new)
                .map_err(|error| crate::extensions::explain_unknown_kind(error.to_string()))?
        }
        Format::Bundle => {
            let dir = graphrt_core::private_fs::create_fresh_temp_dir("graphrt-console-upload")
                .map_err(|error| format!("cannot check the bundle: {error}"))?;
            let path = dir.join(format!("{name}.grt"));
            let built = std::fs::write(&path, bytes)
                .map_err(|error| format!("cannot check the bundle: {error}"))
                .and_then(|()| {
                    crate::bundle_cmd::bundle_graph(&path, &mut options, &mut registry, trust)
                        .map_err(|error| error.to_string())
                });
            let _cleanup = std::fs::remove_dir_all(&dir);
            built?
        }
    };
    Ok(graph.warnings().to_vec())
}

/// ID token signatures, checked with the provider's JWKS (RS256, ES256),
/// fetched again when a token names a key the set does not have yet.
#[derive(Debug)]
pub(crate) struct JwksVerifiers;

impl VerifierSource for JwksVerifiers {
    fn connect<'a>(&'a self, jwks_uri: &'a str) -> BoxFuture<'a, Result<Arc<dyn IdTokenVerifier>, String>> {
        Box::pin(async move {
            let keys = RemoteKeySet::fetch(jwks_uri).await?;
            keys.spawn_refresh();
            Ok(Arc::new(JwksVerifier { keys }) as Arc<dyn IdTokenVerifier>)
        })
    }
}

#[derive(Debug)]
struct JwksVerifier {
    keys: Arc<RemoteKeySet>,
}

impl IdTokenVerifier for JwksVerifier {
    fn verify<'a>(&'a self, id_token: &'a str) -> BoxFuture<'a, Result<Claims, String>> {
        Box::pin(async move {
            let parsed = token::parse(id_token).map_err(|error| error.to_string())?;
            if !parsed.has_candidate(&self.keys.keys()) {
                // A key the provider rotated in since the last fetch.
                self.keys.refresh(MIN_REFETCH_INTERVAL).await;
            }
            // The console checks issuer, audience and nonce; this checks the signature and `exp`.
            let rules = ClaimRules { issuer: None, audience: None, leeway_secs: 60 };
            parsed
                .verify(&self.keys.keys(), &rules, graphrt_core::store::now_ms() / 1000)
                .map_err(|e| e.to_string())?;
            parsed.claims().as_object().cloned().ok_or_else(|| "the claims are not an object".to_owned())
        })
    }
}
