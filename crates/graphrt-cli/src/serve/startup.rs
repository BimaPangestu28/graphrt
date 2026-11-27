//! `graphrt serve` itself: build the state from the flags, bind, print what is
//! served, run until SIGTERM / SIGINT and then drain (see [`crate::drain`]).
//!
//! The steps run in a fixed order so that, of several misconfigurations, the
//! same one is always reported first: workflows, API key and user tokens,
//! telemetry, stores, webhooks, the trusted gateway, the compiled state, then
//! the signal handlers and the listener.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Router;
use futures::future::Either;
use graphrt_core::store::{RunStore, now_ms};

use super::build::{build_state, discover};
use super::local_host::require_local_host;
use super::resume::resume_drained_runs;
use super::router::router_with_uploads;
use super::state::AppState;
use crate::args::ServeArgs;
use crate::drain::ShutdownSignals;
use crate::error::{CliError, EXIT_OK};
use crate::listen::Listener;
use crate::otel::Telemetry;
use crate::run::service_options;
use crate::setup::{self, ServiceOptions};

/// What the startup banner reports besides the state itself.
struct StartupReport<'a> {
    args: &'a ServeArgs,
    has_api_key: bool,
    loopback: bool,
    /// [`crate::user_auth::UserAuth::describe`], with `--verify-jwt`.
    user_auth: Option<String>,
}

/// How the HTTP server stopped.
enum Stopped {
    /// A signal started a drain, which finished.
    Drained,
    /// The server task ended on its own, before any signal.
    ServerEnded,
}

