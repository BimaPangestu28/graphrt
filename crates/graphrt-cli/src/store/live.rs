//! Runs executing in the server: a background task drives each run, records it in
//! the [`RunStore`] and fans its events out to any number of listeners.
//!
//! Events are written to the store in batches of [`FLUSH_EVERY`] events or after
//! [`FLUSH_AFTER`], whichever comes first, so a replay (`/events`) or another
//! replica reading a shared store trails a live run by at most a quarter second.
//! Store writes that fail with an I/O error are retried with bounded backoff.
//!
//! The run goes on while its events are written (see [`Batches`]): a slow store
//! delays the replay, never the run's nodes, its preemption or its cancellation.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll};
use std::time::Duration;

use futures::{Stream, StreamExt};
use graphrt_core::engine::{RunHandle, RunStream};
use graphrt_core::response::ResponseAssembler;
use graphrt_core::store::{
    FinalWrite, RunRecord, RunStore, StoreError, StoreResult, StoredEvent, event_to_stored, now_ms,
};
use graphrt_core::{Event, Value};
use tokio::sync::{broadcast, watch};

/// Events kept in memory per active run for late subscribers.
const FEED_CAP: usize = 10_000;
/// Broadcast buffer per run; slower listeners are refilled from the feed (see
/// [`follow`]). The
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
/// ... or once the oldest unwritten event is this old.
const FLUSH_AFTER: Duration = Duration::from_millis(250);
/// Retries of a store write that failed with an I/O error, with doubling delays
/// from [`RETRY_FIRST_DELAY`]: 25 + 50 + 100 + 200 + 400 ms, so a write gives up
/// after about 0.8 s and a shutdown drain stays within its deadline.
const STORE_RETRIES: u32 = 5;
const RETRY_FIRST_DELAY: Duration = Duration::from_millis(25);

#[derive(Debug)]
struct Feed {
    /// The first [`FEED_CAP`] events of this execution.
    events: Vec<SharedEvent>,
    dropped: bool,
    /// Events emitted so far (including the ones beyond the cap).
    total: u64,
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
    /// No caller waits for the run (async starts, webhooks, resumes after a
    /// restart): after a shutdown drain the server resumes it by itself.
    detached: AtomicBool,
    /// What the answer assembler of this execution started from (see [`AssemblerSeed`]).
    #[cfg(feature = "run-follow")]
    seed: std::sync::OnceLock<AssemblerSeed>,
}

/// What the answer assembler of an execution starts from: the graph, the
/// caller's `sys` values and, for a resumed run, the state it resumed from.
/// A caller that follows the run later (`GET /v1/runs/{id}/events?responses=true`)
/// builds an assembler of its own from it and gets the same answer text, with
/// the same ids, as the caller that started the run.
#[cfg(feature = "run-follow")]
#[derive(Clone, Debug)]
pub struct AssemblerSeed {
    pub graph: Arc<graphrt_core::Graph>,
    pub sys: indexmap::IndexMap<String, Value>,
    pub resumed: Option<graphrt_core::snapshot::FrameSnapshot>,
}

#[cfg(feature = "run-follow")]
impl AssemblerSeed {
    /// A new assembler in the state this execution's own assembler started in.
    pub fn assembler(&self) -> ResponseAssembler {
        let mut assembler = ResponseAssembler::new(&self.graph);
        if let Some(root) = &self.resumed {
            assembler = assembler.resume_from(root);
        }
        for (key, value) in &self.sys {
            assembler = assembler.with_value(&graphrt_core::Selector::of("sys", key), value.clone());
        }
        assembler
    }
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
    /// Index (in this execution) of the first event `live` yields.
    pub next: u64,
}

