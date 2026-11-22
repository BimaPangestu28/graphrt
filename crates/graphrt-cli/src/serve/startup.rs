//! `graphrt serve` itself: build the state from the flags, bind, print what is
//! served and run until Ctrl-C.

use std::sync::Arc;

use graphrt_core::store::{RunStore, now_ms};

use super::build::{build_state, discover};
use super::local_host::require_local_host;
use super::router::router_with_uploads;
use crate::args::ServeArgs;
use crate::error::{CliError, EXIT_OK};
use crate::run::service_options;
use crate::setup;

/// Execute `graphrt serve`.
pub async fn serve(args: ServeArgs) -> Result<u8, CliError> {
    let mut options = service_options(&args.services)?;
    let files = discover(&args.path)?;
    let lookup_env = |name: &str| {
        std::env::var(name).ok().or_else(|| options.env_file.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone()))
    };
    let api_key = lookup_env(&args.api_key_env).filter(|k| !k.is_empty());
    #[cfg(feature = "jwt")]
    crate::user_auth::require_api_key(args.user_auth.verify_jwt.is_some(), api_key.is_some(), &args.api_key_env)?;
    #[cfg(feature = "jwt")]
    let user_auth = crate::user_auth::UserAuth::from_args(&args.user_auth, &args.input_header, lookup_env).await?;
    let limits = setup::limits(args.services.timeout_ms, args.services.max_steps);
    let telemetry = crate::otel::Telemetry::from_args(&args.services, &options.all_vars())?;
    options.capture_content = crate::otel::captures_content(telemetry.as_ref());
    let stores = crate::store::open_stores(args.store.as_ref(), args.max_runs)?;
    let webhooks = match &args.webhooks {
        Some(path) => crate::webhooks::Webhooks::load(path, &options.all_vars(), options.allow_private_network)?,
        None => crate::webhooks::Webhooks::default(),
    };
    #[cfg(feature = "jwt")]
    let user_auth_line = user_auth.as_ref().map(crate::user_auth::UserAuth::describe);
    #[cfg(feature = "online-eval")]
    let judge_limits = limits.clone();
    let state = build_state(&files, &options, limits, api_key.clone())
        .await?
        .with_stores(stores)
        .with_telemetry(telemetry.clone())
        .with_store_inputs(args.store_inputs)
        .with_max_active_runs(args.max_active_runs)
        .with_webhooks(webhooks)?
        .with_header_inputs(&args.input_header)?;
    #[cfg(feature = "jwt")]
    let state = state.with_user_auth(user_auth);
    #[cfg(feature = "langfuse")]
    let state = if args.langfuse_scores {
        let uploader = crate::langfuse::LangfuseUploader::start(&options.all_vars(), options.allow_private_network)?;
        eprintln!("  scores uploaded to {}", uploader.url());
        if telemetry.is_none() {
            eprintln!(
                "warning: --langfuse-scores without OpenTelemetry: runs have no trace id, so no score is uploaded \
                 (add --otel-preset langfuse)"
            );
        }
        state.with_langfuse(Some(uploader))
    } else {
        state
    };
    #[cfg(feature = "online-eval")]
    let state = match &args.evaluators {
        Some(path) => state.with_evaluators(path, &options, judge_limits)?,
        None => state,
    };
    let state = Arc::new(state);
    let interrupted = state.recover().await?;
    if interrupted > 0 {
        eprintln!("marked {interrupted} run(s) interrupted by the previous shutdown as failed");
    }
    if let Some(days) = args.store_ttl_days.filter(|d| *d > 0) {
        spawn_retention(Arc::clone(state.store()), days);
    }
    let app = router_with_uploads(state.clone(), args.max_body_bytes, args.max_upload_bytes);

    let listener = tokio::net::TcpListener::bind(&args.addr)
        .await
        .map_err(|e| CliError::usage(format!("cannot listen on {}: {e}", args.addr)))?;
    let loopback = listener.local_addr().map(|a| a.ip().is_loopback()).unwrap_or(false);
    // Without an API key, a loopback server is reachable from any web page through DNS
    // rebinding (the page's own host name resolving to 127.0.0.1). Such requests carry
    // that foreign host name, so only local `Host` values are accepted.
    let app =
        if api_key.is_none() && loopback { app.layer(axum::middleware::from_fn(require_local_host)) } else { app };
    eprintln!("graphrt serving {} workflow(s) on http://{}", state.workflows.len(), args.addr);
    #[cfg(feature = "jwt")]
    if let Some(line) = &user_auth_line {
        eprintln!("  {line}");
    }
    for name in state.workflows.keys() {
        eprintln!("  POST /v1/workflows/{name}/runs");
    }
    for hook in state.webhooks().hooks() {
        eprintln!("  POST /v1/hooks/{} -> {}", hook.id, hook.workflow);
        if !hook.is_signed() && api_key.is_none() {
            eprintln!("warning: webhook `{}` has no `signature` and no API key is set; anyone can trigger it", hook.id);
        }
    }
    #[cfg(feature = "rag")]
    if state.knowledge().is_some() {
        eprintln!("  knowledge base API under /v1/knowledge (uploads up to {} bytes)", args.max_upload_bytes);
    }
    if api_key.is_none() {
        eprintln!("warning: no API key set (${}); the API is unauthenticated", args.api_key_env);
        if !loopback {
            eprintln!(
                "warning: listening on a non-loopback address without an API key: anyone who can reach {} \
                 can start runs with this server's credentials. Set ${} or bind 127.0.0.1",
                args.addr, args.api_key_env
            );
        }
    }
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
        .map_err(|e| CliError::failed(format!("server error: {e}")))?;
    if let Some(telemetry) = &telemetry {
        telemetry.shutdown().await;
    }
    Ok(EXIT_OK)
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