/// Execute `graphrt serve`.
pub async fn serve(args: ServeArgs) -> Result<u8, CliError> {
    // An agent behind a gateway is started because a run is on its way: the
    // model HTTP client (the system trust store, ~2 ms of certificate files)
    // loads alongside the flow build and the bind instead of in the run's first
    // model call.
    #[cfg(all(feature = "llm", feature = "trust-gateway"))]
    if args.gateway.trust_gateway {
        graphrt_llm::prewarm_http_client();
    }
    let mut options = service_options(&args.services)?;
    let mut args = args;
    super::env_flags::apply(&mut args, |name| lookup_env(&options, name))?;
    let args = args;
    #[cfg(feature = "plugins")]
    {
        options.plugins.pool = plugin_pool(&args)?;
    }
    let files = discover(&args.path)?;
    // A key mounted from a file (Secret Manager, a Kubernetes secret) ends with a
    // newline that clients never send.
    let api_key =
        lookup_env(&options, &args.api_key_env).map(|key| key.trim().to_owned()).filter(|key| !key.is_empty());
    // The console's secrets and registry versions are part of what is served.
    #[cfg(feature = "console")]
    let (console, files) = super::console::open(&args, &mut options, files, api_key.as_deref())?;
    #[cfg(feature = "jwt")]
    let user_auth = user_auth_from_args(&args, &options, api_key.is_some()).await?;
    #[cfg(feature = "jwt")]
    let verifies_user_tokens = user_auth.is_some();
    #[cfg(not(feature = "jwt"))]
    let verifies_user_tokens = false;
    let telemetry = Telemetry::from_args(&args.services, &options.all_vars())?;
    options.capture_content = crate::otel::captures_content(telemetry.as_ref());
    let configured =
        configured_state(&args, &options, &files, api_key.clone(), telemetry.clone(), verifies_user_tokens);
    let state = configured.await?;
    #[cfg(feature = "web-chat")]
    let user_token_header = user_auth.as_ref().map(|auth| auth.header().to_string());
    #[cfg(feature = "jwt")]
    let (state, user_auth_line) = {
        let line = user_auth.as_ref().map(crate::user_auth::UserAuth::describe);
        (state.with_user_auth(user_auth), line)
    };
    #[cfg(not(feature = "jwt"))]
    let user_auth_line = None;
    #[cfg(feature = "files")]
    let state = state.with_workspace_api(args.workspace_api, args.max_upload_bytes)?;
    #[cfg(feature = "cors")]
    let cors = cors_from_args(&args, &options, api_key.is_some())?;
    #[cfg(feature = "conversations")]
    let state = state.with_conversations(conversations_from_args(&args)?);
    #[cfg(feature = "web-chat")]
    let state = {
        let context = super::chat::ChatContext {
            api_key: api_key.as_deref(),
            user_token_header,
            #[cfg(feature = "cors")]
            frame_origins: cors.as_ref().map(|cors| cors.origins().to_vec()).unwrap_or_default(),
            #[cfg(not(feature = "cors"))]
            frame_origins: Vec::new(),
        };
        let chat = super::chat::Chat::new(&args.chat, context, |name| state.start_inputs(name))?;
        state.with_chat(chat)
    };
    #[cfg(feature = "channels")]
    let state = channels_from_args(state, &args, &options)?;
    #[cfg(feature = "jwt")]
    let state = with_user_resume(state, &args, verifies_user_tokens)?;
    #[cfg(feature = "devices")]
    let state = monitoring_from_args(state, &args, &options, api_key.is_some())?;
    #[cfg(feature = "console")]
    let state = state.with_console(console.clone());
    let state = Arc::new(state);
    #[cfg(feature = "channels")]
    if let Some(channels) = state.channels() {
        channels.start(&state);
    }
    #[cfg(feature = "console")]
    super::console::attach(console.as_ref(), &state, &files, &options);
    #[cfg(feature = "approvals")]
    if state.approvals().is_some_and(|approvals| approvals.expire) {
        crate::approvals::spawn_expiry(&state);
    }
    #[cfg(feature = "conversations")]
    if let Some(conversations) = state.conversations() {
        conversations.spawn_retention();
    }
    // An agent behind a gateway is started because a run is on its way: its
    // plugin processes (a Node.js host takes tens of ms) start alongside the
    // bind and that run's dispatch instead of at its first plugin node. A
    // plugin pool fills at startup in any mode.
    #[cfg(feature = "trust-gateway")]
    let behind_gateway = state.gateway_agent().is_some();
    #[cfg(not(feature = "trust-gateway"))]
    let behind_gateway = false;
    if behind_gateway || options.plugins.pool.size > 0 {
        state.prewarm_plugins();
    }
    recover_store(&state, args.store_ttl_days).await?;
    let app = router_with_uploads(Arc::clone(&state), args.max_body_bytes, args.max_upload_bytes);
    // Before listening: from here on SIGTERM / SIGINT start a drain (as PID 1 of a
    // container, an unhandled SIGTERM would be ignored until the SIGKILL).
    let signals = ShutdownSignals::install()?.with_stop_requests(state.stop_requests());
    let listener = Listener::bind(&args.addr).await?;
    let loopback = listener.is_local();
    #[cfg(feature = "files")]
    refuse_open_workspace_api(&state, api_key.is_some(), loopback, &args.api_key_env)?;
    // Granting tools to skills and approving lessons outlive a run: not for anyone who reaches the port.
    #[cfg(any(feature = "skills", feature = "learning"))]
    if api_key.is_none() && !loopback {
        state.close_owner_api();
    }
    let app = guard_keyless_loopback(app, api_key.is_none() && loopback && listener.is_browser_reachable());
    // Outermost: a refusal of any layer below still carries the CORS headers,
    // so a page can read why it was refused.
    #[cfg(feature = "cors")]
    let app = match &cors {
        Some(cors) => {
            eprintln!("  {}", cors.describe());
            app.layer(axum::middleware::from_fn_with_state(Arc::clone(cors), super::cors::apply))
        }
        None => app,
    };
    let report = StartupReport { args: &args, has_api_key: api_key.is_some(), loopback, user_auth: user_auth_line };
    print_startup(&state, &report);
    if let Stopped::ServerEnded = serve_until_drained(listener, app, &state, &args, signals).await? {
        return Ok(EXIT_OK);
    }
    if let Some(telemetry) = &telemetry {
        telemetry.shutdown().await;
    }
    // An owner applied console changes: start again with them.
    #[cfg(feature = "console")]
    if super::console::restart_requested() {
        return super::console::restart();
    }
    Ok(EXIT_OK)
}

