//! `graphrt agent-standby` (internal, hidden): an agent process a gateway
//! starts before any run needs it.
//!
//! A standby has no deployment, no workflow and no gateway keys. It starts the
//! process, the async runtime and the model HTTP client (the system trust
//! store), then waits for one assignment on stdin: one JSON line with the
//! `serve` arguments the gateway would have started an agent with and the
//! deployment's gateway keys. From there it is exactly that agent: the same
//! `serve` code with the same arguments, the keys taken from the assignment
//! instead of `$GRAPHRT_GATEWAY_KEYS`. A standby serves one deployment once;
//! it exits when stdin closes without an assignment (its gateway stopped) and
//! refuses an assignment that is not `serve --trust-gateway`.

use std::io::{BufRead as _, Read as _};

use clap::Parser as _;

use crate::args::{AssignedKeys, Cli, Command};
use crate::error::CliError;

/// Largest assignment line read from stdin.
const MAX_ASSIGNMENT_BYTES: u64 = 1024 * 1024;

/// Reads one line from stdin (blocking); `None` at end of input.
fn read_line() -> Result<Option<String>, CliError> {
    let mut line = String::new();
    let read = std::io::stdin()
        .lock()
        .take(MAX_ASSIGNMENT_BYTES)
        .read_line(&mut line)
        .map_err(|e| CliError::failed(format!("agent-standby: reading the assignment: {e}")))?;
    Ok((read > 0 && !line.trim().is_empty()).then_some(line))
}

/// The `serve` arguments of an assignment line.
///
/// @throws CliError - Usage error for anything but `serve --trust-gateway`.
fn serve_args(line: &str) -> Result<Box<crate::args::ServeArgs>, CliError> {
    // `{"argv": [...], "keys": "..."}`: the `serve` arguments after the program
    // name and the deployment's gateway keys (`kid:hex,...`). Read as a `Value`
    // (already in the binary) rather than a derived type.
    let bad = |what: &str| CliError::usage(format!("agent-standby: bad assignment: {what}"));
    let assignment: serde_json::Value = serde_json::from_str(line).map_err(|e| bad(&e.to_string()))?;
    let keys = assignment.get("keys").and_then(serde_json::Value::as_str).ok_or_else(|| bad("no `keys`"))?;
    let argv = assignment.get("argv").and_then(serde_json::Value::as_array).ok_or_else(|| bad("no `argv`"))?;
    let argv: Vec<String> = argv
        .iter()
        .map(|argument| argument.as_str().map(str::to_owned).ok_or_else(|| bad("`argv` holds a non-string")))
        .collect::<Result<_, _>>()?;
    let argv = std::iter::once("graphrt".to_owned()).chain(argv);
    let cli = Cli::try_parse_from(argv).map_err(|e| CliError::usage(format!("agent-standby: {e}")))?;
    let Command::Serve(mut args) = cli.command else {
        return Err(CliError::usage("agent-standby: an assignment can only run `serve`"));
    };
    if !args.gateway.trust_gateway {
        return Err(CliError::usage("agent-standby: an assignment needs `serve --trust-gateway`"));
    }
    args.gateway.assigned_keys = Some(AssignedKeys::new(keys.to_owned()));
    Ok(args)
}

/// Waits for the assignment: the `serve` arguments to run, or `None` when
/// stdin closed without one (the gateway stopped).
///
/// @throws CliError - A bad assignment.
pub async fn assignment() -> Result<Option<Box<crate::args::ServeArgs>>, CliError> {
    #[cfg(feature = "llm")]
    graphrt_llm::prewarm_http_client();
    let line =
        tokio::task::spawn_blocking(read_line).await.map_err(|e| CliError::failed(format!("agent-standby: {e}")))??;
    line.as_deref().map(serve_args).transpose()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn line(argv: &[&str]) -> String {
        serde_json::json!({ "argv": argv, "keys": "k1:00" }).to_string()
    }

    #[test]
    fn an_assignment_is_serve_with_the_gateway_keys() {
        let args = serve_args(&line(&["serve", "agent.yaml", "--trust-gateway", "--deployment", "shop"])).unwrap();
        assert_eq!(args.deployment.as_deref(), Some("shop"));
        let keys = args.gateway.assigned_keys.as_ref().unwrap();
        assert_eq!(keys.expose(), "k1:00");
        assert!(!format!("{:?}", args.gateway).contains("k1:00"), "keys never in debug output");
    }

    #[test]
    fn anything_else_is_refused() {
        for argv in [&["run", "agent.yaml"][..], &["serve", "agent.yaml"], &["serve", "--no-such-flag"]] {
            assert!(serve_args(&line(argv)).is_err(), "{argv:?}");
        }
        assert!(serve_args("not json").is_err());
    }
}
