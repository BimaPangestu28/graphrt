//! Reclaim grace: what happens to a gateway-started run whose stream broke.
//!
//! A public streaming request aborts its run when the caller goes away. A run
//! the gateway started is not the caller's to lose: the gateway may only have
//! restarted or lost the connection. So when its last stream is dropped before
//! the run finished:
//!
//! 1. the agent waits a few seconds ([`super::DEFAULT_REATTACH_WINDOW`]) for the gateway to reattach
//!    (`GET /internal/v1/runs/{id}/stream`);
//! 2. otherwise it preempts the run (in-flight nodes are cancelled and run
//!    again on resume: at-least-once per node) and keeps the outcome, a
//!    `run_paused` with reason `preempted` and the snapshot, as an [`Orphan`]
//!    for the reclaim grace;
//! 3. the gateway pulls the orphan (`GET /internal/v1/orphans`, then
//!    `DELETE /internal/v1/orphans/{id}`): the agent never calls the gateway,
//!    so it needs no gateway address and no HTTP client;
//! 4. an orphan nobody took over within the grace is dropped. Deciding that the
//!    run is lost (`failed{owner_lost}`) is the gateway's job, which owns the
//!    run's record; this agent's store is only a cache of its executions.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use graphrt_core::store::now_ms;
use serde::Serialize;

use super::GatewayAgent;
use crate::store::live::ActiveRun;

/// Pause reason kind of a run preempted because its gateway stream broke.
pub const PREEMPTED: &str = "preempted";
/// `cause` of that pause reason.
pub const DISCONNECTED_CAUSE: &str = "gateway_disconnected";

fn preempted_reason() -> graphrt_core::event::PauseReason {
    graphrt_core::event::PauseReason {
        frame: Arc::from(""),
        node_id: None,
        kind: PREEMPTED.to_owned(),
        data: graphrt_core::Value::object([("cause", graphrt_core::Value::from(DISCONNECTED_CAUSE))]),
    }
}

/// The outcome of an execution the gateway lost track of.
#[derive(Clone, Debug, Serialize)]
pub struct Orphan {
    pub run_id: String,
    pub epoch: u64,
    pub deployment: String,
    /// Final status of the execution (`paused` unless it finished meanwhile).
    pub status: String,
    /// Global index after the execution's last event: events from the gateway's
    /// own count up to here can be fetched with the reattach stream.
    pub event_count: u64,
    /// The terminal event as streamed (a `run_paused` event carries its snapshot).
    pub terminal: Box<serde_json::value::RawValue>,
    /// Assembled answer text of the execution.
    pub answer: String,
    pub orphaned_ms: u64,
}

impl Orphan {
    /// The orphan as JSON text, through a JSON tree: the one serializer copy the
    /// list and the report share.
    pub fn to_json(&self) -> String {
        serde_json::to_value(self).map(|json| json.to_string()).unwrap_or_default()
    }
}

impl GatewayAgent {
    pub(crate) fn orphan_list(&self) -> Vec<Orphan> {
        // Unordered: the gateway keys orphans by run id.
        self.orphans().values().cloned().collect()
    }

    /// Drop the orphan of `run_id` at `epoch`; returns whether there was one.
    pub(crate) fn release_orphan(&self, run_id: &str, epoch: u64) -> bool {
        let mut orphans = self.orphans();
        if orphans.get(run_id).is_some_and(|o| o.epoch == epoch) {
            orphans.remove(run_id);
            return true;
        }
        false
    }
}

/// Holds one gateway stream's attachment to an execution; see the module docs.
#[derive(Debug)]
pub(crate) struct ReclaimGuard {
    agent: Arc<GatewayAgent>,
    active: Arc<ActiveRun>,
    run_id: String,
    epoch: u64,
    /// The terminal event went out on this stream.
    terminal_sent: AtomicBool,
}

