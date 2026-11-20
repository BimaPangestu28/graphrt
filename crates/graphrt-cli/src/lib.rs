//! Library side of the `graphrt` binary: command implementations shared by the
//! binary and its tests.

pub mod args;
pub mod bundle_cmd;
pub mod catalog;
pub mod diagnostics;
pub mod error;
pub mod extensions;
pub mod files;
#[cfg(feature = "serve")]
pub mod header_inputs;
#[cfg(any(feature = "serve", feature = "telemetry"))]
pub mod hex;
pub mod inspect;
#[cfg(feature = "rag")]
pub mod kb;
#[cfg(all(feature = "serve", feature = "rag"))]
pub mod knowledge_api;
pub mod otel;
#[cfg(any(feature = "plugins", feature = "wasm"))]
pub mod plugin;
#[cfg(any(feature = "plugins", feature = "wasm"))]
pub mod plugin_catalog;
pub mod run;
#[cfg(feature = "serve")]
pub mod serve;
pub mod setup;
#[cfg(feature = "serve")]
pub mod store;
#[cfg(feature = "telemetry")]
pub mod telemetry;
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
        Command::Run(args) => {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| CliError::failed(format!("cannot start runtime: {e}")))?;
            runtime.block_on(run::run(*args))
        }
        #[cfg(feature = "rag")]
        Command::Kb(args) => {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| CliError::failed(format!("cannot start runtime: {e}")))?;
            runtime.block_on(kb::kb(*args))
        }
        #[cfg(feature = "serve")]
        Command::Serve(args) => {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .map_err(|e| CliError::failed(format!("cannot start runtime: {e}")))?;
            runtime.block_on(serve::serve(*args))
        }
    }
}
