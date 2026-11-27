//! End to end: a real collaboration server on a loopback port and headless
//! clients editing the same project at the same time, offline edits merging
//! on reconnect, and the refusals (outsiders, viewers, foreign tokens).

#![cfg(all(feature = "server", feature = "client"))]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use graphrt_collab::ProjectDoc;
use graphrt_collab::client::{Api, ClientError, LiveEvent, LiveOptions, LiveProject, TokenSource};
use graphrt_collab::server::{CollabConfig, Options, Server};
use graphrt_console::Role;
use serde_json::{Value, json};
use tokio::sync::mpsc::UnboundedReceiver;

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("graphrt-collab-e2e-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Start a server; returns its address and the server.
async fn start(name: &str) -> (String, Arc<Server>, tokio::sync::oneshot::Sender<()>) {
    let tcp = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = tcp.local_addr().unwrap();
    let yaml = format!(
        "public_url: http://127.0.0.1:{}\ndata_dir: {}\nlimits: {{ messages_per_second: 500 }}\n",
        addr.port(),
        temp_dir(name).display()
    );
    let config: CollabConfig = graphrt_core::yaml::from_str(&yaml, true).unwrap();
    let server = Server::open(Options {
        config,
        signing_key: None,
        secrets_key: None,
        client_ids: Default::default(),
        client_secrets: Default::default(),
        verifiers: None,
    })
    .unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let running = Arc::clone(&server);
    tokio::spawn(async move {
        graphrt_collab::server::http::serve_on(running, tcp, async {
            let _ = stopped.await;
        })
        .await
        .unwrap();
    });
    (format!("http://127.0.0.1:{}", addr.port()), server, stop)
}

/// A person signed in on the server.
#[derive(Clone)]
struct Person {
    api: Api,
    access: String,
    refresh: String,
    id: String,
}

async fn join(api: &Api, code: &str, name: &str) -> (Person, Value) {
    let redeemed = api.redeem_invite(code, None, Some(name), "e2e").await.unwrap();
    let person = Person {
        api: api.clone(),
        access: redeemed.access_token.clone().unwrap(),
        refresh: redeemed.refresh_token.clone().unwrap(),
        id: redeemed.account["id"].as_str().unwrap().to_owned(),
    };
    (person, redeemed.workspace)
}

fn token_source(person: &Person, workspace: &str) -> TokenSource {
    let api = person.api.clone();
    let access = person.access.clone();
    let workspace = workspace.to_owned();
    Arc::new(move || {
        let api = api.clone();
        let access = access.clone();
        let workspace = workspace.clone();
        Box::pin(async move { api.scoped_token(&access, &workspace).await.map(|(t, _)| t) })
    })
}

fn live(
    person: &Person,
    workspace: &str,
    project: &str,
    doc: ProjectDoc,
) -> (LiveProject, UnboundedReceiver<LiveEvent>) {
    LiveProject::start(
        doc,
        LiveOptions {
            url: person.api.sync_url(workspace, project),
            token: token_source(person, workspace),
            renew_every: Duration::from_secs(240),
            max_backoff: Duration::from_secs(2),
        },
    )
}

/// Wait for an event matching `want`, failing after a few seconds.
async fn wait_for(
    events: &mut UnboundedReceiver<LiveEvent>,
    what: &str,
    want: impl Fn(&LiveEvent) -> bool,
) -> LiveEvent {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let event = tokio::time::timeout_at(deadline, events.recv())
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
            .unwrap_or_else(|| panic!("events ended waiting for {what}"));
        eprintln!("[event] {what}: {event:?}");
        if want(&event) {
            return event;
        }
    }
}