impl ReclaimGuard {
    pub fn new(agent: Arc<GatewayAgent>, active: Arc<ActiveRun>, run_id: &str, epoch: u64) -> Self {
        Self { agent, active, run_id: run_id.to_owned(), epoch, terminal_sent: AtomicBool::new(false) }
    }
}

impl crate::serve::StreamGuard for ReclaimGuard {
    fn terminal_sent(&self) {
        self.terminal_sent.store(true, Ordering::Relaxed);
    }

    /// `hello {run_id, epoch, protocol}`, the first event of a gateway stream.
    fn first_event(&self) -> Option<axum::response::sse::Event> {
        let hello = super::json::Object::new()
            .str("run_id", &self.run_id)
            .num("epoch", self.epoch)
            .num("protocol", super::PROTOCOL_VERSION.into())
            .finish();
        Some(axum::response::sse::Event::default().event("hello").data(hello))
    }
}

impl Drop for ReclaimGuard {
    fn drop(&mut self) {
        let remaining = self.agent.detach(&self.run_id, self.epoch);
        if remaining > 0 || self.terminal_sent.load(Ordering::Relaxed) {
            return;
        }
        // Dropped outside a runtime only at shutdown, when nothing can be reclaimed anyway.
        let Ok(runtime) = tokio::runtime::Handle::try_current() else { return };
        let task = Reclaim {
            agent: Arc::clone(&self.agent),
            active: Arc::clone(&self.active),
            run_id: self.run_id.clone(),
            epoch: self.epoch,
        };
        super::spawn_task(&runtime, Box::pin(task.run()));
    }
}

/// The reclaim of one execution whose last stream broke.
struct Reclaim {
    agent: Arc<GatewayAgent>,
    active: Arc<ActiveRun>,
    run_id: String,
    epoch: u64,
}

impl Reclaim {
    async fn run(self) {
        tokio::time::sleep(self.agent.config.reattach_window).await;
        if self.agent.attached(&self.run_id, self.epoch) > 0 {
            return;
        }
        if !self.active.is_done() {
            eprintln!("run {}: gateway stream lost; preempting it for reclaim", self.run_id);
            self.active.handle().preempt_with(Some(preempted_reason()));
        }
        let outcome = self.active.wait().await;
        // A gateway that reattached meanwhile received the outcome on its stream.
        if self.agent.attached(&self.run_id, self.epoch) > 0 {
            return;
        }
        let Some(orphan) = self.orphan(outcome) else { return };
        self.agent.orphans().insert(self.run_id.clone(), orphan.clone());
        // The gateway pulls it (`GET /internal/v1/orphans`) and releases it with
        // `DELETE`; one nobody took over is dropped at the end of the grace.
        tokio::time::sleep(self.agent.config.reclaim_grace).await;
        self.agent.release_orphan(&self.run_id, self.epoch);
    }

    fn orphan(&self, outcome: crate::store::live::Outcome) -> Option<Orphan> {
        let terminal = outcome.terminal?;
        let status = match &terminal {
            graphrt_core::Event::RunPaused { .. } => "paused",
            graphrt_core::Event::RunSucceeded { .. } => "succeeded",
            graphrt_core::Event::RunFailed { .. } => "failed",
            _ => "aborted",
        };
        let execution = self.agent.execution(&self.run_id)?;
        // Every event of the execution, including any beyond the in-memory feed's cap.
        let events = self.active.subscribe().next;
        Some(Orphan {
            run_id: self.run_id.clone(),
            epoch: self.epoch,
            deployment: self.agent.deployment().to_owned(),
            status: status.to_owned(),
            event_count: execution.base_index + self.active.stored_before + events,
            // Through text, like every other event: one copy of the event serializer.
            terminal: serde_json::to_string(&terminal)
                .ok()
                .and_then(|text| serde_json::value::RawValue::from_string(text).ok())
                .unwrap_or_else(|| serde_json::value::RawValue::NULL.to_owned()),
            answer: outcome.answer,
            orphaned_ms: now_ms(),
        })
    }
}