/// See [`ActiveRun::resubscribe`].
#[derive(Debug)]
struct Refill {
    events: Vec<SharedEvent>,
    gap: u64,
    live: broadcast::Receiver<SharedEvent>,
    next: u64,
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
        let mut backlog = feed.events.clone();
        if feed.dropped
            && let Some(terminal) = &feed.terminal
        {
            // The terminal event is past the cap; listeners still need it.
            backlog.push(Arc::clone(terminal));
        }
        Subscription { backlog, backlog_truncated: feed.dropped, live, done: feed.terminal.is_some(), next: feed.total }
    }

    /// Catch up a listener that lagged behind the broadcast channel: the events
    /// from index `next` that the feed still holds (ending with the terminal event
    /// once the run finished), a new receiver, and how many events in between are
    /// gone (only events beyond the feed's cap can be).
    fn resubscribe(&self, next: u64) -> Refill {
        let mut feed = self.feed();
        let live = feed.tx.get_or_insert_with(|| broadcast::channel(CHANNEL_CAP).0).subscribe();
        let from = usize::try_from(next).unwrap_or(usize::MAX);
        let mut events = feed.events.get(from..).map(<[SharedEvent]>::to_vec).unwrap_or_default();
        let held = u64::try_from(feed.events.len()).unwrap_or(u64::MAX);
        let mut gap = feed.total.saturating_sub(next.max(held));
        if let Some(terminal) = &feed.terminal
            && !events.last().is_some_and(|event| event.is_terminal())
        {
            events.push(Arc::clone(terminal));
            gap = gap.saturating_sub(1);
        }
        Refill { events, gap, live, next: feed.total }
    }

    /// Hand `event` to the feed and the listeners. `answer` gives the final answer
    /// text when the event is terminal.
    fn publish(&self, event: &SharedEvent, answer: impl FnOnce() -> String) {
        let mut feed = self.feed();
        if feed.events.len() < FEED_CAP {
            feed.events.push(Arc::clone(event));
        } else {
            feed.dropped = true;
        }
        feed.total += 1;
        // No receivers is fine: the run is recorded either way.
        if let Some(tx) = &feed.tx {
            let _receivers = tx.send(Arc::clone(event));
        }
        if event.is_terminal() {
            feed.terminal = Some(Arc::clone(event));
            feed.answer = answer();
        }
    }

    /// Remember what this execution's answer assembler starts from. Call it
    /// before the driver starts (in [`Tracking::on_start`]); a second call is ignored.
    #[cfg(feature = "run-follow")]
    pub fn set_assembler_seed(&self, seed: AssemblerSeed) {
        let _already_set = self.seed.set(seed);
    }

    /// An answer assembler for a listener that subscribes from the start of
    /// this execution, when the run was launched with a seed.
    #[cfg(feature = "run-follow")]
    pub fn new_assembler(&self) -> Option<ResponseAssembler> {
        self.seed.get().map(AssemblerSeed::assembler)
    }

    /// Mark the run as one no caller waits for.
    pub fn set_detached(&self) {
        self.detached.store(true, Ordering::Release);
    }

    /// See [`ActiveRun::set_detached`].
    pub fn is_detached(&self) -> bool {
        self.detached.load(Ordering::Acquire)
    }

    /// Kind of the terminal event once the run finished (`run_paused`, ...).
    pub fn terminal_kind(&self) -> Option<&'static str> {
        self.feed().terminal.as_deref().map(|event| match event {
            Event::RunSucceeded { .. } => "succeeded",
            Event::RunPaused { .. } => "paused",
            Event::RunAborted { .. } => "aborted",
            _ => "failed",
        })
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
    /// Online evaluation pool, when an evaluator covers the run's workflow.
    #[cfg(feature = "online-eval")]
    pub online: Option<Arc<crate::online_eval::OnlineEval>>,
    /// The conversation the run's record names (`session_id`), on a server that
    /// keeps conversations: it gets the run's turn when the run succeeds.
    #[cfg(feature = "conversations")]
    pub conversation: Option<crate::conversations::Turn>,
    /// Who hears when the run fails or waits for a person.
    #[cfg(feature = "notify")]
    pub notify: Option<Arc<crate::notify::Notifications>>,
    /// The messaging channels, for a run of a channel conversation: its answer,
    /// or its question, goes to the customer when the execution ends.
    #[cfg(feature = "channels")]
    pub channels: Option<Arc<crate::channels::Channels>>,
}

/// Called when an execution ends (to drop it from the active set).
pub type OnFinish = Box<dyn FnOnce(&str) + Send>;

/// Called with the new execution before its driver starts (to add it to the active set).
pub type OnStart = Box<dyn FnOnce(&Arc<ActiveRun>)>;

/// How the caller follows an execution. `on_start` runs before the driver is
/// spawned, so whatever it registers is in place before `on_finish` can run,
/// even when the run ends at once on another worker thread.
pub struct Tracking {
    pub on_start: OnStart,
    pub on_finish: OnFinish,
}

impl std::fmt::Debug for Tracking {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tracking").finish_non_exhaustive()
    }
}

/// Start driving `run` on the tokio runtime. `record` must already exist in `store`.
pub fn spawn(
    run: RunStream,
    assembler: ResponseAssembler,
    record: RunRecord,
    store: Arc<dyn RunStore>,
    tracking: Tracking,
    tracer: crate::otel::RunTracer,
    capture: Capture,
) -> Arc<ActiveRun> {
    let (done_tx, _) = watch::channel(false);
    let active = Arc::new(ActiveRun {
        run_id: record.run_id.clone(),
        handle: run.handle(),
        stored_before: record.event_count,
        feed: Mutex::new(Feed {
            events: Vec::new(),
            dropped: false,
            total: 0,
            tx: None,
            terminal: None,
            answer: String::new(),
        }),
        done_tx,
        detached: AtomicBool::new(false),
        #[cfg(feature = "run-follow")]
        seed: std::sync::OnceLock::new(),
    });
    let recorder = Recorder {
        tracer,
        capture,
        ids_pending: true,
        #[cfg(feature = "conversations")]
        conversation_variables: indexmap::IndexMap::new(),
        #[cfg(feature = "conversations")]
        tool_digest: String::new(),
        #[cfg(feature = "channels")]
        produced_files: Vec::new(),
    };
    let Tracking { on_start, on_finish } = tracking;
    on_start(&active);
    graphrt_core::task::spawn(drive(run, assembler, record, store, Arc::clone(&active), on_finish, recorder));
    active
}