async fn wait_json(project: &LiveProject, what: &str, want: impl Fn(&Value) -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !want(&project.json()) {
        assert!(tokio::time::Instant::now() < deadline, "timed out waiting for {what}: {}", project.json());
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn flow() -> Value {
    json!({
        "name": "Support bot",
        "flow": {
            "version": "1",
            "graph": {
                "nodes": [
                    {"id": "start", "type": "start"},
                    {"id": "llm", "type": "llm", "prompt": "You answer support questions."},
                    {"id": "end", "type": "end"}
                ],
                "edges": [{"source": "start", "target": "llm"}, {"source": "llm", "target": "end"}]
            }
        },
        "layout": {"start": {"x": 0, "y": 0}, "llm": {"x": 200, "y": 0}, "end": {"x": 400, "y": 0}}
    })
}

fn object(value: Value) -> serde_json::Map<String, Value> {
    match value {
        Value::Object(map) => map,
        _ => panic!("not an object"),
    }
}

fn status(error: &ClientError) -> u16 {
    match error {
        ClientError::Http { status, .. } => *status,
        other => panic!("expected an HTTP error, got {other}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_people_edit_one_project_live() {
    let (url, server, _stop) = start("live").await;
    let api = Api::new(&url).unwrap();
    let info = api.info().await.unwrap();
    assert_eq!(info.server, "graphrt-collab");
    assert!(info.sign_in.invite_codes);

    // Bootstrap: the first owner joins with an owner invite made on the server.
    let (_, owner_code) = server.bootstrap_invite("Acme team", Role::Owner, None, 24).unwrap();
    let (ana, workspace) = join(&api, &owner_code, "Ana").await;
    let ws = workspace["id"].as_str().unwrap().to_owned();
    eprintln!("[step] Ana joined workspace {ws} as owner");

    // Ana shares a project and invites Budi to edit and Citra to view.
    let created = ana
        .api
        .post(
            &ana.access,
            &format!("/v1/workspaces/{ws}/projects"),
            &json!({ "name": "Support bot", "content": flow() }),
        )
        .await
        .unwrap();
    let project = created["id"].as_str().unwrap().to_owned();
    let editor_invite = ana
        .api
        .post(&ana.access, &format!("/v1/workspaces/{ws}/invites"), &json!({ "role": "operator" }))
        .await
        .unwrap();
    let viewer_invite =
        ana.api.post(&ana.access, &format!("/v1/workspaces/{ws}/invites"), &json!({ "role": "viewer" })).await.unwrap();
    let preview = api.preview_invite(editor_invite["link"].as_str().unwrap()).await.unwrap();
    assert_eq!(preview["workspace"]["name"], "Acme team");
    assert_eq!(preview["role"], "operator");
    let (budi, _) = join(&api, editor_invite["link"].as_str().unwrap(), "Budi").await;
    let (citra, _) = join(&api, viewer_invite["code"].as_str().unwrap(), "Citra").await;
    eprintln!("[step] Budi (editor) and Citra (viewer) joined");

    // Both editors open the project.
    let (a, mut a_events) = live(&ana, &ws, &project, ProjectDoc::new());
    let (b, mut b_events) = live(&budi, &ws, &project, ProjectDoc::new());
    wait_for(&mut a_events, "Ana connected", |e| matches!(e, LiveEvent::Connected { read_only: false, .. })).await;
    wait_for(&mut b_events, "Budi connected", |e| matches!(e, LiveEvent::Connected { read_only: false, .. })).await;
    wait_for(&mut a_events, "Ana synced", |e| matches!(e, LiveEvent::Synced(_))).await;
    wait_for(&mut b_events, "Budi synced", |e| matches!(e, LiveEvent::Synced(v) if v["name"] == "Support bot")).await;
    wait_json(&a, "Ana has the shared content", |v| v["name"] == "Support bot").await;
    wait_json(&b, "Budi has the shared content", |v| v["name"] == "Support bot").await;

    // Presence: each sees the other, with the name the server knows.
    a.set_presence(json!({ "focus": "llm" }));
    b.set_presence(json!({ "focus": "end" }));
    let seen = wait_for(&mut b_events, "Budi sees Ana", |e| {
        matches!(e, LiveEvent::Presence(peers) if peers.iter().any(|p| p.person.as_ref().is_some_and(|x| x.name == "Ana")))
    })
    .await;
    if let LiveEvent::Presence(peers) = seen {
        let ana_peer = peers.iter().find(|p| p.person.as_ref().is_some_and(|x| x.name == "Ana")).unwrap();
        assert_eq!(ana_peer.state["focus"], "llm");
        assert_eq!(ana_peer.person.as_ref().unwrap().role, "owner");
    }
    wait_for(&mut a_events, "Ana sees Budi", |e| {
        matches!(e, LiveEvent::Presence(peers) if peers.iter().any(|p| p.person.as_ref().is_some_and(|x| x.name == "Budi")))
    })
    .await;

    // Concurrent edits: Ana types in the prompt while Budi adds a node and types in the same prompt.
    let mut on_a = a.json();
    on_a["flow"]["graph"]["nodes"][1]["prompt"] = json!("Politely, you answer support questions.");
    let mut on_b = b.json();
    on_b["flow"]["graph"]["nodes"][1]["prompt"] = json!("You answer support questions in Indonesian.");
    on_b["flow"]["graph"]["nodes"].as_array_mut().unwrap().insert(2, json!({ "id": "notify", "type": "http" }));
    on_b["layout"]["notify"] = json!({ "x": 300, "y": 120 });
    let (ra, rb) = tokio::join!(a.edit(object(on_a)), b.edit(object(on_b)));
    assert!(ra.unwrap() && rb.unwrap());
    let merged_prompt = "Politely, you answer support questions in Indonesian.";
    let converged = |v: &Value| {
        v["flow"]["graph"]["nodes"][1]["prompt"] == merged_prompt && v["flow"]["graph"]["nodes"][2]["id"] == "notify"
    };
    wait_json(&a, "Ana has both edits", converged).await;
    wait_json(&b, "Budi has both edits", converged).await;
    assert_eq!(a.json(), b.json());
    eprintln!("[step] concurrent edits merged: {}", a.json()["flow"]["graph"]["nodes"][1]["prompt"]);

    // The server stored the merged project.
    let stored = ana.api.get(&ana.access, &format!("/v1/workspaces/{ws}/projects/{project}")).await.unwrap();
    assert_eq!(stored["content"], a.json());
    let online: Vec<&str> = stored["online"].as_array().unwrap().iter().filter_map(|p| p["name"].as_str()).collect();
    assert_eq!(online, ["Ana", "Budi"]);

    // Offline: Budi leaves, edits offline, Ana edits meanwhile, Budi comes back.
    b.close();
    let offline_doc = ProjectDoc::from_state(&b.state()).unwrap();
    let mut offline_edit = offline_doc.to_json();
    offline_edit["layout"]["llm"]["y"] = json!(80);
    offline_edit["name"] = json!("Support bot (offline edit)");
    offline_doc.apply_json(&object(offline_edit)).unwrap();
    let mut on_a = a.json();
    on_a["flow"]["graph"]["edges"].as_array_mut().unwrap().push(json!({ "source": "llm", "target": "notify" }));
    a.edit(object(on_a)).await.unwrap();
    let saved_state = offline_doc.encode_state();
    let (b2, mut b2_events) = live(&budi, &ws, &project, ProjectDoc::from_state(&saved_state).unwrap());
    wait_for(&mut b2_events, "Budi reconnected", |e| matches!(e, LiveEvent::Connected { .. })).await;
    let both = |v: &Value| {
        v["name"] == "Support bot (offline edit)"
            && v["layout"]["llm"]["y"] == 80
            && v["flow"]["graph"]["edges"].as_array().is_some_and(|e| e.len() == 3)
    };
    wait_json(&a, "Ana has Budi's offline edit", both).await;
    wait_json(&b2, "Budi has Ana's edit made while he was offline", both).await;
    assert_eq!(a.json(), b2.json());
    eprintln!("[step] offline edits merged on reconnect");

    // A named version, then a restore.
    let version = ana
        .api
        .post(
            &ana.access,
            &format!("/v1/workspaces/{ws}/projects/{project}/versions"),
            &json!({ "label": "before launch" }),
        )
        .await
        .unwrap();
    let mut renamed = a.json();
    renamed["name"] = json!("Renamed later");
    a.edit(object(renamed)).await.unwrap();
    wait_json(&b2, "Budi sees the rename", |v| v["name"] == "Renamed later").await;
    let n = version["version"].as_u64().unwrap();
    ana.api
        .post(&ana.access, &format!("/v1/workspaces/{ws}/projects/{project}/versions/{n}/restore"), &json!({}))
        .await
        .unwrap();
    wait_json(&a, "the restore reaches Ana", |v| v["name"] == "Support bot (offline edit)").await;
    wait_json(&b2, "the restore reaches Budi", |v| v["name"] == "Support bot (offline edit)").await;
    let versions = ana.api.get(&ana.access, &format!("/v1/workspaces/{ws}/projects/{project}/versions")).await.unwrap();
    assert!(versions["versions"].as_array().unwrap().iter().any(|v| v["label"] == "before launch"));

    // The viewer: sees the project live, cannot change it.
    let (c, mut c_events) = live(&citra, &ws, &project, ProjectDoc::new());
    wait_for(&mut c_events, "Citra connected read only", |e| matches!(e, LiveEvent::Connected { read_only: true, .. }))
        .await;
    wait_json(&c, "Citra has the content", |v| v["name"] == "Support bot (offline edit)").await;
    let mut forbidden = c.json();
    forbidden["name"] = json!("Viewer was here");
    c.edit(object(forbidden)).await.unwrap();
    wait_for(&mut c_events, "Citra's edit refused", |e| matches!(e, LiveEvent::Refused(r) if r.contains("not edit")))
        .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(a.json()["name"], "Support bot (offline edit)");
    let stored = ana.api.get(&ana.access, &format!("/v1/workspaces/{ws}/projects/{project}")).await.unwrap();
    assert_eq!(stored["content"]["name"], "Support bot (offline edit)");
    let refused =
        citra.api.post(&citra.access, &format!("/v1/workspaces/{ws}/projects"), &json!({ "name": "x" })).await;
    assert_eq!(status(&refused.unwrap_err()), 403);
    let refused = citra.api.get(&citra.access, &format!("/v1/workspaces/{ws}/audit")).await;
    assert_eq!(status(&refused.unwrap_err()), 403);
    eprintln!("[step] viewer edits refused, viewer REST writes 403");

    // A role change applies at once: Budi becomes a viewer.
    ana.api
        .put(&ana.access, &format!("/v1/workspaces/{ws}/members/{}", budi.id), &json!({ "role": "viewer" }))
        .await
        .unwrap();
    wait_for(&mut b2_events, "Budi demoted", |e| matches!(e, LiveEvent::Role { read_only: true, .. })).await;
    let mut late = b2.json();
    late["name"] = json!("Budi after demotion");
    b2.edit(object(late)).await.unwrap();
    wait_for(&mut b2_events, "Budi's edit refused after demotion", |e| matches!(e, LiveEvent::Refused(_))).await;

    // Removing Citra ends her session for good.
    ana.api.delete(&ana.access, &format!("/v1/workspaces/{ws}/members/{}", citra.id)).await.unwrap();
    wait_for(&mut c_events, "Citra removed", |e| matches!(e, LiveEvent::Disconnected { final_: true, .. })).await;
    let gone = citra.api.get(&citra.access, &format!("/v1/workspaces/{ws}/projects/{project}")).await;
    assert_eq!(status(&gone.unwrap_err()), 404);

    // The audit log has the story, refusals included.
    let audit = ana.api.get(&ana.access, &format!("/v1/workspaces/{ws}/audit?limit=200")).await.unwrap();
    let actions: Vec<String> = audit["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| format!("{}:{}", e["action"].as_str().unwrap(), e["outcome"].as_str().unwrap()))
        .collect();
    for wanted in [
        "member.join:ok",
        "project.create:ok",
        "invite.create:ok",
        "version.save:ok",
        "version.restore:ok",
        "member.role:ok",
        "member.remove:ok",
        "project.create:denied",
        "audit.read:denied",
    ] {
        assert!(actions.iter().any(|a| a == wanted), "audit lacks {wanted}: {actions:?}");
    }
    eprintln!("[step] audit log: {} entries", actions.len());
    a.close();
    b2.close();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn outsiders_and_foreign_tokens_are_refused() {
    let (url, server, _stop) = start("outsiders").await;
    let api = Api::new(&url).unwrap();
    let (_, code) = server.bootstrap_invite("Team A", Role::Owner, None, 24).unwrap();
    let (ana, workspace) = join(&api, &code, "Ana").await;
    let ws = workspace["id"].as_str().unwrap().to_owned();
    let created = ana
        .api
        .post(&ana.access, &format!("/v1/workspaces/{ws}/projects"), &json!({ "name": "Secret plan" }))
        .await
        .unwrap();
    let project = created["id"].as_str().unwrap().to_owned();

    // Eve owns another workspace on the same server.
    let (_, other_code) = server.bootstrap_invite("Team B", Role::Owner, None, 24).unwrap();
    let (eve, other) = join(&api, &other_code, "Eve").await;
    let eve_ws = other["id"].as_str().unwrap().to_owned();

    // Not a member: the workspace and project do not exist for her (404, not 403).
    for path in [
        format!("/v1/workspaces/{ws}"),
        format!("/v1/workspaces/{ws}/projects/{project}"),
        format!("/v1/workspaces/{ws}/projects"),
    ] {
        let refused = eve.api.get(&eve.access, &path).await.unwrap_err();
        assert_eq!(status(&refused), 404, "{path}");
    }
    let refused = eve.api.scoped_token(&eve.access, &ws).await.unwrap_err();
    assert_eq!(status(&refused), 404);

    // Her token for her own workspace does not open Ana's project.
    let (eve_scoped, _) = eve.api.scoped_token(&eve.access, &eve_ws).await.unwrap();
    let fixed: TokenSource = Arc::new(move || {
        let token = eve_scoped.clone();
        Box::pin(async move { Ok(token) })
    });
    let (_session, mut events) = LiveProject::start(
        ProjectDoc::new(),
        LiveOptions {
            url: api.sync_url(&ws, &project),
            token: fixed,
            renew_every: Duration::from_secs(240),
            max_backoff: Duration::from_secs(60),
        },
    );
    let refused = wait_for(&mut events, "Eve refused", |e| matches!(e, LiveEvent::Disconnected { .. })).await;
    assert!(matches!(&refused, LiveEvent::Disconnected { reason, .. } if reason.contains("401")), "{refused:?}");

    // An account token (not scoped) cannot open a sync session either.
    let unscoped = ana.access.clone();
    let source: TokenSource = Arc::new(move || {
        let token = unscoped.clone();
        Box::pin(async move { Ok(token) })
    });
    let (_session, mut events) = LiveProject::start(
        ProjectDoc::new(),
        LiveOptions {
            url: api.sync_url(&ws, &project),
            token: source,
            renew_every: Duration::from_secs(240),
            max_backoff: Duration::from_secs(60),
        },
    );
    let refused = wait_for(&mut events, "unscoped refused", |e| matches!(e, LiveEvent::Disconnected { .. })).await;
    assert!(matches!(&refused, LiveEvent::Disconnected { reason, .. } if reason.contains("401")), "{refused:?}");

    // Ana's workspace token is not valid for Eve's workspace REST routes.
    let (ana_scoped, _) = ana.api.scoped_token(&ana.access, &ws).await.unwrap();
    let refused = ana.api.get(&ana_scoped, &format!("/v1/workspaces/{eve_ws}")).await.unwrap_err();
    assert!(matches!(status(&refused), 403 | 404));

    // Forged and garbage tokens.
    // Change the first character of the signature (the last one may encode padding bits only).
    let (signed, signature) = ana.access.rsplit_once('.').unwrap();
    let first = if signature.starts_with('A') { 'B' } else { 'A' };
    let forged = format!("{signed}.{first}{}", &signature[1..]);
    assert_eq!(status(&api.get(&forged, "/v1/me").await.unwrap_err()), 401);
    assert_eq!(status(&api.get("nonsense", "/v1/me").await.unwrap_err()), 401);

    // A used invite does not work twice; a refresh token works once.
    assert_eq!(status(&api.redeem_invite(&code, None, Some("Mallory"), "x").await.unwrap_err()), 404);
    let tokens = api.refresh(&ana.refresh).await.unwrap();
    assert_eq!(status(&api.refresh(&ana.refresh).await.unwrap_err()), 401);
    // Using the old one again ended that sign in: the new token stopped working too.
    assert_eq!(status(&api.refresh(&tokens.refresh_token).await.unwrap_err()), 401);

    // The server's audit log has the refused session.
    let entries = std::fs::read_dir(server.config().data_dir.join("audit")).unwrap().count();
    assert!(entries > 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejects_plain_http_and_reports_unreachable_servers() {
    assert!(matches!(Api::new("http://collab.example.com"), Err(ClientError::Address(_))));
    let api = Api::new("http://127.0.0.1:9").unwrap();
    assert!(matches!(api.info().await, Err(ClientError::Unreachable(_))));
}
