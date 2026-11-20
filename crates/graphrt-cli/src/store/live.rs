//! Runs executing in the server: a background task drives each run, records it in
//! the [`RunStore`] and fans its events out to any number of listeners.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use futures::StreamExt;
use graphrt_core::engine::{RunHandle, RunStream};
use graphrt_core::response::ResponseAssembler;
use graphrt_core::store::{RunRecord, RunStore, StoredEvent, event_to_stored, now_ms};
use graphrt_core::{Event, Value};
use tokio::sync::{broadcast, watch};

/// Events kept in memory per active run for late subscribers.
const FEED_CAP: usize = 10_000;
/// Broadcast buffer per run; slower listeners skip ahead (and are told so). The
/// channel is created by the first subscriber: most runs (blocking and async
/// requests) never have one, and allocating it for every run cost about a third
/// of the server's CPU time. Slots hold an `Arc<Event>` (8 bytes, not a
/// ~256-byte event), so a streaming run's channel is a few tens of KiB instead
/// of over a MiB.
const CHANNEL_CAP: usize = 4_096;

/// An event shared by the feed, the broadcast channel and its listeners
/// (one allocation per event instead of a deep copy per holder).
pub type SharedEvent = Arc<Event>;
/// Stored events are written in batches of this size (and at every terminal event).
const FLUSH_EVERY: usize = 32;

#[derive(Debug)]
struct Feed {
    events: Vec<SharedEvent>,
    dropped: bool,
    /// Created on the first [`ActiveRun::subscribe`].
    tx: Option<broadcast::Sender<SharedEvent>>,
    terminal: Option<SharedEvent>,
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
    pub backlog: Vec<SharedEvent>,
    /// Whether the backlog is incomplete because the in-memory cap was reached.
    pub backlog_truncated: bool,
    pub live: broadcast::Receiver<SharedEvent>,
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
        let mut feed = self.feed();
        // Created under the feed lock the driver also sends under, so the backlog
        // and the live receiver never miss or repeat an event.
        let live = feed.tx.get_or_insert_with(|| broadcast::channel(CHANNEL_CAP).0).subscribe();
        Subscription {
            backlog: feed.events.clone(),
            backlog_truncated: feed.dropped,
            live,
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
        Outcome { terminal: feed.terminal.as_deref().cloned(), answer: feed.answer.clone() }
    }

    pub fn is_done(&self) -> bool {
        *self.done_tx.borrow()
    }
}

/// What the driver copies from a run's events into its record, besides status.
#[derive(Debug, Default, Clone)]
pub struct Capture {
    /// Root start node: its validated outputs become `RunRecord::inputs`
    /// (`--store-inputs`). `None` keeps inputs out of the record.
    pub start_node: Option<Arc<str>>,
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
    capture: Capture,
) -> Arc<ActiveRun> {
    let (done_tx, _) = watch::channel(false);
    let active = Arc::new(ActiveRun {
        run_id: record.run_id.clone(),
        handle: run.handle(),
        stored_before: record.event_count,
        feed: Mutex::new(Feed { events: Vec::new(), dropped: false, tx: None, terminal: None, answer: String::new() }),
        done_tx,
    });
    let recorder = Recorder { tracer, capture, ids_pending: true };
    tokio::spawn(drive(run, assembler, record, store, Arc::clone(&active), on_finish, recorder));
    active
}

/// Feeds the tracer and copies trace ids and (opt-in) inputs into the record.
#[derive(Debug)]
struct Recorder {
    tracer: crate::otel::RunTracer,
    capture: Capture,
    /// Trace ids not copied yet (only ever true while tracing).
    ids_pending: bool,
}

impl Recorder {
    fn on_event(&mut self, event: &Event, record: &mut RunRecord) {
        self.tracer.on_event(event);
        if self.ids_pending {
            self.ids_pending = false;
            if let Some((trace_id, span_id)) = self.tracer.ids() {
                record.trace_id = Some(trace_id);
                record.span_id = Some(span_id);
            }
        }
        if let Some(start) = &self.capture.start_node
            && let Event::NodeSucceeded { frame, node_id, outputs, .. } = event
            && frame.is_empty()
            && node_id == start
        {
            // Through JSON so secret values are stored as their mask, never in memory.
            let inputs = outputs.iter().map(|(k, v)| (k.to_string(), Value::from_json(v.to_json()))).collect();
            record.inputs = Some(inputs);
        }
    }
}

async fn drive(
    mut run: RunStream,
    mut assembler: ResponseAssembler,
    mut record: RunRecord,
    store: Arc<dyn RunStore>,
    active: Arc<ActiveRun>,
    on_finish: OnFinish,
    mut recorder: Recorder,
) {
    let run_id = record.run_id.clone();
    let mut pending: Vec<StoredEvent> = Vec::new();
    // Only the assembled text is kept (`text_so_far`); the chunks themselves are dropped.
    let mut chunks = Vec::new();
    while let Some(event) = run.next().await {
        recorder.on_event(&event, &mut record);
        assembler.push_into(&event, &mut chunks);
        chunks.clear();
        let terminal = event.is_terminal();
        pending.push(event_to_stored(&event));
        let event = Arc::new(event);
        {
            let mut feed = active.feed();
            if feed.events.len() < FEED_CAP {
                feed.events.push(Arc::clone(&event));
            } else {
                feed.dropped = true;
            }
            // No receivers is fine: the run is recorded either way.
            if let Some(tx) = &feed.tx {
                let _receivers = tx.send(Arc::clone(&event));
            }
            if terminal {
                feed.terminal = Some(Arc::clone(&event));
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
