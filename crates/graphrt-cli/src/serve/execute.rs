//! Launching runs: drive a run in the background, track it as active, and
//! answer its caller in the requested [`Delivery`].

use std::sync::Arc;

use axum::Json;
use axum::http::StatusCode;
use axum::response::sse::{KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use graphrt_core::engine::RunInput;
use graphrt_core::response::ResponseAssembler;
use graphrt_core::store::{RunRecord, StoreError, now_ms};
use graphrt_core::{Engine, Graph, Selector, Value};
use indexmap::IndexMap;

use super::delivery::{AbortOnDrop, Delivery, EventScope, Reply, result_json};
use super::errors::{error_response, store_error};
use super::sse::sse_stream;
use super::state::AppState;
use crate::sse_ids::Cursor;
use crate::store::live::{self, ActiveRun};

/// Caller context stored on a run record, for its spans.
fn record_context(record: &RunRecord) -> crate::otel::RunContext {
    crate::otel::RunContext {
        session_id: record.session_id.clone(),
        user_id: record.user_id.clone(),
        tags: record.tags.clone(),
        metadata: record.metadata.clone(),
    }
}

/// Request JSON values as engine values.
pub(crate) fn to_values(map: serde_json::Map<String, serde_json::Value>) -> IndexMap<String, Value> {
    map.into_iter().map(|(k, v)| (k, Value::from_json(v))).collect()
}

/// W3C `traceparent` of the incoming request, continued by the run's trace.
pub(crate) fn traceparent(headers: &axum::http::HeaderMap) -> Option<String> {
    headers.get("traceparent").and_then(|v| v.to_str().ok()).map(str::to_owned)
}

/// Launch a run and answer its caller in `reply`'s delivery mode.
pub(super) async fn execute(
    state: &Arc<AppState>,
    engine: &Engine,
    record: RunRecord,
    input: RunInput,
    reply: Reply,
    traceparent: Option<String>,
) -> Response {
    // The launched run's assembler records the answer in the store; a second one,
    // fed from the same events, produces `response` SSE events for a streaming caller.
    let stream_assembler = new_assembler(engine, &input);
    let run_id = record.run_id.clone();
    let active = launch(state, engine, record, input, traceparent.as_deref());
    match reply.delivery {
        Delivery::Detached => {
            // No caller waits: a shutdown drain leaves it for the next server to resume.
            active.set_detached();
            accepted_response(&run_id)
        }
        Delivery::Stream => stream_response(active, stream_assembler, reply.scope),
        Delivery::Blocking => {
            let guard = AbortOnDrop(Arc::clone(&active));
            let outcome = active.wait().await;
            drop(guard);
            result_json(&run_id, outcome, reply.scope)
        }
    }
}

/// The SSE response of a streaming caller, numbered from the run's first event
/// of this execution; the caller disconnecting aborts the run.
fn stream_response(active: Arc<ActiveRun>, assembler: ResponseAssembler, scope: EventScope) -> Response {
    // The subscription's backlog covers events emitted before subscribing.
    let events = live::follow(Arc::clone(&active), active.subscribe());
    let first_index = active.stored_before;
    let guard = Box::new(AbortOnDrop(active));
    let events = sse_stream(events, assembler, first_index, Cursor::ALL, guard, scope);
    Sse::new(events).keep_alive(KeepAlive::default()).into_response()
}

/// `202` for a detached run, with where to follow it.
fn accepted_response(run_id: &str) -> Response {
    let body = serde_json::json!({
        "run_id": run_id,
        "status": "running",
        "run": format!("/v1/runs/{run_id}"),
        "events": format!("/v1/runs/{run_id}/events"),
    });
    (StatusCode::ACCEPTED, Json(body)).into_response()
}

/// The answer assembler of a run, seeded with what a resumed run already said
/// and with the caller's `sys` values.
pub(super) fn new_assembler(engine: &Engine, input: &RunInput) -> ResponseAssembler {
    let mut assembler = ResponseAssembler::new(engine.graph());
    if let Some(resume) = &input.resume {
        // Nodes that finished before the pause will not re-run; seed their values.
        assembler = assembler.resume_from(&resume.snapshot.root);
    }
    for (k, v) in &input.sys {
        assembler = assembler.with_value(&Selector::of("sys", k), v.clone());
    }
    assembler
}

/// Start driving a run in the background and track it as active.
pub(super) fn launch(
    state: &Arc<AppState>,
    engine: &Engine,
    record: RunRecord,
    input: RunInput,
    traceparent: Option<&str>,
) -> Arc<ActiveRun> {
    // The context lives on the record; building it is skipped when nothing traces.
    let tracer = match &state.telemetry {
        Some(telemetry) => telemetry.tracer(engine.graph(), traceparent, &record_context(&record)),
        None => crate::otel::RunTracer::default(),
    };
    let capture = live::Capture {
        start_node: state.store_inputs.then(|| root_start_node(engine.graph())).flatten(),
        #[cfg(feature = "online-eval")]
        online: state.online.as_ref().filter(|online| online.covers(&record.workflow)).cloned(),
        #[cfg(feature = "conversations")]
        conversation: state
            .conversations
            .as_ref()
            .zip(record.session_id.as_deref())
            .map(|(conversations, session)| crate::conversations::Turn::of(conversations, session)),
    };
    let assembler = new_assembler(engine, &input);
    #[cfg(feature = "run-follow")]
    let seed = live::AssemblerSeed {
        graph: Arc::clone(engine.graph()),
        sys: input.sys.clone(),
        resumed: input.resume.as_ref().map(|resume| resume.snapshot.root.clone()),
    };
    let run = engine.run(input);
    // Tracked before the driver starts: a run that ends at once must not be
    // untracked first and then left in the active set for good.
    let tracked = Arc::clone(state);
    let weak = Arc::downgrade(state);
    let tracking = live::Tracking {
        on_start: Box::new(move |active| {
            #[cfg(feature = "run-follow")]
            active.set_assembler_seed(seed);
            tracked.track(Arc::clone(active));
        }),
        on_finish: Box::new(move |id| {
            if let Some(state) = weak.upgrade() {
                state.untrack(id);
            }
        }),
    };
    let active = live::spawn(run, assembler, record, Arc::clone(&state.store), tracking, tracer, capture);
    #[cfg(feature = "telemetry")]
    crate::telemetry::count_run();
    active
}

#[cfg(feature = "trust-gateway")]
impl AppState {
    /// Start driving a run in the background and track it as active (for the
    /// gateway protocol). `record` must already exist in the store.
    pub(crate) fn launch_run(
        self: &Arc<Self>,
        engine: &Engine,
        record: RunRecord,
        input: RunInput,
        traceparent: Option<&str>,
    ) -> Arc<ActiveRun> {
        launch(self, engine, record, input, traceparent)
    }
}

/// Id of the root graph's start node, whose outputs are the run's validated inputs.
fn root_start_node(graph: &Graph) -> Option<Arc<str>> {
    graph.nodes().iter().find(|n| n.parent.is_none() && n.kind.as_ref() == "start").map(|n| Arc::clone(&n.id))
}

/// Result of [`launch_detached`].
#[derive(Debug)]
pub(crate) enum Launch {
    /// A new run is executing.
    Started(Arc<ActiveRun>),
    /// A run with this id already exists (an idempotent redelivery).
    Duplicate { run_id: String, status: String },
}

/// Start a background run of workflow `name` with these inputs, for callers other
/// than the run API (webhooks). With `run_id`, an existing run of that id is
/// reported as [`Launch::Duplicate`] instead of starting another.
///
/// @throws Response - 404 for an unknown workflow, or the store's error response.
pub(crate) async fn launch_detached(
    state: &Arc<AppState>,
    name: &str,
    run_id: Option<String>,
    inputs: IndexMap<String, Value>,
    traceparent: Option<String>,
) -> Result<Launch, Response> {
    let Some(workflow) = state.workflows.get(name) else {
        return Err(error_response(StatusCode::NOT_FOUND, &format!("unknown workflow `{name}`")));
    };
    // Webhook runs count against `--max-active-runs` like API runs; senders retry on 429.
    if let Some(busy) = state.refuse_if_busy() {
        return Err(busy);
    }
    let run_id = run_id.unwrap_or_else(|| state.new_run_id());
    let record = RunRecord::running(&run_id, name, workflow.engine.graph().fingerprint(), now_ms());
    match state.store.create(record.clone()).await {
        Ok(()) => {}
        // `create` is atomic, so concurrent redeliveries start one run between them.
        Err(StoreError::Conflict(_)) => {
            let status = match state.store.get(&run_id).await {
                Ok(Some(existing)) => crate::store::status_name(existing.status),
                Ok(None) | Err(_) => "unknown".to_owned(),
            };
            return Ok(Launch::Duplicate { run_id, status });
        }
        Err(e) => return Err(store_error(&e)),
    }
    let mut input = RunInput::new();
    input.run_id = Some(run_id);
    input.inputs = inputs;
    input.env = workflow.env.clone();
    let active = launch(state, &workflow.engine, record, input, traceparent.as_deref());
    // Webhook runs have no waiting caller: after a drain the next server resumes them.
    active.set_detached();
    Ok(Launch::Started(active))
}
