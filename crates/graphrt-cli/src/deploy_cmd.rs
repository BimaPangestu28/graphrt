//! `graphrt deploy`: ship a version through a graphrt gateway (feature
//! `gateway`) or straight to a platform with `--direct`: Fly.io (feature
//! `deploy-fly`, see `docs/deploy-direct.md`), Google Cloud Run (feature
//! `deploy-cloud-run`, see `docs/deploy-cloud-run.md`) or AWS Lambda (feature
//! `deploy-aws`, see `docs/deploy-direct-aws.md`).
//!
//! - `graphrt deploy --target <t> --deployment <d> [--version <v>] <workflow>`: a gateway deploy.
//! - `graphrt deploy --target fly|cloud-run|lambda --direct --deployment <d> --version <v> [--json] <agent>`: a direct deploy.
//! - `graphrt deploy status|list-versions|rollback <v>|pause|resume|remove --target fly|cloud-run|lambda --direct --deployment <d>`:
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
    /// Cloud Run service behind the gateway (feature `gateway-cloud-run`), or
    /// with `--direct` a public service of its own (feature `deploy-cloud-run`).
    CloudRun,
    /// ECS Fargate task definition (feature `gateway-ecs`).
    Ecs,
    /// Lambda function version behind the gateway (feature `gateway-lambda`), or
    /// with `--direct` a function behind a public Function URL (feature `deploy-aws`).
    Lambda,
    /// Fly.io: a Machine pool behind the gateway (feature `gateway-fly`), or with
    /// `--direct` an app served by the Fly proxy (feature `deploy-fly`).
    Fly,
    /// Azure Container App revision (feature `gateway-aca`).
    Aca,
}

impl DeployTarget {
    /// The value of `--target` (`cloud-run`).
    pub fn flag(self) -> &'static str {
        match self {
            Self::CloudRun => "cloud-run",
            other => other.kind(),
        }
    }

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
    /// Direct: the server keeps chat conversations (uses the store: a volume on Fly,
    /// a bucket on Cloud Run, store on Lambda in an S3 bucket).
    #[arg(long)]
    pub conversations: bool,
    /// Direct: serve the monitoring page at `/monitor/`, with device pairing and
    /// the approvals inbox (uses the store: a volume on Fly, a bucket on Cloud Run,
    /// store on Lambda in an S3 bucket).
    #[arg(long)]
    pub monitor: bool,
    /// Direct: pair devices with their own revocable token (uses the store; store on Lambda in an S3 bucket).
    #[arg(long)]
    pub devices: bool,
    /// Direct: serve the approvals inbox (`/v1/approvals`).
    #[arg(long)]
    pub approvals: bool,
    /// Direct: carry out `on_expire` of requests whose time limit passed.
    #[arg(long)]
    pub expire_approvals: bool,
    /// Direct: send push notifications when a run fails or waits for a person;
    /// CONTACT is `mailto:you@example.com` or an `https:` URL (uses the store; store on Lambda in an S3 bucket).
    #[arg(long, value_name = "CONTACT")]
    pub push: Option<String>,
    /// Direct: answer customers on `telegram`, `whatsapp`, `slack` or `teams`, credentials
    /// passed with `--secret` (docs/deploy-direct.md). Uses the store (store on Lambda in an
    /// S3 bucket, where Telegram is refused). Repeatable.
    #[arg(long, value_name = "KIND")]
    pub channel: Vec<String>,
    /// Direct: delete the store (volume or bucket) and what is in it (paired devices,
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
    /// Manage an existing direct deployment instead of deploying, or list what
    /// each target supports (`capabilities`).
    #[command(subcommand)]
    pub action: Option<DeployCommand>,
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
    /// Deploy straight to the platform, without a graphrt gateway (`--target fly`, `cloud-run` or `lambda`).
    #[arg(long)]
    pub direct: bool,
    /// Deployment name (gateway: configured there; direct: names the Fly app, Cloud Run service or Lambda function).
    #[arg(long, required = true)]
    pub deployment: Option<String>,
    /// New immutable version id (gateway default: a timestamp; required with `--direct`).
    #[arg(long)]
    pub version: Option<String>,
    /// Direct: Fly region of the Machine (default: Fly picks the closest), Cloud Run
    /// region (default: us-central1), or AWS region (default: `AWS_REGION`, else the profile's).
    #[arg(long, value_name = "CODE")]
    pub region: Option<String>,
    /// Direct, Lambda: AWS profile of `~/.aws/credentials` / `~/.aws/config`
    /// [default: `AWS_PROFILE`, else the AWS environment variables, else `default`].
    #[arg(long, value_name = "NAME")]
    pub profile: Option<String>,
    /// Direct, Cloud Run: Google Cloud project id [default: $GOOGLE_CLOUD_PROJECT].
    #[arg(long, value_name = "ID")]
    pub project: Option<String>,
    /// Direct, Cloud Run: label `KEY=VALUE` of every resource the deploy creates. Repeatable.
    #[arg(long = "label", value_name = "KEY=VALUE")]
    pub labels: Vec<String>,
    /// Direct: always-on or scale-to-zero [default: scale-to-zero].
    #[arg(long, value_enum)]
    pub mode: Option<DirectMode>,
    /// Direct: Fly organization slug used when the app is created [default: personal].
    #[arg(long, value_name = "SLUG")]
    pub org: Option<String>,
    /// Direct: Fly app, Cloud Run service or Lambda function name [default: the
    /// deployment name, sanitised; `graphrt-<name>` on Lambda].
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
    /// Container image (container targets; direct: default the published graphrt image of this version,
    /// pulled on Cloud Run through an Artifact Registry remote repository).
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

