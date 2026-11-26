//! `POST /internal/v1/stop`: the gateway's stand-in for `SIGTERM` (how the
//! local-process provider stops agents on Windows). The real binary drains and
//! exits 0; on Windows also over the named pipe the provider makes it listen on.
#![cfg(all(feature = "trust-gateway", feature = "serve"))]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]

use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use graphrt_cli::gateway_agent::signature::{self, GatewayKey, Signed};

const BIN: &str = env!("CARGO_BIN_EXE_graphrt");
const HELLO_WORKFLOW: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../examples/hello.yaml");
const SECRET: [u8; 32] = [0x31; 32];

fn keys() -> String {
    format!("k1:{}", "31".repeat(32))
}

/// The signed stop request, as the provider sends it (`host`: a loopback name
/// over TCP, as a keyless loopback server requires).
fn stop_request(host: &str) -> String {
    let signed =
        Signed { deployment: "shop", method: "POST", path: "/internal/v1/stop", run_id: "", epoch: "", body: b"" };
    let header = signature::sign(&GatewayKey::new("k1", &SECRET).unwrap(), signature::unix_now(), &signed);
    format!(
        "POST /internal/v1/stop HTTP/1.1\r\nhost: {host}\r\ngraphrt-protocol: 1\r\ngraphrt-sig: {header}\r\n\
         graphrt-deployment: shop\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
    )
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("graphrt-gw-stop-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn spawn_agent(listen: &str, store: &std::path::Path, log: &std::path::Path) -> Child {
    Command::new(BIN)
        .args(["serve", HELLO_WORKFLOW, "--listen", listen, "--trust-gateway", "--deployment", "shop"])
        .args(["--drain-timeout", "1s", "--no-default-plugins", "--store", store.to_str().unwrap()])
        .env("GRAPHRT_GATEWAY_KEYS", keys())
        .env_remove("GRAPHRT_API_KEY")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(log).unwrap())
        .spawn()
        .expect("spawn graphrt serve")
}

/// Sends `request` over a fresh connection made by `connect` (retried until the
/// agent listens) and returns the response.
fn exchange<S: Read + Write>(mut connect: impl FnMut() -> std::io::Result<S>, request: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut stream = loop {
        match connect() {
            Ok(stream) => break stream,
            Err(error) => {
                assert!(Instant::now() < deadline, "the agent did not listen: {error}");
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    };
    stream.write_all(request.as_bytes()).unwrap();
    let mut response = String::new();
    let mut buffer = [0u8; 4096];
    // Read the head and the short body; the agent may keep a pipe open.
    while !response.contains("\r\n\r\n") || !response.trim_end().ends_with('}') {
        match stream.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(read) => response.push_str(&String::from_utf8_lossy(&buffer[..read])),
        }
    }
    response
}

fn wait_exit(child: &mut Child, limit: Duration, log: &std::path::Path) -> std::process::ExitStatus {
    let began = Instant::now();
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        if began.elapsed() > limit {
            let _ = child.kill();
            panic!("the agent did not stop within {limit:?}\n{}", std::fs::read_to_string(log).unwrap_or_default());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn assert_stops(mut agent: Child, response: &str, log: &std::path::Path) {
    assert!(response.starts_with("HTTP/1.1 202"), "{response}");
    let status = wait_exit(&mut agent, Duration::from_secs(10), log);
    assert!(status.success(), "exit {status:?}");
    let log = std::fs::read_to_string(log).unwrap_or_default();
    assert!(log.contains("received gateway stop: draining"), "{log}");
}

#[test]
fn a_signed_stop_request_drains_and_exits_like_sigterm() {
    let dir = temp_dir("tcp");
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let log = dir.join("agent.log");
    let agent = spawn_agent(&format!("127.0.0.1:{port}"), &dir.join("store"), &log);
    let response = exchange(|| std::net::TcpStream::connect(("127.0.0.1", port)), &stop_request("127.0.0.1"));
    assert_stops(agent, &response, &log);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_unsigned_stop_request_is_refused() {
    let dir = temp_dir("unsigned");
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let log = dir.join("agent.log");
    let mut agent = spawn_agent(&format!("127.0.0.1:{port}"), &dir.join("store"), &log);
    let request = "POST /internal/v1/stop HTTP/1.1\r\nhost: 127.0.0.1\r\ngraphrt-protocol: 1\r\n\
                   graphrt-deployment: shop\r\ncontent-length: 0\r\nconnection: close\r\n\r\n";
    let response = exchange(|| std::net::TcpStream::connect(("127.0.0.1", port)), request);
    assert!(response.starts_with("HTTP/1.1 401"), "{response}");
    std::thread::sleep(Duration::from_millis(300));
    assert!(agent.try_wait().unwrap().is_none(), "an unsigned request must not stop the agent");
    let _ = agent.kill();
    let _ = agent.wait();
    let _ = std::fs::remove_dir_all(&dir);
}

/// The local-process provider's transport on Windows.
#[cfg(windows)]
#[test]
fn a_stop_request_over_the_agents_named_pipe_drains_and_exits() {
    let dir = temp_dir("pipe");
    let name = format!(r"\\.\pipe\graphrt-test-stop-{}", std::process::id());
    let log = dir.join("agent.log");
    let agent = spawn_agent(&format!("pipe:{name}"), &dir.join("store"), &log);
    let open = || std::fs::OpenOptions::new().read(true).write(true).open(&name);
    let response = exchange(open, &stop_request("agent"));
    assert_stops(agent, &response, &log);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A second process cannot take over a pipe name an agent is listening on.
#[cfg(windows)]
#[test]
fn a_pipe_name_in_use_is_refused() {
    let dir = temp_dir("pipe-taken");
    let name = format!(r"\\.\pipe\graphrt-test-taken-{}", std::process::id());
    let log = dir.join("first.log");
    let mut first = spawn_agent(&format!("pipe:{name}"), &dir.join("store-1"), &log);
    let open = || std::fs::OpenOptions::new().read(true).write(true).open(&name);
    let _ = exchange(open, "GET /healthz HTTP/1.1\r\nhost: agent\r\nconnection: close\r\n\r\n");
    let second_log = dir.join("second.log");
    let mut second = spawn_agent(&format!("pipe:{name}"), &dir.join("store-2"), &second_log);
    let status = wait_exit(&mut second, Duration::from_secs(10), &second_log);
    assert!(!status.success(), "the second agent must not share the pipe");
    let _ = first.kill();
    let _ = first.wait();
    let _ = std::fs::remove_dir_all(&dir);
}
