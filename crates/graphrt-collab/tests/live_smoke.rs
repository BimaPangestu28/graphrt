//! A smoke test of a deployed server (deploy/collab/hosted/README.md): two clients
//! co-edit, an outsider is refused, the free plan's quotas hold. It runs only with
//! GRAPHRT_COLLAB_LIVE_URL, GRAPHRT_COLLAB_LIVE_OWNER and GRAPHRT_COLLAB_LIVE_OUTSIDER
//! (owner invite codes of two workspaces) set, so it does nothing in CI.
#![cfg(all(feature = "server", feature = "client"))]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic, clippy::indexing_slicing, clippy::print_stdout)]

use std::sync::Arc;
use std::time::Duration;

use graphrt_collab::ProjectDoc;
use graphrt_collab::client::{Api, ClientError, LiveEvent, LiveOptions, LiveProject, TokenSource};
use serde_json::{Value, json};
use tokio::sync::mpsc::UnboundedReceiver;

/// A person signed in on the server.
#[derive(Clone)]
struct Person {
    api: Api,
    access: String,
}

async fn join(api: &Api, code: &str, name: &str) -> (Person, Value) {
    let redeemed = api.redeem_invite(code, None, Some(name), "e2e").await.unwrap();
    let person = Person { api: api.clone(), access: redeemed.access_token.clone().unwrap() };
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
async fn live_smoke() {
    let Ok(url) = std::env::var("GRAPHRT_COLLAB_LIVE_URL") else { return };
    let owner_code = std::env::var("GRAPHRT_COLLAB_LIVE_OWNER").unwrap();
    let outsider_code = std::env::var("GRAPHRT_COLLAB_LIVE_OUTSIDER").unwrap();
    let api = Api::new(&url).unwrap();
    let info = api.info().await.unwrap();
    println!("[info] {} {} mode={} invite_codes={}", info.server, info.version, info.mode, info.sign_in.invite_codes);

    let (ana, workspace) = join(&api, &owner_code, "Ana (smoke)").await;
    let ws = workspace["id"].as_str().unwrap().to_owned();
    println!("[ok] Ana joined {} as owner", workspace["name"]);
    let created = ana
        .api
        .post(&ana.access, &format!("/v1/workspaces/{ws}/projects"), &json!({"name": "Support bot", "content": flow()}))
        .await
        .unwrap();
    let project = created["id"].as_str().unwrap().to_owned();
    let invite = ana
        .api
        .post(&ana.access, &format!("/v1/workspaces/{ws}/invites"), &json!({"role": "operator", "max_uses": 10}))
        .await
        .unwrap();
    let link = invite["link"].as_str().unwrap().to_owned();
    println!("[ok] invite link starts {}", &link[..link.find('#').unwrap() + 1]);
    let (budi, _) = join(&api, &link, "Budi (smoke)").await;
    println!("[ok] Budi joined with the invite link as operator");

    // Two clients co-edit.
    let (a, mut a_events) = live(&ana, &ws, &project, ProjectDoc::new());
    let (b, mut b_events) = live(&budi, &ws, &project, ProjectDoc::new());
    wait_for(&mut a_events, "Ana synced", |e| matches!(e, LiveEvent::Synced(_))).await;
    wait_for(&mut b_events, "Budi synced", |e| matches!(e, LiveEvent::Synced(v) if v["name"] == "Support bot")).await;
    wait_json(&a, "Ana content", |v| v["name"] == "Support bot").await;
    let mut on_a = a.json();
    on_a["flow"]["graph"]["nodes"][1]["prompt"] = json!("Politely, you answer support questions.");
    let mut on_b = b.json();
    on_b["flow"]["graph"]["nodes"][1]["prompt"] = json!("You answer support questions in Indonesian.");
    on_b["flow"]["graph"]["nodes"].as_array_mut().unwrap().insert(2, json!({"id": "notify", "type": "http"}));
    let (ra, rb) = tokio::join!(a.edit(object(on_a)), b.edit(object(on_b)));
    assert!(ra.unwrap() && rb.unwrap());
    let merged = "Politely, you answer support questions in Indonesian.";
    let converged = |v: &Value| {
        v["flow"]["graph"]["nodes"][1]["prompt"] == merged && v["flow"]["graph"]["nodes"][2]["id"] == "notify"
    };
    wait_json(&a, "Ana has both edits", converged).await;
    wait_json(&b, "Budi has both edits", converged).await;
    assert_eq!(a.json(), b.json());
    println!(
        "[ok] concurrent edits merged on both clients: prompt = {:?}",
        a.json()["flow"]["graph"]["nodes"][1]["prompt"]
    );
    tokio::time::sleep(Duration::from_millis(500)).await;
    let stored = ana.api.get(&ana.access, &format!("/v1/workspaces/{ws}/projects/{project}")).await.unwrap();
    println!(
        "[ok] server stored the merged project: {}",
        stored["content"]["flow"]["graph"]["nodes"][1]["prompt"] == merged
    );

    // Outsider.
    let (eve, eve_ws) = join(&api, &outsider_code, "Eve (smoke outsider)").await;
    for path in [
        format!("/v1/workspaces/{ws}"),
        format!("/v1/workspaces/{ws}/projects"),
        format!("/v1/workspaces/{ws}/projects/{project}"),
    ] {
        let refused = eve.api.get(&eve.access, &path).await.unwrap_err();
        println!("[ok] outsider GET {path} -> {}", status(&refused));
        assert!(matches!(status(&refused), 403 | 404));
    }
    let refused = eve.api.scoped_token(&eve.access, &ws).await.unwrap_err();
    println!("[ok] outsider workspace token -> {}", status(&refused));
    let eve_ws_id = eve_ws["id"].as_str().unwrap().to_owned();
    let (eve_scoped, _) = eve.api.scoped_token(&eve.access, &eve_ws_id).await.unwrap();
    let foreign: TokenSource = Arc::new(move || {
        let t = eve_scoped.clone();
        Box::pin(async move { Ok(t) })
    });
    let (_e, mut e_events) = LiveProject::start(
        ProjectDoc::new(),
        LiveOptions {
            url: api.sync_url(&ws, &project),
            token: foreign,
            renew_every: Duration::from_secs(240),
            max_backoff: Duration::from_secs(2),
        },
    );
    let gone = wait_for(&mut e_events, "outsider refused", |e| matches!(e, LiveEvent::Disconnected { .. })).await;
    println!("[ok] outsider sync session with another workspace's token: {gone:?}");
    assert!(!e_events.try_recv().is_ok_and(|e| matches!(e, LiveEvent::Synced(_))));

    // Quotas: 2 workspaces per account.
    let mut made = Vec::new();
    for n in 1..=3 {
        match ana.api.post(&ana.access, "/v1/workspaces", &json!({"name": format!("Smoke extra {n}")})).await {
            Ok(w) => {
                println!("[ok] workspace {n} created");
                made.push(w["id"].as_str().unwrap().to_owned());
            }
            Err(e) => println!("[ok] workspace {n} refused: {e}"),
        }
    }
    assert_eq!(made.len(), 2, "workspaces_per_account is 2");
    // 10 projects per workspace (one exists).
    let mut refused_project = None;
    for n in 2..=11 {
        if let Err(e) =
            ana.api.post(&ana.access, &format!("/v1/workspaces/{ws}/projects"), &json!({"name": format!("p{n}")})).await
        {
            refused_project = Some((n, e.to_string()));
            break;
        }
    }
    println!("[ok] project quota: {refused_project:?}");
    assert_eq!(refused_project.as_ref().map(|r| r.0), Some(11));
    // 5 members per workspace (Ana, Budi + 3).
    let mut joined = 2;
    let mut refused_member = None;
    for n in 3..=6 {
        match api.redeem_invite(&link, None, Some(&format!("Member {n} (smoke)")), "e2e").await {
            Ok(_) => joined += 1,
            Err(e) => {
                refused_member = Some((n, e.to_string()));
                break;
            }
        }
    }
    println!("[ok] member quota: {joined} members, refused {refused_member:?}");
    assert_eq!(joined, 5);
    // 2 MiB per project document (a sync message is at most 512 KiB).
    let mut refusal = None;
    for n in 1..=8 {
        let mut big = a.json();
        big[format!("notes{n}")] = json!("x".repeat(400 * 1024));
        let _ = a.edit(object(big)).await;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        while let Ok(Some(event)) = tokio::time::timeout_at(deadline, a_events.recv()).await {
            if let LiveEvent::Refused(why) = &event {
                refusal = Some((n, why.clone()));
                break;
            }
            if matches!(event, LiveEvent::Disconnected { .. }) {
                refusal = Some((n, format!("{event:?}")));
                break;
            }
        }
        if refusal.is_some() {
            break;
        }
    }
    println!("[ok] document size quota: {refusal:?}");
    assert!(refusal.is_some_and(|r| r.0 <= 6));
    a.close();
    b.close();
}