/// Feeds the tracer and copies trace ids and (opt-in) inputs into the record.
#[derive(Debug)]
struct Recorder {
    tracer: crate::otel::RunTracer,
    capture: Capture,
    /// Trace ids not copied yet (only ever true while tracing).
    ids_pending: bool,
    /// The `conversation` variables this execution wrote, for its conversation.
    #[cfg(feature = "conversations")]
    conversation_variables: indexmap::IndexMap<String, serde_json::Value>,
    /// `turn_digest` outputs of the turn, joined.
    #[cfg(feature = "conversations")]
    tool_digest: String,
    /// The files the run's nodes produced (their `files` outputs), for its channel.
    #[cfg(feature = "channels")]
    produced_files: Vec<graphrt_core::value::File>,
}

/// Bytes of tool digest kept for one turn of a conversation.
#[cfg(feature = "conversations")]
const MAX_TOOL_DIGEST_BYTES: usize = 4_000;
/// Most files of one run sent to a channel.
#[cfg(feature = "channels")]
const MAX_PRODUCED_FILES: usize = 10;

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
        #[cfg(feature = "conversations")]
        if self.capture.conversation.is_some()
            && let Event::VariableUpdated { selector, value, .. } = event
            && selector.namespace() == "conversation"
        {
            // Through JSON so a secret is stored as its mask.
            self.conversation_variables.insert(selector.name().to_owned(), value.to_json());
        }
        // What the agents' tools did in this turn, for the next turn's history.
        #[cfg(feature = "conversations")]
        if self.capture.conversation.is_some()
            && let Event::NodeSucceeded { outputs, .. } = event
            && let Some(Value::String(digest)) = outputs.get("turn_digest")
            && self.tool_digest.len() < MAX_TOOL_DIGEST_BYTES
        {
            if !self.tool_digest.is_empty() {
                self.tool_digest.push('\n');
            }
            let room = MAX_TOOL_DIGEST_BYTES - self.tool_digest.len();
            let mut end = digest.len().min(room);
            while !digest.is_char_boundary(end) {
                end -= 1;
            }
            self.tool_digest.push_str(digest.get(..end).unwrap_or_default());
        }
        #[cfg(feature = "channels")]
        if self.capture.channels.is_some()
            && let Event::NodeSucceeded { outputs, .. } = event
            && let Some(Value::Array(files)) = outputs.get("files")
        {
            for file in files.iter() {
                let Value::File(file) = file else { continue };
                let known = self.produced_files.iter().any(|other| other.reference == file.reference);
                if !known && self.produced_files.len() < MAX_PRODUCED_FILES {
                    self.produced_files.push((**file).clone());
                }
            }
        }
    }

    /// Hand the execution's end to its conversation, before the terminal event
    /// goes out: a client that sends its next message as soon as it sees the
    /// answer finds this turn in the history.
    #[cfg(feature = "conversations")]
    async fn finish_turn(&mut self, run_id: &str, terminal: &Event, answer: &str) {
        use crate::conversations::TurnEnd;
        let Some(turn) = &self.capture.conversation else { return };
        let end = match terminal {
            Event::RunSucceeded { .. } => TurnEnd::Succeeded,
            Event::RunPaused { .. } => TurnEnd::Paused,
            _ => TurnEnd::Dropped,
        };
        let tools = Some(std::mem::take(&mut self.tool_digest)).filter(|digest| !digest.is_empty());
        turn.finish_with_tools(run_id, end, answer, std::mem::take(&mut self.conversation_variables), tools).await;
    }
}

