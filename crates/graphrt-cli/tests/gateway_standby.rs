//! Standby agents (`graphrt agent-standby`) of the local-process provider,
//! with the real binary: a wake assigns a pre-started standby instead of
//! starting a process, the standby is the deployment's agent (its keys came
//! over stdin), and versions a standby cannot take start as before.
#![cfg(feature = "gateway")]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]

use std::io::Write as _;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use graphrt_gateway::agent_client::AgentClient;
use graphrt_gateway::local::{LocalProvider, LocalVersion};
use graphrt_gateway::signature::GatewayKey;
use graphrt_provider_api::{DeploymentRef, Provider, ReleaseMode};

const BIN: &str = env!("CARGO_BIN_EXE_graphrt");
const HELLO_WORKFLOW: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../examples/hello.yaml");
const SECRET: [u8; 32] = [0x42; 32];

fn keys(kid: &str) -> String {
    format!("{kid}:{}", "42".repeat(32))
}

fn temp_dir(name: &str) -> PathBuf {
    // Short: the agents' Unix sockets live here.
    let dir = std::env::temp_dir().join(format!("gsb-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn version(env: Vec<(String, String)>, kid: &str) -> LocalVersion {
    LocalVersion {
        command: vec![BIN.to_owned(), "serve".to_owned(), HELLO_WORKFLOW.to_owned(), "--no-default-plugins".to_owned()],
        keys: keys(kid),
        env,
    }
}

/// Waits until `condition` holds (10 s at most).
async fn eventually(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Wakes `deployment` and asks its agent who it is (signed with `kid`).
async fn wake_and_hello(provider: &LocalProvider, deployment: &DeploymentRef, kid: &str) -> serde_json::Value {
    let endpoints = provider.wake(deployment, 1).await.unwrap();
    let endpoint = endpoints.first().expect("one replica");
    let key = GatewayKey::new(kid, &SECRET).unwrap();
    AgentClient::default().hello(&endpoint.address, &key, &deployment.name).await.unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_wake_assigns_a_standby_and_a_new_one_takes_its_place() {
    graphrt_gateway::install_crypto_provider();
    let dir = temp_dir("assign");
    let provider = LocalProvider::new(&dir, Duration::from_secs(2)).unwrap().with_standby(
        std::path::Path::new(BIN),
        1,
        vec!["GRAPHRT_TEST_GATEWAY_SECRET".to_owned()],
    );
    let shop = DeploymentRef::new("shop", "v1", "local");
    let blog = DeploymentRef::new("blog", "v1", "local");
    provider.register(&shop, version(Vec::new(), "k1"));
    provider.register(&blog, version(Vec::new(), "k2"));
    assert_eq!(provider.standby_ready(), 1, "registering a version it can take starts the standby");
    let hello = wake_and_hello(&provider, &shop, "k1").await;
    assert_eq!(hello["deployment"], "shop", "{hello}");
    assert_eq!(provider.standby_assigned(), 1, "the first wake took the standby");
    // The replacement started in the background serves the next deployment,
    // with that deployment's keys only.
    eventually("the replacement standby", || provider.standby_ready() == 1).await;
    let hello = wake_and_hello(&provider, &blog, "k2").await;
    assert_eq!(hello["deployment"], "blog", "{hello}");
    assert_eq!(provider.standby_assigned(), 2);
    let foreign = GatewayKey::new("k1", &SECRET).unwrap();
    let blog_address = provider.wake(&blog, 1).await.unwrap()[0].address.clone();
    assert!(AgentClient::default().hello(&blog_address, &foreign, "blog").await.is_err(), "shop's key is refused");
    // A version with its own environment starts a process as before.
    let docs = DeploymentRef::new("docs", "v1", "local");
    provider.register(&docs, version(vec![("GRAPHRT_TEST_MARK".into(), "1".into())], "k3"));
    assert_eq!(wake_and_hello(&provider, &docs, "k3").await["deployment"], "docs");
    assert_eq!(provider.standby_assigned(), 2, "not a standby");
    for deployment in [&shop, &blog, &docs] {
        for replica in provider.list(deployment).await.unwrap() {
            provider.release(&replica.replica, ReleaseMode::Stop).await.unwrap();
        }
    }
    eventually("the agents to stop", || {
        std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .all(|entry| entry.path().extension().is_none_or(|extension| extension != "pid"))
    })
    .await;
    drop(provider);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A standby whose gateway goes away (its stdin closes) exits; one that gets
/// anything but `serve --trust-gateway` refuses it and exits with an error.
#[test]
fn a_standby_exits_without_an_assignment_and_refuses_a_bad_one() {
    let mut idle = Command::new(BIN)
        .arg("agent-standby")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    drop(idle.stdin.take());
    assert_eq!(idle.wait().unwrap().code(), Some(0));
    for line in [
        "{\"argv\":[\"run\",\"x.yaml\"],\"keys\":\"k1:00\"}",
        "{\"argv\":[\"serve\",\"x.yaml\"],\"keys\":\"\"}",
        "nonsense",
    ] {
        let mut refused = Command::new(BIN)
            .arg("agent-standby")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdin = refused.stdin.take().unwrap();
        writeln!(stdin, "{line}").unwrap();
        drop(stdin);
        let output = refused.wait_with_output().unwrap();
        assert!(!output.status.success(), "{line}");
        assert!(!String::from_utf8_lossy(&output.stderr).contains("k1:00"), "keys are never printed");
    }
}
