//! Online evaluation in `graphrt serve` (`--evaluators <file>`, cargo feature
//! `online-eval`).
//!
//! When a run of a covered workflow finishes (succeeded, failed or aborted),
//! the run driver asks [`OnlineEval::submit`] whether any evaluator of the
//! [`graphrt_eval::online`] config matches and samples it. A sampled run is
//! queued, without waiting, to a bounded pool: one task runs at
//! most `concurrency` evaluations at a time from a queue of `queue` runs. When
//! the queue is full the run is dropped and counted ([`OnlineEval::dropped`],
//! and the `graphrt.eval.dropped` metric with OpenTelemetry). The run's
//! response never waits for any of this: submission happens after the run is
//! recorded and its waiters are woken.
//!
//! Each evaluation becomes an `online` score of the run ([`crate::scores`]),
//! stored with the run and reported to the score sinks (an `evaluate {name}`
//! span, the `graphrt.eval.score` metric, a Langfuse upload).
//!
//! Without `--evaluators` nothing here exists: no task, no queue, and the
//! driver skips the check on an `Option` that is `None`.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use graphrt_core::Event;
use graphrt_core::store::{RunRecord, RunStatus};
use graphrt_eval::online::{OnlineConfig, OnlineEvaluators, RunInfo};
use graphrt_eval::{AssertionResult, RunCapture};
use serde_json::{Map, Value as Json};
use tokio::sync::mpsc;

use crate::scores::{Score, ScoreSinks, ScoreSource, ScoreStore, ScoreValue};
use crate::store::live::SharedEvent;

/// A finished run waiting for its evaluations.
#[derive(Debug)]
struct Job {
    record: RunRecord,
    events: Vec<SharedEvent>,
    /// Indexes of the evaluators that selected the run.
    evaluators: Vec<usize>,
}

#[derive(Debug, Default)]
struct Counters {
    queued: AtomicU64,
    dropped: AtomicU64,
    scored: AtomicU64,
}

/// The online evaluation pool of a server.
#[derive(Debug)]
pub struct OnlineEval {
    evaluators: Arc<OnlineEvaluators>,
    tx: mpsc::Sender<Job>,
    counters: Arc<Counters>,
    sinks: ScoreSinks,
}

/// What an evaluation job needs besides the run.
struct Worker {
    evaluators: Arc<OnlineEvaluators>,
    store: Arc<dyn ScoreStore>,
    sinks: ScoreSinks,
    counters: Arc<Counters>,
}

impl OnlineEval {
    /// Start the pool (a dispatcher task on the current tokio runtime).
    /// Scores go to `store` and `sinks`.
    pub fn start(
        config: &OnlineConfig,
        evaluators: OnlineEvaluators,
        store: Arc<dyn ScoreStore>,
        sinks: ScoreSinks,
    ) -> Arc<Self> {
        let evaluators = Arc::new(evaluators);
        let counters = Arc::new(Counters::default());
        let (tx, rx) = mpsc::channel(config.queue.max(1));
        let worker = Arc::new(Worker {
            evaluators: Arc::clone(&evaluators),
            store,
            sinks: sinks.clone(),
            counters: Arc::clone(&counters),
        });
        graphrt_core::task::spawn(dispatch(rx, worker, config.concurrency.max(1)));
        Arc::new(Self { evaluators, tx, counters, sinks })
    }

    /// Whether any evaluator may score runs of `workflow` (others skip the
    /// check at the end of every run).
    pub fn covers(&self, workflow: &str) -> bool {
        self.evaluators.covers(workflow)
    }

    /// Queue a finished run for the evaluators that match and sample it.
    /// Never waits: a full queue drops the run (counted). Paused and running
    /// runs are ignored.
    pub fn submit(&self, record: &RunRecord, events: &[SharedEvent]) {
        let status = match record.status {
            RunStatus::Succeeded => graphrt_eval::RunStatus::Succeeded,
            RunStatus::Failed => graphrt_eval::RunStatus::Failed,
            RunStatus::Aborted => graphrt_eval::RunStatus::Aborted,
            _ => return,
        };
        let run = RunInfo::new(&record.run_id, &record.workflow, status, &record.tags, &record.metadata);
        let selected = self.evaluators.select(&run);
        if selected.is_empty() {
            return;
        }
        let job = Job { record: record.clone(), events: events.to_vec(), evaluators: selected };
        match self.tx.try_send(job) {
            Ok(()) => {
                self.counters.queued.fetch_add(1, Ordering::Relaxed);
            }
            Err(mpsc::error::TrySendError::Full(job) | mpsc::error::TrySendError::Closed(job)) => {
                let dropped = self.counters.dropped.fetch_add(1, Ordering::Relaxed) + 1;
                self.sinks.dropped(&job.record.workflow);
                // The first drop, then every thousandth: a full queue is a sizing problem, not an event.
                if dropped == 1 || dropped.is_multiple_of(1000) {
                    eprintln!(
                        "warning: online evaluation queue full; {dropped} run(s) not evaluated so far \
                         (raise `queue` or `concurrency` in the evaluator config)"
                    );
                }
            }
        }
    }

