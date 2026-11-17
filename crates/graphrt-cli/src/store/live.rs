//! Runs executing in the server: a background task drives each run, records it in
//! the [`RunStore`] and fans its events out to any number of listeners.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use futures::StreamExt;
use graphrt_core::Event;
use graphrt_core::engine::{RunHandle, RunStream};
use graphrt_core::response::ResponseAssembler;
use graphrt_core::store::{RunRecord, RunStore, event_to_json, now_ms};
use tokio::sync::{broadcast, watch};

/// Events kept in memory per active run for late subscribers.
const FEED_CAP: usize = 10_000;
/// Broadcast buffer per run; slower listeners skip ahead (and are told so).
const CHANNEL_CAP: usize = 4_096;
/// Stored events are written in batches of this size (and at every terminal event).
const FLUSH_EVERY: usize = 32;

#[derive(Debug)]
struct Feed {
    events: Vec<Event>,
    dropped: bool,
    tx: broadcast::Sender<Event>,
    terminal: Option<Event>,
    answer: String,
}

/// A run executing in this process.
#[derive(Debug)]
pub struct ActiveRun {
    pub run_id: String,
    handle: RunHandle,
    /// Events already stored before this execution (a resumed run's earlier part).
    pub stored_before: u64,
    feed: Mutex<Feed>,
    done_tx: watch::Sender<bool>,
}

/// What a listener receives when it subscribes.
#[derive(Debug)]
pub struct Subscription {
    /// Events emitted before subscribing (this execution only).
    pub backlog: Vec<Event>,
    /// Whether the backlog is incomplete because the in-memory cap was reached.
    pub backlog_truncated: bool,
    pub live: broadcast::Receiver<Event>,
    pub done: bool,
}

/// Final state of a finished execution.
#[derive(Debug, Clone)]
pub struct Outcome {
    pub terminal: Option<Event>,
    pub answer: String,
}

impl ActiveRun {
    fn feed(&self) -> MutexGuard<'_, Feed> {
        self.feed.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn handle(&self) -> &RunHandle {
        &self.handle
    }

    /// Backlog plus a live receiver, taken atomically so no event is missed or repeated.
    pub fn subscribe(&self) -> Subscription {
        let feed = self.feed();
        Subscription {
            backlog: feed.events.clone(),
            backlog_truncated: feed.dropped,
            live: feed.tx.subscribe(),
            done: feed.terminal.is_some(),
        }
    }

    /// Wait until the run finished and return its terminal event and answer text.
    pub async fn wait(&self) -> Outcome {
        let mut done = self.done_tx.subscribe();
        // `wait_for` returns immediately when the run already finished; an error means
        // the driver is gone, in which case the feed holds whatever it reached.
        let _finished = done.wait_for(|d| *d).await;
        let feed = self.feed();
        Outcome { terminal: feed.terminal.clone(), answer: feed.answer.clone() }
    }

    pub fn is_done(&self) -> bool {
        *self.done_tx.borrow()
    }
}

/// Called when an execution ends (to drop it from the active set).
pub type OnFinish = Box<dyn FnOnce(&str) + Send>;

/// Start driving `run` on the tokio runtime. `record` must already exist in `store`.
pub fn spawn(
    run: RunStream,
    assembler: ResponseAssembler,
    record: RunRecord,
    store: Arc<dyn RunStore>,
    on_finish: OnFinish,
    tracer: crate::otel::RunTracer,
) -> Arc<ActiveRun> {
    let (tx, _) = broadcast::channel(CHANNEL_CAP);
    let (done_tx, _) = watch::channel(false);
    let active = Arc::new(ActiveRun {
        run_id: record.run_id.clone(),
        handle: run.handle(),
        stored_before: record.event_count,
        feed: Mutex::new(Feed { events: Vec::new(), dropped: false, tx, terminal: None, answer: String::new() }),
        done_tx,
    });
    tokio::spawn(drive(run, assembler, record, store, Arc::clone(&active), on_finish, tracer));
    active
}

async fn drive(
    mut run: RunStream,
    mut assembler: ResponseAssembler,
    mut record: RunRecord,
    store: Arc<dyn RunStore>,
    active: Arc<ActiveRun>,
    on_finish: OnFinish,
    mut tracer: crate::otel::RunTracer,
) {
    let run_id = record.run_id.clone();
    let mut pending: Vec<serde_json::Value> = Vec::new();
    while let Some(event) = run.next().await {
        tracer.on_event(&event);
        assembler.push(&event);
        let terminal = event.is_terminal();
        pending.push(event_to_json(&event));
        {
            let mut feed = active.feed();
            if feed.events.len() < FEED_CAP {
                feed.events.push(event.clone());
            } else {
                feed.dropped = true;
            }
            // No receivers is fine: the run is recorded either way.
            let _receivers = feed.tx.send(event.clone());
            if terminal {
                feed.terminal = Some(event.clone());
                feed.answer = assembler.text_so_far().to_owned();
            }
        }
        if (pending.len() >= FLUSH_EVERY || terminal)
            && let Err(e) = store.append_events(&run_id, std::mem::take(&mut pending)).await
        {
            eprintln!("warning: run {run_id}: cannot store events: {e}");
        }
        if terminal {
            if let Some(snapshot) = record.apply_terminal(&event, now_ms())
                && let Err(e) = store.save_snapshot(&run_id, &snapshot).await
            {
                eprintln!("warning: run {run_id}: cannot store snapshot, it will not be resumable by id: {e}");
            }
            record.answer = assembler.text_so_far().to_owned();
            // `put` keeps the store's own event counters.
            if let Err(e) = store.put(record.clone()).await {
                eprintln!("warning: run {run_id}: cannot store final status: {e}");
            }
        }
    }
    if !pending.is_empty()
        && let Err(e) = store.append_events(&run_id, pending).await
    {
        eprintln!("warning: run {run_id}: cannot store events: {e}");
    }
    on_finish(&run_id);
    active.done_tx.send_replace(true);
}
