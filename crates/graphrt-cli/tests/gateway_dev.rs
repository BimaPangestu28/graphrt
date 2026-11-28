//! `graphrt gateway --dev` end to end: the gateway starts a local agent of this
//! same binary (`graphrt serve --trust-gateway`) on the first request and the
//! run succeeds.
//!
//! The `gateway` feature must bring `trust-gateway` with it: a build with
//! `--no-default-features --features gateway` used to answer every run with
//! `agent exited during start-up (exit status: 2)`. CI runs this file with that
//! feature set (`gateway without default features` job) as well as with
//! `--all-features`.

#![cfg(feature = "gateway")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use std::io::{Read as _, Write as _};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

// Fails the build, not just the test, if `gateway` stops enabling the agent side.
const _: () =
    assert!(cfg!(feature = "trust-gateway"), "the gateway feature must enable trust-gateway for its local agents");

const BIN: &str = env!("CARGO_BIN_EXE_graphrt");
const HELLO_WORKFLOW: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../examples/hello.yaml");

/// A `gateway --dev` process in its own directory; kills it and its agents on drop.
struct DevGateway {
    dir: PathBuf,
    port: u16,
    child: Child,
    /// When the gateway was spawned, for the diagnostics of a failed request.
    started: Instant,
}

impl Drop for DevGateway {
    fn drop(&mut self) {
        let _killed = self.child.kill();
        let _reaped = self.child.wait();
        // Agents run in their own process groups and leave `.pid` files in the socket directory
        // (or in the short directory the gateway logged when that one was too long).
        let log = self.log();
        let short_dir =
            log.lines().find_map(|line| line.split_once("agents listen in ").map(|(_, dir)| PathBuf::from(dir)));
        let socket_dirs = [Some(self.dir.join("graphrt-gateway")), short_dir.clone()];
        for path in socket_dirs
            .iter()
            .flatten()
            .filter_map(|dir| std::fs::read_dir(dir).ok())
            .flatten()
            .flatten()
            .map(|entry| entry.path())
        {
            if path.extension().is_some_and(|extension| extension == "pid")
                && let Some(pid) =
                    std::fs::read_to_string(&path).ok().and_then(|text| text.lines().next().map(str::to_owned))
            {
                if cfg!(windows) {
                    let _killed = Command::new("taskkill")
                        .args(["/F", "/T", "/PID", &pid])
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .status();
                } else {
                    let _killed =
                        Command::new("kill").arg("-KILL").arg(format!("-{pid}")).stderr(Stdio::null()).status();
                    let _killed = Command::new("kill").arg("-KILL").arg(pid).stderr(Stdio::null()).status();
                }
            }
        }
        let _cleanup = std::fs::remove_dir_all(&self.dir);
        if let Some(short_dir) = short_dir {
            let _cleanup = std::fs::remove_dir_all(short_dir);
        }
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

/// Send one HTTP/1.1 request and return the status code and body.
fn http(port: u16, method: &str, path: &str, body: &str) -> (u16, String) {
    try_http(port, method, path, body).unwrap_or_else(|(stage, error)| panic!("{method} {path}: {stage}: {error}"))
}

/// [`http`], with the stage that failed (connect, write, read) instead of a panic.
fn try_http(port: u16, method: &str, path: &str, body: &str) -> Result<(u16, String), (&'static str, std::io::Error)> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).map_err(|error| ("connect", error))?;
    stream.set_read_timeout(Some(Duration::from_secs(30))).map_err(|error| ("connect", error))?;
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .map_err(|error| ("write", error))?;
    let mut response = String::new();
    stream.read_to_string(&mut response).map_err(|error| ("read", error))?;
    let status = response.split(' ').nth(1).and_then(|code| code.parse().ok()).unwrap_or(0);
    let body = response.split_once("\r\n\r\n").map(|(_, rest)| rest.to_owned()).unwrap_or_default();
    Ok((status, body))
}

impl DevGateway {
    /// Starts `gateway --dev` with `$TMPDIR` (and so its socket directory,
    /// `$TMPDIR/graphrt-gateway`) set to `dir`.
    fn start(dir: PathBuf) -> Self {
        Self::start_with(dir, std::path::Path::new(HELLO_WORKFLOW))
    }

    /// Like [`Self::start`], serving `workflow`.
    fn start_with(dir: PathBuf, workflow: &std::path::Path) -> Self {
        let _cleanup = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let port = free_port();
        let workflow = workflow.to_str().unwrap();
        let child = Command::new(BIN)
            .args(["gateway", "--dev", workflow, "--listen", &format!("127.0.0.1:{port}")])
            .current_dir(&dir)
            // The default socket directory is `$TMPDIR/graphrt-gateway` (`%TMP%` on
            // Windows): keep it inside the test directory.
            .env("TMPDIR", &dir)
            .env("TMP", &dir)
            .env("TEMP", &dir)
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(dir.join("gateway.log")).unwrap())
            .spawn()
            .unwrap();
        let gateway = Self { dir, port, child, started: Instant::now() };
        let deadline = Instant::now() + Duration::from_secs(15);
        while TcpStream::connect(("127.0.0.1", port)).is_err() {
            assert!(Instant::now() < deadline, "graphrt gateway --dev did not listen within 15 s");
            std::thread::sleep(Duration::from_millis(10));
        }
        gateway
    }

    fn log(&self) -> String {
        std::fs::read_to_string(self.dir.join("gateway.log")).unwrap_or_default()
    }

    fn run_hello(&self) {
        let path = "/v1/deployments/dev/runs";
        let (status, body) =
            try_http(self.port, "POST", path, r#"{"inputs":{"name":"Ada"}}"#).unwrap_or_else(|(stage, error)| {
                // Diagnostics of a reset seen once in CI: was it this gateway, and what had it logged?
                panic!(
                    "POST {path} failed at {stage} after {} ms since spawn (gateway pid {}, port {}): {error}\n\
                     --- gateway log ---\n{}",
                    self.started.elapsed().as_millis(),
                    self.child.id(),
                    self.port,
                    self.log()
                )
            });
        assert_eq!(status, 200, "run through the dev gateway failed: {body}\n{}", self.log());
        let record: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(record["status"], "succeeded", "{record}");
        assert_eq!(record["outputs"]["greeting"], "Hello, Ada.", "{record}");
    }
}