    /// Runs queued for evaluation so far.
    pub fn queued(&self) -> u64 {
        self.counters.queued.load(Ordering::Relaxed)
    }

    /// Sampled runs dropped because the queue was full.
    pub fn dropped(&self) -> u64 {
        self.counters.dropped.load(Ordering::Relaxed)
    }

    /// Scores stored so far.
    pub fn scored(&self) -> u64 {
        self.counters.scored.load(Ordering::Relaxed)
    }
}

/// Take jobs off the queue as evaluation slots free up: up to `concurrency`
/// evaluations interleave on this one task (they wait on models and judge
/// runs, not the CPU), and the next job is taken only when a slot is free, so
/// the channel stays the bound.
async fn dispatch(rx: mpsc::Receiver<Job>, worker: Arc<Worker>, concurrency: usize) {
    use futures::StreamExt;
    let jobs = futures::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|job| (job, rx)) });
    jobs.for_each_concurrent(concurrency, |job| worker.evaluate(job)).await;
}

impl Worker {
    async fn evaluate(&self, job: Job) {
        let Job { record, events, evaluators } = job;
        let latency_ms = events.iter().rev().find_map(|e| elapsed_ms(e)).unwrap_or(0);
        let events: Vec<Event> = events.iter().map(|e| (**e).clone()).collect();
        let vars = run_inputs(&record, &events);
        let capture = RunCapture::from_events(events, record.answer.clone(), latency_ms);
        let status = match record.status {
            RunStatus::Failed => graphrt_eval::RunStatus::Failed,
            RunStatus::Aborted => graphrt_eval::RunStatus::Aborted,
            _ => graphrt_eval::RunStatus::Succeeded,
        };
        let run = RunInfo::new(&record.run_id, &record.workflow, status, &record.tags, &record.metadata);
        let mut scores = Vec::with_capacity(evaluators.len());
        for index in evaluators {
            let Some(evaluator) = self.evaluators.evaluators().get(index) else { continue };
            let started = unix_nanos();
            let Some(result) = self.evaluators.evaluate(index, &run, vars.clone(), &capture).await else { continue };
            let timing = Some((started, unix_nanos()));
            scores.push((score_of(&record.run_id, &evaluator.spec.name, result), timing));
        }
        if scores.is_empty() {
            return;
        }
        let stored = scores.iter().map(|(score, _)| score.clone()).collect();
        if let Err(e) = self.store.put_scores(&record.run_id, stored).await {
            eprintln!("warning: run {}: cannot store online evaluation scores: {e}", record.run_id);
            return;
        }
        self.counters.scored.fetch_add(u64::try_from(scores.len()).unwrap_or(u64::MAX), Ordering::Relaxed);
        for (score, timing) in &scores {
            self.sinks.publish(&record, score, *timing);
        }
    }
}

/// Unix time in nanoseconds, for evaluation spans.
fn unix_nanos() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

fn elapsed_ms(event: &Event) -> Option<u64> {
    match event {
        Event::RunSucceeded { elapsed_ms, .. }
        | Event::RunFailed { elapsed_ms, .. }
        | Event::RunAborted { elapsed_ms, .. } => Some(*elapsed_ms),
        _ => None,
    }
}

/// The run's inputs: the stored ones (`--store-inputs`), else the root start
/// node's outputs from the events. Secrets are masked either way.
fn run_inputs(record: &RunRecord, events: &[Event]) -> Map<String, Json> {
    if let Some(inputs) = &record.inputs {
        return inputs.iter().map(|(k, v)| (k.clone(), v.to_json())).collect();
    }
    let started = events.iter().find_map(|event| match event {
        Event::NodeStarted { frame, node_id, kind, .. } if frame.is_empty() && kind.as_ref() == "start" => {
            Some(node_id)
        }
        _ => None,
    });
    let Some(start) = started else { return Map::new() };
    events
        .iter()
        .find_map(|event| match event {
            Event::NodeSucceeded { frame, node_id, outputs, .. } if frame.is_empty() && node_id == start => {
                Some(outputs.iter().map(|(k, v)| (k.to_string(), v.to_json())).collect())
            }
            _ => None,
        })
        .unwrap_or_default()
}

