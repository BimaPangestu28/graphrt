//! `graphrt deploy`: ship a version through a graphrt gateway (feature
//! `gateway`) or straight to a platform with `--direct` (feature
//! `deploy-fly`, see `docs/deploy-direct.md`).
//!
//! - `graphrt deploy --target <t> --deployment <d> [--version <v>] <workflow>`: a gateway deploy.
//! - `graphrt deploy --target fly --direct --deployment <d> --version <v> [--json] <agent>`: a direct deploy.
//! - `graphrt deploy status|list-versions|rollback <v>|pause|resume|remove --target fly --direct --deployment <d>`:
//!   manage a direct deployment.

use std::path::PathBuf;

use clap::{Args, Subcommand, ValueEnum};

use crate::error::CliError;

/// Deploy targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "kebab-case")]
pub enum DeployTarget {
    /// local-process agents managed by the gateway.
    Local,
    /// Kubernetes Deployment (feature `gateway-k8s`).
    K8s,
    /// Cloud Run service (feature `gateway-cloud-run`).
    CloudRun,
    /// ECS Fargate task definition (feature `gateway-ecs`).
    Ecs,
    /// Lambda function version (feature `gateway-lambda`).
    Lambda,
    /// Fly.io: a Machine pool behind the gateway (feature `gateway-fly`), or with
    /// `--direct` an app served by the Fly proxy (feature `deploy-fly`).
    Fly,
    /// Azure Container App revision (feature `gateway-aca`).
    Aca,
}

impl DeployTarget {
    /// The provider kind name of `gateway.yaml`.
    pub fn kind(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::K8s => "k8s",
            Self::CloudRun => "cloud_run",
            Self::Ecs => "ecs",
            Self::Lambda => "lambda",
            Self::Fly => "fly",
            Self::Aca => "aca",
        }
    }
}

/// How a direct deployment runs (`--mode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "kebab-case")]
pub enum DirectMode {
    /// One Machine always running.
    AlwaysOn,
    /// Stopped when idle, started by the Fly proxy on the next request.
    ScaleToZero,
}

/// Who may use the chat page of a direct deployment (`--chat-access`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "kebab-case")]
pub enum DirectChatAccess {
    /// Holders of the deployment's API key: the page asks for it once.
    Key,
    /// Anyone who opens the page, within the guest limits.
    Guests,
}

/// What a direct deployment serves besides the API (`graphrt serve` flags of
/// the same names, see docs/deploy-direct.md). Everything is off unless given.
#[derive(Debug, Default, Args)]
pub struct DirectServeArgs {
    /// Direct: serve a chat page for the agent at `/chat/` and the widget `/chat/embed.js`.
    #[arg(long)]
    pub chat: bool,
    /// Direct: who may chat, `key` or `guests` [default: key]. Needs `--chat`.
    #[arg(long, value_enum, value_name = "WHO")]
    pub chat_access: Option<DirectChatAccess>,
    /// Direct: title of the chat page and the widget [default: the workflow name]. Needs `--chat`.
    #[arg(long, value_name = "TEXT")]
    pub chat_title: Option<String>,
    /// Direct: runs per minute one chat guest may start [default: 10]. Needs `--chat`.
    #[arg(long, value_name = "N")]
    pub chat_guest_rate_limit: Option<u32>,
    /// Direct: runs per minute all chat guests together may start [default: 120]. Needs `--chat`.
    #[arg(long, value_name = "N")]
    pub chat_guest_budget: Option<u32>,
    /// Direct: let pages of this site call the agent and show the chat widget,
    /// e.g. `https://www.example.com`. Repeatable.
    #[arg(long, value_name = "ORIGIN")]
    pub cors_origin: Vec<String>,
    /// Direct: the server keeps chat conversations (uses the store volume).
    #[arg(long)]
    pub conversations: bool,
    /// Direct: serve the monitoring page at `/monitor/`, with device pairing and
    /// the approvals inbox (uses the store volume).
    #[arg(long)]
    pub monitor: bool,
    /// Direct: pair devices with their own revocable token (uses the store volume).
    #[arg(long)]
    pub devices: bool,
    /// Direct: serve the approvals inbox (`/v1/approvals`).
    #[arg(long)]
    pub approvals: bool,
    /// Direct: carry out `on_expire` of requests whose time limit passed.
    #[arg(long)]
    pub expire_approvals: bool,
    /// Direct: send push notifications when a run fails or waits for a person;
    /// CONTACT is `mailto:you@example.com` or an `https:` URL (uses the store volume).
    #[arg(long, value_name = "CONTACT")]
    pub push: Option<String>,
    /// Direct: answer customers on `telegram`, `whatsapp`, `slack` or `teams`, credentials
    /// passed with `--secret` (docs/deploy-direct.md). Repeatable.
    #[arg(long, value_name = "KIND")]
    pub channel: Vec<String>,
    /// Direct: delete the store volume and what is on it (paired devices,
    /// push subscriptions, conversations) once this deploy serves. Not with an
    /// option that uses the store.
    #[arg(long)]
    pub drop_store: bool,
}