/// The background task of a run: record, publish and store each event until the run ends.
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
    let writer = EventWriter {
        run_id: run_id.clone(),
        store: Arc::clone(&store),
        first_index: record.event_count,
        written: 0,
        behind: Vec::new(),
    };
    let mut batches = Batches::new(writer);
    // Only the assembled text is kept (`text_so_far`); the chunks themselves are dropped.
    let mut chunks = Vec::new();
    let terminal = loop {
        let next = std::future::poll_fn(|cx| batches.poll_next(&mut run, active.handle(), cx)).await;
        let event = match next {
            Next::Wrote => continue,
            Next::Event(None) => break None,
            Next::Event(Some(event)) => event,
        };
        recorder.on_event(&event, &mut record);
        assembler.push_into(&event, &mut chunks);
        chunks.clear();
        let stored = event_to_stored(&event);
        if !event.is_terminal() {
            active.publish(&Arc::new(event), String::new);
            batches.push(stored);
            continue;
        }
        #[cfg(feature = "conversations")]
        recorder.finish_turn(&run_id, &event, assembler.text_so_far()).await;
        let event = Arc::new(event);
        active.publish(&event, || assembler.text_so_far().to_owned());
        batches.push_last(stored);
        break Some(event);
    };
    // The engine is done: from here on the driver waits for the store.
    let (mut writer, last) = batches.finish().await;
    match terminal {
        Some(event) => {
            store_final_state(&*store, &mut writer, &mut record, &event, assembler.text_so_far(), last).await;
            // After the state is stored: whoever follows the notice finds it.
            #[cfg(feature = "notify")]
            if let Some(notifications) = &recorder.capture.notify {
                notifications.run_ended(&record);
            }
            #[cfg(feature = "channels")]
            if let Some(channels) = &recorder.capture.channels {
                channels.run_ended(&record, std::mem::take(&mut recorder.produced_files));
            }
        }
        None => writer.write(last).await,
    }
    on_finish(&run_id);
    active.done_tx.send_replace(true);
    // After the waiters are woken, so evaluation never delays a response.
    #[cfg(feature = "online-eval")]
    if let Some(online) = &recorder.capture.online {
        online.submit(&record, &active.feed().events);
    }
}

/// Unwritten events a run may hold while a write of its events is in flight.
/// Beyond it the driver takes no more events from the engine until the write
/// is done (backpressure), unless the run was asked to stop: a paused,
/// preempted or aborted run always gets to its terminal event.
const MAX_QUEUED: usize = 4 * FEED_CAP;
/// How often a run held back by [`MAX_QUEUED`] checks whether it was asked to stop.
const STOP_CHECK: Duration = Duration::from_millis(10);

/// A write of a run's events; it hands the writer back when done.
type Write = futures::future::BoxFuture<'static, EventWriter>;

/// Where a run's [`EventWriter`] is: free, or in a write.
enum Writer {
    Idle(EventWriter),
    Busy(Write),
}

/// What the driver gets from [`Batches::poll_next`].
enum Next {
    /// The engine's next event (`None`: the run's stream ended).
    Event(Option<Event>),
    /// A write finished or started; nothing for the driver to do.
    Wrote,
}

/// A run's events on their way to the store, in batches of [`FLUSH_EVERY`]
/// events or [`FLUSH_AFTER`] after the first, one write at a time.
///
/// A write is polled next to the engine, never in its place: the run's nodes go
/// on (and so do its preemption and cancellation) while the store is slow.
/// Events that come during a write are held and go out together in the next one.
struct Batches {
    /// Events not handed to a write yet.
    pending: Vec<StoredEvent>,
    /// `pending` is to be written as soon as the writer is free.
    due: bool,
    /// Fires [`FLUSH_AFTER`] after the first event of a batch (a timer per
    /// batch, not per event).
    timer: Pin<Box<tokio::time::Sleep>>,
    timer_armed: bool,
    writer: Writer,
    /// [`MAX_QUEUED`] (lower in tests).
    max_queued: usize,
    /// Armed only while the run is held back by [`MAX_QUEUED`].
    stop_check: Option<Pin<Box<tokio::time::Sleep>>>,
}

impl Batches {
    fn new(writer: EventWriter) -> Self {
        Self {
            pending: Vec::new(),
            due: false,
            timer: Box::pin(tokio::time::sleep(FLUSH_AFTER)),
            timer_armed: false,
            writer: Writer::Idle(writer),
            max_queued: MAX_QUEUED,
            stop_check: None,
        }
    }

    /// Queue a (non terminal) event.
    fn push(&mut self, event: StoredEvent) {
        self.pending.push(event);
        if self.pending.len() >= FLUSH_EVERY {
            self.due = true;
            self.start_write();
        } else if !self.timer_armed {
            self.timer.as_mut().reset(tokio::time::Instant::now() + FLUSH_AFTER);
            self.timer_armed = true;
        }
    }

    /// Queue the terminal event: it goes out with the run's final state.
    fn push_last(&mut self, event: StoredEvent) {
        self.pending.push(event);
    }

    /// Hand the pending events to the writer when they are due and it is free.
    fn start_write(&mut self) {
        if !self.due || self.pending.is_empty() {
            return;
        }
        let Writer::Idle(_) = &self.writer else { return };
        let batch = std::mem::take(&mut self.pending);
        // A placeholder, replaced below before it could be polled (a boxed
        // zero sized future does not allocate).
        let previous = std::mem::replace(&mut self.writer, Writer::Busy(Box::pin(futures::future::pending())));
        let Writer::Idle(mut writer) = previous else { return };
        self.writer = Writer::Busy(Box::pin(async move {
            writer.write(batch).await;
            writer
        }));
        self.due = false;
        self.timer_armed = false;
    }

