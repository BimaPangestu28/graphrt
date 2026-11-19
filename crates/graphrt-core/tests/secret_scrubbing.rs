//! Node error messages that echo a secret's plaintext (a provider error body that
//! repeats the API key, an HTTP error snippet...) are scrubbed before they reach
//! events, retries, error outputs or the terminal run event.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use std::sync::Arc;

use futures::StreamExt;
use futures::future::BoxFuture;
use graphrt_core::Workflow;
use graphrt_core::engine::{Engine, RunInput};
use graphrt_core::event::Event;
use graphrt_core::graph::Graph;
use graphrt_core::node::{ConfigError, Node, NodeContext, NodeError, NodeResult, Registry};
use graphrt_core::pool::Selector;
use graphrt_core::value::Value;
use serde_json::{Value as Json, json};

const ENV_SECRET: &str = "sk-env-0123456789";
const INPUT_SECRET: &str = "sk-input-abcdef";

/// Fails with a message that quotes the plaintext of a secret, like a provider
/// error body would. Retryable when configured, so retries can be observed.
#[derive(Debug)]
struct EchoingFailure {
    secret: Selector,
    retryable: bool,
}

impl Node for EchoingFailure {
    fn run(&self, ctx: NodeContext) -> BoxFuture<'_, NodeResult> {
        Box::pin(async move {
            let plaintext =
                ctx.get(&self.secret).and_then(|v| v.expose_secret().map(str::to_owned)).unwrap_or_default();
            let message = format!("401 Unauthorized: {{\"error\": \"Incorrect API key provided: {plaintext}\"}}");
            Err(if self.retryable { NodeError::new("auth", message) } else { NodeError::fatal("auth", message) })
        })
    }

    fn inputs(&self) -> Vec<Selector> {
        vec![self.secret.clone()]
    }
}

fn registry() -> Registry {
    let mut registry = Registry::with_builtins();
    registry.register("echoing-failure", |doc| {
        let secret: Selector = serde_json::from_value(doc.config.get("secret").cloned().unwrap_or(Json::Null))
            .map_err(|e| ConfigError::new(doc, e.to_string()))?;
        let retryable = doc.config.get("retryable").and_then(Json::as_bool).unwrap_or(false);
        Ok(Arc::new(EchoingFailure { secret, retryable }))
    });
    registry
}

fn engine(doc: Json) -> Engine {
    Engine::new(Arc::new(Graph::build(Workflow::from_value(doc).unwrap(), &registry()).unwrap()))
}

fn doc(secret: Json, node_extra: Json) -> Json {
    let mut node = json!({"id": "call", "type": "echoing-failure", "secret": secret});
    if let (Some(node), Some(extra)) = (node.as_object_mut(), node_extra.as_object()) {
        node.extend(extra.clone());
    }
    json!({
        "env": [{"name": "API_KEY", "type": "secret"}],
        "graph": {"nodes": [
            {"id": "start", "type": "start", "variables": [{"variable": "token", "type": "secret"}]},
            node,
            {"id": "end", "type": "end", "outputs": [{"variable": "msg", "value_selector": ["call", "error_message"]}]}
        ], "edges": [{"source": "start", "target": "call"}, {"source": "call", "target": "end"}]}
    })
}

async fn events(doc: Json) -> Vec<Event> {
    let input = RunInput::new().env("API_KEY", Value::Secret(Arc::from(ENV_SECRET))).input("token", INPUT_SECRET);
    engine(doc).run(input).collect().await
}

fn assert_no_plaintext(events: &[Event]) {
    let text = format!("{events:?}");
    for secret in [ENV_SECRET, INPUT_SECRET] {
        assert!(!text.contains(secret), "plaintext secret `{secret}` in events: {text}");
    }
}

#[tokio::test]
async fn env_secret_echoed_in_a_node_error_is_masked() {
    let events = events(doc(json!(["env", "API_KEY"]), json!({}))).await;
    assert_no_plaintext(&events);
    let failed = events.iter().find_map(|e| match e {
        Event::NodeFailed { error, .. } => Some(error.clone()),
        _ => None,
    });
    assert_eq!(
        failed.as_deref(),
        Some("401 Unauthorized: {\"error\": \"Incorrect API key provided: ******\"}"),
        "{events:?}"
    );
    match events.last() {
        Some(Event::RunFailed { error, .. }) => assert!(error.contains("provided: ******"), "{error}"),
        other => panic!("expected a failed run, got {other:?}"),
    }
}

#[tokio::test]
async fn secret_typed_start_input_echoed_in_a_node_error_is_masked() {
    let events = events(doc(json!(["start", "token"]), json!({}))).await;
    assert_no_plaintext(&events);
    assert!(matches!(events.last(), Some(Event::RunFailed { error, .. }) if error.contains("******")), "{events:?}");
}

#[tokio::test]
async fn handled_error_outputs_are_masked() {
    let events =
        events(doc(json!(["env", "API_KEY"]), json!({"error_strategy": "default-value", "default_value": []}))).await;
    assert_no_plaintext(&events);
    match events.last() {
        Some(Event::RunSucceeded { outputs, .. }) => {
            let message = outputs.get("msg").and_then(Value::as_str).unwrap_or_default();
            assert!(message.ends_with("provided: ******\"}"), "{message}");
        }
        other => panic!("expected success, got {other:?}"),
    }
}

#[tokio::test]
async fn retry_events_are_masked() {
    let events = events(doc(
        json!(["env", "API_KEY"]),
        json!({"retryable": true, "retry": {"max_retries": 2, "interval_ms": 0}}),
    ))
    .await;
    assert_no_plaintext(&events);
    let retries =
        events.iter().filter(|e| matches!(e, Event::NodeRetry { error, .. } if error.contains("******"))).count();
    assert_eq!(retries, 2, "{events:?}");
}
