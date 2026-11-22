//! Real two-process failover of active/passive gateways (plan F3): two
//! `graphrt gateway` processes with one config, one SQLite store and one
//! lease directory, local-process agents of this binary.
//!
//! 1. `gw-a` becomes active, `gw-b` waits as a passive (measured RSS).
//! 2. A version is deployed (agents: `graphrt serve --trust-gateway` of this
//!    binary, calling a slow local upstream), a run and a webhook delivery go
//!    through `gw-a`.
//! 3. `kill -9 gw-a` while a run waits on its upstream call: `gw-b` takes over
//!    (measured), adopts the agents, reclaims and finishes that run, serves
//!    the stored runs, dedupes the webhook redelivery and runs new requests.
//! 4. `gw-a` restarts as a passive. `SIGSTOP gw-b` with the lease lock taken
//!    (a freeze inside a heartbeat): `gw-a` steals the lease after
//!    `stale_after`, kills the frozen holder and serves (measured).
//! 5. A new passive `gw-c` and `SIGTERM gw-a`: graceful handover (measured).
//!
//! Timings print with `--nocapture`.

#![cfg(feature = "gateway")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing, clippy::print_stdout)]

use std::io::{Read as _, Write as _};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value as Json, json};

const BIN: &str = env!("CARGO_BIN_EXE_graphrt");
const API_KEY: &str = "grk_failover_test_key";
const ADMIN_TOKEN: &str = "failover-admin-token";
const HMAC_KEYS: &str = "k1:00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";

struct World {
    dir: PathBuf,
    port: u16,
    children: Vec<Child>,
}

impl Drop for World {
    fn drop(&mut self) {
        for child in &mut self.children {
            let _killed = child.kill();
            let _reaped = child.wait();
        }
        // Agents run in their own process groups: kill them through their pid files.
        if let Ok(entries) = std::fs::read_dir(self.dir.join("sockets")) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().is_some_and(|extension| extension == "pid")
                    && let Some(pid) =
                        std::fs::read_to_string(&path).ok().and_then(|text| text.lines().next().map(str::to_owned))
                {
                    let _killed =
                        Command::new("kill").arg("-KILL").arg(format!("-{pid}")).stderr(Stdio::null()).status();
                    let _killed = Command::new("kill").arg("-KILL").arg(pid).stderr(Stdio::null()).status();
                }
            }
        }
        let _cleanup = std::fs::remove_dir_all(&self.dir);
    }
}

fn sha256_hex(text: &str) -> String {
    use sha2::Digest as _;
    sha2::Sha256::digest(text.as_bytes()).iter().map(|byte| format!("{byte:02x}")).collect()
}

/// An upstream the workflow calls: answers after 800 ms (a slow tool call).
fn slow_upstream() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            std::thread::spawn(move || {
                let mut stream = stream;
                let mut buffer = [0_u8; 4096];
                let _read = stream.read(&mut buffer);
                std::thread::sleep(Duration::from_millis(800));
                let _written =
                    stream.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 4\r\nconnection: close\r\n\r\ndone");
            });
        }
    });
    port
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