impl DirectServeArgs {
    /// The first of these flags that was given, if any.
    pub fn first_given(&self) -> Option<&'static str> {
        [
            ("--chat", self.chat),
            ("--chat-access", self.chat_access.is_some()),
            ("--chat-title", self.chat_title.is_some()),
            ("--chat-guest-rate-limit", self.chat_guest_rate_limit.is_some()),
            ("--chat-guest-budget", self.chat_guest_budget.is_some()),
            ("--cors-origin", !self.cors_origin.is_empty()),
            ("--conversations", self.conversations),
            ("--monitor", self.monitor),
            ("--devices", self.devices),
            ("--approvals", self.approvals),
            ("--expire-approvals", self.expire_approvals),
            ("--push", self.push.is_some()),
            ("--channel", !self.channel.is_empty()),
            ("--drop-store", self.drop_store),
        ]
        .into_iter()
        .find_map(|(flag, given)| given.then_some(flag))
    }

    /// The chat option given without `--chat`, if one was.
    pub fn chat_option_without_chat(&self) -> Option<&'static str> {
        if self.chat {
            return None;
        }
        [
            ("--chat-access", self.chat_access.is_some()),
            ("--chat-title", self.chat_title.is_some()),
            ("--chat-guest-rate-limit", self.chat_guest_rate_limit.is_some()),
            ("--chat-guest-budget", self.chat_guest_budget.is_some()),
        ]
        .into_iter()
        .find_map(|(flag, given)| given.then_some(flag))
    }
}

/// Options of `graphrt deploy`.
#[derive(Debug, Args)]
#[command(subcommand_negates_reqs = true, args_conflicts_with_subcommands = true)]
pub struct DeployArgs {
    /// Manage an existing direct deployment instead of deploying.
    #[command(subcommand)]
    pub action: Option<DeployAction>,
    #[command(flatten)]
    pub run: DeployRunArgs,
}

