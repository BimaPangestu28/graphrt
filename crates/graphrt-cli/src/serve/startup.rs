//! `graphrt serve` itself: build the state from the flags, bind, print what is
//! served and run until Ctrl-C.
//!
//! The steps run in a fixed order so that, of several misconfigurations, the
//! same one is always reported first: workflows, API key and user tokens,
//! telemetry, stores, webhooks, the compiled state, then the listener.

use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use graphrt_core::store::{RunStore, now_ms};
use tokio::net::TcpListener;

use super::build::{build_state, discover};
use super::local_host::require_local_host;
use super::router::router_with_uploads;
use super::state::AppState;
use crate::args::ServeArgs;
use crate::error::{CliError, EXIT_OK};
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

/// Execute `graphrt serve`.
pub async fn serve(args: ServeArgs) -> Result<u8, CliError> {
    let mut options = service_options(&args.services)?;
    let files = discover(&args.path)?;
    let api_key = lookup_env(&options, &args.api_key_env).filter(|key| !key.is_empty());
    #[cfg(feature = "jwt")]
    let user_auth = user_auth_from_args(&args, &options, api_key.is_some()).await?;
    let telemetry = Telemetry::from_args(&args.services, &options.all_vars())?;
    options.capture_content = crate::otel::captures_content(telemetry.as_ref());
    let state = configured_state(&args, &options, &files, api_key.clone(), telemetry.clone()).await?;
    #[cfg(feature = "jwt")]
    let (state, user_auth_line) = {
        let line = user_auth.as_ref().map(crate::user_auth::UserAuth::describe);
        (state.with_user_auth(user_auth), line)
    };
    #[cfg(not(feature = "jwt"))]
    let user_auth_line = None;
    let state = Arc::new(state);
    recover_store(&state, args.store_ttl_days).await?;
    let app = router_with_uploads(Arc::clone(&state), args.max_body_bytes, args.max_upload_bytes);
    let listener = bind(&args.addr).await?;
    let loopback = listener.local_addr().map(|a| a.ip().is_loopback()).unwrap_or(false);
    let app = guard_keyless_loopback(app, api_key.is_none() && loopback);
    let report = StartupReport { args: &args, has_api_key: api_key.is_some(), loopback, user_auth: user_auth_line };
    print_startup(&state, &report);
    serve_until_ctrl_c(listener, app).await?;
    if let Some(telemetry) = &telemetry {
        telemetry.shutdown().await;
    }
    Ok(EXIT_OK)
}

/// A variable of the process environment, else of `--env-file`.
fn lookup_env(options: &ServiceOptions, name: &str) -> Option<String> {
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

/// Compile the workflows and attach everything the flags configure, except
/// user-token verification (see [`serve`]).
///
/// @throws CliError - On an unusable store, webhooks file, workflow, header
///   mapping, Langfuse setup or evaluators config.
async fn configured_state(
    args: &ServeArgs,
    options: &ServiceOptions,
    files: &[(String, PathBuf)],
    api_key: Option<String>,
    telemetry: Option<Arc<Telemetry>>,
) -> Result<AppState, CliError> {
    let limits = setup::limits(args.services.timeout_ms, args.services.max_steps);
    let stores = crate::store::open_stores(args.store.as_ref(), args.max_runs)?;
    let webhooks = match &args.webhooks {
        Some(path) => crate::webhooks::Webhooks::load(path, &options.all_vars(), options.allow_private_network)?,
        None => crate::webhooks::Webhooks::default(),
    };
    #[cfg(feature = "online-eval")]
    let judge_limits = limits.clone();
    let has_telemetry = telemetry.is_some();
    let state = build_state(files, options, limits, api_key)
        .await?
        .with_stores(stores)
        .with_telemetry(telemetry)
        .with_store_inputs(args.store_inputs)
        .with_max_active_runs(args.max_active_runs)
        .with_webhooks(webhooks)?
        .with_header_inputs(&args.input_header)?;
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

/// @throws CliError (usage) - When the address cannot be bound.
async fn bind(addr: &str) -> Result<TcpListener, CliError> {
    TcpListener::bind(addr).await.map_err(|e| CliError::usage(format!("cannot listen on {addr}: {e}")))
}

/// Without an API key, a loopback server is reachable from any web page through
/// DNS rebinding (the page's own host name resolving to 127.0.0.1). Such
/// requests carry that foreign host name, so only local `Host` values are
/// accepted when `keyless_loopback`.
fn guard_keyless_loopback(app: Router, keyless_loopback: bool) -> Router {
    if keyless_loopback { app.layer(axum::middleware::from_fn(require_local_host)) } else { app }
}

/// The startup banner on stderr: address, routes, and warnings about
/// unauthenticated access.
fn print_startup(state: &AppState, report: &StartupReport<'_>) {
    eprintln!("graphrt serving {} workflow(s) on http://{}", state.workflows.len(), report.args.addr);
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
    #[cfg(feature = "rag")]
    if state.knowledge().is_some() {
        eprintln!("  knowledge base API under /v1/knowledge (uploads up to {} bytes)", report.args.max_upload_bytes);
    }
    if !report.has_api_key {
        print_keyless_warnings(report);
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

/// Serve `app` until Ctrl-C, then finish the requests in flight.
///
/// @throws CliError - On a server error.
async fn serve_until_ctrl_c(listener: TcpListener, app: Router) -> Result<(), CliError> {
    // Without TCP_NODELAY, the second and later small writes of a streamed (SSE)
    // response wait for the client's delayed ACK: about 40 ms per run on Linux.
    let listener = axum::serve::ListenerExt::tap_io(listener, |tcp: &mut tokio::net::TcpStream| {
        if let Err(e) = tcp.set_nodelay(true) {
            eprintln!("warning: cannot set TCP_NODELAY: {e}");
        }
    });
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
            eprintln!("shutting down");
        })
        .await
        .map_err(|e| CliError::failed(format!("server error: {e}")))
}

/// Delete runs older than `days` (any status except running), hourly.
fn spawn_retention(store: Arc<dyn RunStore>, days: u64) {
    tokio::spawn(async move {
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