impl World {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("graphrt-gw-failover-{}", std::process::id()));
        let _cleanup = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let port = free_port();
        let upstream = slow_upstream();
        std::fs::write(
            dir.join("work.yaml"),
            format!(
                r#"version: "1"
name: work
graph:
  nodes:
    - id: start
      type: start
      variables:
        - {{ variable: payload, type: json, required: false }}
    - id: call
      type: http-request
      method: get
      url: "http://127.0.0.1:{upstream}/work"
      timeout: {{ connect: 5, read: 60 }}
    - {{ id: end, type: end, outputs: [{{ variable: body, value_selector: [call, body] }}] }}
  edges:
    - {{ source: start, target: call }}
    - {{ source: call, target: end }}
"#
            ),
        )
        .unwrap();
        let config = format!(
            r#"listen: 127.0.0.1:{port}
store: {store}
socket_dir: {sockets}
admin_token_env: FAILOVER_ADMIN_TOKEN
backup: {{ dir: {backups}, interval_secs: 3600 }}
ha: {{ heartbeat_ms: 200, stale_after_ms: 1500, poll_ms: 50 }}
deployments:
  - name: shop
    keys_env: FAILOVER_KEYS
    api_keys: ["{api_key}"]
    hooks: [{{ id: orders, scheme: none, idempotency_header: x-delivery }}]
"#,
            store = dir.join("data/gateway.db").display(),
            sockets = dir.join("sockets").display(),
            backups = dir.join("backups").display(),
            api_key = sha256_hex(API_KEY),
        );
        std::fs::write(dir.join("gateway.yaml"), config).unwrap();
        Self { dir, port, children: Vec::new() }
    }

    fn gateway(&mut self, node: &str) -> u32 {
        let log = std::fs::File::create(self.dir.join(format!("{node}.log"))).unwrap();
        let child = Command::new(BIN)
            .args(["gateway", "--config"])
            .arg(self.dir.join("gateway.yaml"))
            .args(["--ha-node", node])
            .env("FAILOVER_ADMIN_TOKEN", ADMIN_TOKEN)
            .env("FAILOVER_KEYS", HMAC_KEYS)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(log)
            .spawn()
            .unwrap();
        let pid = child.id();
        self.children.push(child);
        pid
    }

    fn log(&self, node: &str) -> String {
        std::fs::read_to_string(self.dir.join(format!("{node}.log"))).unwrap_or_default()
    }

    fn lease(&self) -> Json {
        let output = Command::new(BIN)
            .args(["gateway", "ha", "status", "--config"])
            .arg(self.dir.join("gateway.yaml"))
            .output()
            .unwrap();
        serde_json::from_slice(&output.stdout).unwrap()
    }

    fn request(&self, method: &str, path: &str, headers: &[(&str, &str)], body: Option<&Json>) -> Option<(u16, Json)> {
        let mut stream = TcpStream::connect(("127.0.0.1", self.port)).ok()?;
        stream.set_read_timeout(Some(Duration::from_secs(20))).ok()?;
        let body = body.map(Json::to_string).unwrap_or_default();
        let mut request = format!(
            "{method} {path} HTTP/1.1\r\nhost: 127.0.0.1\r\nconnection: close\r\ncontent-type: application/json\r\ncontent-length: {}\r\n",
            body.len()
        );
        for (name, value) in headers {
            request.push_str(&format!("{name}: {value}\r\n"));
        }
        request.push_str("\r\n");
        request.push_str(&body);
        stream.write_all(request.as_bytes()).ok()?;
        let mut response = String::new();
        stream.read_to_string(&mut response).ok()?;
        let status = response.split_whitespace().nth(1)?.parse().ok()?;
        let payload = response.split_once("\r\n\r\n").map(|(_, payload)| payload).unwrap_or_default();
        // Tolerate chunked encoding: parse the first JSON value found.
        let start = payload.find(['{', '[']).unwrap_or(payload.len());
        let end = payload.rfind(['}', ']']).map_or(payload.len(), |end| end + 1);
        let json = serde_json::from_str(payload.get(start..end).unwrap_or_default()).unwrap_or(Json::Null);
        Some((status, json))
    }

    fn healthy(&self) -> bool {
        self.request("GET", "/healthz", &[], None).is_some_and(|(status, _)| status == 200)
    }

    /// Milliseconds until `/healthz` answers 200.
    fn wait_healthy(&self, limit: Duration) -> u128 {
        let started = Instant::now();
        while !self.healthy() {
            assert!(started.elapsed() < limit, "gateway not healthy within {limit:?}");
            std::thread::sleep(Duration::from_millis(10));
        }
        started.elapsed().as_millis()
    }

    fn run(&self, body: &Json) -> (u16, Json) {
        let auth = format!("Bearer {API_KEY}");
        self.request("POST", "/v1/deployments/shop/runs", &[("authorization", &auth)], Some(body)).unwrap()
    }

    fn run_status(&self, run_id: &str) -> Option<String> {
        let auth = format!("Bearer {API_KEY}");
        let (_, record) =
            self.request("GET", &format!("/v1/deployments/shop/runs/{run_id}"), &[("authorization", &auth)], None)?;
        record["status"].as_str().map(str::to_owned)
    }

    fn webhook(&self, delivery: &str) -> (u16, Json) {
        self.request(
            "POST",
            "/v1/deployments/shop/hooks/orders",
            &[("x-delivery", delivery)],
            Some(&json!({ "order": 7 })),
        )
        .unwrap()
    }
}

