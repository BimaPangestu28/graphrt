//! Opt-in usage telemetry, end to end through the `graphrt` binary.
#![cfg(feature = "telemetry")]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use serde_json::Value;

/// Variables that would turn telemetry off (CI runners set some of them).
const BLOCKING_VARS: &[&str] = &[
    "DO_NOT_TRACK",
    "CI",
    "CONTINUOUS_INTEGRATION",
    "BUILD_NUMBER",
    "GITHUB_ACTIONS",
    "GITLAB_CI",
    "CIRCLECI",
    "TRAVIS",
    "BUILDKITE",
    "JENKINS_URL",
    "TEAMCITY_VERSION",
    "TF_BUILD",
    "BITBUCKET_BUILD_NUMBER",
    "CODEBUILD_BUILD_ID",
    "DRONE",
    "APPVEYOR",
    "SEMAPHORE",
    "BUILDKITE_BUILD_ID",
    "GRAPHRT_TELEMETRY",
];

/// A temp config directory, removed on drop.
struct ConfigDir(PathBuf);

impl ConfigDir {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("graphrt-telemetry-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Self(dir)
    }

    fn state_file(&self) -> PathBuf {
        self.0.join("telemetry.json")
    }

    fn state(&self) -> Value {
        serde_json::from_slice(&std::fs::read(self.state_file()).expect("state file")).unwrap()
    }
}

impl Drop for ConfigDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// `graphrt` with a clean telemetry environment, this config dir and `endpoint`.
fn graphrt(config: &ConfigDir, endpoint: Option<&str>) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_graphrt"));
    for var in BLOCKING_VARS {
        command.env_remove(var);
    }
    command.env("GRAPHRT_CONFIG_DIR", &config.0);
    match endpoint {
        Some(url) => command.env("GRAPHRT_TELEMETRY_URL", url),
        // An empty override counts as "no endpoint" (the build may embed one).
        None => command.env("GRAPHRT_TELEMETRY_URL", ""),
    };
    command
}

fn run(command: &mut Command) -> Output {
    let out = command.output().expect("spawn graphrt");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    out
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn status_json(config: &ConfigDir, endpoint: Option<&str>, extra: &[(&str, &str)]) -> Value {
    let mut command = graphrt(config, endpoint);
    command.args(["telemetry", "status", "--json"]).envs(extra.iter().copied());
    serde_json::from_str(&stdout(&run(&mut command))).unwrap()
}

fn hello() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/hello.yaml")
}

/// An address on which nothing listens (reports to it fail fast).
fn closed_endpoint() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    format!("http://127.0.0.1:{port}/v1/report")
}

const ENDPOINT: &str = "https://telemetry.example.invalid/v1/report";

#[test]
fn off_by_default_and_status_shows_the_payload() {
    let config = ConfigDir::new("default");
    let status = status_json(&config, Some(ENDPOINT), &[]);
    assert_eq!(status["enabled"], false);
    assert!(status["reason"].as_str().unwrap().contains("not enabled"), "{status}");
    let report = &status["next_report"];
    let mut keys: Vec<&str> = report.as_object().unwrap().keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(keys, ["arch", "build", "days", "features", "install_id", "os", "schema", "version"]);
    assert_eq!(report["build"], "cli");
    assert_eq!(report["version"], env!("CARGO_PKG_VERSION"));
    assert!(report["features"].as_array().unwrap().iter().any(|f| f == "telemetry"));
    assert!(!config.state_file().exists(), "status writes nothing");

    let text = stdout(&run(graphrt(&config, Some(ENDPOINT)).args(["telemetry", "status"])));
    assert!(text.starts_with("telemetry: off (not enabled"), "{text}");
    assert!(text.contains("\"install_id\""), "the payload is printed: {text}");
}

#[test]
fn enable_disable_round_trip() {
    let config = ConfigDir::new("toggle");
    let text = stdout(&run(graphrt(&config, Some(ENDPOINT)).args(["telemetry", "enable"])));
    assert!(text.starts_with("telemetry enabled"), "{text}");
    let state = config.state();
    assert_eq!(state["enabled"], true);
    let install_id = state["install_id"].as_str().unwrap().to_owned();
    assert_eq!(install_id.len(), 32);

    let status = status_json(&config, Some(ENDPOINT), &[]);
    assert_eq!(status["enabled"], true, "{status}");
    assert_eq!(status["next_report"]["install_id"], install_id.as_str());

    // DO_NOT_TRACK, CI and GRAPHRT_TELEMETRY=0 override the opt-in.
    for (var, value) in [("DO_NOT_TRACK", "1"), ("CI", "true"), ("GITHUB_ACTIONS", "true"), ("GRAPHRT_TELEMETRY", "0")]
    {
        let status = status_json(&config, Some(ENDPOINT), &[(var, value)]);
        assert_eq!(status["enabled"], false, "{var}={value}: {status}");
    }

    run(graphrt(&config, Some(ENDPOINT)).args(["telemetry", "disable"]));
    assert_eq!(config.state(), serde_json::json!({ "enabled": false }), "id and counts are deleted");
    assert_eq!(status_json(&config, Some(ENDPOINT), &[])["enabled"], false);
}

