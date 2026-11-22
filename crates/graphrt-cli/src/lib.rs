//! Library side of the `graphrt` binary: command implementations shared by the
//! binary and its tests.

pub mod args;
#[cfg(feature = "serve")]
pub mod body_drain;
pub mod bundle_cmd;
pub mod catalog;
pub mod diagnostics;
#[cfg(feature = "serve")]
pub mod drain;
pub mod error;
#[cfg(feature = "eval")]
pub mod eval;
#[cfg(all(feature = "eval", feature = "otel"))]
pub mod eval_trace;
pub mod extensions;
pub mod files;
#[cfg(feature = "trust-gateway")]
pub mod gateway_agent;
#[cfg(feature = "serve")]
pub mod header_inputs;
#[cfg(any(feature = "serve", feature = "telemetry"))]
pub mod hex;
pub mod inspect;
#[cfg(feature = "rag")]
pub mod kb;
#[cfg(all(feature = "serve", feature = "rag"))]
pub mod knowledge_api;
#[cfg(feature = "langfuse")]
pub mod langfuse;
#[cfg(feature = "serve")]
pub mod listen;
#[cfg(feature = "online-eval")]
pub mod online_eval;
pub mod otel;
#[cfg(any(feature = "plugins", feature = "wasm"))]
pub mod plugin;
#[cfg(any(feature = "plugins", feature = "wasm"))]
pub mod plugin_catalog;
pub mod run;
#[cfg(feature = "scores")]
pub mod scores;
#[cfg(feature = "serve")]
pub mod serve;
pub mod setup;
#[cfg(feature = "serve")]
pub mod sse_ids;
#[cfg(feature = "serve")]
pub mod store;
#[cfg(feature = "telemetry")]
pub mod telemetry;
#[cfg(feature = "jwt")]
pub mod user_auth;
#[cfg(feature = "wasm")]
pub mod wasm_loader;
#[cfg(feature = "serve")]
pub mod webhooks;

use args::{Cli, Command};
use error::{CliError, EXIT_OK};

/// Features compiled into this build.
pub fn enabled_features() -> Vec<&'static str> {
    let mut features = vec!["core"];
    if cfg!(feature = "llm") {
        features.push("llm");
    }
    if cfg!(feature = "http") {
        features.push("http");
    }
    if cfg!(feature = "code") {
        features.push("code");
    }
    if cfg!(feature = "python") {
        features.push("python");
    }
    if cfg!(feature = "mcp") {
        features.push("mcp");
    }
    if cfg!(feature = "doc") {
        features.push("doc");
    }
    if cfg!(feature = "rag") {
        features.push("rag");
    }
    if cfg!(feature = "eval") {
        features.push("eval");
    }
    if cfg!(feature = "bundle") {
        features.push("bundle");
    }
    if cfg!(feature = "plugins") {
        features.push("plugins");
    }
    if cfg!(feature = "wasm") {
        features.push("wasm");
    }
    if cfg!(feature = "otel") {
        features.push("otel");
    }
    if cfg!(feature = "serve") {
        features.push("serve");
    }
    if cfg!(feature = "trust-gateway") {
        features.push("trust-gateway");
    }
    if cfg!(feature = "scores") {
        features.push("scores");
    }
    if cfg!(feature = "online-eval") {
        features.push("online-eval");
    }
    if cfg!(feature = "langfuse") {
        features.push("langfuse");
    }
    if cfg!(feature = "telemetry") {
        features.push("telemetry");
    }
    features
}

/// Execute a parsed command and return the process exit code.
pub fn execute(cli: Cli) -> Result<u8, CliError> {
    match cli.command {
        Command::Validate(arg) => inspect::validate(&arg.workflow, arg.format, &arg.plugins.options()),
        Command::Schema(args) => catalog::schema(&args),
        Command::Inspect(arg) => inspect::inspect(&arg.workflow, &arg.plugins.options()),
        #[cfg(any(feature = "plugins", feature = "wasm"))]
        Command::Plugin(args) => plugin::plugin(args),
        Command::Nodes(args) => catalog::nodes(&args),
        #[cfg(feature = "telemetry")]
        Command::Telemetry(args) => telemetry::telemetry(&args),
        #[cfg(feature = "bundle")]
        Command::Bundle(args) => bundle_cmd::bundle(args),
        Command::Version => {
            use std::io::Write;
            let mut out = std::io::stdout().lock();
            writeln!(out, "graphrt {} ({})", env!("CARGO_PKG_VERSION"), enabled_features().join(", "))
                .map_err(|e| CliError::failed(e.to_string()))?;
            Ok(EXIT_OK)
        }
        Command::Run(args) => block_on_current_thread(run::run(*args)),
        #[cfg(feature = "eval")]
        Command::Eval(args) => block_on_current_thread(eval::eval(*args)),
        #[cfg(feature = "rag")]
        Command::Kb(args) => block_on_current_thread(kb::kb(*args)),
        #[cfg(feature = "serve")]
        Command::Serve(args) => {
            let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().map_err(runtime_error)?;
            let result = runtime.block_on(serve::serve(*args));
            // A node stuck in blocking code must not hold the exit past the drain deadline.
            runtime.shutdown_timeout(std::time::Duration::from_millis(500));
            result
        }
    }
}

/// Run a command on a single-threaded runtime: one run or evaluation at a
/// time needs no worker pool (only `serve` gets a multi-threaded one).
///
/// @throws CliError - When the runtime cannot start, or the command's own error.
fn block_on_current_thread(command: impl std::future::Future<Output = Result<u8, CliError>>) -> Result<u8, CliError> {
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(runtime_error)?;
    runtime.block_on(command)
}

fn runtime_error(e: std::io::Error) -> CliError {
    CliError::failed(format!("cannot start runtime: {e}"))
}