/// Refuse to serve the workspace routes to a network without an API key:
/// anyone who reaches the port could read and replace the agents' files.
///
/// @throws CliError (usage) - `--workspace-api` without a key on an address other hosts can reach.
#[cfg(feature = "files")]
fn refuse_open_workspace_api(
    state: &AppState,
    has_api_key: bool,
    loopback: bool,
    api_key_env: &str,
) -> Result<(), CliError> {
    if state.file_api_line().is_none() || has_api_key || loopback {
        return Ok(());
    }
    Err(CliError::usage(format!(
        "`--workspace-api` on an address other hosts can reach needs an API key: set ${api_key_env}"
    )))
}

/// The cross-origin policy of `--cors-origin` (else `$GRAPHRT_CORS_ORIGINS`).
/// Pages may send the headers mapped with `--input-header` besides the standard ones.
///
/// @throws CliError (usage) - On an entry that is not an origin, or `*` without an API key.
#[cfg(feature = "cors")]
fn cors_from_args(
    args: &ServeArgs,
    options: &ServiceOptions,
    has_api_key: bool,
) -> Result<Option<Arc<super::cors::Cors>>, CliError> {
    let origins = super::cors::origins_from(&args.cors_origin, lookup_env(options, super::cors::ORIGINS_ENV));
    let mapped = args.input_header.iter().map(|spec| spec.header.as_str());
    Ok(super::cors::Cors::new(&origins, mapped, has_api_key)?.map(Arc::new))
}

/// The run store directory: `--store`, else `$GRAPHRT_STORE`.
#[cfg(any(feature = "conversations", feature = "devices", feature = "console"))]
pub(super) fn store_dir(args: &ServeArgs) -> Option<PathBuf> {
    let env_dir = std::env::var(crate::store::STORE_ENV).ok().filter(|dir| !dir.is_empty()).map(PathBuf::from);
    args.store.clone().or(env_dir)
}

/// What `--monitor`, `--devices`, `--approvals`, `--expire-approvals` and
/// `--push` turn on. Their files live in the run store directory, or in memory
/// without one.
///
/// @throws CliError (usage) - A `--public-url` that is not an address, a `--push`
///   contact of another form, or files that cannot be read or created.
#[cfg(feature = "devices")]
fn monitoring_from_args(
    state: AppState,
    args: &ServeArgs,
    options: &ServiceOptions,
    has_api_key: bool,
) -> Result<AppState, CliError> {
    let flags = &args.monitor;
    let dir = store_dir(args);
    #[cfg(feature = "web-monitor")]
    let page = flags.monitor;
    #[cfg(not(feature = "web-monitor"))]
    let page = false;
    let public_url = flags
        .public_url
        .clone()
        .or_else(|| lookup_env(options, "GRAPHRT_PUBLIC_URL"))
        .filter(|url| !url.trim().is_empty())
        .map(|url| {
            crate::devices::api::server_url(&url).map_err(|why| CliError::usage(format!("--public-url `{url}`: {why}")))
        })
        .transpose()?;
    let devices = if flags.devices || page { Some(crate::devices::Devices::open(dir.as_deref())?) } else { None };
    if let Some(devices) = &devices {
        eprintln!("  {}", devices.describe());
    }
    let state = state.with_devices(devices, public_url);
    #[cfg(feature = "approvals")]
    let state = {
        let on = flags.approvals || flags.expire_approvals || page;
        let approvals = on.then_some(crate::approvals::Approvals { expire: flags.expire_approvals });
        if let Some(approvals) = &approvals {
            eprintln!("  {}", approvals.describe());
        }
        state.with_approvals(approvals)
    };
    #[cfg(feature = "web-push")]
    let state = {
        use crate::notify::{Notifications, Notifier, webpush::WebPush};
        let push = flags
            .push
            .as_deref()
            .map(|contact| WebPush::open(dir.as_deref(), contact, options.allow_private_network))
            .transpose()?
            .map(Arc::new);
        if let Some(push) = &push {
            eprintln!("  {}", push.describe());
        }
        let notifiers: Vec<Arc<dyn Notifier>> = push.iter().map(|push| Arc::clone(push) as Arc<dyn Notifier>).collect();
        // With `--user-resume`, a pause of a user's own run is a question to that user.
        state.with_push(push).with_notifications(Some(Notifications::new(notifiers, args.user_resume)))
    };
    #[cfg(feature = "web-monitor")]
    let state = {
        let monitor = page.then(|| super::monitor::Monitor::new(has_api_key));
        if let Some(monitor) = &monitor {
            eprintln!("  {}", monitor.describe());
        }
        state.with_monitor(monitor)
    };
    #[cfg(not(feature = "web-monitor"))]
    let _ = has_api_key;
    #[cfg(not(feature = "web-push"))]
    let _ = options;
    Ok(state)
}