    /// The engine's next event, while the write in flight (if any) makes progress.
    fn poll_next(&mut self, run: &mut RunStream, handle: &RunHandle, cx: &mut Context<'_>) -> Poll<Next> {
        if let Writer::Busy(write) = &mut self.writer
            && let Poll::Ready(writer) = write.as_mut().poll(cx)
        {
            self.writer = Writer::Idle(writer);
            self.start_write();
            return Poll::Ready(Next::Wrote);
        }
        if self.timer_armed && self.timer.as_mut().poll(cx).is_ready() {
            self.timer_armed = false;
            self.due = true;
            self.start_write();
            return Poll::Ready(Next::Wrote);
        }
        let held_back = matches!(self.writer, Writer::Busy(_)) && self.pending.len() >= self.max_queued;
        if held_back && !handle.stop_requested() {
            let check = self.stop_check.get_or_insert_with(|| Box::pin(tokio::time::sleep(STOP_CHECK)));
            if check.as_mut().poll(cx).is_ready() {
                check.as_mut().reset(tokio::time::Instant::now() + STOP_CHECK);
                // Polls the new timer (and checks the stop request) again.
                cx.waker().wake_by_ref();
            }
            return Poll::Pending;
        }
        self.stop_check = None;
        run.poll_next_unpin(cx).map(Next::Event)
    }

    /// Wait for the write in flight; the writer and the events not handed to it.
    async fn finish(self) -> (EventWriter, Vec<StoredEvent>) {
        let writer = match self.writer {
            Writer::Idle(writer) => writer,
            Writer::Busy(write) => write.await,
        };
        (writer, self.pending)
    }
}

/// Record a finished execution: its last events (`last`, after any whose write
/// failed before), its status, its snapshot when it paused, and the final
/// answer text, in one store write ([`RunStore::finish`]): when a shutdown
/// stops many runs at once, each one is stored as soon as its own write went
/// through, not only once every run's three writes did.
///
/// When that write fails, the parts are written one by one, each failure
/// handled on its own: lost events are logged, and a snapshot that could not be
/// stored is recorded as `error_type` `store_unavailable`, so the paused run
/// shows why it cannot be resumed by id.
async fn store_final_state(
    store: &dyn RunStore,
    writer: &mut EventWriter,
    record: &mut RunRecord,
    terminal: &Event,
    answer: &str,
    last: Vec<StoredEvent>,
) {
    let run_id = record.run_id.clone();
    let snapshot = record.apply_terminal(terminal, now_ms());
    record.answer = answer.to_owned();
    // Fenced by the epoch this execution was claimed at; the store keeps its
    // own event counters.
    let epoch = record.epoch;
    let (first_index, events) = writer.unwritten(last);
    let finished = with_retries(|| {
        store.finish(FinalWrite {
            first_index,
            events: &events,
            snapshot: snapshot.as_ref(),
            record,
            expected_epoch: epoch,
        })
    })
    .await;
    let Err(error) = finished else {
        writer.stored(events.len());
        return;
    };
    // Every part is idempotent, so the ones that went through are not stored twice.
    eprintln!("warning: run {run_id}: cannot store the final state in one write, writing it in parts: {error}");
    writer.write(events).await;
    if let Some(snapshot) = snapshot {
        let saved = with_retries(|| store.save_snapshot(&run_id, &snapshot)).await;
        if let Err(e) = saved {
            eprintln!("warning: run {run_id}: cannot store snapshot, it will not be resumable by id: {e}");
            record.error = Some(format!("the paused run's snapshot could not be stored: {e}"));
            record.error_type = Some("store_unavailable".to_owned());
        }
    }
    if let Err(e) = with_retries(|| store.put_if(record.clone(), epoch)).await {
        eprintln!("warning: run {run_id}: cannot store final status: {e}");
    }
}

/// Writes a run's event batches to the store at their indices, retrying I/O errors.
#[derive(Debug)]
struct EventWriter {
    run_id: String,
    store: Arc<dyn RunStore>,
    /// Index of this execution's first event in the run's log.
    first_index: u64,
    /// Events of this execution written so far.
    written: u64,
    /// Events after `written` whose write failed: they go out again, at the same
    /// index, in front of the next batch (appends are idempotent per index, so a
    /// write that failed after reaching the store is not stored twice).
    behind: Vec<StoredEvent>,
}

/// Events kept for a later retry after failed writes; beyond it the oldest are
/// given up on (and every later append of the run is refused as a gap).
const MAX_BEHIND: usize = 4 * FEED_CAP;

impl EventWriter {
    /// The events not stored yet (those whose write failed, then `batch`), and
    /// the index of the first; [`EventWriter::stored`] reports them written.
    fn unwritten(&mut self, batch: Vec<StoredEvent>) -> (u64, Vec<StoredEvent>) {
        let mut events = std::mem::take(&mut self.behind);
        events.extend(batch);
        (self.first_index + self.written, events)
    }

    /// `count` events from [`EventWriter::unwritten`] were stored.
    fn stored(&mut self, count: usize) {
        self.written += u64::try_from(count).unwrap_or(u64::MAX);
    }

