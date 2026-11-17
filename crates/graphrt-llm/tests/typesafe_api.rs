//! TypeSafe System One integration: the client, the classifier engine and the
//! `system-one` node, against an in-test HTTP server returning documented responses.

#![cfg(feature = "typesafe")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use futures::StreamExt;
use futures::future::BoxFuture;
use graphrt_core::Workflow;
use graphrt_core::engine::{Engine, RunInput};
use graphrt_core::event::Event;
use graphrt_core::graph::Graph;
use graphrt_core::node::{NoTimerClock, Registry, Services};
use graphrt_core::value::{File, FileKind, FileTransfer, Value};
use graphrt_llm::LlmError;
use graphrt_llm::credentials::{Credential, CredentialSource};
use graphrt_llm::typesafe::{Question, SystemOneClient, SystemOneConfig, SystemOneRequest};
use indexmap::IndexMap;
use serde_json::{Value as Json, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

// --- in-test HTTP server ---------------------------------------------------------------

#[derive(Clone, Debug)]
struct Recorded {
    path: String,
    headers: HashMap<String, String>,
    body: Json,
}

struct Reply {
    status: u16,
    headers: Vec<(&'static str, &'static str)>,
    body: String,
}

fn ok(body: Json) -> Reply {
    Reply { status: 200, headers: vec![], body: body.to_string() }
}

#[derive(Clone)]
struct Server {
    base: String,
    requests: Arc<Mutex<Vec<Recorded>>>,
}

impl Server {
    fn requests(&self) -> Vec<Recorded> {
        self.requests.lock().unwrap().clone()
    }
}

async fn serve(replies: Vec<Reply>) -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let requests = Arc::new(Mutex::new(Vec::new()));
    let queue = Arc::new(Mutex::new(VecDeque::from(replies)));
    let recorded = requests.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else { return };
            let recorded = recorded.clone();
            let queue = queue.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 8192];
                let header_end = loop {
                    let n = socket.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        break pos + 4;
                    }
                };
                let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
                let mut lines = head.split("\r\n");
                let path = lines.next().unwrap_or_default().split(' ').nth(1).unwrap_or_default().to_owned();
                let headers: HashMap<String, String> = lines
                    .filter_map(|l| l.split_once(':'))
                    .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_owned()))
                    .collect();
                let len: usize = headers.get("content-length").and_then(|v| v.parse().ok()).unwrap_or(0);
                while buf.len() < header_end + len {
                    let n = socket.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                }
                let body = serde_json::from_slice(&buf[header_end..]).unwrap_or(Json::Null);
                recorded.lock().unwrap().push(Recorded { path, headers, body });
                let reply = queue.lock().unwrap().pop_front().unwrap_or(Reply {
                    status: 500,
                    headers: vec![],
                    body: "{\"detail\":\"no scripted reply\"}".into(),
                });
                let mut out = format!(
                    "HTTP/1.1 {} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n",
                    reply.status,
                    reply.body.len()
                );
                for (k, v) in &reply.headers {
                    out.push_str(&format!("{k}: {v}\r\n"));
                }
                out.push_str("\r\n");
                out.push_str(&reply.body);
                let _ = socket.write_all(out.as_bytes()).await;
                let _ = socket.shutdown().await;
            });
        }
    });
    Server { base, requests }
}

fn client(server: &Server) -> Arc<SystemOneClient> {
    Arc::new(SystemOneClient::new(SystemOneConfig::with_api_key("ts-key").base_url(&server.base)).unwrap())
}

// --- engine helpers -----------------------------------------------------------------------

fn graph(doc: Json) -> Arc<Graph> {
    let mut registry = Registry::with_builtins();
    graphrt_llm::register(&mut registry);
    Arc::new(Graph::build(Workflow::from_value(doc).unwrap(), &registry).unwrap())
}

async fn run(graph: Arc<Graph>, client: Arc<SystemOneClient>, input: RunInput) -> Vec<Event> {
    let engine = Engine::new(graph).with_services(Services::new().with(client)).with_clock(Arc::new(NoTimerClock));
    engine.run(input).collect().await
}

fn outputs(events: &[Event]) -> IndexMap<String, Value> {
    match events.last() {
        Some(Event::RunSucceeded { outputs, .. }) => outputs.clone(),
        other => panic!("expected RunSucceeded, got {other:?}"),
    }
}