/// The conversations of `--conversations`, kept in the run store directory
/// (`--store`, else `$GRAPHRT_STORE`) or in memory without one.
///
/// @throws CliError (usage) - When the directory cannot be created.
#[cfg(feature = "conversations")]
fn conversations_from_args(args: &ServeArgs) -> Result<Option<crate::conversations::Conversations>, CliError> {
    let flags = &args.conversations;
    // Every customer of a channel has a conversation.
    #[cfg(feature = "channels")]
    let for_channels = !args.channels.channel.is_empty() || args.channels.channels.is_some();
    #[cfg(not(feature = "channels"))]
    let for_channels = false;
    if !flags.conversations && !for_channels {
        return Ok(None);
    }
    let dir = store_dir(args);
    let limits = crate::conversations::Limits {
        max_messages: flags.conversation_max_messages,
        per_user: flags.conversations_per_user,
        ttl_days: flags.conversation_ttl_days,
    };
    let conversations = crate::conversations::Conversations::open(dir.as_deref(), limits)?;
    eprintln!("  {}", conversations.describe(dir.is_some()));
    Ok(Some(conversations))
}

/// The channels of `--channel` and `--channels`, with their secrets from the
/// environment or `--env-file`.
///
/// @throws CliError (usage) - A malformed flag or file, a missing secret, or a
///   channel of a workflow that is not served.
#[cfg(feature = "channels")]
fn channels_from_args(state: AppState, args: &ServeArgs, options: &ServiceOptions) -> Result<AppState, CliError> {
    let flags = &args.channels;
    let defaults = crate::channels::Defaults { rate_limit: flags.channel_rate_limit, budget: flags.channel_budget };
    let lookup = |name: &str| lookup_env(options, name);
    let configs = crate::channels::config::load(&flags.channel, flags.channels.as_deref(), defaults, &lookup)?;
    #[cfg(feature = "devices")]
    let public_url = args
        .monitor
        .public_url
        .clone()
        .or_else(|| lookup_env(options, "GRAPHRT_PUBLIC_URL"))
        .filter(|url| !url.trim().is_empty())
        .map(|url| url.trim().trim_end_matches('/').to_owned());
    #[cfg(not(feature = "devices"))]
    let public_url = None;
    let channels = crate::channels::Channels::new(configs, |name| state.serves(name), public_url, args.user_resume)?;
    if let Some(channels) = &channels {
        for line in channels.describe() {
            eprintln!("  {line}");
        }
    }
    Ok(state.with_channels(channels.map(Arc::new)))
}

