//! `pool.idle_mode: freeze` end to end with the real binary: an idle agent is
//! frozen (counted in `/metrics`), the next run thaws it without starting a
//! process, and a model connection the provider closed while the agent was
//! frozen is not reused (the first run after a thaw succeeds).
#![cfg(all(feature = "gateway", unix))]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use std::io::{Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Value as Json, json};

const BIN: &str = env!("CARGO_BIN_EXE_graphrt");
const API_KEY: &str = "grk_freeze_test_key";
const ADMIN_TOKEN: &str = "freeze-admin-token";
const HMAC_KEYS: &str = "k1:00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";

fn sha256_hex(text: &str) -> String {
    use sha2::Digest as _;
    sha2::Sha256::digest(text.as_bytes()).iter().map(|byte| format!("{byte:02x}")).collect()
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

/// An OpenAI-compatible streaming model that closes a keep-alive connection
/// idle for 300 ms, as providers do after their own idle timeout.
#[derive(Default)]
struct Model {
    connections: AtomicUsize,
    requests: AtomicUsize,
}

fn serve_model(model: Arc<Model>) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let model = Arc::clone(&model);
            model.connections.fetch_add(1, Ordering::SeqCst);
            std::thread::spawn(move || serve_connection(stream, &model));
        }
    });
    port
}

fn serve_connection(mut stream: TcpStream, model: &Model) {
    let _timeout = stream.set_read_timeout(Some(Duration::from_millis(300)));
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 8192];
    loop {
        // One request: the head, then `content-length` bytes of body.
        let head_end = loop {
            if let Some(end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
                break end + 4;
            }
            match stream.read(&mut chunk) {
                Ok(0) | Err(_) => return, // closed, or idle past 300 ms: close it
                Ok(read) => buffer.extend_from_slice(&chunk[..read]),
            }
        };
        let head = String::from_utf8_lossy(&buffer[..head_end]).to_ascii_lowercase();
        let length: usize = head
            .lines()
            .find_map(|line| line.strip_prefix("content-length:"))
            .and_then(|value| value.trim().parse().ok())
            .unwrap_or(0);
        while buffer.len() < head_end + length {
            match stream.read(&mut chunk) {
                Ok(0) | Err(_) => return,
                Ok(read) => buffer.extend_from_slice(&chunk[..read]),
            }
        }
        buffer.drain(..head_end + length);
        model.requests.fetch_add(1, Ordering::SeqCst);
        let mut body = String::new();
        for token in ["hello ", "there"] {
            body.push_str(&format!(
                "data: {}\n\n",
                json!({ "choices": [{ "index": 0, "delta": { "content": token } }] })
            ));
        }
        body.push_str(&format!(
            "data: {}\n\ndata: [DONE]\n\n",
            json!({ "choices": [{ "index": 0, "delta": {}, "finish_reason": "stop" }] })
        ));
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\n\r\n{body}",
            body.len()
        );
        if stream.write_all(response.as_bytes()).is_err() {
            return;
        }
    }
}

struct World {
    dir: PathBuf,
    sockets: PathBuf,
    port: u16,
    gateway: Option<Child>,
}

impl Drop for World {
    fn drop(&mut self) {
        if let Some(gateway) = &mut self.gateway {
            let _killed = gateway.kill();
            let _reaped = gateway.wait();
        }
        // Agents run in their own process groups (frozen ones too): kill them.
        for pid in self.agent_pids() {
            let _killed = Command::new("kill").arg("-KILL").arg(format!("-{pid}")).stderr(Stdio::null()).status();
        }
        let _cleanup = std::fs::remove_dir_all(&self.dir);
        let _cleanup = std::fs::remove_dir_all(&self.sockets);
    }
}

impl World {
    fn agent_pids(&self) -> Vec<String> {
        let Ok(entries) = std::fs::read_dir(&self.sockets) else { return Vec::new() };
        entries
            .flatten()
            .filter(|entry| entry.path().extension().is_some_and(|extension| extension == "pid"))
            .filter_map(|entry| std::fs::read_to_string(entry.path()).ok()?.lines().next().map(str::to_owned))
            .collect()
    }

