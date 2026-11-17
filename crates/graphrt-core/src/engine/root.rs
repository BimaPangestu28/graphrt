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
use super::{Engine, RunInput};
use crate::dsl::VariableDecl;
use crate::event::{Event, PauseReason, Usage};
use crate::node::EventSink;
use crate::pool::{CONVERSATION, ENV, SYS, VariablePool};
use crate::snapshot::{SNAPSHOT_FORMAT, Snapshot};
use crate::value::Value;

/// Drive a whole run, emitting every event through `sink`.
pub(crate) async fn run_root(engine: Engine, input: RunInput, control: Arc<Control>, sink: EventSink) {
    let started_ms = engine.clock.now_ms();
    let emit_failure = |error: String| {
        (sink)(Event::RunFailed { error, usage: Usage::default(), elapsed_ms: 0 });
    };

    let workflow = engine.graph.workflow();
    let env = match resolve_decls(&workflow.env, &input.env, true) {
        Ok(env) => env,
        Err(e) => return emit_failure(e),
    };
    let conversation = match resolve_decls(&workflow.conversation, &input.conversation, false) {
        Ok(c) => c,
        Err(e) => return emit_failure(e),
    };

    // Resume validation.
    let (prior_elapsed, prior_steps, prior_usage, prior_errors, run_id, resume_snapshot, resume_inputs) =
        match input.resume {
            Some(resume) => {
                let snap = resume.snapshot;
                if snap.format != SNAPSHOT_FORMAT {
                    return emit_failure(crate::Error::SnapshotFormat(snap.format).to_string());
                }
                if snap.graph_fingerprint != engine.graph.fingerprint() {
                    return emit_failure(
                        crate::Error::SnapshotMismatch {
                            expected: engine.graph.fingerprint().to_owned(),
                            actual: snap.graph_fingerprint.clone(),
                        }
                        .to_string(),
                    );
                }
                (
                    snap.elapsed_ms,
                    snap.steps,
                    snap.usage,
                    snap.handled_errors,
                    snap.run_id.clone(),
                    Some(snap.root),
                    resume.inputs,
                )
            }
            None => (
                0,
                0,
                Usage::default(),
                0,
                input.run_id.clone().unwrap_or_else(|| new_id(started_ms)),
                None,
                IndexMap::new(),
            ),
        };
    let resumed = resume_snapshot.is_some();

    let shared = Arc::new(RunShared {
        graph: engine.graph.clone(),
        services: engine.services.clone(),
        clock: engine.clock.clone(),
        limits: engine.limits.clone(),
        control: control.clone(),
        sink: sink.clone(),
        steps: AtomicU64::new(prior_steps),
        handled_errors: AtomicU32::new(prior_errors),
        usage: Mutex::new(prior_usage),
        env: env.clone(),
        resume_inputs,
    });

    sink(Event::RunStarted { run_id: run_id.clone(), resumed });

    let frame = match resume_snapshot {
        Some(snapshot) => Frame::restore(shared.clone(), Arc::from(""), None, 0, snapshot).map(|mut f| {
            // Host-supplied values take precedence over persisted ones on resume.
            f_apply(&mut f, &input.sys, &input.conversation);
            f
        }),
        None => {
            let mut pool = VariablePool::new();
            for (name, value) in &env {
                pool.insert(ENV, name, value.clone());
            }
            for (name, value) in &conversation {
                pool.insert(CONVERSATION, name, value.clone());
            }
            for (name, value) in &input.sys {
                pool.insert(SYS, name, value.clone());
            }
            pool.insert(SYS, "run_id", Value::from(run_id.as_str()));
            // Start inputs live in the entry node's namespace; the start node validates
            // and re-emits them as its outputs. They are never copied into `sys`.
            if let Some(entry) = engine.graph.root_scope().and_then(|s| engine.graph.node(s.entry)) {
                for (name, value) in &input.inputs {
                    pool.insert(&entry.id, name, value.clone());
                }
            }
            Frame::fresh(shared.clone(), Arc::from(""), None, 0, pool)
        }
    };
    let frame = match frame {
        Ok(f) => f,
        Err(e) => {
            sink(Event::RunFailed { error: e.to_string(), usage: shared.total_usage(), elapsed_ms: 0 });
            return;
        }
    };

    let budget = engine.limits.max_duration_ms.map(|ms| ms.saturating_sub(prior_elapsed));
    let outcome = match budget {
        Some(ms) if engine.clock.supports_timers() => {
            let run = frame.run();
            futures::pin_mut!(run);
            match futures::future::select(run, engine.clock.sleep(Duration::from_millis(ms))).await {
                Either::Left((outcome, _)) => outcome,
                Either::Right(((), _)) => {
                    control.stop(Stop::Fail(format!(
                        "run exceeded its duration limit of {} ms",
                        engine.limits.max_duration_ms.unwrap_or_default()
                    )));
                    FrameOutcome::Stopped
                }
            }
        }
        _ => frame.run().await,
    };

    let elapsed_ms = prior_elapsed.saturating_add(engine.clock.now_ms().saturating_sub(started_ms));
    let usage = shared.total_usage();
    let terminal = match outcome {
        FrameOutcome::Completed { outputs, .. } => Event::RunSucceeded {
            outputs,
            usage,
            handled_errors: shared.handled_errors.load(Ordering::Acquire),
            elapsed_ms,
        },
        FrameOutcome::Failed { error, .. } => Event::RunFailed { error: error.to_string(), usage, elapsed_ms },
        FrameOutcome::Paused { snapshot, .. } => {
            let mut reasons = control.reasons();
            if reasons.is_empty() && control.pause_requested() {
                reasons.push(PauseReason {
                    frame: Arc::from(""),
                    node_id: None,
                    kind: "external".to_owned(),
                    data: Value::Null,
                });
            }
            Event::RunPaused {
                reasons,
                snapshot: Box::new(Snapshot {
                    format: SNAPSHOT_FORMAT,
                    run_id,
                    graph_fingerprint: engine.graph.fingerprint().to_owned(),
                    elapsed_ms,
                    steps: shared.steps.load(Ordering::Acquire),
                    usage,
                    handled_errors: shared.handled_errors.load(Ordering::Acquire),
                    root: *snapshot,
                }),
            }
        }
        FrameOutcome::Stopped => match control.stop_reason() {
            Some(Stop::Fail(error)) => Event::RunFailed { error, usage, elapsed_ms },
            Some(Stop::Abort(reason)) => Event::RunAborted { reason, elapsed_ms },
            None => Event::RunAborted { reason: "stopped".to_owned(), elapsed_ms },
        },
    };
    sink(terminal);
}

/// Re-apply host-provided `sys` and `conversation` values onto a restored root frame.
fn f_apply(frame: &mut Frame, sys: &IndexMap<String, Value>, conversation: &IndexMap<String, Value>) {
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