/// `--user-resume`, which needs users: `--verify-jwt` or a chat page for guests.
///
/// @throws CliError (usage) - When the flag is set and no user can ever call.
#[cfg(feature = "jwt")]
fn with_user_resume(state: AppState, args: &ServeArgs, verifies_user_tokens: bool) -> Result<AppState, CliError> {
    if !args.user_resume {
        return Ok(state);
    }
    #[cfg(feature = "web-chat")]
    let has_guests = state.chat.as_deref().is_some_and(super::chat::Chat::is_public);
    #[cfg(not(feature = "web-chat"))]
    let has_guests = false;
    #[cfg(feature = "channels")]
    let has_customers = state.channels().is_some();
    #[cfg(not(feature = "channels"))]
    let has_customers = false;
    if !verifies_user_tokens && !has_guests && !has_customers {
        return Err(CliError::usage("--user-resume needs users: set --verify-jwt, --chat-access guests or --channel"));
    }
    Ok(state.with_user_resume(true))
}

/// `--plugin-pool` and `--plugin-idle`.
///
/// @throws CliError (usage) - `--plugin-idle` without a pool.
#[cfg(feature = "plugins")]
fn plugin_pool(args: &ServeArgs) -> Result<crate::extensions::PluginPool, CliError> {
    let idle_timeout = args.plugin_idle.filter(|idle| !idle.is_zero());
    if idle_timeout.is_some() && args.plugin_pool == 0 {
        return Err(CliError::usage("`--plugin-idle` needs `--plugin-pool 1` or more"));
    }
    Ok(crate::extensions::PluginPool { size: usize::from(args.plugin_pool), idle_timeout })
}

/// A variable of the process environment, else of `--env-file`.
pub(super) fn lookup_env(options: &ServiceOptions, name: &str) -> Option<String> {
    std::env::var(name).ok().or_else(|| options.env_file.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone()))
}

/// End-user JWT verification (`--verify-jwt`), which needs an API key too.
///
/// @throws CliError (usage) - Without an API key, or on invalid JWT flags.
#[cfg(feature = "jwt")]
async fn user_auth_from_args(
    args: &ServeArgs,
    options: &ServiceOptions,
    has_api_key: bool,
) -> Result<Option<crate::user_auth::UserAuth>, CliError> {
    crate::user_auth::require_api_key(args.user_auth.verify_jwt.is_some(), has_api_key, &args.api_key_env)?;
    let lookup = |name: &str| lookup_env(options, name);
    crate::user_auth::UserAuth::from_args(&args.user_auth, &args.input_header, lookup).await
}

/// `--deployment` (or `$GRAPHRT_DEPLOYMENT`), validated: one key segment of a
/// gateway store (`[A-Za-z0-9_.:=-]`, no `/`).
///
/// @throws CliError (usage) - On an invalid name.
pub(crate) fn deployment(args: &ServeArgs, options: &ServiceOptions) -> Result<Option<String>, CliError> {
    let Some(name) = args.deployment.clone().or_else(|| lookup_env(options, "GRAPHRT_DEPLOYMENT")) else {
        return Ok(None);
    };
    if name.is_empty() {
        return Ok(None);
    }
    if name.contains('/') || graphrt_core::coordination::validate_key(&name).is_err() {
        return Err(CliError::usage(format!("invalid deployment name `{name}` (letters, digits and _ . : = -)")));
    }
    Ok(Some(name))
}

/// The trusted gateway (`--trust-gateway`), which excludes `--verify-jwt`:
/// behind a gateway, the gateway verifies end-user tokens.
///
/// @throws CliError (usage) - On invalid gateway flags, or with `--verify-jwt`.
#[cfg(feature = "trust-gateway")]
fn gateway_agent_from_args(
    args: &ServeArgs,
    options: &ServiceOptions,
    verifies_user_tokens: bool,
) -> Result<Option<Arc<crate::gateway_agent::GatewayAgent>>, CliError> {
    let lookup = |name: &str| lookup_env(options, name);
    let deployment = deployment(args, options)?;
    let config =
        crate::gateway_agent::GatewayAgentConfig::from_args(&args.gateway, deployment, lookup, args.max_body_bytes)?;
    if config.is_some() && verifies_user_tokens {
        return Err(CliError::usage(
            "--trust-gateway and --verify-jwt are exclusive: behind a gateway, the gateway verifies end-user tokens",
        ));
    }
    Ok(config.map(|config| Arc::new(crate::gateway_agent::GatewayAgent::new(config))))
}

