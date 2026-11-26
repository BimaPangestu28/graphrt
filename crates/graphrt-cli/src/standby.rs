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
use serde::Deserialize;

use crate::args::{AssignedKeys, Cli, Command};
use crate::error::CliError;

/// Largest assignment line read from stdin.
const MAX_ASSIGNMENT_BYTES: u64 = 1024 * 1024;

/// What the gateway sends a standby (one JSON line).
#[derive(Deserialize)]
struct Assignment {
    /// The arguments after the program name, starting with `serve`.
    argv: Vec<String>,
    /// The deployment's gateway keys (`kid:hex,...`).
    keys: String,
}

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
    let assignment: Assignment =
        serde_json::from_str(line).map_err(|e| CliError::usage(format!("agent-standby: bad assignment: {e}")))?;
    let argv = std::iter::once("graphrt".to_owned()).chain(assignment.argv);
    let cli = Cli::try_parse_from(argv).map_err(|e| CliError::usage(format!("agent-standby: {e}")))?;
    let Command::Serve(mut args) = cli.command else {
        return Err(CliError::usage("agent-standby: an assignment can only run `serve`"));
    };
    if !args.gateway.trust_gateway {
        return Err(CliError::usage("agent-standby: an assignment needs `serve --trust-gateway`"));
    }
    args.gateway.assigned_keys = Some(AssignedKeys::new(assignment.keys));
    Ok(args)
}

/// Waits for the assignment, then serves it.
///
/// @throws CliError - A bad assignment, or `serve`'s own error.
pub async fn standby() -> Result<u8, CliError> {
    #[cfg(feature = "llm")]
    graphrt_llm::prewarm_http_client();
    let line =
        tokio::task::spawn_blocking(read_line).await.map_err(|e| CliError::failed(format!("agent-standby: {e}")))??;
    let Some(line) = line else { return Ok(crate::error::EXIT_OK) };
    crate::serve::serve(*serve_args(&line)?).await
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