/// Subcommands of `graphrt deploy`.
#[derive(Debug, Subcommand)]
pub enum DeployCommand {
    /// What every direct target supports in this build (flags, modes, store,
    /// channels, credentials), for apps: `--json` prints one JSON object.
    Capabilities(CapabilitiesArgs),
    #[command(flatten)]
    Direct(DeployAction),
}

/// Options of `graphrt deploy capabilities`.
#[derive(Debug, Args)]
pub struct CapabilitiesArgs {
    /// Print one JSON object (the contract is `docs/deploy-capabilities.md`).
    #[arg(long)]
    pub json: bool,
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
    /// Delete the deployment: the Fly app with its Machines, secrets and IPs, or
    /// the Cloud Run service with its secrets, bucket and service account, or the
    /// Lambda function with its image repository, role and logs.
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
    /// Target platform (`fly`, `cloud-run` or `lambda`).
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
    /// Fly app, Cloud Run service or Lambda function name [default: from the deployment name].
    #[arg(long)]
    pub app: Option<String>,
    /// Cloud Run: Google Cloud project id [default: $GOOGLE_CLOUD_PROJECT].
    #[arg(long, value_name = "ID")]
    pub project: Option<String>,
    /// Cloud Run: region of the service [default: us-central1]. Lambda: AWS region
    /// [default: `AWS_REGION`, else the profile's].
    #[arg(long, value_name = "CODE")]
    pub region: Option<String>,
    /// Lambda: AWS profile [default: `AWS_PROFILE`, else the environment, else `default`].
    #[arg(long, value_name = "NAME")]
    pub profile: Option<String>,
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
        Some(DeployCommand::Capabilities(capabilities)) => crate::deploy_capabilities::print(capabilities.json),
        Some(DeployCommand::Direct(action)) => manage(&action),
        None if args.run.direct => deploy_direct(&args.run),
        None => deploy_with_gateway(args.run),
    }
}

/// The targets `--direct` supports in this build.
const DIRECT_TARGETS: &[DeployTarget] = &[
    #[cfg(feature = "deploy-fly")]
    DeployTarget::Fly,
    #[cfg(feature = "deploy-cloud-run")]
    DeployTarget::CloudRun,
    #[cfg(feature = "deploy-aws")]
    DeployTarget::Lambda,
];

/// Checks `--direct` and a target this build deploys to directly; with `--json`
/// a refusal is also an `error` line on stdout.
fn check_direct_target(target: DeployTarget, direct: bool, json: bool) -> Result<(), CliError> {
    let refusal = if !direct {
        "status, list-versions, rollback, pause, resume and remove need --direct".to_owned()
    } else if DIRECT_TARGETS.is_empty() {
        "this build has no direct deploys (cargo features `deploy-fly`, `deploy-cloud-run`, `deploy-aws`)".to_owned()
    } else if !DIRECT_TARGETS.contains(&target) {
        let names: Vec<&str> = DIRECT_TARGETS.iter().map(|target| target.flag()).collect();
        format!("--direct supports --target {} only, not {}", names.join(" or "), target.flag())
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

#[cfg(any(feature = "deploy-fly", feature = "deploy-cloud-run", feature = "deploy-aws"))]
fn manage(action: &DeployAction) -> Result<u8, CliError> {
    let target = action.target();
    check_direct_target(target.target, target.direct, target.json)?;
    crate::direct_cmd::manage(action)
}

#[cfg(not(any(feature = "deploy-fly", feature = "deploy-cloud-run", feature = "deploy-aws")))]
fn manage(action: &DeployAction) -> Result<u8, CliError> {
    let target = action.target();
    check_direct_target(target.target, target.direct, target.json)?;
    Ok(0)
}

#[cfg(any(feature = "deploy-fly", feature = "deploy-cloud-run", feature = "deploy-aws"))]
fn deploy_direct(args: &DeployRunArgs) -> Result<u8, CliError> {
    check_direct_target(args.target, true, args.json)?;
    crate::direct_cmd::deploy(args)
}

#[cfg(not(any(feature = "deploy-fly", feature = "deploy-cloud-run", feature = "deploy-aws")))]
fn deploy_direct(args: &DeployRunArgs) -> Result<u8, CliError> {
    check_direct_target(args.target, true, args.json)?;
    Ok(0)
}

fn reject_direct_only_flags(args: &DeployRunArgs) -> Result<(), CliError> {
    let used = [
        ("--region", args.region.is_some()),
        ("--mode", args.mode.is_some()),
        ("--org", args.org.is_some()),
        ("--app", args.app.is_some()),
        ("--rotate-api-key", args.rotate_api_key),
        ("--project", args.project.is_some()),
        ("--profile", args.profile.is_some()),
        ("--label", !args.labels.is_empty()),
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