/// Compile the workflows and attach everything the flags configure, except
/// user-token verification (see [`serve`]). `verifies_user_tokens` tells
/// whether `--verify-jwt` is on, which a trusted gateway excludes.
///
/// @throws CliError - On an unusable store, webhooks file, gateway setup,
///   workflow, header mapping, Langfuse setup or evaluators config.
async fn configured_state(
    args: &ServeArgs,
    options: &ServiceOptions,
    files: &[(String, PathBuf)],
    api_key: Option<String>,
    telemetry: Option<Arc<Telemetry>>,
    verifies_user_tokens: bool,
) -> Result<AppState, CliError> {
    let limits = setup::limits(args.services.timeout_ms, args.services.max_steps);
    let stores = crate::store::open_stores(args.store.as_ref(), args.max_runs)?;
    let webhooks = match &args.webhooks {
        Some(path) => crate::webhooks::Webhooks::load(path, &options.all_vars(), options.allow_private_network)?,
        None => crate::webhooks::Webhooks::default(),
    }
    .with_deployment(deployment(args, options)?);
    #[cfg(feature = "online-eval")]
    let judge_limits = limits.clone();
    #[cfg(feature = "trust-gateway")]
    let gateway = gateway_agent_from_args(args, options, verifies_user_tokens)?;
    #[cfg(not(feature = "trust-gateway"))]
    let _ = verifies_user_tokens;
    let has_telemetry = telemetry.is_some();
    let state = build_state(files, options, limits, api_key)
        .await?
        .with_stores(stores)
        .with_telemetry(telemetry)
        .with_store_inputs(args.store_inputs)
        .with_max_active_runs(args.max_active_runs)
        .with_webhooks(webhooks)?
        .with_header_inputs(&args.input_header)?;
    #[cfg(feature = "trust-gateway")]
    let state = state.with_gateway_agent(gateway);
    #[cfg(feature = "langfuse")]
    let state = with_langfuse_scores(state, args, options, has_telemetry)?;
    #[cfg(not(feature = "langfuse"))]
    let _ = has_telemetry;
    #[cfg(feature = "online-eval")]
    let state = match &args.evaluators {
        Some(path) => state.with_evaluators(path, options, judge_limits)?,
        None => state,
    };
    Ok(state)
}

/// Upload scores to Langfuse with `--langfuse-scores`. Call before
/// [`AppState::with_evaluators`], whose pool reports to the sinks set then.
///
/// @throws CliError - When the uploader cannot start (credentials, egress policy).
#[cfg(feature = "langfuse")]
fn with_langfuse_scores(
    state: AppState,
    args: &ServeArgs,
    options: &ServiceOptions,
    has_telemetry: bool,
) -> Result<AppState, CliError> {
    if !args.langfuse_scores {
        return Ok(state);
    }
    let uploader = crate::langfuse::LangfuseUploader::start(&options.all_vars(), options.allow_private_network)?;
    eprintln!("  scores uploaded to {}", uploader.url());
    if !has_telemetry {
        eprintln!(
            "warning: --langfuse-scores without OpenTelemetry: runs have no trace id, so no score is uploaded \
             (add --otel-preset langfuse)"
        );
    }
    Ok(state.with_langfuse(Some(uploader)))
}

/// After a restart, mark the runs the previous process left executing as
/// failed; then start the `--store-ttl-days` cleanup.
///
/// @throws CliError - When the store cannot be read.
async fn recover_store(state: &AppState, ttl_days: Option<u64>) -> Result<(), CliError> {
    let interrupted = state.recover().await?;
    if interrupted > 0 {
        eprintln!("marked {interrupted} run(s) interrupted by the previous shutdown as failed");
    }
    if let Some(days) = ttl_days.filter(|d| *d > 0) {
        spawn_retention(Arc::clone(state.store()), days);
    }
    Ok(())
}