    /// Append `batch`, the events right after the ones already handed over.
    async fn write(&mut self, batch: Vec<StoredEvent>) {
        let run_id = self.run_id.as_str();
        if batch.is_empty() && self.behind.is_empty() {
            return;
        }
        let events = if self.behind.is_empty() {
            batch
        } else {
            let mut events = std::mem::take(&mut self.behind);
            events.extend(batch);
            events
        };
        let first = self.first_index + self.written;
        let mut result = self.store.append_events(run_id, first, &events).await;
        let mut delay = RETRY_FIRST_DELAY;
        for _ in 0..STORE_RETRIES {
            if !matches!(result, Err(StoreError::Io(_))) {
                break;
            }
            tokio::time::sleep(delay).await;
            delay = delay.saturating_mul(2);
            result = self.store.append_events(run_id, first, &events).await;
        }
        let count = events.len();
        match result {
            Ok(_) => self.written += u64::try_from(count).unwrap_or(u64::MAX),
            Err(StoreError::Io(e)) if count <= MAX_BEHIND => {
                eprintln!(
                    "warning: run {run_id}: cannot store {count} event(s) yet, retrying with the next write: {e}"
                );
                self.behind = events;
            }
            Err(e) => {
                eprintln!("warning: run {run_id}: cannot store {count} event(s), they are missing from replays: {e}");
            }
        }
    }
}

/// Run a store write, retrying I/O errors with doubling delays.
async fn with_retries<'a, T, F>(mut write: F) -> StoreResult<T>
where
    F: FnMut() -> futures::future::BoxFuture<'a, StoreResult<T>>,
{
    let mut delay = RETRY_FIRST_DELAY;
    let mut result = write().await;
    for _ in 0..STORE_RETRIES {
        if !matches!(result, Err(StoreError::Io(_))) {
            break;
        }
        tokio::time::sleep(delay).await;
        delay = delay.saturating_mul(2);
        result = write().await;
    }
    result
}

/// What [`follow`] yields.
#[derive(Debug, Clone)]
pub enum Followed {
    Event(SharedEvent),
    /// This many events are gone for this listener: it fell behind further than
    /// the feed keeps (more than [`FEED_CAP`] events into a run).
    Lagged(u64),
}

