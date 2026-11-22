//! The agent side of the gateway protocol (`serve --trust-gateway`): signed
//! `/internal/v1` requests, SSE ids, resume via body, drain, orphans, and
//! gap-free reconnects with `Last-Event-ID`.
#![cfg(feature = "trust-gateway")]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use graphrt_cli::gateway_agent::signature::{self, GatewayKey, Signed, Verifier};
use graphrt_cli::gateway_agent::{GatewayAgent, GatewayAgentConfig, client::GatewayAddress};
use graphrt_cli::serve::{AppState, build_state, discover, router};
use graphrt_cli::setup::ServiceOptions;
use graphrt_core::engine::Limits;
use http_body_util::BodyExt;
use serde_json::{Value as Json, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tower::ServiceExt;

const SECRET: [u8; 32] = [0x42; 32];
const DEPLOYMENT: &str = "shop";

/// The approval example plus a linear workflow with three answer nodes.
fn workflows_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("graphrt-gw-protocol-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let examples = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples");
    std::fs::copy(examples.join("approval.yaml"), dir.join("approval.yaml")).unwrap();
    std::fs::write(
        dir.join("linear.yaml"),
        r#"version: "1"
name: linear
graph:
  nodes:
    - { id: start, type: start, variables: [] }
    - { id: a, type: answer, answer: "one " }
    - { id: b, type: answer, answer: "two " }
    - { id: c, type: answer, answer: "three" }
  edges:
    - { source: start, target: a }
    - { source: a, target: b }
    - { source: b, target: c }
"#,
    )
    .unwrap();
    dir
}

fn key() -> GatewayKey {
    GatewayKey::new("k1", &SECRET).unwrap()
}

fn config(gateway: Option<GatewayAddress>, grace: Duration) -> GatewayAgentConfig {
    GatewayAgentConfig {
        deployment: DEPLOYMENT.to_owned(),
        keys: vec![key()],
        gateway,
        reclaim_grace: grace,
        reattach_window: Duration::from_millis(50),
        max_body_bytes: 1 << 20,
    }
}

async fn state(tag: &str, agent: Option<GatewayAgentConfig>) -> Arc<AppState> {
    let files = discover(&workflows_dir(tag)).expect("discover");
    let options = ServiceOptions {
        plugins: graphrt_cli::extensions::PluginOptions { dirs: Vec::new(), no_default: true, trusted: Vec::new() },
        ..ServiceOptions::default()
    };
    let state = build_state(&files, &options, Limits::default(), None).await.expect("state");
    Arc::new(state.with_gateway_agent(agent.map(|config| Arc::new(GatewayAgent::new(config)))))
}

async fn app(tag: &str) -> axum::Router {
    router(state(tag, Some(config(None, Duration::from_secs(60)))).await, 1 << 20)
}

/// A request signed for `deployment` like a gateway would send it.
fn signed_for(deployment: &str, method: &str, uri: &str, run: Option<(&str, u64)>, body: &[u8]) -> Request<Body> {
    let epoch = run.map(|(_, epoch)| epoch.to_string()).unwrap_or_default();
    let run_id = run.map_or("", |(id, _)| id);
    let parts = Signed { deployment, method, path: uri, run_id, epoch: &epoch, body };
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .header("graphrt-protocol", "1")
        .header("graphrt-deployment", deployment)
        .header("graphrt-sig", signature::sign(&key(), signature::unix_now(), &parts));
    if let Some((id, epoch)) = run {
        builder = builder.header("graphrt-run-id", id).header("graphrt-epoch", epoch.to_string());
    }
    builder.body(Body::from(body.to_vec())).unwrap()
}

fn signed(method: &str, uri: &str, run: Option<(&str, u64)>, body: &Json) -> Request<Body> {
    let bytes = if body.is_null() { Vec::new() } else { body.to_string().into_bytes() };
    signed_for(DEPLOYMENT, method, uri, run, &bytes)
}

async fn send(app: &axum::Router, req: Request<Body>) -> (StatusCode, axum::http::HeaderMap, String) {
    let res = app.clone().oneshot(req).await.expect("response");
    let (status, headers) = (res.status(), res.headers().clone());
    let body = res.into_body().collect().await.expect("body").to_bytes();
    (status, headers, String::from_utf8_lossy(&body).into_owned())
}

/// One SSE message.
#[derive(Clone, Debug, PartialEq)]
struct Sse {
    id: Option<String>,
    event: String,
    data: String,
}