/// Without an API key, a loopback server is reachable from any web page through
/// DNS rebinding (the page's own host name resolving to 127.0.0.1). Such
/// requests carry that foreign host name, so only local `Host` values are
/// accepted when `keyless_loopback` (a Unix socket is out of a browser's reach
/// and needs no such check).
fn guard_keyless_loopback(app: Router, keyless_loopback: bool) -> Router {
    if keyless_loopback { app.layer(axum::middleware::from_fn(require_local_host)) } else { app }
}

/// The startup banner on stderr: address, routes, and warnings about
/// unauthenticated access.
fn print_startup(state: &AppState, report: &StartupReport<'_>) {
    let addr = &report.args.addr;
    let shown_addr = if crate::listen::unix_path(addr).is_some() { addr.clone() } else { format!("http://{addr}") };
    eprintln!("graphrt serving {} workflow(s) on {shown_addr}", state.workflows.len());
    #[cfg(feature = "trust-gateway")]
    if let Some(agent) = state.gateway_agent() {
        eprintln!("  trusting the gateway of deployment `{}` (signed /internal/v1)", agent.deployment());
    }
    if let Some(line) = &report.user_auth {
        eprintln!("  {line}");
    }
    for name in state.workflows.keys() {
        eprintln!("  POST /v1/workflows/{name}/runs");
    }
    for hook in state.webhooks().hooks() {
        eprintln!("  POST /v1/hooks/{} -> {}", hook.id, hook.workflow);
        if !hook.is_signed() && !report.has_api_key {
            eprintln!("warning: webhook `{}` has no `signature` and no API key is set; anyone can trigger it", hook.id);
        }
    }
    #[cfg(feature = "files")]
    if let Some(line) = state.file_api_line() {
        eprintln!("  {line}");
    }
    #[cfg(feature = "web-chat")]
    if let Some(chat) = state.chat.as_deref() {
        for line in chat.describe() {
            eprintln!("  {line}");
        }
        if chat.is_public() && !report.loopback {
            eprintln!(
                "warning: the chat page is open to anyone who can reach {}: guests start runs with this server's \
                 credentials, within --chat-guest-rate-limit and --chat-guest-budget",
                report.args.addr
            );
        }
    }
    #[cfg(feature = "rag")]
    if state.knowledge().is_some() {
        eprintln!("  knowledge base API under /v1/knowledge (uploads up to {} bytes)", report.args.max_upload_bytes);
    }
    #[cfg(feature = "skills")]
    print_bundled_skills(state);
    #[cfg(any(feature = "skills", feature = "learning"))]
    if !report.has_api_key && !report.loopback {
        eprintln!("  /v1/skills and /v1/learning are refused: they need an API key on a network address");
    }
    if !report.has_api_key {
        print_keyless_warnings(report);
    }
}

/// Say which skills each served bundle runs on, and what they may use: the
/// grants came with the bundle, so the operator sees them at every start.
#[cfg(feature = "skills")]
fn print_bundled_skills(state: &AppState) {
    for (workflow, store) in state.bundled_skills() {
        let skills = match store.list() {
            Ok(skills) => skills,
            Err(error) => {
                eprintln!("warning: the skills bundled with `{workflow}` cannot be read: {error}");
                continue;
            }
        };
        for skill in skills {
            let grants: Vec<String> = skill
                .grants
                .iter()
                .map(|grant| match grant.mode {
                    graphrt_skill::GrantMode::Allow => format!("{} without asking", grant.tool),
                    graphrt_skill::GrantMode::Ask => format!("{} asking first", grant.tool),
                })
                .collect();
            let tools = if grants.is_empty() { "no tools".to_owned() } else { grants.join(", ") };
            eprintln!("  workflow `{workflow}` runs on its bundled skill `{}` ({tools})", skill.name);
        }
    }
}

fn print_keyless_warnings(report: &StartupReport<'_>) {
    let args = report.args;
    eprintln!("warning: no API key set (${}); the API is unauthenticated", args.api_key_env);
    if !report.loopback {
        eprintln!(
            "warning: listening on a non-loopback address without an API key: anyone who can reach {} \
             can start runs with this server's credentials. Set ${} or bind 127.0.0.1",
            args.addr, args.api_key_env
        );
    }
}