/// The events of `subscription` (backlog, then live) up to the terminal event.
///
/// A listener that falls behind the broadcast channel does not skip events: it is
/// refilled from the run's feed and continues on a new receiver. Only events the
/// feed no longer holds are reported as [`Followed::Lagged`], never dropped silently.
pub fn follow(active: Arc<ActiveRun>, subscription: Subscription) -> impl Stream<Item = Followed> + Send + 'static {
    struct Follower {
        active: Arc<ActiveRun>,
        backlog: std::collections::VecDeque<SharedEvent>,
        live: broadcast::Receiver<SharedEvent>,
        /// Index of the next live event.
        next: u64,
        /// No live events to wait for (terminal reached or already in the backlog).
        finished: bool,
        /// Events between the backlog and `next` the feed did not keep.
        gap: u64,
    }
    let held = u64::try_from(subscription.backlog.len()).unwrap_or(u64::MAX);
    let follower = Follower {
        active,
        // Non-zero only when the backlog was cut at the feed's cap.
        gap: subscription.next.saturating_sub(held),
        backlog: subscription.backlog.into(),
        live: subscription.live,
        next: subscription.next,
        finished: subscription.done,
    };
    futures::stream::unfold(follower, |mut f| async move {
        loop {
            if let Some(event) = f.backlog.front() {
                // The gap comes before a terminal event that follows it.
                if event.is_terminal() && f.gap > 0 {
                    return Some((Followed::Lagged(std::mem::take(&mut f.gap)), f));
                }
                let Some(event) = f.backlog.pop_front() else { continue };
                if event.is_terminal() {
                    f.finished = true;
                    f.backlog.clear();
                }
                return Some((Followed::Event(event), f));
            }
            if f.gap > 0 {
                return Some((Followed::Lagged(std::mem::take(&mut f.gap)), f));
            }
            if f.finished {
                return None;
            }
            match f.live.recv().await {
                Ok(event) => {
                    f.next += 1;
                    f.finished = event.is_terminal();
                    return Some((Followed::Event(event), f));
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    // Events the feed still holds come first, then the gap (if any)
                    // before the live events of the new receiver.
                    let refill = f.active.resubscribe(f.next);
                    f.finished = refill.events.last().is_some_and(|event| event.is_terminal());
                    f.backlog = refill.events.into();
                    f.gap = refill.gap;
                    f.live = refill.live;
                    f.next = refill.next;
                }
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An active run without a driver; the test publishes its events.
    fn active_run() -> ActiveRun {
        let (done_tx, _) = watch::channel(false);
        let workflow = graphrt_core::dsl::Workflow::from_yaml(
            "version: '1'\nname: t\ngraph:\n  nodes:\n    - { id: start, type: start, variables: [] }\n  edges: []\n",
        )
        .unwrap_or_else(|e| unreachable!("test workflow: {e}"));
        let graph = graphrt_core::Graph::build(workflow, &graphrt_core::node::Registry::with_builtins())
            .unwrap_or_else(|e| unreachable!("test workflow: {e}"));
        let run = graphrt_core::Engine::new(Arc::new(graph)).run(graphrt_core::engine::RunInput::new());
        ActiveRun {
            run_id: "r".to_owned(),
            handle: run.handle(),
            stored_before: 0,
            feed: Mutex::new(Feed {
                events: Vec::new(),
                dropped: false,
                total: 0,
                tx: None,
                terminal: None,
                answer: String::new(),
            }),
            done_tx,
            detached: AtomicBool::new(false),
            #[cfg(feature = "run-follow")]
            seed: std::sync::OnceLock::new(),
        }
    }

    fn numbered(n: u64) -> SharedEvent {
        Arc::new(Event::RunStarted { run_id: n.to_string(), resumed: false })
    }

    fn terminal() -> SharedEvent {
        Arc::new(Event::RunAborted { reason: "end".to_owned(), elapsed_ms: 0 })
    }

    /// Run ids of the numbered events, and the lag reports.
    async fn drain_follow(active: Arc<ActiveRun>, subscription: Subscription) -> (Vec<u64>, Vec<u64>, bool) {
        let mut seen = Vec::new();
        let mut lagged = Vec::new();
        let mut ended_with_terminal = false;
        let mut events = std::pin::pin!(follow(active, subscription));
        while let Some(followed) = events.next().await {
            match followed {
                Followed::Event(event) => match event.as_ref() {
                    Event::RunStarted { run_id, .. } => seen.push(run_id.parse().unwrap_or(u64::MAX)),
                    _ => ended_with_terminal = event.is_terminal(),
                },
                Followed::Lagged(n) => lagged.push(n),
            }
        }
        (seen, lagged, ended_with_terminal)
    }

    /// A run of `length` quick nodes in a line, and a store holding its record.
    async fn chain_run(length: usize) -> (RunStream, Arc<crate::store::MemoryStore>) {
        let mut yaml =
            String::from("version: '1'\nname: t\ngraph:\n  nodes:\n    - { id: start, type: start, variables: [] }\n");
        let mut edges = String::new();
        let mut previous = "start".to_owned();
        for index in 0..length {
            yaml.push_str(&format!("    - {{ id: p{index}, type: template-transform, template: x, variables: [] }}\n"));
            edges.push_str(&format!("    - {{ source: {previous}, target: p{index} }}\n"));
            previous = format!("p{index}");
        }
        yaml.push_str("  edges:\n");
        yaml.push_str(&edges);
        let workflow = graphrt_core::dsl::Workflow::from_yaml(&yaml).unwrap_or_else(|e| unreachable!("workflow: {e}"));
        let graph = graphrt_core::Graph::build(workflow, &graphrt_core::node::Registry::with_builtins())
            .unwrap_or_else(|e| unreachable!("workflow: {e}"));
        let run = graphrt_core::Engine::new(Arc::new(graph)).run(graphrt_core::engine::RunInput::new());
        let store = Arc::new(crate::store::MemoryStore::new());
        store.create(RunRecord::running("r", "t", "fp", 0)).await.unwrap_or_else(|e| unreachable!("create: {e}"));
        (run, store)
    }

    /// Batches whose writer is in a write that ends when `release` is sent.
    fn stuck_batches(
        store: Arc<crate::store::MemoryStore>,
        max_queued: usize,
    ) -> (Batches, tokio::sync::oneshot::Sender<()>) {
        let writer = EventWriter { run_id: "r".to_owned(), store, first_index: 0, written: 0, behind: Vec::new() };
        let (release, released) = tokio::sync::oneshot::channel::<()>();
        let mut batches = Batches::new(writer);
        let Writer::Idle(writer) =
            std::mem::replace(&mut batches.writer, Writer::Busy(Box::pin(futures::future::pending())))
        else {
            unreachable!("a new writer is idle")
        };
        batches.writer = Writer::Busy(Box::pin(async move {
            let _released = released.await;
            writer
        }));
        batches.max_queued = max_queued;
        (batches, release)
    }

    /// The next step of `batches`, or `None` when it is held back for 100 ms.
    async fn step(batches: &mut Batches, run: &mut RunStream, handle: &RunHandle) -> Option<Next> {
        let next = std::future::poll_fn(|cx| batches.poll_next(run, handle, cx));
        tokio::time::timeout(Duration::from_millis(100), next).await.ok()
    }

    #[tokio::test]
    async fn a_write_in_flight_holds_the_run_back_only_past_the_queue_bound_and_never_a_stop() {
        let (mut run, store) = chain_run(20).await;
        let handle = run.handle();
        let (mut batches, _release) = stuck_batches(Arc::clone(&store), 4);
        // Events go on while the write is in flight, up to the bound.
        let mut seen = 0;
        while let Some(next) = step(&mut batches, &mut run, &handle).await {
            let Next::Event(Some(event)) = next else { unreachable!("the write never ends, the run neither") };
            assert!(!event.is_terminal());
            batches.push(event_to_stored(&event));
            seen += 1;
        }
        assert_eq!((seen, batches.pending.len()), (4, 4));
        // Asked to stop, the run gets to its terminal event although the write still hangs.
        handle.preempt("test");
        let terminal = loop {
            match step(&mut batches, &mut run, &handle).await {
                Some(Next::Event(Some(event))) if event.is_terminal() => break event,
                Some(Next::Event(Some(event))) => batches.push(event_to_stored(&event)),
                other => unreachable!("held back after the stop request: {}", other.is_some()),
            }
        };
        // The nodes are quick: they may all have run before the stop request.
        assert!(matches!(terminal, Event::RunPaused { .. } | Event::RunSucceeded { .. }), "{terminal:?}");
    }

    #[tokio::test]
    async fn events_held_during_a_write_go_out_together_in_the_next_one() {
        let (mut run, store) = chain_run(20).await;
        let handle = run.handle();
        let (mut batches, release) = stuck_batches(Arc::clone(&store), 4);
        let mut events = 0_u64;
        while let Some(Next::Event(Some(event))) = step(&mut batches, &mut run, &handle).await {
            batches.push(event_to_stored(&event));
            events += 1;
        }
        assert_eq!(events, 4);
        let _sent = release.send(());
        // The write ends; the held events are due (past the batch timer) and
        // the run goes on to its end.
        let mut terminal = None;
        while terminal.is_none() {
            match step(&mut batches, &mut run, &handle).await {
                Some(Next::Wrote) => {}
                Some(Next::Event(Some(event))) if event.is_terminal() => {
                    events += 1;
                    batches.push_last(event_to_stored(&event));
                    terminal = Some(event);
                }
                Some(Next::Event(Some(event))) => {
                    events += 1;
                    batches.push(event_to_stored(&event));
                }
                Some(Next::Event(None)) | None => unreachable!("the run ended without a terminal event"),
            }
        }
        let (mut writer, last) = batches.finish().await;
        let written = writer.written;
        assert!(written >= 4, "{written}");
        writer.write(last).await;
        assert_eq!(writer.written, events);
        let stored = store.events("r", 0).await.unwrap_or_default();
        assert_eq!(u64::try_from(stored.len()).unwrap_or(0), events);
    }

    #[tokio::test]
    async fn a_lagging_listener_is_refilled_from_the_feed_without_gaps() {
        let active = Arc::new(active_run());
        let subscription = active.subscribe();
        // Far more than the broadcast buffer before the listener reads anything.
        let count = u64::try_from(CHANNEL_CAP).unwrap_or(0) * 2;
        for n in 0..count {
            active.publish(&numbered(n), String::new);
        }
        active.publish(&terminal(), String::new);
        let (seen, lagged, terminal_seen) = drain_follow(Arc::clone(&active), subscription).await;
        assert_eq!(seen, (0..count).collect::<Vec<_>>());
        assert!(lagged.is_empty(), "{lagged:?}");
        assert!(terminal_seen);
    }

    #[tokio::test]
    async fn events_beyond_the_feed_cap_are_reported_as_lagged_not_skipped_silently() {
        let active = Arc::new(active_run());
        let subscription = active.subscribe();
        let count = u64::try_from(FEED_CAP + CHANNEL_CAP + 10).unwrap_or(0);
        for n in 0..count {
            active.publish(&numbered(n), String::new);
        }
        active.publish(&terminal(), String::new);
        let (seen, lagged, terminal_seen) = drain_follow(Arc::clone(&active), subscription).await;
        let missing: u64 = lagged.iter().sum();
        assert!(missing > 0);
        // Every event is either delivered, in order, or counted as missing.
        assert_eq!(u64::try_from(seen.len()).unwrap_or(0) + missing, count);
        assert!(seen.windows(2).all(|w| w[0] < w[1]));
        assert!(terminal_seen);
    }

    #[tokio::test]
    async fn a_late_subscriber_past_the_cap_still_gets_the_terminal_event() {
        let active = Arc::new(active_run());
        let count = u64::try_from(FEED_CAP + 5).unwrap_or(0);
        for n in 0..count {
            active.publish(&numbered(n), String::new);
        }
        active.publish(&terminal(), String::new);
        let subscription = active.subscribe();
        let (seen, lagged, terminal_seen) = drain_follow(Arc::clone(&active), subscription).await;
        assert_eq!(seen.len(), FEED_CAP);
        assert_eq!(lagged, [5]);
        assert!(terminal_seen);
    }
}