/// Options of a deploy (no subcommand).
#[derive(Debug, Args)]
pub struct DeployRunArgs {
    /// Workflow file the agents serve; with `--direct` also a bundle (`.grt`) of any size: above
    /// 1 MiB it is uploaded onto the store volume.
    #[arg(value_name = "WORKFLOW", required = true)]
    pub workflow: Option<PathBuf>,
    /// Target platform.
    #[arg(long, value_enum, default_value = "local")]
    pub target: DeployTarget,
    /// Deploy straight to the platform, without a graphrt gateway (`--target fly`).
    #[arg(long)]
    pub direct: bool,
    /// Deployment name (gateway: configured there; direct: names the Fly app).
    #[arg(long, required = true)]
    pub deployment: Option<String>,
    /// New immutable version id (gateway default: a timestamp; required with `--direct`).
    #[arg(long)]
    pub version: Option<String>,
    /// Direct: Fly region of the Machine (default: Fly picks the closest).
    #[arg(long, value_name = "CODE")]
    pub region: Option<String>,
    /// Direct: always-on or scale-to-zero [default: scale-to-zero].
    #[arg(long, value_enum)]
    pub mode: Option<DirectMode>,
    /// Direct: Fly organization slug used when the app is created [default: personal].
    #[arg(long, value_name = "SLUG")]
    pub org: Option<String>,
    /// Direct: Fly app name [default: the deployment name, sanitised].
    #[arg(long)]
    pub app: Option<String>,
    /// Direct: replace the deployment's API key (the new one is printed once).
    #[arg(long)]
    pub rotate_api_key: bool,
    /// Direct: print JSON Lines (progress steps, then one result or error).
    #[arg(long)]
    pub json: bool,
    #[command(flatten)]
    pub serve: DirectServeArgs,
    /// Gateway URL; default `$GRAPHRT_GATEWAY_URL` or `http://127.0.0.1:8700`.
    #[arg(long)]
    pub gateway: Option<String>,
    /// Environment variable holding the gateway admin token.
    #[arg(long, default_value = "GRAPHRT_GATEWAY_ADMIN_TOKEN")]
    pub token_env: String,
    /// Gateway configuration with the operator's `providers:` (remote targets).
    #[arg(long, value_name = "FILE")]
    pub config: Option<PathBuf>,
    /// Entry under `providers:` (default: the only one of the target's kind).
    #[arg(long)]
    pub provider: Option<String>,
    /// Container image (container targets; direct: default the published graphrt image of this version).
    #[arg(long)]
    pub image: Option<String>,
    /// Provider setting `KEY=VALUE` (JSON values allowed), repeatable.
    #[arg(long = "set", value_name = "KEY=VALUE")]
    pub settings: Vec<String>,
    /// Plain environment `NAME=VALUE` of the agents, repeatable (never secrets).
    #[arg(long = "env", value_name = "NAME=VALUE")]
    pub env: Vec<String>,
    /// Gateway: secret `ENV=REFERENCE` (a secret store reference). Direct: `NAME`, whose
    /// value is read from the environment variable of the same name. Repeatable.
    #[arg(long = "secret", value_name = "NAME")]
    pub secrets: Vec<String>,
    /// Concurrent runs per replica.
    #[arg(long, default_value_t = 8)]
    pub max_active_runs: u32,
    /// Warm replicas.
    #[arg(long, default_value_t = 0)]
    pub min_replicas: u32,
    /// Upper bound of replicas.
    #[arg(long, default_value_t = 4)]
    pub max_replicas: u32,
    /// SIGTERM → SIGKILL grace set on the platform.
    #[arg(long, default_value_t = 30)]
    pub grace_secs: u64,
    /// Extra arguments for `graphrt serve` (local), or the container command (remote targets).
    #[arg(last = true)]
    pub serve_args: Vec<String>,
}

/// Management subcommands of a direct deployment.
#[derive(Debug, Subcommand)]
pub enum DeployAction {
    /// State (online, paused, deploying, failed, offline), URL, current version and Machines.
    Status(DirectTargetArgs),
    /// Versions kept for rollback, newest first.
    ListVersions(DirectTargetArgs),
    /// Make a kept version current again.
    Rollback {
        /// Version label (from `list-versions`).
        version: String,
        #[command(flatten)]
        target: DirectTargetArgs,
    },
    /// Stop the deployment and take it off the internet (no autostart).
    Pause(DirectTargetArgs),
    /// Bring a paused deployment back.
    Resume(DirectTargetArgs),
    /// Delete the Fly app with its Machines, secrets and IPs.
    Remove(DirectTargetArgs),
}

impl DeployAction {
    /// The flags shared by every action.
    pub fn target(&self) -> &DirectTargetArgs {
        match self {
            Self::Status(target)
            | Self::ListVersions(target)
            | Self::Pause(target)
            | Self::Resume(target)
            | Self::Remove(target)
            | Self::Rollback { target, .. } => target,
        }
    }

    /// The action's name in JSON results (`status`, `list-versions`...).
    pub fn name(&self) -> &'static str {
        match self {
            Self::Status(_) => "status",
            Self::ListVersions(_) => "list-versions",
            Self::Rollback { .. } => "rollback",
            Self::Pause(_) => "pause",
            Self::Resume(_) => "resume",
            Self::Remove(_) => "remove",
        }
    }
}