/// Serve `app` until SIGTERM / SIGINT, drain the active runs within
/// `--drain-timeout`, then close the connections still open. Drained runs of
/// the previous process are resumed in the background (unless
/// `--no-resume-drained`).
///
/// @throws CliError - On a server error.
async fn serve_until_drained(
    listener: Listener,
    app: Router,
    state: &Arc<AppState>,
    args: &ServeArgs,
    mut signals: ShutdownSignals,
) -> Result<Stopped, CliError> {
    let (close_tx, close_rx) = tokio::sync::oneshot::channel::<()>();
    let mut server = tokio::spawn(listener.serve(app, async {
        let _closed = close_rx.await;
    }));
    if !args.no_resume_drained {
        // After listening, so a store scan never delays the first health check.
        tokio::spawn(resume_drained_runs(Arc::clone(state)));
    }
    let cause = match futures::future::select(&mut server, std::pin::pin!(signals.recv())).await {
        Either::Left((ended, _)) => {
            server_ended(ended)?;
            return Ok(Stopped::ServerEnded);
        }
        Either::Right((cause, _)) => cause,
    };
    let deadline = Instant::now() + args.drain_timeout;
    drain_runs(state, args.drain_timeout, cause, &mut signals).await;
    // Connections still open (idle keep-alives, finished streams) get the rest of
    // the deadline, at least a moment, to close.
    let _closing = close_tx.send(());
    close_connections(server, deadline).await?;
    Ok(Stopped::Drained)
}

/// Pause (and near the deadline preempt) every active run, and report how
/// they ended on stderr.
async fn drain_runs(state: &AppState, timeout: Duration, cause: &str, signals: &mut ShutdownSignals) {
    eprintln!("received {cause}: draining (no new runs; active runs pause, deadline {:.1} s)", timeout.as_secs_f64());
    let report = crate::drain::drain(state, timeout, cause, Some(signals)).await;
    eprintln!("{report}");
    if report.unfinished > 0 {
        eprintln!(
            "warning: {} run(s) did not stop in time; the next start marks them failed (raise --drain-timeout)",
            report.unfinished
        );
    }
}

/// Wait for the HTTP server to close its connections until `deadline` (at
/// least a moment), then abort the ones left.
///
/// @throws CliError - On a server error.
async fn close_connections(
    mut server: tokio::task::JoinHandle<std::io::Result<()>>,
    deadline: Instant,
) -> Result<(), CliError> {
    let rest = deadline.saturating_duration_since(Instant::now()).max(Duration::from_millis(100));
    match tokio::time::timeout(rest, &mut server).await {
        Ok(ended) => server_ended(ended),
        Err(_) => {
            server.abort();
            eprintln!("warning: closing the remaining connections at the drain deadline");
            Ok(())
        }
    }
}

/// The result of the HTTP server task.
///
/// @throws CliError - When the server failed or its task panicked.
fn server_ended(ended: Result<std::io::Result<()>, tokio::task::JoinError>) -> Result<(), CliError> {
    match ended {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(CliError::failed(format!("server error: {e}"))),
        Err(e) => Err(CliError::failed(format!("server task failed: {e}"))),
    }
}

/// Delete runs older than `days` (any status except running), hourly.
fn spawn_retention(store: Arc<dyn RunStore>, days: u64) {
    graphrt_core::task::spawn(async move {
        let ttl_ms = days.saturating_mul(24 * 60 * 60 * 1000);
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(60 * 60));
        loop {
            tick.tick().await;
            match store.purge_before(now_ms().saturating_sub(ttl_ms)).await {
                Ok(0) => {}
                Ok(n) => eprintln!("run store: removed {n} run(s) older than {days} day(s)"),
                Err(e) => eprintln!("warning: run store cleanup failed: {e}"),
            }
        }
    });
}
