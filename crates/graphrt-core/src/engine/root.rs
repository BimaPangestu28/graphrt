//! Root-run orchestration: bootstrap the variable pool, run the root frame,
//! enforce the run-wide duration budget and emit the terminal event.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::Either;
use indexmap::IndexMap;

use super::control::{Control, Stop};
use super::frame::{Frame, FrameOutcome, RunShared};
use super::ids::new_id;
use super::{Engine, ResumeFrom, RunInput};
use crate::dsl::VariableDecl;
use crate::event::{Event, PauseReason, Usage};
use crate::node::{EventSink, NodeError};
use crate::pool::{CONVERSATION, ENV, SYS, Selector, VariablePool};
use crate::redact::SecretSet;
use crate::snapshot::{FrameSnapshot, SNAPSHOT_FORMAT, Snapshot};
use crate::value::Value;

/// A failure before the run starts: the message and its `error_type`.
type StartFailure = (String, &'static str);

/// Which run this is and where it starts.
struct RunOrigin {
    run_id: String,
    /// Run time spent before this segment (the snapshot's, on resume).
    prior_elapsed_ms: u64,
    /// The root frame to restore; `None` for a new run.
    root_snapshot: Option<FrameSnapshot>,
}

/// Counters and resume data a snapshot carries into [`RunShared`] (all empty
/// for a new run).
#[derive(Default)]
struct CarriedState {
    steps: u64,
    usage: Usage,
    handled_errors: u32,
    resume_inputs: IndexMap<String, Value>,
    secrets: Vec<(Selector, Value)>,
}

/// The host's values for the run, `env` and `conversation` resolved against
/// the workflow's declarations.
struct HostValues {
    env: Vec<(String, Value)>,
    conversation: Vec<(String, Value)>,
    /// The host's own `conversation` values, re-applied onto a restored frame.
    conversation_overrides: IndexMap<String, Value>,
    sys: IndexMap<String, Value>,
    inputs: IndexMap<String, Value>,
}

/// Drive a whole run, emitting every event through `sink`.
pub(crate) async fn run_root(engine: Engine, input: RunInput, control: Arc<Control>, sink: EventSink) {
    let started_ms = engine.clock.now_ms();
    let (origin, host, carried) = match prepare_run(&engine, input, started_ms) {
        Ok(prepared) => prepared,
        Err((error, error_type)) => return emit_start_failure(&sink, error, error_type),
    };
    let shared = share_run(&engine, &control, &sink, &host, carried);
    let RunOrigin { run_id, prior_elapsed_ms, root_snapshot } = origin;
    sink(Event::RunStarted { run_id: run_id.clone(), resumed: root_snapshot.is_some() });
    let frame = match root_snapshot {
        Some(snapshot) => restored_root(&shared, &host, snapshot),
        None => Frame::fresh(shared.clone(), Arc::from(""), None, 0, fresh_root_pool(&engine, &host, &run_id)),
    };
    let frame = match frame {
        Ok(frame) => frame,
        Err(e) => return emit_frame_failure(&shared, &e),
    };
    // The run-wide duration budget (`Limits::max_duration_ms`, minus the time
    // before a resume) stops the root frame when it runs out.
    let outcome = match duration_budget_ms(&shared, prior_elapsed_ms) {
        Some(ms) => {
            let run = frame.run();
            futures::pin_mut!(run);
            match futures::future::select(run, shared.clock.sleep(Duration::from_millis(ms))).await {
                Either::Left((outcome, _)) => outcome,
                Either::Right(((), _)) => stop_over_duration_limit(&shared),
            }
        }
        None => frame.run().await,
    };
    let elapsed_ms = prior_elapsed_ms.saturating_add(engine.clock.now_ms().saturating_sub(started_ms));
    sink(terminal_event(&shared, outcome, run_id, elapsed_ms));
}

/// Validate the host's values and the resume snapshot, in that order.
///
/// @throws StartFailure - `invalid_input` for a value of the wrong type or a bad
///   resume secret, `snapshot_mismatch` for a snapshot of another format or graph.
fn prepare_run(
    engine: &Engine,
    input: RunInput,
    started_ms: u64,
) -> Result<(RunOrigin, HostValues, CarriedState), StartFailure> {
    let workflow = engine.graph.workflow();
    let env = resolve_decls(&workflow.env, &input.env, true).map_err(invalid_input)?;
    let conversation = resolve_decls(&workflow.conversation, &input.conversation, false).map_err(invalid_input)?;
    let (origin, carried) = match input.resume {
        Some(resume) => resume_origin(engine, resume)?,
        None => {
            let run_id = input.run_id.unwrap_or_else(|| new_id(started_ms));
            (RunOrigin { run_id, prior_elapsed_ms: 0, root_snapshot: None }, CarriedState::default())
        }
    };
    let host = HostValues {
        env,
        conversation,
        conversation_overrides: input.conversation,
        sys: input.sys,
        inputs: input.inputs,
    };
    Ok((origin, host, carried))
}

fn invalid_input(error: String) -> StartFailure {
    (error, "invalid_input")
}

/// Where a resumed run continues: its snapshot, once it matches this engine.
///
/// @throws StartFailure - See [`prepare_run`].
fn resume_origin(engine: &Engine, resume: ResumeFrom) -> Result<(RunOrigin, CarriedState), StartFailure> {
    let snapshot = resume.snapshot;
    check_snapshot(engine, &snapshot)?;
    let secrets = parse_secrets(&resume.secrets).map_err(invalid_input)?;
    let origin = RunOrigin {
        run_id: snapshot.run_id,
        prior_elapsed_ms: snapshot.elapsed_ms,
        root_snapshot: Some(snapshot.root),
    };
    let carried = CarriedState {
        steps: snapshot.steps,
        usage: snapshot.usage,
        handled_errors: snapshot.handled_errors,
        resume_inputs: resume.inputs,
        secrets,
    };
    Ok((origin, carried))
}

/// @throws StartFailure (`snapshot_mismatch`) - A snapshot of another format,
///   or of a graph other than this engine's.
fn check_snapshot(engine: &Engine, snapshot: &Snapshot) -> Result<(), StartFailure> {
    if snapshot.format != SNAPSHOT_FORMAT {
        return Err((crate::Error::SnapshotFormat(snapshot.format).to_string(), "snapshot_mismatch"));
    }
    if snapshot.graph_fingerprint != engine.graph.fingerprint() {
        let mismatch = crate::Error::SnapshotMismatch {
            expected: engine.graph.fingerprint().to_owned(),
            actual: snapshot.graph_fingerprint.clone(),
        };
        return Err((mismatch.to_string(), "snapshot_mismatch"));
    }
    Ok(())
}

fn emit_start_failure(sink: &EventSink, error: String, error_type: &str) {
    sink(Event::RunFailed { error, error_type: error_type.to_owned(), usage: Usage::default(), elapsed_ms: 0 });
}

fn emit_frame_failure(shared: &RunShared, error: &NodeError) {
    shared.emit(Event::RunFailed {
        error: error.to_string(),
        error_type: error.kind.clone(),
        usage: shared.total_usage(),
        elapsed_ms: 0,
    });
}

/// The state every frame of the run shares, with every secret known up front
/// observed so its plaintext is scrubbed from node error messages. Nodes add
/// the secrets they produce (see `Frame::commit`).
fn share_run(
    engine: &Engine,
    control: &Arc<Control>,
    sink: &EventSink,
    host: &HostValues,
    carried: CarriedState,
) -> Arc<RunShared> {
    let secret_values = Arc::new(SecretSet::new());
    // Includes secret-typed conversation variables, scrubbed like any other secret.
    let initial = host
        .env
        .iter()
        .map(|(_, v)| v)
        .chain(carried.secrets.iter().map(|(_, v)| v))
        .chain(host.inputs.values())
        .chain(host.conversation.iter().map(|(_, v)| v));
    initial.for_each(|value| secret_values.observe(value));
    Arc::new(RunShared {
        graph: engine.graph.clone(),
        services: engine.services.clone(),
        clock: engine.clock.clone(),
        limits: engine.limits.clone(),
        control: control.clone(),
        sink: sink.clone(),
        steps: AtomicU64::new(carried.steps),
        handled_errors: AtomicU32::new(carried.handled_errors),
        usage: Mutex::new(carried.usage),
        env: host.env.clone(),
        secrets: carried.secrets,
        resume_inputs: carried.resume_inputs,
        secret_values,
    })
}

/// The root frame of a resumed run. Host-supplied `sys` and `conversation`
/// values take precedence over persisted ones.
fn restored_root(shared: &Arc<RunShared>, host: &HostValues, snapshot: FrameSnapshot) -> Result<Frame, NodeError> {
    let mut frame = Frame::restore(shared.clone(), Arc::from(""), None, 0, snapshot)?;
    apply_host_values(&mut frame, &host.sys, &host.conversation_overrides);
    Ok(frame)
}

/// The variable pool of a new run's root frame.
fn fresh_root_pool(engine: &Engine, host: &HostValues, run_id: &str) -> VariablePool {
    // Node namespaces plus `env`, `conversation` and `sys`.
    let mut pool = VariablePool::with_namespace_capacity(engine.graph.nodes().len() + 3);
    for (name, value) in &host.env {
        pool.insert(ENV, name, value.clone());
    }
    for (name, value) in &host.conversation {
        pool.insert(CONVERSATION, name, value.clone());
    }
    for (name, value) in &host.sys {
        pool.insert(SYS, name, value.clone());
    }
    pool.insert(SYS, "run_id", Value::from(run_id));
    // Start inputs live in the entry node's namespace; the start node validates
    // and re-emits them as its outputs. They are never copied into `sys`.
    if let Some(entry) = engine.graph.root_scope().and_then(|s| engine.graph.node(s.entry)) {
        for (name, value) in &host.inputs {
            pool.insert(&entry.id, name, value.clone());
        }
    }
    pool
}

/// Milliseconds the run may still take, when it has a duration limit and the
/// clock can time it.
fn duration_budget_ms(shared: &RunShared, prior_elapsed_ms: u64) -> Option<u64> {
    let budget = shared.limits.max_duration_ms.map(|ms| ms.saturating_sub(prior_elapsed_ms))?;
    shared.clock.supports_timers().then_some(budget)
}

/// Stop a run that exceeded `Limits::max_duration_ms`.
fn stop_over_duration_limit(shared: &RunShared) -> FrameOutcome {
    let limit = shared.limits.max_duration_ms.unwrap_or_default();
    shared.control.stop(Stop::Fail(format!("run exceeded its duration limit of {limit} ms")));
    FrameOutcome::Stopped
}

/// The run's terminal event for the root frame's `outcome`.
fn terminal_event(shared: &RunShared, outcome: FrameOutcome, run_id: String, elapsed_ms: u64) -> Event {
    let usage = shared.total_usage();
    match outcome {
        FrameOutcome::Completed { outputs, .. } => Event::RunSucceeded {
            outputs,
            usage,
            handled_errors: shared.handled_errors.load(Ordering::Acquire),
            elapsed_ms,
        },
        FrameOutcome::Failed { error, .. } => {
            Event::RunFailed { error: error.to_string(), error_type: error.kind.clone(), usage, elapsed_ms }
        }
        FrameOutcome::Paused { snapshot, .. } => {
            let snapshot = Snapshot {
                format: SNAPSHOT_FORMAT,
                run_id,
                graph_fingerprint: shared.graph.fingerprint().to_owned(),
                elapsed_ms,
                steps: shared.steps.load(Ordering::Acquire),
                usage,
                handled_errors: shared.handled_errors.load(Ordering::Acquire),
                root: *snapshot,
            };
            Event::RunPaused { reasons: pause_reasons(&shared.control), snapshot: Box::new(snapshot) }
        }
        FrameOutcome::Stopped => stopped_event(&shared.control, usage, elapsed_ms),
    }
}

/// Why the run paused: its nodes' reasons, or an `external` one for a pause
/// requested through the run handle.
fn pause_reasons(control: &Control) -> Vec<PauseReason> {
    let mut reasons = control.reasons();
    if reasons.is_empty() && control.pause_requested() {
        reasons.push(PauseReason {
            frame: Arc::from(""),
            node_id: None,
            kind: "external".to_owned(),
            data: Value::Null,
        });
    }
    reasons
}

/// The terminal event of a stopped run: a limit failure or an abort.
fn stopped_event(control: &Control, usage: Usage, elapsed_ms: u64) -> Event {
    match control.stop_reason() {
        Some(Stop::Fail(error)) => {
            Event::RunFailed { error, error_type: "limit_exceeded".to_owned(), usage, elapsed_ms }
        }
        Some(Stop::Abort(reason)) => Event::RunAborted { reason, elapsed_ms },
        None => Event::RunAborted { reason: "stopped".to_owned(), elapsed_ms },
    }
}

/// Parse `ResumeFrom::secrets` keys into selectors; values become secrets.
fn parse_secrets(secrets: &IndexMap<String, Value>) -> Result<Vec<(Selector, Value)>, String> {
    let mut out = Vec::with_capacity(secrets.len());
    for (key, value) in secrets {
        let selector = Selector::parse(key).map_err(|e| format!("resume secret `{key}`: {e}"))?;
        let plaintext = value.expose_secret().ok_or_else(|| format!("resume secret `{key}` must be a string"))?;
        out.push((selector, Value::Secret(Arc::from(plaintext))));
    }
    Ok(out)
}

/// Re-apply host-provided `sys` and `conversation` values onto a restored root frame.
fn apply_host_values(frame: &mut Frame, sys: &IndexMap<String, Value>, conversation: &IndexMap<String, Value>) {
    for (name, value) in sys {
        frame.set_variable(SYS, name, value.clone());
    }
    for (name, value) in conversation {
        frame.set_variable(CONVERSATION, name, value.clone());
    }
}

/// Merge declared variables with host overrides, coercing to the declared types.
/// Declarations without a value and without an override are omitted when
/// `omit_missing`, otherwise they get their type's zero value.
fn resolve_decls(
    decls: &[VariableDecl],
    overrides: &IndexMap<String, Value>,
    omit_missing: bool,
) -> Result<Vec<(String, Value)>, String> {
    let mut out: Vec<(String, Value)> = Vec::new();
    for decl in decls {
        let raw = overrides.get(&decl.name).cloned().or_else(|| decl.value.clone());
        let value = match raw {
            Some(v) => decl.ty.coerce(v).map_err(|e| format!("variable `{}`: {e}", decl.name))?,
            None if omit_missing => continue,
            None => decl.ty.zero(),
        };
        out.push((decl.name.clone(), value));
    }
    for (name, value) in overrides {
        if !decls.iter().any(|d| &d.name == name) {
            out.push((name.clone(), value.clone()));
        }
    }
    Ok(out)
}
