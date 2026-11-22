//! What the server offers: `GET /v1/workflows`, `GET /v1/nodes` and
//! `POST /v1/validate`.

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use graphrt_core::Graph;

use super::state::{AppState, ServedWorkflow};

/// `GET /v1/workflows`.
pub(super) async fn list_workflows(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let items: Vec<serde_json::Value> =
        state.workflows.iter().map(|(name, workflow)| workflow_json(name, workflow)).collect();
    Json(serde_json::json!({ "workflows": items }))
}

/// A served workflow as listed: its start inputs, and which declared env
/// variables have a value (never the value itself).
fn workflow_json(name: &str, workflow: &ServedWorkflow) -> serde_json::Value {
    let doc = workflow.engine.graph().workflow();
    let inputs = doc
        .graph
        .nodes
        .iter()
        .filter(|n| n.kind == "start")
        .find_map(|n| n.config.get("variables").cloned())
        .unwrap_or(serde_json::Value::Array(Vec::new()));
    let env: Vec<serde_json::Value> = doc
        .env
        .iter()
        .map(|e| {
            serde_json::json!({
                "name": e.name,
                "configured": workflow.env.contains_key(&e.name) || e.value.is_some(),
            })
        })
        .collect();
    serde_json::json!({
        "name": name,
        "title": doc.name,
        "description": doc.description,
        "inputs": inputs,
        "env": env,
    })
}

/// `GET /v1/nodes`.
pub(super) async fn list_nodes(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    Json(serde_json::json!({ "nodes": state.extensions.registry.descriptors() }))
}

/// Validate a workflow document against the node kinds of this build.
///
/// The body is either the document itself (JSON) or `{"document": "<YAML or JSON text>"}`.
pub(super) async fn validate_workflow(
    State(state): State<Arc<AppState>>,
    Json(body): Json<serde_json::Value>,
) -> Json<serde_json::Value> {
    Json(validation_report(body, &state.extensions.registry))
}

/// `{valid, errors, warnings}` for a workflow document.
pub fn validation_report(body: serde_json::Value, registry: &graphrt_core::Registry) -> serde_json::Value {
    let parsed = match body.get("document").and_then(serde_json::Value::as_str) {
        Some(text) => graphrt_core::Workflow::parse(text),
        None => graphrt_core::Workflow::from_value(body),
    };
    let report = |valid: bool, errors: Vec<String>, warnings: Vec<String>| serde_json::json!({ "valid": valid, "errors": errors, "warnings": warnings });
    let workflow = match parsed {
        Ok(w) => w,
        Err(e) => return report(false, vec![e.to_string()], Vec::new()),
    };
    match Graph::build(workflow, registry) {
        Ok(graph) => report(true, Vec::new(), graph.warnings().to_vec()),
        Err(e) => report(false, vec![crate::extensions::explain_unknown_kind(e.to_string())], Vec::new()),
    }
}
