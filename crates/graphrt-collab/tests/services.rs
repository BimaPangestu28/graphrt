//! Plugin services on a real collaboration server: a Python fixture plugin
//! declares a service, an owner enables it with a workspace secret, and the
//! server delivers events (with project content, as the service asks) and
//! member calls, with the roles of the service.

#![cfg(all(feature = "services", feature = "client", unix))]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use graphrt_collab::ProjectDoc;
use graphrt_collab::client::{Api, ClientError, LiveEvent, LiveOptions, LiveProject, TokenSource};
use graphrt_collab::server::{CollabConfig, Options, Server};
use graphrt_console::Role;
use serde_json::{Value, json};

const SECRETS_KEY: &str = "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=";

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("graphrt-collab-services-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// The fixture plugin in `dir/plugins/recorder`, pointed at the repository's Python SDK.
fn install_fixture(dir: &Path) -> PathBuf {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/recorder");
    let plugin = dir.join("plugins/recorder");
    std::fs::create_dir_all(&plugin).unwrap();
    std::fs::copy(fixture.join("plugin.py"), plugin.join("plugin.py")).unwrap();
    let mut manifest: Value =
        serde_json::from_str(&std::fs::read_to_string(fixture.join("graphrt-plugin.json")).unwrap()).unwrap();
    let sdk = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../sdk/python").canonicalize().unwrap();
    manifest["env"] = json!({ "GRAPHRT_PYTHON_SDK": sdk.to_str().unwrap() });
    std::fs::write(plugin.join("graphrt-plugin.json"), manifest.to_string()).unwrap();
    plugin.join("recorded.jsonl")
}

fn recorded(path: &Path) -> Vec<Value> {
    std::fs::read_to_string(path).unwrap_or_default().lines().map(|line| serde_json::from_str(line).unwrap()).collect()
}

async fn wait_recorded(path: &Path, what: &str, want: impl Fn(&Value) -> bool) -> Value {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(entry) = recorded(path).into_iter().find(|e| want(e)) {
            return entry;
        }
        assert!(tokio::time::Instant::now() < deadline, "timed out waiting for {what}: {:?}", recorded(path));
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn status(error: &ClientError) -> u16 {
    match error {
        ClientError::Http { status, .. } => *status,
        other => panic!("expected an HTTP error, got {other}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn services_get_events_and_calls() {
    if std::process::Command::new("python3").arg("--version").output().is_err() {
        eprintln!("python3 is not installed: skipping");
        return;
    }
    let dir = temp_dir("events");
    let out = install_fixture(&dir);
    let tcp = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = tcp.local_addr().unwrap().port();
    let yaml = format!(
        "public_url: http://127.0.0.1:{port}\ndata_dir: {}\nplugins: {{ dirs: [{}], trusted: [recorder] }}\n",
        dir.join("data").display(),
        dir.join("plugins").display()
    );
    let config: CollabConfig = graphrt_core::yaml::from_str(&yaml, true).unwrap();
    let server = Server::open(Options {
        config,
        signing_key: None,
        secrets_key: Some(SECRETS_KEY.into()),
        client_ids: Default::default(),
        client_secrets: Default::default(),
        verifiers: None,
    })
    .unwrap();
    let running = Arc::clone(&server);
    tokio::spawn(async move { graphrt_collab::server::http::serve_on(running, tcp, std::future::pending()).await });
    let api = Api::new(&format!("http://127.0.0.1:{port}")).unwrap();

    let (_, code) = server.bootstrap_invite("Team", Role::Owner, None, 24).unwrap();
    let owner = api.redeem_invite(&code, None, Some("Ana"), "test").await.unwrap();
    let access = owner.access_token.unwrap();
    let ws = owner.workspace["id"].as_str().unwrap().to_owned();

    // Offered, off until enabled.
    let listed = api.get(&access, &format!("/v1/workspaces/{ws}/services")).await.unwrap();
    assert_eq!(listed["services"][0]["plugin"], "recorder");
    assert_eq!(listed["services"][0]["enabled"], false);
    let refused = api.post(&access, &format!("/v1/workspaces/{ws}/services/recorder/log/call"), &json!({})).await;
    assert_eq!(status(&refused.unwrap_err()), 404);

    api.put(&access, &format!("/v1/workspaces/{ws}/secrets/TOKEN"), &json!({ "value": "workspace-token" }))
        .await
        .unwrap();
    api.put(&access, &format!("/v1/workspaces/{ws}/secrets/OTHER"), &json!({ "value": "not granted" })).await.unwrap();
    let grant = api
        .put(
            &access,
            &format!("/v1/workspaces/{ws}/services/recorder/log"),
            &json!({ "enabled": true, "secrets": ["TOKEN"] }),
        )
        .await
        .unwrap();
    assert_eq!(grant["enabled"], true);

    // A live edit: `project.changed` once editing stops, with the content (reads_projects) and only the granted secret.
    let created = api
        .post(
            &access,
            &format!("/v1/workspaces/{ws}/projects"),
            &json!({ "name": "Bot", "content": { "name": "Bot" } }),
        )
        .await
        .unwrap();
    let project = created["id"].as_str().unwrap().to_owned();
    let (scoped, _) = api.scoped_token(&access, &ws).await.unwrap();
    let token: TokenSource = Arc::new(move || {
        let token = scoped.clone();
        Box::pin(async move { Ok(token) })
    });
    let (live, mut events) = LiveProject::start(
        ProjectDoc::new(),
        LiveOptions {
            url: api.sync_url(&ws, &project),
            token,
            renew_every: Duration::from_secs(240),
            max_backoff: Duration::from_secs(2),
        },
    );
    // Edit once the server's state arrived (an edit of an empty replica would race the shared one).
    loop {
        if matches!(events.recv().await, Some(LiveEvent::Synced(_))) {
            break;
        }
    }
    let mut edited = serde_json::Map::new();
    edited.insert("name".into(), json!("Bot, edited live"));
    live.edit(edited).await.unwrap();
    let changed = wait_recorded(&out, "project.changed", |e| e["args"]["event"] == "project.changed").await;
    assert_eq!(changed["args"]["project"]["content"]["name"], "Bot, edited live");
    assert_eq!(changed["args"]["workspace"]["name"], "Team");
    assert_eq!(changed["secrets"], json!({ "TOKEN": "workspace-token" }));
    assert_eq!(changed["env_has_token"], false);

    // A saved version and a new member are events too.
    api.post(&access, &format!("/v1/workspaces/{ws}/projects/{project}/versions"), &json!({ "label": "v1" }))
        .await
        .unwrap();
    wait_recorded(&out, "version.saved", |e| {
        e["args"]["event"] == "version.saved" && e["args"]["details"]["label"] == "v1"
    })
    .await;
    let invite =
        api.post(&access, &format!("/v1/workspaces/{ws}/invites"), &json!({ "role": "viewer" })).await.unwrap();
    let viewer = api.redeem_invite(invite["code"].as_str().unwrap(), None, Some("Citra"), "test").await.unwrap();
    wait_recorded(&out, "member.joined", |e| e["args"]["event"] == "member.joined").await;

    // A member call with the service's role: operators may, viewers may not.
    let answer = api
        .post(
            &access,
            &format!("/v1/workspaces/{ws}/services/recorder/log/call"),
            &json!({ "ping": 1, "project": project }),
        )
        .await
        .unwrap();
    assert_eq!(answer["data"]["echo"]["ping"], 1);
    let called = wait_recorded(&out, "call", |e| e["args"]["call"]["ping"] == 1).await;
    assert_eq!(called["args"]["caller"]["name"], "Ana");
    assert_eq!(called["args"]["project"]["content"]["name"], "Bot, edited live");
    let viewer_access = viewer.access_token.unwrap();
    let refused =
        api.post(&viewer_access, &format!("/v1/workspaces/{ws}/services/recorder/log/call"), &json!({})).await;
    assert_eq!(status(&refused.unwrap_err()), 403);

    // Disabled again: no more calls. Grants of secrets the service does not ask for are refused.
    let bad = api
        .put(
            &access,
            &format!("/v1/workspaces/{ws}/services/recorder/log"),
            &json!({ "enabled": true, "secrets": ["OTHER"] }),
        )
        .await;
    assert_eq!(status(&bad.unwrap_err()), 400);
    api.put(&access, &format!("/v1/workspaces/{ws}/services/recorder/log"), &json!({ "enabled": false }))
        .await
        .unwrap();
    let refused = api.post(&access, &format!("/v1/workspaces/{ws}/services/recorder/log/call"), &json!({})).await;
    assert_eq!(status(&refused.unwrap_err()), 404);

    let audit = api.get(&access, &format!("/v1/workspaces/{ws}/audit?action=service.")).await.unwrap();
    let actions: Vec<String> = audit["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| format!("{}:{}", e["action"].as_str().unwrap(), e["outcome"].as_str().unwrap()))
        .collect();
    for wanted in ["service.grant:ok", "service.event:ok", "service.call:ok", "service.call:denied"] {
        assert!(actions.iter().any(|a| a == wanted), "{wanted} in {actions:?}");
    }
    live.close();
}
