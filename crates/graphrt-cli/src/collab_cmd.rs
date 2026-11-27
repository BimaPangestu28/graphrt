//! `graphrt collab` (feature `collab`): the collaboration server.
//!
//! - `graphrt collab serve --config collab.yaml`: serve it (hosted or self hosted, per the config).
//! - `graphrt collab invite --config collab.yaml --workspace "Acme"`: the first
//!   owner invite of a new server (works without the server running).
//! - `graphrt collab check --config collab.yaml`: check a config and print a summary.
//! - `graphrt collab key`: a new random key for `GRAPHRT_COLLAB_SIGNING_KEY` or the secrets key.
//!
//! See docs/collab.md.

use std::path::PathBuf;
use std::sync::Arc;

use clap::{Args, Subcommand, ValueEnum};
use graphrt_collab::server::{CollabConfig, Options, Server};

use crate::error::CliError;

/// The variable that names the config when `--config` is not given.
const CONFIG_ENV: &str = "GRAPHRT_COLLAB_CONFIG";

/// Options of `graphrt collab`.
#[derive(Debug, Args)]
pub struct CollabArgs {
    #[command(subcommand)]
    pub command: CollabCommand,
}

/// A role in a workspace.
#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum RoleArg {
    Owner,
    /// Can edit.
    Operator,
    Viewer,
}

/// Subcommands of `graphrt collab`.
#[derive(Debug, Subcommand)]
pub enum CollabCommand {
    /// Serve the collaboration server.
    Serve {
        /// The config file (YAML); default `$GRAPHRT_COLLAB_CONFIG`.
        #[arg(long, value_name = "FILE")]
        config: Option<PathBuf>,
        /// Listen address (overrides the config).
        #[arg(long, value_name = "HOST:PORT")]
        listen: Option<std::net::SocketAddr>,
    },
    /// Make an invite to a workspace (created when there is none of that name):
    /// how the first owner of a new server gets in.
    Invite {
        #[arg(long, value_name = "FILE")]
        config: Option<PathBuf>,
        /// The workspace name.
        #[arg(long)]
        workspace: String,
        #[arg(long, value_enum, default_value = "owner")]
        role: RoleArg,
        /// Only this verified email may use it (with an identity provider).
        #[arg(long)]
        email: Option<String>,
        /// Hours the invite lasts.
        #[arg(long, default_value_t = 72)]
        hours: u64,
    },
    /// Check a config and print a summary.
    Check {
        #[arg(long, value_name = "FILE")]
        config: Option<PathBuf>,
    },
    /// Print a new random 32 byte key (hex).
    Key,
}

fn config_path(config: Option<PathBuf>) -> Result<PathBuf, CliError> {
    config
        .or_else(|| std::env::var_os(CONFIG_ENV).filter(|v| !v.is_empty()).map(PathBuf::from))
        .ok_or_else(|| CliError::usage(format!("give --config FILE or set ${CONFIG_ENV}")))
}

fn env_value(name: &str) -> Option<String> {
    std::env::var(name).ok().map(|v| v.trim().to_owned()).filter(|v| !v.is_empty())
}

/// Open the server of the config at `path`, keys from the environment.
fn open(path: &std::path::Path, listen: Option<std::net::SocketAddr>) -> Result<Arc<Server>, CliError> {
    let usage = |error: graphrt_collab::server::ServerError| CliError::usage(format!("collab: {error}"));
    let mut config = CollabConfig::load(path).map_err(usage)?;
    if let Some(listen) = listen {
        config.listen = listen;
    }
    let secrets_key = match env_value(&config.secrets.key_env) {
        Some(key) => Some(key),
        None => match &config.secrets.key_file {
            Some(file) => Some(std::fs::read_to_string(file).map_err(|error| {
                CliError::usage(format!("collab: cannot read the secrets key file {}: {error}", file.display()))
            })?),
            None => None,
        },
    };
    // A provider whose client id variable is empty is off; one with a client id
    // needs the secret its config names.
    let mut client_ids = std::collections::BTreeMap::new();
    let mut client_secrets = std::collections::BTreeMap::new();
    for provider in config.auth.all_providers() {
        let client_id = match &provider.client_id_env {
            Some(name) => env_value(name),
            None => provider.client_id.clone(),
        };
        let Some(client_id) = client_id else { continue };
        if let Some(name) = &provider.client_secret_env {
            let secret = env_value(name).ok_or_else(|| {
                CliError::usage(format!("collab: the client secret variable ${name} of `{}` is not set", provider.id))
            })?;
            client_secrets.insert(provider.id.clone(), secret);
        }
        client_ids.insert(provider.id, client_id);
    }
    let signing_key = env_value(&config.auth.signing_key_env);
    Server::open(Options {
        config,
        signing_key,
        secrets_key,
        client_ids,
        client_secrets,
        verifiers: Some(Arc::new(crate::serve::console::JwksVerifiers)),
    })
    .map_err(usage)
}

/// Run `graphrt collab`.
///
/// @returns The exit code.
/// @throws CliError - On a bad config, a port in use, or a failed server.
pub fn collab(args: CollabArgs) -> Result<u8, CliError> {
    match args.command {
        CollabCommand::Key => {
            use ring::rand::SecureRandom as _;
            let mut key = [0u8; 32];
            ring::rand::SystemRandom::new().fill(&mut key).map_err(|_| CliError::failed("no system randomness"))?;
            print_line(&crate::hex::to_hex(&key))?;
            Ok(0)
        }
        CollabCommand::Check { config } => {
            let server = open(&config_path(config)?, None)?;
            print_line(&server.describe())?;
            Ok(0)
        }
        CollabCommand::Invite { config, workspace, role, email, hours } => {
            let server = open(&config_path(config)?, None)?;
            let role = match role {
                RoleArg::Owner => graphrt_console::Role::Owner,
                RoleArg::Operator => graphrt_console::Role::Operator,
                RoleArg::Viewer => graphrt_console::Role::Viewer,
            };
            let (workspace_id, code) = server
                .bootstrap_invite(&workspace, role, email, hours)
                .map_err(|error| CliError::failed(format!("collab: {error}")))?;
            print_line(&format!("workspace: {workspace} ({workspace_id})"))?;
            print_line(&format!("role:      {}", role.name()))?;
            print_line(&format!("link:      {}", server.invite_link(&code)))?;
            print_line(&format!("code:      {code}"))?;
            eprintln!("Open Graphrt Desktop, choose Share > Join a team and paste the link. It works once.");
            Ok(0)
        }
        CollabCommand::Serve { config, listen } => {
            let server = open(&config_path(config)?, listen)?;
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .map_err(|e| CliError::failed(format!("cannot start runtime: {e}")))?;
            eprintln!("graphrt {}: {}", env!("CARGO_PKG_VERSION"), server.describe());
            eprintln!(
                "  listening on {}{}",
                server.config().listen,
                if server.config().tls.is_some() { " (TLS)" } else { "" }
            );
            let result = runtime.block_on(graphrt_collab::server::http::serve(server, shutdown_signal()));
            runtime.shutdown_timeout(std::time::Duration::from_secs(2));
            result.map(|()| 0).map_err(|error| CliError::failed(format!("collab: {error}")))
        }
    }
}

fn print_line(text: &str) -> Result<(), CliError> {
    use std::io::Write as _;
    writeln!(std::io::stdout().lock(), "{text}").map_err(|error| CliError::failed(error.to_string()))
}

/// Ctrl-C or SIGTERM.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut term = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(term) => term,
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
                return;
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
    eprintln!("collab: shutting down, saving open projects");
}