    fn http(&self, method: &str, path: &str, body: Option<&Json>) -> (u16, String) {
        let mut stream = TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
        let body = body.map(Json::to_string).unwrap_or_default();
        let request = format!(
            "{method} {path} HTTP/1.1\r\nhost: 127.0.0.1\r\nconnection: close\r\nauthorization: Bearer {API_KEY}\r\n\
             content-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(request.as_bytes()).unwrap();
        let mut response = String::new();
        let _read = stream.read_to_string(&mut response);
        let status = response.split_whitespace().nth(1).and_then(|status| status.parse().ok()).unwrap_or(0);
        (status, response.split_once("\r\n\r\n").map(|(_, payload)| payload.to_owned()).unwrap_or_default())
    }

    fn run(&self) -> Json {
        let (status, payload) =
            self.http("POST", "/v1/deployments/chat/runs", Some(&json!({ "sys": { "query": "hi" } })));
        assert_eq!(status, 200, "{payload}");
        let start = payload.find('{').unwrap_or(0);
        let end = payload.rfind('}').map_or(payload.len(), |end| end + 1);
        serde_json::from_str(&payload[start..end]).unwrap()
    }

    fn frozen_agents(&self) -> Option<u64> {
        let (_, metrics) = self.http("GET", "/metrics", None);
        metrics.lines().find_map(|line| line.strip_prefix("graphrt_gateway_frozen_agents ")?.trim().parse().ok())
    }

    fn wait_frozen(&self, count: u64) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while self.frozen_agents() != Some(count) {
            assert!(Instant::now() < deadline, "frozen agents never reached {count}: {:?}", self.frozen_agents());
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

#[test]
fn an_idle_agent_is_frozen_and_the_next_run_thaws_it() {
    let model = Arc::new(Model::default());
    let model_port = serve_model(Arc::clone(&model));
    let dir = std::env::temp_dir().join(format!("graphrt-gw-freeze-{}", std::process::id()));
    let _cleanup = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    // Short: the agents' Unix sockets live here.
    let sockets = PathBuf::from("/tmp").join(format!("grt-fz-{}", std::process::id()));
    let _cleanup = std::fs::remove_dir_all(&sockets);
    let port = free_port();
    std::fs::write(
        dir.join("chat.yaml"),
        "version: \"1\"\nname: chat\ngraph:\n  nodes:\n    - { id: start, type: start }\n    - id: llm\n      type: llm\n      \
         model: { provider: openai, name: test }\n      prompt_template:\n        - { role: system, text: \"hi\" }\n    \
         - { id: answer, type: answer, answer: \"{{#llm.text#}}\" }\n  edges:\n    - { source: start, target: llm }\n    \
         - { source: llm, target: answer }\n",
    )
    .unwrap();
    let config = format!(
        "listen: 127.0.0.1:{port}\nstore: {store}\nsocket_dir: {sockets}\nadmin_token_env: FREEZE_ADMIN_TOKEN\n\
         deployments:\n  - name: chat\n    keys_env: FREEZE_KEYS\n    api_keys: [\"{api_key}\"]\n    \
         pool: {{ idle_timeout_secs: 0, idle_mode: freeze }}\n",
        store = dir.join("gateway.db").display(),
        sockets = sockets.display(),
        api_key = sha256_hex(API_KEY),
    );
    std::fs::write(dir.join("gateway.yaml"), config).unwrap();
    let log = std::fs::File::create(dir.join("gateway.log")).unwrap();
    let gateway = Command::new(BIN)
        .args(["gateway", "--config"])
        .arg(dir.join("gateway.yaml"))
        .env("FREEZE_ADMIN_TOKEN", ADMIN_TOKEN)
        .env("FREEZE_KEYS", HMAC_KEYS)
        .env("OPENAI_API_KEY", "test")
        .env("OPENAI_BASE_URL", format!("http://127.0.0.1:{model_port}/v1"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log)
        .spawn()
        .unwrap();
    let mut world = World { dir: dir.clone(), sockets, port, gateway: Some(gateway) };
    let deadline = Instant::now() + Duration::from_secs(20);
    while TcpStream::connect(("127.0.0.1", port)).is_err() {
        assert!(Instant::now() < deadline, "the gateway did not listen");
        std::thread::sleep(Duration::from_millis(20));
    }
    let deployed = Command::new(BIN)
        .args(["deploy", "--target", "local", "--deployment", "chat", "--version", "v1"])
        .arg(dir.join("chat.yaml"))
        .args(["--gateway", &format!("http://127.0.0.1:{port}")])
        .args(["--", "--no-default-plugins"])
        .env("GRAPHRT_GATEWAY_ADMIN_TOKEN", ADMIN_TOKEN)
        .output()
        .unwrap();
    assert!(deployed.status.success(), "deploy: {}", String::from_utf8_lossy(&deployed.stderr));

    let first = world.run();
    assert_eq!(first["status"], "succeeded", "{first}");
    let agents = world.agent_pids();
    assert_eq!(agents.len(), 1, "{agents:?}");
    world.wait_frozen(1);
    // The model closes its idle connection while the agent is frozen.
    std::thread::sleep(Duration::from_secs(1));
    for round in 0..2 {
        let after = world.run();
        assert_eq!(after["status"], "succeeded", "round {round} after a thaw: {after}");
        assert_eq!(world.agent_pids(), agents, "round {round}: thawed, no agent started");
        assert_eq!(world.frozen_agents(), Some(0));
        world.wait_frozen(1);
        std::thread::sleep(Duration::from_secs(1));
    }
    assert_eq!(model.requests.load(Ordering::SeqCst), 3);
    assert!(model.connections.load(Ordering::SeqCst) >= 3, "closed connections were not reused");
    let _gateway = world.gateway.take().map(|mut gateway| {
        let _killed = gateway.kill();
        gateway.wait()
    });
}