fn node_outputs(events: &[Event], id: &str) -> IndexMap<String, Value> {
    events
        .iter()
        .find_map(|e| match e {
            Event::NodeSucceeded { node_id, outputs, .. } if node_id == id => Some(outputs.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("node `{id}` did not succeed: {events:?}"))
}

fn node_metadata(events: &[Event], id: &str) -> IndexMap<String, Value> {
    events
        .iter()
        .find_map(|e| match e {
            Event::NodeSucceeded { node_id, metadata, .. } if node_id == id => Some(metadata.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

fn choice_reply(choice: &str, confidence: f64, probabilities: Json) -> Reply {
    ok(json!({
        "model": "jev-1.14.0",
        "answers": {"category": {"type": "choice", "choice": choice, "confidence": confidence, "probabilities": probabilities}},
        "usage": {"input_tokens": 120, "output_tokens": 12}
    }))
}

fn classifier_doc(extra: Json) -> Json {
    let mut classifier = json!({
        "id": "qc", "type": "question-classifier",
        "model": {"provider": "typesafe", "name": "jev-latest"},
        "query_variable_selector": ["start", "text"],
        "instruction": "Support tickets for a SaaS product.",
        "classes": [
            {"id": "billing", "name": "Billing", "description": "Payments, refunds, invoices"},
            {"id": "bug", "name": "Bug report"},
            {"id": "other", "name": "Other"}
        ]
    });
    if let (Some(c), Json::Object(e)) = (classifier.as_object_mut(), extra) {
        c.extend(e);
    }
    json!({"graph": {"nodes": [
        {"id": "start", "type": "start", "variables": [{"variable": "text", "type": "paragraph", "required": true}]},
        classifier,
        {"id": "a_billing", "type": "answer", "answer": "billing route"},
        {"id": "a_bug", "type": "answer", "answer": "bug route"},
        {"id": "a_other", "type": "answer", "answer": "other route"}
    ], "edges": [
        {"source": "start", "target": "qc"},
        {"source": "qc", "target": "a_billing", "sourceHandle": "billing"},
        {"source": "qc", "target": "a_bug", "sourceHandle": "bug"},
        {"source": "qc", "target": "a_other", "sourceHandle": "other"}
    ]}})
}

// --- client --------------------------------------------------------------------------------

#[tokio::test]
async fn client_sends_the_documented_request() {
    let server = serve(vec![ok(json!({
        "model": "jev-latest",
        "answers": {"tone": {"type": "choice", "choice": "calm", "confidence": 0.7,
                             "probabilities": {"angry": 0.15, "calm": 0.8, "excited": 0.05}}},
        "usage": {"input_tokens": 120, "output_tokens": 12}
    }))])
    .await;
    let mut criteria = IndexMap::new();
    criteria.insert("angry".to_owned(), json!("An upset or hostile message"));
    criteria.insert("calm".to_owned(), json!("A neutral or polite message"));
    criteria.insert("excited".to_owned(), json!("An enthusiastic or eager message"));
    let request = SystemOneRequest::new(
        "jev-latest",
        json!("I was charged twice for my subscription this month. Please refund the extra charge."),
    )
    .question("tone", Question::choice("What is the tone of this message?", criteria));
    let response = client(&server).ask(&request).await.unwrap();
    assert_eq!(response.answer("tone").and_then(|a| a.choice_probability()), Some(0.8));

    let reqs = server.requests();
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].path, "/v1/systemone");
    assert_eq!(reqs[0].headers["authorization"], "Bearer ts-key");
    assert!(!reqs[0].headers.contains_key("typesafe-danger-zone-images"));
    assert_eq!(
        reqs[0].body,
        json!({
            "model": "jev-latest",
            "state": "I was charged twice for my subscription this month. Please refund the extra charge.",
            "questions": {"tone": {"type": "choice", "instructions": "What is the tone of this message?",
                "criteria": {"angry": "An upset or hostile message", "calm": "A neutral or polite message",
                             "excited": "An enthusiastic or eager message"}}}
        })
    );
}

#[derive(Debug, Default)]
struct Rotating {
    invalidations: AtomicU32,
    fetches: AtomicU32,
}

impl CredentialSource for Rotating {
    fn credential<'a>(&'a self, provider: &'a str) -> BoxFuture<'a, Result<Credential, LlmError>> {
        assert_eq!(provider, "typesafe");
        self.fetches.fetch_add(1, Ordering::SeqCst);
        let n = self.invalidations.load(Ordering::SeqCst);
        Box::pin(futures::future::ready(Ok(Credential::new(format!("token-{n}")).header("x-app-user", "u1"))))
    }

    fn invalidate(&self, _provider: &str) {
        self.invalidations.fetch_add(1, Ordering::SeqCst);
    }
}

#[tokio::test]
async fn rejected_credential_is_refreshed_once() {
    let noul = json!({"model": "jev-latest", "answers": {"billing": {"type": "noul", "noul": 0.98}},
                      "usage": {"input_tokens": 1, "output_tokens": 1}});
    let server = serve(vec![
        Reply { status: 401, headers: vec![], body: "{\"detail\":\"expired\"}".into() },
        ok(noul),
        Reply { status: 401, headers: vec![], body: "{\"detail\":\"bad\"}".into() },
        Reply { status: 401, headers: vec![], body: "{\"detail\":\"still bad\"}".into() },
    ])
    .await;
    let source = Arc::new(Rotating::default());
    let client =
        SystemOneClient::new(SystemOneConfig::default().base_url(&server.base).credentials(source.clone())).unwrap();
    let request = SystemOneRequest::new("jev-latest", json!("charged twice"))
        .question("billing", Question::noul("Is this message about billing?"));
    let response = client.ask(&request).await.unwrap();
    assert!(matches!(response.answer("billing"), Some(graphrt_llm::typesafe::Answer::Noul { .. })));
    let reqs = server.requests();
    assert_eq!(reqs[0].headers["authorization"], "Bearer token-0");
    assert_eq!(reqs[1].headers["authorization"], "Bearer token-1");
    assert_eq!(reqs[1].headers["x-app-user"], "u1");

    // A second rejection after the refresh is reported, not retried forever.
    let err = client.ask(&request).await.unwrap_err();
    assert!(matches!(err, LlmError::Auth(_)), "{err:?}");
    assert_eq!(server.requests().len(), 4);
}

#[tokio::test]
async fn error_statuses_map_to_typed_errors() {
    let server = serve(vec![
        Reply { status: 429, headers: vec![("retry-after", "2")], body: "{\"detail\":\"slow down\"}".into() },
        Reply {
            status: 422,
            headers: vec![],
            body: r#"{"detail":[{"loc":["body","questions","t","criteria"],"msg":"Field required","type":"missing"}]}"#
                .into(),
        },
        Reply { status: 529, headers: vec![], body: "{\"detail\":\"overloaded\"}".into() },
    ])
    .await;
    let c = client(&server);
    let request = SystemOneRequest::new("jev-latest", json!("x")).question("n", Question::noul("?"));
    assert_eq!(
        c.ask(&request).await.unwrap_err(),
        LlmError::RateLimit { message: "slow down".into(), retry_after_ms: Some(2000) }
    );
    let err = c.ask(&request).await.unwrap_err();
    assert!(matches!(&err, LlmError::BadRequest(m) if m.contains("questions.t.criteria: Field required")), "{err:?}");
    assert!(!err.is_retryable());
    assert!(c.ask(&request).await.unwrap_err().is_retryable());
    // Validation failures never reach the server.
    let invalid = SystemOneRequest::new("jev-latest", json!("x"));
    assert!(matches!(c.ask(&invalid).await, Err(LlmError::BadRequest(_))));
    assert_eq!(server.requests().len(), 3);
}

// --- question-classifier -------------------------------------------------------------------------

#[tokio::test]
async fn classifier_routes_with_system_one() {
    let server = serve(vec![choice_reply("billing", 0.86, json!({"billing": 0.91, "bug": 0.06, "other": 0.03}))]).await;
    let events =
        run(graph(classifier_doc(json!({}))), client(&server), RunInput::new().input("text", "I was charged twice"))
            .await;
    assert_eq!(outputs(&events)["answer"], Value::from("billing route"));
    let out = node_outputs(&events, "qc");
    assert_eq!(out["class_id"], Value::from("billing"));
    assert_eq!(out["confidence"], Value::Float(0.86));
    assert_eq!(out["probabilities"].get_path(&["bug"]), Some(Value::Float(0.06)));
    assert_eq!(out["usage"].get_path(&["total_tokens"]), Some(Value::Int(132)));

    let body = &server.requests()[0].body;
    assert_eq!(body["model"], "jev-latest");
    assert_eq!(body["state"], "I was charged twice");
    let q = &body["questions"]["category"];
    assert_eq!(q["type"], "choice");
    assert_eq!(q["criteria"]["billing"], "Billing: Payments, refunds, invoices");
    assert_eq!(q["criteria"]["bug"], "Bug report");
    assert!(q["instructions"].as_str().unwrap().contains("Support tickets"));
    assert!(body["questions"].get("fits_any_category").is_none());
}

#[tokio::test]
async fn low_confidence_uses_fallback_or_fails() {
    let unsure = || choice_reply("bug", 0.42, json!({"billing": 0.4, "bug": 0.45, "other": 0.15}));
    let server = serve(vec![unsure(), unsure()]).await;

    let doc = classifier_doc(json!({"min_confidence": 0.6, "fallback_class": "other"}));
    let events = run(graph(doc), client(&server), RunInput::new().input("text", "hmm")).await;
    assert_eq!(outputs(&events)["answer"], Value::from("other route"));
    let meta = node_metadata(&events, "qc");
    assert_eq!(meta["low_confidence"], Value::Bool(true));
    assert_eq!(meta["predicted_class"], Value::from("bug"));

    let doc = classifier_doc(json!({"min_probability": 0.7}));
    let events = run(graph(doc), client(&server), RunInput::new().input("text", "hmm")).await;
    match events.last() {
        Some(Event::RunFailed { error_type, error, .. }) => {
            assert_eq!(error_type, "low_confidence");
            assert!(error.contains("probability 0.450 < 0.7"), "{error}");
        }
        other => panic!("expected RunFailed, got {other:?}"),
    }
}

#[tokio::test]
async fn companion_noul_detects_no_fitting_class() {
    let reply = ok(json!({
        "model": "jev-1.14.0",
        "answers": {
            "category": {"type": "choice", "choice": "billing", "confidence": 0.94,
                         "probabilities": {"billing": 0.96, "bug": 0.03, "other": 0.01}},
            "fits_any_category": {"type": "noul", "noul": 0.02}
        },
        "usage": {"input_tokens": 549, "output_tokens": 63}
    }));
    let server = serve(vec![reply]).await;
    let doc = classifier_doc(json!({"companion_noul": true, "companion_threshold": 0.7, "fallback_class": "other"}));
    let events = run(graph(doc), client(&server), RunInput::new().input("text", "a lamp from another shop")).await;
    assert_eq!(outputs(&events)["answer"], Value::from("other route"));
    let body = &server.requests()[0].body;
    assert_eq!(body["questions"]["fits_any_category"]["type"], "noul");
    assert!(body["questions"]["fits_any_category"]["criteria"]["false"].is_string());
    assert_eq!(node_metadata(&events, "qc")["fits_any_probability"], Value::Float(0.02));
}

#[test]
fn classifier_thresholds_require_the_typesafe_engine() {
    let doc = json!({"graph": {"nodes": [
        {"id": "start", "type": "start", "variables": []},
        {"id": "qc", "type": "question-classifier", "model": {"provider": "openai", "name": "gpt"},
         "query_variable_selector": ["sys", "query"], "classes": [{"id": "a", "name": "A"}], "min_confidence": 0.5}
    ], "edges": [{"source": "start", "target": "qc"}]}});
    let mut registry = Registry::with_builtins();
    graphrt_llm::register(&mut registry);
    assert!(Graph::build(Workflow::from_value(doc).unwrap(), &registry).is_err());
}

// --- system-one node ---------------------------------------------------------------------

fn moderation_doc(extra: Json) -> Json {
    let mut node = json!({
        "id": "judge", "type": "system-one",
        "model": "jev-latest",
        "state": {"message": "{{#start.text#}}", "channel": "support"},
        "questions": {
            "topic": {"type": "choice", "instructions": "What is `message` about?",
                      "criteria": {"billing": "Payments and refunds", "bug": "Something is broken"}},
            "urgency": {"type": "score", "instructions": "How urgent is `message`?",
                        "criteria": ["Can wait", "Needs attention this week", "Needs attention today"]},
            "spam": {"type": "noul", "instructions": "Is `message` spam?"}
        },
        "route_by": "topic"
    });
    if let (Some(c), Json::Object(e)) = (node.as_object_mut(), extra) {
        c.extend(e);
    }
    json!({"graph": {"nodes": [
        {"id": "start", "type": "start", "variables": [{"variable": "text", "type": "paragraph", "required": true}]},
        node,
        {"id": "a_billing", "type": "answer", "answer": "billing {{#judge.urgency.score#}}"},
        {"id": "a_bug", "type": "answer", "answer": "bug"},
        {"id": "a_low", "type": "answer", "answer": "review"}
    ], "edges": [
        {"source": "start", "target": "judge"},
        {"source": "judge", "target": "a_billing", "sourceHandle": "billing"},
        {"source": "judge", "target": "a_bug", "sourceHandle": "bug"},
        {"source": "judge", "target": "a_low", "sourceHandle": "low_confidence"}
    ]}})
}

fn moderation_reply(confidence: f64) -> Reply {
    ok(json!({
        "model": "jev-1.14.0",
        "answers": {
            "topic": {"type": "choice", "choice": "billing", "confidence": confidence,
                      "probabilities": {"billing": 0.9, "bug": 0.1}},
            "urgency": {"type": "score", "score": 1.7, "confidence": 0.55,
                        "legend": {"0": "Can wait", "1": "Needs attention this week", "2": "Needs attention today"},
                        "probabilities": {"0": 0.1, "1": 0.1, "2": 0.8}},
            "spam": {"type": "noul", "noul": 0.03}
        },
        "usage": {"input_tokens": 300, "output_tokens": 30}
    }))
}

#[tokio::test]
async fn system_one_node_answers_and_routes() {
    let server = serve(vec![moderation_reply(0.9), moderation_reply(0.3)]).await;
    let events = run(
        graph(moderation_doc(json!({"min_confidence": 0.5}))),
        client(&server),
        RunInput::new().input("text", "Refund me now"),
    )
    .await;
    assert_eq!(outputs(&events)["answer"], Value::from("billing 1.7"));
    let out = node_outputs(&events, "judge");
    assert_eq!(out["spam"].get_path(&["noul"]), Some(Value::Float(0.03)));
    assert_eq!(out["topic"].get_path(&["choice"]), Some(Value::from("billing")));
    assert_eq!(out["answers"].get_path(&["urgency", "type"]), Some(Value::from("score")));
    assert_eq!(out["model"], Value::from("jev-1.14.0"));

    let body = &server.requests()[0].body;
    assert_eq!(body["state"], json!({"message": "Refund me now", "channel": "support"}));
    assert_eq!(body["questions"]["urgency"]["criteria"][2], "Needs attention today");
    assert!(body.get("assets").is_none());

    // Below the confidence threshold the low_confidence handle is taken.
    let events = run(
        graph(moderation_doc(json!({"min_confidence": 0.5}))),
        client(&server),
        RunInput::new().input("text", "Refund me now"),
    )
    .await;
    assert_eq!(outputs(&events)["answer"], Value::from("review"));
}

#[tokio::test]
async fn system_one_node_attaches_images() {
    let reply = ok(json!({
        "model": "jev-1.14.0",
        "answers": {"where": {"type": "box", "box": {"top": 0.2, "bottom": 0.66, "left": 0.19, "right": 0.68},
                              "confidence": null}},
        "usage": {"input_tokens": 285, "output_tokens": 20}
    }));
    let server = serve(vec![reply]).await;
    let doc = json!({"graph": {"nodes": [
        {"id": "start", "type": "start", "variables": [{"variable": "photo", "type": "file", "required": true}]},
        {"id": "eye", "type": "system-one", "model": "jev-preview",
         "images": {"photo": ["start", "photo"]},
         "questions": {"where": {"type": "box", "instructions": "Locate the red rectangle in `photo`."}}},
        {"id": "end", "type": "end", "outputs": [{"variable": "box", "value_selector": ["eye", "where"]}]}
    ], "edges": [{"source": "start", "target": "eye"}, {"source": "eye", "target": "end"}]}});
    let png = "iVBORw0KGgoAAAANSUhEUgAAABAAAAAQCAIAAACQkWg2AAAAHElEQVR4nGP4TyJgoKcGBgbsaFQDfTUQB4aDBgDAen2fpitsVAAAAABJRU5ErkJggg==";
    let file = File {
        marker: true,
        name: "rect.png".into(),
        kind: FileKind::Image,
        transfer: FileTransfer::Inline,
        url: Some(format!("data:image/png;base64,{png}")),
        mime_type: Some("image/png".into()),
        ..File::default()
    };
    let events = run(graph(doc), client(&server), RunInput::new().input("photo", Value::from(file))).await;
    assert_eq!(outputs(&events)["box"].get_path(&["box", "left"]), Some(Value::Float(0.19)));

    let req = &server.requests()[0];
    assert_eq!(req.headers["typesafe-danger-zone-images"], "true");
    let reference = req.body["state"]["photo"].as_str().unwrap().to_owned();
    assert!(reference.starts_with("graphrt-image-"));
    let asset = &req.body["assets"][&reference];
    assert_eq!(asset["type"], "image");
    assert_eq!(asset["format"], "png");
    assert_eq!(asset["data_base64"], png);
}

#[test]
fn example_workflows_build() {
    let mut registry = Registry::with_builtins();
    graphrt_llm::register(&mut registry);
    for name in ["ticket-router.yaml", "moderation.yaml"] {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/typesafe").join(name);
        let text = std::fs::read_to_string(&path).unwrap();
        let graph = Graph::from_text(&text, &registry).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert!(graph.warnings().is_empty(), "{name}: {:?}", graph.warnings());
    }
}