/// Which direct deployment a management subcommand acts on.
#[derive(Debug, Args)]
pub struct DirectTargetArgs {
    /// Target platform (only `fly` supports `--direct`).
    #[arg(long, value_enum)]
    pub target: DeployTarget,
    /// Required: these subcommands manage direct deployments only.
    #[arg(long)]
    pub direct: bool,
    /// Deployment name.
    #[arg(long)]
    pub deployment: String,
    /// Fly organization slug (accepted for symmetry; apps are addressed by name).
    #[arg(long, value_name = "SLUG")]
    pub org: Option<String>,
    /// Fly app name [default: the deployment name, sanitised].
    #[arg(long)]
    pub app: Option<String>,
    /// Print JSON Lines.
    #[arg(long)]
    pub json: bool,
}

/// `graphrt deploy`.
///
/// # Errors
/// Usage errors, platform and gateway failures (with `--json`, after the error line).
pub fn deploy(args: DeployArgs) -> Result<u8, CliError> {
    match args.action {
        Some(action) => manage(&action),
        None if args.run.direct => deploy_direct(&args.run),
        None => deploy_with_gateway(args.run),
    }
}

/// Checks `--target fly --direct`; with `--json` a refusal is also an `error` line on stdout.
fn check_direct_target(target: DeployTarget, direct: bool, json: bool) -> Result<(), CliError> {
    let refusal = if !direct {
        "status, list-versions, rollback, pause, resume and remove need --direct".to_owned()
    } else if target != DeployTarget::Fly {
        format!("--direct supports --target fly only, not {}", target.kind())
    } else {
        return Ok(());
    };
    Err(direct_usage_error(json, refusal))
}

/// A usage error of a direct command, reported as JSON too when `--json` is set.
fn direct_usage_error(json: bool, message: String) -> CliError {
    if json {
        let line = serde_json::json!({"type": "error", "code": "invalid_input", "message": message});
        let _ignored = std::io::Write::write_all(&mut std::io::stdout().lock(), format!("{line}\n").as_bytes());
    }
    CliError::usage(message)
}

#[cfg(not(feature = "deploy-fly"))]
const NO_DIRECT_DEPLOYS: &str = "this build has no direct deploys (cargo feature `deploy-fly`)";

#[cfg(feature = "deploy-fly")]
fn manage(action: &DeployAction) -> Result<u8, CliError> {
    let target = action.target();
    check_direct_target(target.target, target.direct, target.json)?;
    crate::direct_cmd::manage(action)
}

#[cfg(not(feature = "deploy-fly"))]
fn manage(action: &DeployAction) -> Result<u8, CliError> {
    let target = action.target();
    check_direct_target(target.target, target.direct, target.json)?;
    Err(direct_usage_error(target.json, NO_DIRECT_DEPLOYS.to_owned()))
}

#[cfg(feature = "deploy-fly")]
fn deploy_direct(args: &DeployRunArgs) -> Result<u8, CliError> {
    check_direct_target(args.target, true, args.json)?;
    crate::direct_cmd::deploy(args)
}

#[cfg(not(feature = "deploy-fly"))]
fn deploy_direct(args: &DeployRunArgs) -> Result<u8, CliError> {
    check_direct_target(args.target, true, args.json)?;
    Err(direct_usage_error(args.json, NO_DIRECT_DEPLOYS.to_owned()))
}

fn reject_direct_only_flags(args: &DeployRunArgs) -> Result<(), CliError> {
    let used = [
        ("--region", args.region.is_some()),
        ("--mode", args.mode.is_some()),
        ("--org", args.org.is_some()),
        ("--app", args.app.is_some()),
        ("--rotate-api-key", args.rotate_api_key),
        ("--json", args.json),
    ];
    let serve_flag = args.serve.first_given();
    match used.iter().find(|(_, set)| *set).map(|(flag, _)| *flag).or(serve_flag) {
        Some(flag) => Err(CliError::usage(format!("{flag} needs --direct"))),
        None => Ok(()),
    }
}

#[cfg(feature = "gateway")]
fn deploy_with_gateway(args: DeployRunArgs) -> Result<u8, CliError> {
    reject_direct_only_flags(&args)?;
    crate::gateway_cmd::deploy(args)
}

#[cfg(not(feature = "gateway"))]
fn deploy_with_gateway(args: DeployRunArgs) -> Result<u8, CliError> {
    reject_direct_only_flags(&args)?;
    Err(CliError::usage(
        "this build deploys only with --direct (gateway deploys need the gateway build, cargo feature `gateway`)",
    ))
}