/// An assertion result as an `online` score of `run_id`.
fn score_of(run_id: &str, name: &str, result: AssertionResult) -> Score {
    let mut score = Score::new(run_id, name, ScoreValue::Number(result.score), ScoreSource::Online, None);
    score.label = Some(result.verdict().as_str().to_owned());
    score.comment = Some(result.reason).filter(|r| !r.is_empty());
    score.kind = Some(result.kind);
    score.judge = result.judge.and_then(|judge| serde_json::to_value(judge).ok());
    score
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn results_map_to_labelled_scores() {
        let pass = AssertionResult::new("judge", true, 0.9, 1.0, "judge: pass: grounded");
        let score = score_of("run-1", "faithful", pass);
        assert_eq!((score.label.as_deref(), score.kind.as_deref()), (Some("pass"), Some("judge")));
        assert_eq!(score.value, ScoreValue::Number(0.9));
        assert_eq!(score.source, ScoreSource::Online);
        assert_eq!(score.id, crate::scores::score_id("run-1", ScoreSource::Online, "faithful", None));
        let fail = score_of("run-1", "polite", AssertionResult::new("not-icontains", false, 0.0, 1.0, "found"));
        assert_eq!((fail.label.as_deref(), fail.kind.as_deref()), (Some("fail"), Some("not-icontains")));
    }

    /// A judge that waits until the test opens the gate.
    #[derive(Debug)]
    struct Gated(Arc<tokio::sync::Semaphore>);

    impl graphrt_eval::Assertion for Gated {
        fn evaluate<'a>(
            &'a self,
            _ctx: graphrt_eval::AssertionContext<'a>,
        ) -> futures::future::BoxFuture<'a, graphrt_eval::Outcome> {
            Box::pin(async move {
                let _pass = self.0.acquire().await;
                graphrt_eval::Outcome::graded(true, 0.8, "judge: pass")
            })
        }
    }

    fn finished(store: &crate::store::MemoryStore, run_id: &str) -> RunRecord {
        use graphrt_core::store::RunStore;
        let mut record = RunRecord::running(run_id, "chat", "fp", 1);
        record.status = RunStatus::Succeeded;
        futures::executor::block_on(store.create(record.clone())).unwrap();
        record
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_full_queue_drops_and_counts_and_never_waits() {
        let config = OnlineConfig::parse(
            "concurrency: 1\nqueue: 2\nevaluators: [{name: slow, assert: {type: judge, workflow: j.yaml}}]",
        )
        .unwrap();
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let mut registry = graphrt_eval::AssertionRegistry::with_builtins();
        let judge_gate = Arc::clone(&gate);
        registry.register("judge", move |_spec| {
            Ok(Box::new(Gated(Arc::clone(&judge_gate))) as Box<dyn graphrt_eval::Assertion>)
        });
        let evaluators = OnlineEvaluators::new(&config, registry).unwrap();
        let store = Arc::new(crate::store::MemoryStore::new());
        let pool = OnlineEval::start(&config, evaluators, store.clone(), ScoreSinks::default());
        assert!(pool.covers("chat"));
        // No await between submissions: the dispatcher cannot take anything off
        // the queue, so exactly `queue` runs fit and the rest are dropped.
        let records: Vec<RunRecord> = (0..10).map(|i| finished(&store, &format!("run-{i}"))).collect();
        for record in &records {
            pool.submit(record, &[]);
        }
        assert_eq!((pool.queued(), pool.dropped()), (2, 8));
        // Paused runs are never evaluated.
        let mut paused = finished(&store, "paused");
        paused.status = RunStatus::Paused;
        pool.submit(&paused, &[]);
        assert_eq!(pool.queued() + pool.dropped(), 10);

        gate.add_permits(100);
        for _ in 0..200 {
            if pool.scored() == 2 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert_eq!(pool.scored(), 2);
        let scores = store.scores("run-0").await.unwrap();
        assert_eq!(scores.len(), 1);
        assert_eq!((scores[0].label.as_deref(), &scores[0].value), (Some("pass"), &ScoreValue::Number(0.8)));
        assert!(store.scores("run-9").await.unwrap().is_empty(), "dropped run has no score");
    }

    #[test]
    fn inputs_come_from_the_record_or_the_start_node() {
        use graphrt_core::Value;
        let mut record = RunRecord::running("r", "chat", "fp", 1);
        let mut inputs = graphrt_core::IndexMap::new();
        inputs.insert("q".to_owned(), Value::from("stored"));
        record.inputs = Some(inputs);
        assert_eq!(run_inputs(&record, &[])["q"], "stored");
        record.inputs = None;
        let events: Vec<Event> = serde_json::from_value(serde_json::json!([
            {"type": "node_started", "frame": "", "node_id": "begin", "exec_id": "e1", "kind": "start",
             "title": "Start", "attempt": 1},
            {"type": "node_succeeded", "frame": "", "node_id": "begin", "exec_id": "e1",
             "outputs": {"q": "from events"}, "elapsed_ms": 0}
        ]))
        .unwrap();
        assert_eq!(run_inputs(&record, &events)["q"], "from events");
        assert!(run_inputs(&record, &[]).is_empty());
    }
}
