//! Runs executing in the server: a background task drives each run, records it in
//! the [`RunStore`] and fans its events out to any number of listeners.
//!
//! Events are written to the store in batches of [`FLUSH_EVERY`] events or after
//! [`FLUSH_AFTER`], whichever comes first, so a replay (`/events`) or another
//! replica reading a shared store trails a live run by at most a quarter second.
//! Store writes that fail with an I/O error are retried with bounded backoff.

use std::pin::pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use futures::future::Either;
use futures::{Stream, StreamExt};
use graphrt_core::engine::{RunHandle, RunStream};
use graphrt_core::response::ResponseAssembler;
use graphrt_core::store::{RunRecord, RunStore, StoreError, StoreResult, StoredEvent, event_to_stored, now_ms};
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
    let mut writer =
        EventWriter { store: Arc::clone(&store), first_index: record.event_count, written: 0, behind: Vec::new() };
    let mut pending: Vec<StoredEvent> = Vec::new();
    // Only the assembled text is kept (`text_so_far`); the chunks themselves are dropped.
    let mut chunks = Vec::new();
    // Armed when the first event of a batch is queued (a timer per batch, not per event).
    let mut flush_timer = pin!(tokio::time::sleep(FLUSH_AFTER));
    let mut timer_armed = false;
    loop {
        let next = if timer_armed {
            match futures::future::select(run.next(), flush_timer.as_mut()).await {
                Either::Left((event, _)) => event,
                Either::Right(((), _)) => {
                    timer_armed = false;
                    writer.write(&run_id, std::mem::take(&mut pending)).await;
                    continue;
                }
            }
        } else {
            run.next().await
        };
        let Some(event) = next else { break };
        recorder.on_event(&event, &mut record);
        assembler.push_into(&event, &mut chunks);
        chunks.clear();
        let terminal = event.is_terminal();
        pending.push(event_to_stored(&event));
        let event = Arc::new(event);
        active.publish(&event, || assembler.text_so_far().to_owned());
        if pending.len() >= FLUSH_EVERY || terminal {
            timer_armed = false;
            writer.write(&run_id, std::mem::take(&mut pending)).await;
        } else if !timer_armed {
            flush_timer.as_mut().reset(tokio::time::Instant::now() + FLUSH_AFTER);
            timer_armed = true;
        }
        if terminal {
            store_final_state(&*store, &mut record, &event, assembler.text_so_far()).await;
        }
    }
    writer.write(&run_id, pending).await;
    on_finish(&run_id);
    active.done_tx.send_replace(true);
    // After the waiters are woken, so evaluation never delays a response.
    #[cfg(feature = "online-eval")]
    if let Some(online) = &recorder.capture.online {
        online.submit(&record, &active.feed().events);
    }
}

/// Record a finished execution: its status, its snapshot when it paused, and
/// the final answer text. Failures are logged; a snapshot that could not be
/// stored is recorded as `error_type` `store_unavailable`, so the paused run
/// shows why it cannot be resumed by id.
async fn store_final_state(store: &dyn RunStore, record: &mut RunRecord, terminal: &Event, answer: &str) {
    let run_id = record.run_id.clone();
    if let Some(snapshot) = record.apply_terminal(terminal, now_ms()) {
        let saved = with_retries(|| store.save_snapshot(&run_id, &snapshot)).await;
        if let Err(e) = saved {
            eprintln!("warning: run {run_id}: cannot store snapshot, it will not be resumable by id: {e}");
            record.error = Some(format!("the paused run's snapshot could not be stored: {e}"));
            record.error_type = Some("store_unavailable".to_owned());
        }
    }
    record.answer = answer.to_owned();
    // Fenced by the epoch this execution was claimed at; the store keeps its
    // own event counters.
    let epoch = record.epoch;
    if let Err(e) = with_retries(|| store.put_if(record.clone(), epoch)).await {
        eprintln!("warning: run {run_id}: cannot store final status: {e}");
    }
}

/// Writes a run's event batches to the store at their indices, retrying I/O errors.
#[derive(Debug)]
struct EventWriter {
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
    /// Append `batch`, the events right after the ones already handed over.
    async fn write(&mut self, run_id: &str, batch: Vec<StoredEvent>) {
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