#[test]
fn dev_gateway_wakes_a_local_agent_of_this_binary_and_runs_the_workflow() {
    let base = if std::path::Path::new("/tmp").is_dir() { PathBuf::from("/tmp") } else { std::env::temp_dir() };
    let gateway = DevGateway::start(base.join(format!("grt-gw-dev-{}", std::process::id())));
    gateway.run_hello();
}

/// The run record carries the answer text the agent streamed (`response`
/// chunks), as a record of `graphrt serve` does: clients of a gateway read
/// `answer` the same way. It used to be empty behind a gateway.
#[test]
fn dev_gateway_records_the_answer_of_a_run() {
    let base = if std::path::Path::new("/tmp").is_dir() { PathBuf::from("/tmp") } else { std::env::temp_dir() };
    let workflow = base.join(format!("grt-gw-answer-{}.yaml", std::process::id()));
    std::fs::write(
        &workflow,
        "version: \"1\"\nname: greeter\ngraph:\n  nodes:\n    - { id: start, type: start }\n    - { id: answer, type: answer, answer: \"Hello from the agent.\" }\n  edges:\n    - { source: start, target: answer }\n",
    )
    .unwrap();
    let gateway = DevGateway::start_with(base.join(format!("grt-gw-answer-{}", std::process::id())), &workflow);
    let (status, body) = http(gateway.port, "POST", "/v1/deployments/dev/runs", r#"{"sys":{"query":"hi"}}"#);
    assert_eq!(status, 200, "{body}\n{}", gateway.log());
    let record: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(record["status"], "succeeded", "{record}");
    assert_eq!(record["answer"], "Hello from the agent.", "{record}");
    let (_, stored) =
        http(gateway.port, "GET", &format!("/v1/deployments/dev/runs/{}", record["run_id"].as_str().unwrap()), "");
    let stored: serde_json::Value = serde_json::from_str(&stored).unwrap();
    assert_eq!(stored["answer"], "Hello from the agent.", "the stored record keeps it: {stored}");
    drop(gateway);
    let _cleanup = std::fs::remove_file(workflow);
}

/// The page context of a web chat (`sys.context`) that a client sends through
/// the gateway is checked by the agent. A build without the cargo feature
/// `web-chat` cannot check it, so it refuses every context: one never reaches
/// a run unchecked. CI runs this file in both builds.
#[test]
fn dev_gateway_never_runs_an_unchecked_page_context() {
    let base = if std::path::Path::new("/tmp").is_dir() { PathBuf::from("/tmp") } else { std::env::temp_dir() };
    let gateway = DevGateway::start(base.join(format!("grt-gw-context-{}", std::process::id())));
    gateway.run_hello();
    let run = |body: &str| -> serde_json::Value {
        let (status, text) = http(gateway.port, "POST", "/v1/deployments/dev/runs", body);
        serde_json::from_str(&text).unwrap_or_else(|_| panic!("{status}: {text}\n{}", gateway.log()))
    };
    // The agent's `400` reaches the client as a failed run that never started.
    let refused = |record: &serde_json::Value, reason: &str| {
        assert_eq!(record["status"], "failed", "{record}");
        assert_eq!(record["event_count"], 0, "no node ran: {record}");
        let error = record["error"].as_str().unwrap_or_default();
        assert!(error.contains("agent refused (400)") && error.contains(reason), "{record}");
    };
    let invalid = run(r#"{"inputs":{"name":"Ada"},"sys":{"context":"not an object"}}"#);
    let valid = run(r#"{"inputs":{"name":"Ada"},"sys":{"context":{"product_id":"sku-1"}}}"#);
    if cfg!(feature = "web-chat") {
        refused(&invalid, "must be a JSON object");
        assert_eq!(valid["status"], "succeeded", "{valid}");
    } else {
        refused(&invalid, "needs a build with the cargo feature `web-chat`");
        refused(&valid, "needs a build with the cargo feature `web-chat`");
    }
}

/// Socket paths must fit `sun_path` (104 bytes on macOS, 108 on Linux). With a
/// socket directory longer than that the agents used to exit with status 2;
/// the gateway now moves them to a short private directory. (Windows agents
/// listen on named pipes, which have no such limit.)
#[cfg(unix)]
#[test]
fn dev_gateway_runs_when_its_socket_dir_is_too_long_for_unix_sockets() {
    let base = std::env::temp_dir().join(format!("grt-gw-long-{}", std::process::id()));
    let gateway = DevGateway::start(base.join("x".repeat(110)));
    gateway.run_hello();
    assert!(
        gateway.log().contains("is too long for Unix sockets; agents listen in /tmp/graphrt-gw-"),
        "{}",
        gateway.log()
    );
    drop(gateway);
    let _cleanup = std::fs::remove_dir_all(base);
}