#[test]
fn env_opt_in_without_endpoint_is_inert() {
    let config = ConfigDir::new("inert");
    let status = status_json(&config, None, &[("GRAPHRT_TELEMETRY", "1")]);
    assert_eq!(status["enabled"], false);
    assert!(status["reason"].as_str().unwrap().contains("no endpoint"), "{status}");
    run(graphrt(&config, None).env("GRAPHRT_TELEMETRY", "1").args(["run"]).arg(hello()).args(["-i", "name=Ada"]));
    assert!(!config.0.exists(), "nothing is written without an endpoint");
}

#[test]
fn runs_are_counted_locally_per_day() {
    let config = ConfigDir::new("counts");
    let endpoint = closed_endpoint();
    run(graphrt(&config, Some(&endpoint)).args(["telemetry", "enable"]));
    for _ in 0..2 {
        run(graphrt(&config, Some(&endpoint)).arg("run").arg(hello()).args(["-i", "name=Ada", "-o", "json"]));
    }
    let state = config.state();
    let days = state["days"].as_object().unwrap();
    assert_eq!(days.len(), 1, "{state}");
    let today = days.values().next().unwrap();
    assert_eq!(today["runs"], 2, "{state}");
    assert_eq!(today["serve_requests"], 0);
}

#[test]
fn not_counted_when_disabled() {
    let config = ConfigDir::new("disabled");
    run(graphrt(&config, Some(ENDPOINT)).arg("run").arg(hello()).args(["-i", "name=Ada", "-o", "json"]));
    assert!(!config.state_file().exists());
}

/// Accept one HTTP request and return its request line, headers and body.
fn capture_one(listener: TcpListener) -> mpsc::Receiver<(String, Vec<u8>)> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut head = String::new();
        let mut length = 0usize;
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            if line == "\r\n" || line.is_empty() {
                break;
            }
            if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                length = value.trim().parse().unwrap();
            }
            head.push_str(&line);
        }
        let mut body = vec![0; length];
        reader.read_exact(&mut body).unwrap();
        let mut stream = stream;
        stream.write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
        let _ = tx.send((head, body));
    });
    rx
}

#[test]
fn serve_sends_one_daily_report() {
    let config = ConfigDir::new("serve");
    std::fs::create_dir_all(&config.0).unwrap();
    // Counts of a complete day waiting to be reported.
    let pending = serde_json::json!({ "enabled": true, "days": { "2000-01-01": { "runs": 7, "serve_requests": 3 } } });
    std::fs::write(config.state_file(), pending.to_string()).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}/v1/report", listener.local_addr().unwrap());
    let received = capture_one(listener);

    let mut server = graphrt(&config, Some(&endpoint))
        .arg("serve")
        .arg(hello())
        .args(["--addr", "127.0.0.1:0"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let report = received.recv_timeout(Duration::from_secs(30));
    let _ = server.kill();
    let _ = server.wait();
    let (head, body) = report.expect("a report reaches the collector");

    assert!(head.starts_with("POST /v1/report HTTP/1.1"), "{head}");
    assert!(head.to_ascii_lowercase().contains("content-type: application/json"), "{head}");
    let payload: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(payload["schema"], 1);
    assert_eq!(payload["os"], std::env::consts::OS);
    assert_eq!(payload["arch"], std::env::consts::ARCH);
    assert_eq!(payload["days"], serde_json::json!([{ "date": "2000-01-01", "runs": 7, "serve_requests": 3 }]));
    let install_id = payload["install_id"].as_str().unwrap();
    assert_eq!(install_id.len(), 32);

    // Nothing about the machine, user, paths or workflow is in the payload.
    let text = String::from_utf8_lossy(&body);
    let path = Path::new(&config.0).display().to_string();
    for forbidden in [path.as_str(), "hello", "Ada", "127.0.0.1"] {
        assert!(!text.contains(forbidden), "payload contains `{forbidden}`: {text}");
    }
    if let Ok(user) = std::env::var("USER") {
        assert!(!text.contains(&format!("/{user}")), "{text}");
    }

    // The reported day is gone and today is marked as sent.
    let state = config.state();
    assert!(state.get("days").is_none_or(|days| days.get("2000-01-01").is_none()), "{state}");
    assert!(state["last_sent_day"].is_string(), "{state}");
    assert_eq!(state["install_id"], install_id);
}