fn parse_sse(text: &str) -> Vec<Sse> {
    text.split("\n\n")
        .filter_map(|block| {
            let mut item = Sse { id: None, event: String::new(), data: String::new() };
            for line in block.lines() {
                if let Some(v) = line.strip_prefix("id:") {
                    item.id = Some(v.trim().to_owned());
                } else if let Some(v) = line.strip_prefix("event:") {
                    item.event = v.trim().to_owned();
                } else if let Some(v) = line.strip_prefix("data:") {
                    item.data.push_str(v.trim_start());
                }
            }
            (!item.event.is_empty()).then_some(item)
        })
        .collect()
}

fn engine_ids(items: &[Sse]) -> Vec<String> {
    items.iter().filter(|i| i.event != "response" && i.event != "hello").filter_map(|i| i.id.clone()).collect()
}

#[tokio::test]
async fn internal_requests_need_a_valid_signature_for_this_deployment() {
    let app = app("auth").await;
    let (status, headers, body) = send(&app, signed("GET", "/internal/v1/hello", None, &Json::Null)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(headers["graphrt-protocol"], "1");
    let hello: Json = serde_json::from_str(&body).unwrap();
    assert_eq!((hello["protocol"].as_u64(), hello["deployment"].as_str()), (Some(1), Some(DEPLOYMENT)));
    assert_eq!(hello["draining"], false);
    let names: Vec<&str> = hello["workflows"].as_array().unwrap().iter().filter_map(|w| w["name"].as_str()).collect();
    assert!(names.contains(&"linear") && names.contains(&"approval"), "{hello}");
    assert!(hello["workflows"][0]["fingerprint"].as_str().is_some_and(|f| !f.is_empty()));

    // The very same signed request again: a replay.
    let request = signed("GET", "/internal/v1/hello", None, &Json::Null);
    let replay = Request::builder()
        .uri("/internal/v1/hello")
        .header("graphrt-protocol", "1")
        .header("graphrt-deployment", DEPLOYMENT)
        .header("graphrt-sig", request.headers()["graphrt-sig"].clone())
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(&app, request).await.0, StatusCode::OK);
    let (status, _, body) = send(&app, replay).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    assert!(body.contains("already used"), "{body}");

    let unsigned = Request::get("/internal/v1/hello").header("graphrt-protocol", "1").body(Body::empty()).unwrap();
    let (status, headers, _) = send(&app, unsigned).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(headers["graphrt-protocol"], "1", "refusals carry the protocol too");

    let mut old_protocol = signed("GET", "/internal/v1/hello", None, &Json::Null);
    old_protocol.headers_mut().insert("graphrt-protocol", "0".parse().unwrap());
    let (status, _, body) = send(&app, old_protocol).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("unsupported_protocol"), "{body}");

    let other = signed_for("other", "GET", "/internal/v1/hello", None, b"");
    let (status, _, body) = send(&app, other).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");

    // A server that trusts no gateway has no internal API.
    let public = router(state("auth-public", None).await, 1 << 20);
    assert_eq!(send(&public, signed("GET", "/internal/v1/hello", None, &Json::Null)).await.0, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_gateway_run_streams_hello_global_ids_and_response_text_and_is_never_executed_twice() {
    let app = app("start").await;
    let body = json!({ "workflow": "linear", "event_index": 10 });
    let (status, _, text) = send(&app, signed("POST", "/internal/v1/runs", Some(("gw-run-1", 1)), &body)).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let items = parse_sse(&text);
    let hello: Json = serde_json::from_str(&items[0].data).unwrap();
    assert_eq!((items[0].event.as_str(), &items[0].id), ("hello", &None));
    assert_eq!(hello, json!({ "run_id": "gw-run-1", "epoch": 1, "protocol": 1 }));
    // Engine events count on from the gateway's event index, without gaps.
    let ids = engine_ids(&items);
    let expected: Vec<String> = (10..10 + ids.len() as u64).map(|i| i.to_string()).collect();
    assert_eq!(ids, expected);
    assert_eq!(items.last().unwrap().event, "run_succeeded");
    // Response text is `<event>.<k>`, sent just before the event it came from.
    let responses: Vec<(usize, &Sse)> = items.iter().enumerate().filter(|(_, i)| i.event == "response").collect();
    assert!(responses.len() >= 3, "{items:?}");
    for (at, response) in responses {
        let (index, chunk) = response.id.as_deref().unwrap().split_once('.').unwrap();
        let source = items[at + 1..].iter().find(|i| i.event != "response").unwrap();
        assert_eq!(source.id.as_deref(), Some(index));
        assert!(chunk.parse::<usize>().is_ok());
    }
    let answer: String = items
        .iter()
        .filter(|i| i.event == "response")
        .map(|i| serde_json::from_str::<Json>(&i.data).unwrap()["text"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(answer, "one two three");

    // A retried start of the same epoch replays the execution instead of running it again.
    let (status, _, again) = send(&app, signed("POST", "/internal/v1/runs", Some(("gw-run-1", 1)), &body)).await;
    assert_eq!(status, StatusCode::OK, "{again}");
    assert_eq!(engine_ids(&parse_sse(&again)), ids);
    let (_, _, record) = send(&app, Request::get("/v1/runs/gw-run-1").body(Body::empty()).unwrap()).await;
    let record: Json = serde_json::from_str(&record).unwrap();
    assert_eq!(record["event_count"].as_u64(), Some(ids.len() as u64), "executed once: {record}");
    assert_eq!((record["epoch"].as_u64(), record["deployment"].as_str()), (Some(1), Some(DEPLOYMENT)));

    // An older epoch is refused.
    let (status, _, text) = send(&app, signed("POST", "/internal/v1/runs", Some(("gw-run-1", 0)), &body)).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(text.contains("stale_epoch"), "{text}");

    // Without run headers, or for an unknown workflow / another version.
    let (status, _, _) = send(&app, signed("POST", "/internal/v1/runs", None, &body)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let unknown = json!({ "workflow": "nope" });
    assert_eq!(
        send(&app, signed("POST", "/internal/v1/runs", Some(("r2", 1)), &unknown)).await.0,
        StatusCode::NOT_FOUND
    );
    let changed = json!({ "workflow": "linear", "fingerprint": "something-else" });
    let (status, _, text) = send(&app, signed("POST", "/internal/v1/runs", Some(("r3", 1)), &changed)).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(text.contains("fingerprint_mismatch"), "{text}");
}

/// Start `approval` at epoch 1, then resume it at epoch 2 with the snapshot
/// inline (how a gateway moves a paused run to another replica).
#[tokio::test]
async fn a_paused_run_resumes_from_the_snapshot_in_the_body_with_continuing_ids() {
    let app = app("resume").await;
    let start = json!({ "workflow": "approval", "inputs": { "amount": 50, "reason": "Lunch" } });
    let (status, _, text) = send(&app, signed("POST", "/internal/v1/runs", Some(("gw-pay", 1)), &start)).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let first = parse_sse(&text);
    let paused = first.last().unwrap();
    assert_eq!(paused.event, "run_paused");
    let snapshot = serde_json::from_str::<Json>(&paused.data).unwrap()["snapshot"].clone();
    assert!(snapshot.is_object(), "the live run_paused carries the snapshot");
    let count = engine_ids(&first).len() as u64;

    let resume = json!({
        "workflow": "approval",
        "event_index": count,
        "resume": { "snapshot": snapshot, "inputs": { "review": { "action": "approve", "values": { "note": "ok" } } } },
    });
    let (status, _, text) = send(&app, signed("POST", "/internal/v1/runs", Some(("gw-pay", 2)), &resume)).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let second = parse_sse(&text);
    let ids = engine_ids(&second);
    assert_eq!(ids.first().map(String::as_str), Some(count.to_string().as_str()), "ids continue the run's log");
    assert_eq!(second.last().unwrap().event, "run_succeeded", "{second:?}");

    let wrong_run = json!({ "workflow": "approval", "resume": { "snapshot": snapshot } });
    assert_eq!(
        send(&app, signed("POST", "/internal/v1/runs", Some(("other", 1)), &wrong_run)).await.0,
        StatusCode::BAD_REQUEST
    );
}

async fn wait_settled(app: &axum::Router, run_id: &str) -> Json {
    for _ in 0..500 {
        let (_, _, body) = send(app, Request::get(format!("/v1/runs/{run_id}")).body(Body::empty()).unwrap()).await;
        let run: Json = serde_json::from_str(&body).unwrap();
        if run["status"] != "running" && run["active"] == false {
            return run;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("run {run_id} did not settle");
}

async fn events_after(app: &axum::Router, run_id: &str, last_event_id: Option<&str>) -> Vec<Sse> {
    let mut request = Request::get(format!("/v1/runs/{run_id}/events"));
    if let Some(last) = last_event_id {
        request = request.header("last-event-id", last);
    }
    let (status, _, body) = send(app, request.body(Body::empty()).unwrap()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    parse_sse(&body)
}

/// Property (I5 on one server): whatever event a client last saw, reconnecting
/// with `Last-Event-ID` delivers exactly the rest of the log, in order, across
/// the run's executions (start, then resume).
#[tokio::test]
async fn reconnecting_after_any_event_continues_without_gaps_or_duplicates() {
    let app = router(state("reconnect", None).await, 1 << 20);
    let started = json!({ "inputs": { "amount": 50, "reason": "Lunch" }, "async": true });
    let request = Request::post("/v1/workflows/approval/runs")
        .header("content-type", "application/json")
        .body(Body::from(started.to_string()))
        .unwrap();
    let (status, _, body) = send(&app, request).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    let run_id = serde_json::from_str::<Json>(&body).unwrap()["run_id"].as_str().unwrap().to_owned();
    assert_eq!(wait_settled(&app, &run_id).await["status"], "paused");
    let resume = json!({ "inputs": { "review": { "action": "approve", "values": { "note": "ok" } } } });
    let request = Request::post(format!("/v1/runs/{run_id}/resume"))
        .header("content-type", "application/json")
        .body(Body::from(resume.to_string()))
        .unwrap();
    assert_eq!(send(&app, request).await.0, StatusCode::OK);

    let full = events_after(&app, &run_id, None).await;
    let ids: Vec<String> = full.iter().filter_map(|i| i.id.clone()).collect();
    let expected: Vec<String> = (0..full.len()).map(|i| i.to_string()).collect();
    assert_eq!(ids, expected, "every event has its log index as id, across the resume");
    assert!(full.iter().any(|i| i.event == "run_paused") && full.last().unwrap().event == "run_succeeded");
    for (at, last) in ids.iter().enumerate() {
        let rest = events_after(&app, &run_id, Some(last)).await;
        assert_eq!(rest, full[at + 1..], "reconnect after id {last}");
    }
    // A client that saw part of an event's response text resumes at that event.
    let rest = events_after(&app, &run_id, Some("3.0")).await;
    assert_eq!(rest, full[3..]);
    // Garbage ids are ignored (the whole log).
    assert_eq!(events_after(&app, &run_id, Some("nonsense")).await, full);
}

#[tokio::test]
async fn a_gateway_drain_refuses_new_runs() {
    let app = app("drain").await;
    let (status, _, body) =
        send(&app, signed("POST", "/internal/v1/drain", None, &json!({ "deadline_ms": 500 }))).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    let (_, _, hello) = send(&app, signed("GET", "/internal/v1/hello", None, &Json::Null)).await;
    assert_eq!(serde_json::from_str::<Json>(&hello).unwrap()["draining"], true);
    let run = json!({ "workflow": "linear" });
    let (status, _, body) = send(&app, signed("POST", "/internal/v1/runs", Some(("late", 1)), &run)).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert!(body.contains("draining"), "{body}");
}

/// A minimal gateway that answers one `POST /internal/v1/orphans/{id}`.
async fn fake_gateway() -> (GatewayAddress, tokio::task::JoinHandle<(String, Vec<(String, String)>, Vec<u8>)>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = GatewayAddress::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut received = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            let n = socket.read(&mut chunk).await.unwrap();
            received.extend_from_slice(&chunk[..n]);
            let text = String::from_utf8_lossy(&received).into_owned();
            if let Some(end) = text.find("\r\n\r\n") {
                let length = text[..end]
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length: "))
                    .and_then(|v| v.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                if received.len() >= end + 4 + length {
                    let head = text[..end].to_owned();
                    let body = received[end + 4..end + 4 + length].to_vec();
                    socket.write_all(b"HTTP/1.1 202 Accepted\r\ncontent-length: 0\r\n\r\n").await.unwrap();
                    let mut lines = head.lines();
                    let request_line = lines.next().unwrap().to_owned();
                    let headers =
                        lines.filter_map(|l| l.split_once(": ").map(|(k, v)| (k.to_owned(), v.to_owned()))).collect();
                    return (request_line, headers, body);
                }
            }
        }
    });
    (address, server)
}

#[tokio::test]
async fn a_lost_gateway_stream_becomes_an_orphan_reported_to_the_gateway() {
    let (address, gateway) = fake_gateway().await;
    let app = router(state("orphan-report", Some(config(Some(address), Duration::from_secs(5)))).await, 1 << 20);
    let start = json!({ "workflow": "approval", "inputs": { "amount": 5, "reason": "Tea" } });
    let response =
        app.clone().oneshot(signed("POST", "/internal/v1/runs", Some(("gw-lost", 3)), &start)).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    // The gateway goes away before reading the stream.
    drop(response);

    let (request_line, headers, body) = tokio::time::timeout(Duration::from_secs(10), gateway).await.unwrap().unwrap();
    assert_eq!(request_line, "POST /internal/v1/orphans/gw-lost HTTP/1.1");
    let header = |name: &str| headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str()).unwrap_or("");
    assert_eq!((header("graphrt-run-id"), header("graphrt-epoch"), header("graphrt-protocol")), ("gw-lost", "3", "1"));
    // Signed both ways with the same scheme: the gateway can verify it.
    let parts = Signed {
        deployment: DEPLOYMENT,
        method: "POST",
        path: "/internal/v1/orphans/gw-lost",
        run_id: "gw-lost",
        epoch: "3",
        body: &body,
    };
    let verifier = Verifier::new(vec![key()]);
    assert_eq!(verifier.verify(Some(header("graphrt-sig")), &parts, signature::unix_now()), Ok(()));
    let orphan: Json = serde_json::from_slice(&body).unwrap();
    assert_eq!((orphan["status"].as_str(), orphan["epoch"].as_u64()), (Some("paused"), Some(3)));
    assert_eq!(orphan["terminal"]["type"], "run_paused");
    assert!(orphan["terminal"]["snapshot"].is_object(), "the snapshot travels with the orphan: {orphan}");
    assert!(orphan["event_count"].as_u64().unwrap() > 0);

    // Accepted by the gateway: no longer held.
    tokio::time::sleep(Duration::from_millis(50)).await;
    let (_, _, list) = send(&app, signed("GET", "/internal/v1/orphans", None, &Json::Null)).await;
    assert_eq!(serde_json::from_str::<Json>(&list).unwrap()["orphans"], json!([]));
}

#[tokio::test]
async fn orphans_are_listed_for_reclaim_and_lost_after_the_grace() {
    let app = router(state("orphan-list", Some(config(None, Duration::from_millis(400)))).await, 1 << 20);
    let start = json!({ "workflow": "approval", "inputs": { "amount": 5, "reason": "Tea" } });
    for run_id in ["kept", "lost"] {
        let response =
            app.clone().oneshot(signed("POST", "/internal/v1/runs", Some((run_id, 1)), &start)).await.unwrap();
        drop(response);
    }
    let mut listed = Json::Null;
    for _ in 0..100 {
        let (_, _, list) = send(&app, signed("GET", "/internal/v1/orphans", None, &Json::Null)).await;
        listed = serde_json::from_str::<Json>(&list).unwrap()["orphans"].clone();
        if listed.as_array().is_some_and(|l| l.len() == 2) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let ids: Vec<&str> = listed.as_array().unwrap().iter().filter_map(|o| o["run_id"].as_str()).collect();
    assert!(ids.contains(&"kept") && ids.contains(&"lost"), "{listed}");

    // The gateway reclaims one of them.
    assert_eq!(
        send(&app, signed("DELETE", "/internal/v1/orphans/kept", Some(("kept", 1)), &Json::Null)).await.0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        send(&app, signed("DELETE", "/internal/v1/orphans/kept", Some(("kept", 1)), &Json::Null)).await.0,
        StatusCode::NOT_FOUND
    );

    // The other one is lost once the grace ran out.
    tokio::time::sleep(Duration::from_millis(700)).await;
    let (_, _, list) = send(&app, signed("GET", "/internal/v1/orphans", None, &Json::Null)).await;
    assert_eq!(serde_json::from_str::<Json>(&list).unwrap()["orphans"], json!([]));
    let (_, _, lost) = send(&app, Request::get("/v1/runs/lost").body(Body::empty()).unwrap()).await;
    let lost: Json = serde_json::from_str(&lost).unwrap();
    assert_eq!((lost["status"].as_str(), lost["error_type"].as_str()), (Some("failed"), Some("owner_lost")), "{lost}");
    let (_, _, kept) = send(&app, Request::get("/v1/runs/kept").body(Body::empty()).unwrap()).await;
    assert_eq!(serde_json::from_str::<Json>(&kept).unwrap()["status"], "paused", "a reclaimed run is left alone");
}