fn rss_kib(pid: u32) -> u64 {
    let output = Command::new("ps").args(["-o", "rss=", "-p", &pid.to_string()]).output().unwrap();
    String::from_utf8_lossy(&output.stdout).trim().parse().unwrap_or(0)
}

fn alive(pid: u32) -> bool {
    Command::new("kill").args(["-0", &pid.to_string()]).stderr(Stdio::null()).status().unwrap().success()
}

fn signal(pid: u32, name: &str) {
    assert!(Command::new("kill").arg(format!("-{name}")).arg(pid.to_string()).status().unwrap().success());
}

fn wait_for(limit: Duration, mut condition: impl FnMut() -> bool) -> bool {
    let started = Instant::now();
    while started.elapsed() < limit {
        if condition() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

/// Takes the lease read-modify-write lock (`flock`), as a gateway frozen
/// between its heartbeat's lock and unlock would keep it.
fn hold_lease_lock(world: &World) -> std::fs::File {
    let path = world.dir.join("data/graphrt-gateway.lease.lock");
    let file = std::fs::OpenOptions::new().read(true).write(true).open(path).unwrap();
    assert!(wait_for(Duration::from_secs(5), || file.try_lock().is_ok()), "lease lock never free");
    file
}

fn deploy(world: &World, workflow: &Path) {
    let output = Command::new(BIN)
        .args(["deploy", "--target", "local", "--deployment", "shop", "--version", "v1"])
        .arg(workflow)
        .args(["--gateway", &format!("http://127.0.0.1:{}", world.port)])
        .args(["--", "--allow-private-network", "--no-default-plugins"])
        .env("GRAPHRT_GATEWAY_ADMIN_TOKEN", ADMIN_TOKEN)
        .output()
        .unwrap();
    assert!(output.status.success(), "deploy: {}", String::from_utf8_lossy(&output.stderr));
}

#[test]
fn passive_takes_over_after_kill_freeze_and_handover() {
    let mut world = World::new();
    let first = world.gateway("gw-a");
    world.wait_healthy(Duration::from_secs(20));
    let second = world.gateway("gw-b");
    assert!(wait_for(Duration::from_secs(30), || world.log("gw-b").contains("passive")), "{}", world.log("gw-b"));
    assert_eq!(world.lease()["lease"]["node"], "gw-a");
    deploy(&world, &world.dir.join("work.yaml"));
    let (status, record) = world.run(&json!({ "inputs": {} }));
    assert_eq!((status, record["status"].as_str()), (200, Some("succeeded")), "{record}");

    let (status, hook) = world.webhook("delivery-1");
    assert_eq!(status, 202, "{hook}");
    let hook_run = hook["run_id"].as_str().unwrap().to_owned();
    std::thread::sleep(Duration::from_millis(300));
    let passive_rss = rss_kib(second);
    let active_rss = rss_kib(first);

    // 1. kill -9 of the active while a run waits on its slow tool call.
    let (status, in_flight) = world.run(&json!({ "async": true, "inputs": {} }));
    assert_eq!(status, 202, "{in_flight}");
    let in_flight = in_flight["run_id"].as_str().unwrap().to_owned();
    assert!(wait_for(Duration::from_secs(30), || world.run_status(&in_flight).as_deref() == Some("running")));
    std::thread::sleep(Duration::from_millis(200));
    signal(first, "KILL");
    let killed_at = Instant::now();
    world.wait_healthy(Duration::from_secs(30));
    let kill_takeover = killed_at.elapsed().as_millis();
    assert_eq!(world.lease()["lease"]["node"], "gw-b");
    let auth = format!("Bearer {API_KEY}");
    let (status, stored) = world
        .request("GET", &format!("/v1/deployments/shop/runs/{hook_run}"), &[("authorization", &auth)], None)
        .unwrap();
    assert_eq!(status, 200, "the run stored by the old active is served by the new one: {stored}");
    let (status, duplicate) = world.webhook("delivery-1");
    assert_eq!((status, duplicate["duplicate"].as_bool()), (200, Some(true)), "{duplicate}");
    assert_eq!(duplicate["run_id"], hook_run.as_str(), "the redelivery maps to the run of the first delivery");
    let (status, second_hook) = world.webhook("delivery-2");
    assert_eq!(status, 202, "the new active accepts new deliveries: {second_hook}");
    let reclaim_started = Instant::now();
    assert!(
        wait_for(Duration::from_secs(30), || world.run_status(&in_flight).as_deref() == Some("succeeded")),
        "the run in flight at the kill is reclaimed and finishes: {:?}\n{}",
        world.run_status(&in_flight),
        world.log("gw-b")
    );
    let reclaim = reclaim_started.elapsed().as_millis() + kill_takeover;
    let (status, record) = world.run(&json!({ "inputs": {} }));
    assert_eq!((status, record["status"].as_str()), (200, Some("succeeded")), "the new active runs agents: {record}");

    // 2. The old node comes back as a passive; freeze the active.
    let third = world.gateway("gw-a");
    assert!(wait_for(Duration::from_secs(30), || world.log("gw-a").contains("passive")));
    assert!(alive(third));
    // Worst case, made deterministic: the freeze lands inside a heartbeat, so
    // the lease lock stays taken for the whole freeze (the test holds it).
    let lease_lock = hold_lease_lock(&world);
    signal(second, "STOP");
    let frozen_at = Instant::now();
    assert!(wait_for(Duration::from_secs(30), || world.lease()["lease"]["node"] == "gw-a"), "no steal");
    drop(lease_lock);
    world.wait_healthy(Duration::from_secs(30));
    let freeze_takeover = frozen_at.elapsed().as_millis();
    assert!(
        wait_for(Duration::from_secs(5), || !alive(second)
            || world
                .children
                .iter_mut()
                .any(|child| child.id() == second && child.try_wait().ok().flatten().is_some())),
        "the frozen holder was killed"
    );
    let (status, again) = world.webhook("delivery-1");
    assert_eq!((status, again["duplicate"].as_bool()), (200, Some(true)), "{again}");

    // 3. Graceful handover.
    let fourth = world.gateway("gw-c");
    assert!(wait_for(Duration::from_secs(30), || world.log("gw-c").contains("passive")));
    signal(third, "TERM");
    let handover_at = Instant::now();
    assert!(wait_for(Duration::from_secs(30), || world.lease()["lease"]["node"] == "gw-c"));
    world.wait_healthy(Duration::from_secs(30));
    let handover = handover_at.elapsed().as_millis();
    assert!(world.log("gw-a").contains("ha_released"), "{}", world.log("gw-a"));
    assert!(alive(fourth));
    let token = world.lease()["lease"]["token"].as_u64().unwrap();
    assert!(token >= 4, "every take bumps the token: {token}");

    println!(
        "gateway failover: passive RSS {passive_rss} KiB, active RSS {active_rss} KiB, takeover after kill -9 {kill_takeover} ms (in-flight run finished {reclaim} ms after the kill), after SIGSTOP (stale_after 1500 ms) {freeze_takeover} ms, graceful handover {handover} ms"
    );
    assert!(kill_takeover < 30_000 && freeze_takeover < 30_000 && handover < 30_000);
}
