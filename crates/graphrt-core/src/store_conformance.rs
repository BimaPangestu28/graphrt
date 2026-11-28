//! Abstract conformance suite for [`RunStore`] and [`CoordinationStore`]
//! implementations (feature `store-conformance`).
//!
//! Every store backend (memory, directory, SQLite, object storage...) runs the
//! same checks, so a host can swap one for another without changing behavior.
//! The functions panic with a description of the first violated rule; call them
//! from a test on the runtime the store needs:
//!
//! ```ignore
//! #[tokio::test(flavor = "multi_thread")]
//! async fn my_store_conforms() {
//!     let store = MyStore::open(..);
//!     graphrt_core::store_conformance::run_store_suite(&store).await;
//!     graphrt_core::store_conformance::append_retry_property(&store, 200).await;
//!     graphrt_core::store_conformance::coordination_suite(&store).await;
//! }
//! ```
//!
//! Run ids and keys used by the suite start with `conformance-` / `conformance/`,
//! so one store instance can run every check.

// Test helpers: a violated rule panics with its description.
#![allow(clippy::expect_used, clippy::panic, clippy::indexing_slicing, clippy::missing_panics_doc)]

use futures::future::join;

use crate::coordination::{CoordinationStore, MAX_VALUE_BYTES};
use crate::store::{FinalWrite, RunFilter, RunRecord, RunStatus, RunStore, StoreError, StoredEvent};

/// Events `{"type":"e","i":<i>}` for `indices`.
pub fn numbered_events(indices: std::ops::Range<u64>) -> Vec<StoredEvent> {
    indices.map(|i| numbered_event(i, "e")).collect()
}

fn numbered_event(index: u64, kind: &str) -> StoredEvent {
    let text = format!(r#"{{"type":"{kind}","i":{index}}}"#);
    serde_json::value::RawValue::from_string(text).unwrap_or_else(|_| serde_json::value::RawValue::NULL.to_owned())
}

fn texts(events: &[StoredEvent]) -> Vec<String> {
    events.iter().map(|e| e.get().to_owned()).collect()
}

fn record(run_id: &str) -> RunRecord {
    RunRecord::running(run_id, "conformance-wf", "fp", 1)
}

/// Run every [`RunStore`] check.
pub async fn run_store_suite(store: &dyn RunStore) {
    create_and_fields(store).await;
    appends_are_idempotent(store).await;
    conditional_writes(store).await;
    claims_are_exclusive_and_bump_the_epoch(store).await;
    queued_runs_survive_recovery_and_purge(store).await;
    final_writes_store_events_snapshot_and_record(store).await;
}

async fn create_and_fields(store: &dyn RunStore) {
    let mut rec = record("conformance-fields");
    rec.deployment = Some("shop".into());
    rec.deployment_version = Some("v3".into());
    rec.owner = Some("replica-a".into());
    rec.epoch = 7;
    store.create(rec.clone()).await.expect("create a new run");
    let got = store.get("conformance-fields").await.expect("get").expect("created run exists");
    assert_eq!(got, rec, "a record reads back as written, new fields included");
    assert!(
        matches!(store.create(rec).await, Err(StoreError::Conflict(_))),
        "creating an existing run id is a conflict"
    );
    assert!(store.get("conformance-missing").await.expect("get").is_none(), "unknown run reads as none");
    assert!(matches!(store.get("../x").await, Err(StoreError::InvalidId(_))), "invalid run ids are refused");
}

async fn appends_are_idempotent(store: &dyn RunStore) {
    let id = "conformance-append";
    store.create(record(id)).await.expect("create");
    let all = numbered_events(0..10);
    assert_eq!(store.append_events(id, 0, &all[0..4]).await.expect("first batch"), 4);
    // The reply was lost: the same batch again is accepted and not stored twice.
    assert_eq!(store.append_events(id, 0, &all[0..4]).await.expect("retried batch"), 4);
    // A retry that overlaps the stored tail stores only the new part.
    assert_eq!(store.append_events(id, 2, &all[2..7]).await.expect("overlapping batch"), 7);
    assert_eq!(store.append_events(id, 7, &[]).await.expect("empty batch"), 7);
    assert!(
        matches!(store.append_events(id, 9, &all[9..10]).await, Err(StoreError::Conflict(_))),
        "an append past the event count (a gap) is a conflict"
    );
    let other = vec![numbered_event(5, "different")];
    assert!(
        matches!(store.append_events(id, 5, &other).await, Err(StoreError::Conflict(_))),
        "a retried event that differs from the stored one is a conflict"
    );
    assert_eq!(store.append_events(id, 7, &all[7..10]).await.expect("last batch"), 10);
    assert_eq!(texts(&store.events(id, 0).await.expect("events")), texts(&all), "events are stored once, in order");
    assert_eq!(texts(&store.events(id, 6).await.expect("events from 6")), texts(&all[6..]));
    let rec = store.get(id).await.expect("get").expect("exists");
    assert_eq!(rec.event_count, 10);
    assert!(!rec.events_truncated);
    assert!(
        matches!(store.append_events("conformance-nope", 0, &all[0..1]).await, Err(StoreError::NotFound(_))),
        "appending to an unknown run is not found"
    );
}

async fn conditional_writes(store: &dyn RunStore) {
    let id = "conformance-put-if";
    store.create(record(id)).await.expect("create");
    store.append_events(id, 0, &numbered_events(0..3)).await.expect("append");
    let mut update = record(id);
    update.answer = "first".into();
    update.epoch = 1;
    store.put_if(update.clone(), 0).await.expect("put_if at the stored epoch");
    let got = store.get(id).await.expect("get").expect("exists");
    assert_eq!((got.answer.as_str(), got.epoch, got.event_count), ("first", 1, 3), "put_if keeps event counters");
    let mut stale = update.clone();
    stale.answer = "stale".into();
    assert!(
        matches!(store.put_if(stale, 0).await, Err(StoreError::Precondition(_))),
        "a writer with an old epoch is refused"
    );
    assert_eq!(store.get(id).await.expect("get").expect("exists").answer, "first", "a refused write changes nothing");
    assert!(
        matches!(store.put_if(record("conformance-put-if-missing"), 0).await, Err(StoreError::NotFound(_))),
        "put_if of an unknown run is not found"
    );
    // Two writers with the same expected epoch: exactly one wins.
    let (mut a, mut b) = (update.clone(), update);
    a.epoch = 2;
    a.answer = "a".into();
    b.epoch = 2;
    b.answer = "b".into();
    let (ra, rb) = join(store.put_if(a, 1), store.put_if(b, 1)).await;
    assert_eq!(usize::from(ra.is_ok()) + usize::from(rb.is_ok()), 1, "exactly one of two concurrent put_if wins");
}

async fn final_writes_store_events_snapshot_and_record(store: &dyn RunStore) {
    let id = "conformance-finish";
    store.create(record(id)).await.expect("create");
    let events = numbered_events(0..5);
    store.append_events(id, 0, &events[0..2]).await.expect("append");
    let snapshot = crate::Snapshot {
        format: crate::snapshot::SNAPSHOT_FORMAT,
        run_id: id.into(),
        graph_fingerprint: "fp".into(),
        elapsed_ms: 3,
        steps: 2,
        usage: crate::event::Usage::default(),
        handled_errors: 0,
        root: crate::snapshot::FrameSnapshot::default(),
        ..crate::Snapshot::default()
    };
    let mut paused = record(id);
    paused.status = RunStatus::Paused;
    paused.answer = "half".into();
    let write = FinalWrite {
        first_index: 2,
        events: &events[2..5],
        snapshot: Some(&snapshot),
        record: &paused,
        expected_epoch: 0,
    };
    store.finish(write).await.expect("finish at the stored epoch");
    // The reply was lost: the same final write again changes nothing.
    store.finish(write).await.expect("retried finish");
    assert_eq!(texts(&store.events(id, 0).await.expect("events")), texts(&events), "events are stored once, in order");
    let got = store.get(id).await.expect("get").expect("exists");
    assert_eq!((got.status, got.answer.as_str(), got.event_count), (RunStatus::Paused, "half", 5));
    assert_eq!(store.load_snapshot(id).await.expect("load").expect("saved"), snapshot, "the snapshot is stored");

    let fenced = "conformance-finish-fenced";
    store.create(record(fenced)).await.expect("create");
    let mut done = record(fenced);
    done.status = RunStatus::Succeeded;
    let stale = FinalWrite { first_index: 0, events: &[], snapshot: None, record: &done, expected_epoch: 4 };
    assert!(
        matches!(store.finish(stale).await, Err(StoreError::Precondition(_))),
        "a final write at an old epoch is refused"
    );
    let got = store.get(fenced).await.expect("get").expect("exists");
    assert_eq!(got.status, RunStatus::Running, "a refused final write leaves the record");
    let current = FinalWrite { expected_epoch: 0, ..stale };
    store.finish(current).await.expect("finish without events or snapshot");
    assert_eq!(store.get(fenced).await.expect("get").expect("exists").status, RunStatus::Succeeded);
}

async fn claims_are_exclusive_and_bump_the_epoch(store: &dyn RunStore) {
    let id = "conformance-claim";
    let mut paused = record(id);
    paused.status = RunStatus::Paused;
    paused.epoch = 3;
    store.create(paused).await.expect("create");
    let (a, b) = join(store.claim_resume(id, 5), store.claim_resume(id, 5)).await;
    assert_eq!(usize::from(a.is_ok()) + usize::from(b.is_ok()), 1, "exactly one of two concurrent resumes wins");
    let claimed = a.or(b).expect("one claim");
    assert_eq!((claimed.status, claimed.epoch), (RunStatus::Running, 4), "a resume claim increments the epoch");
    assert_eq!(store.get(id).await.expect("get").expect("exists").epoch, 4);

    let id = "conformance-dispatch";
    store.create(RunRecord::queued(id, "conformance-wf", "fp", Some("shop".into()), 1)).await.expect("create queued");
    let (a, b) = join(
        store.claim_dispatch(id, 0, Some("replica-a".into()), 2),
        store.claim_dispatch(id, 0, Some("replica-b".into()), 2),
    )
    .await;
    assert_eq!(usize::from(a.is_ok()) + usize::from(b.is_ok()), 1, "exactly one of two concurrent dispatches wins");
    let claimed = a.or(b).expect("one dispatch");
    assert_eq!((claimed.status, claimed.epoch), (RunStatus::Running, 1));
    let stored = store.get(id).await.expect("get").expect("exists");
    assert_eq!((stored.epoch, &stored.owner), (1, &claimed.owner), "the winner's claim is stored");
    assert!(store.claim_dispatch(id, 0, None, 3).await.is_err(), "a dispatched run cannot be dispatched again");
}

async fn queued_runs_survive_recovery_and_purge(store: &dyn RunStore) {
    let id = "conformance-queued";
    store.create(RunRecord::queued(id, "conformance-wf", "fp", None, 1)).await.expect("create queued");
    store.recover_interrupted(10).await.expect("recover");
    assert_eq!(store.get(id).await.expect("get").expect("exists").status, RunStatus::Queued, "recovery keeps queued");
    store.purge_before(u64::MAX).await.expect("purge");
    assert!(store.get(id).await.expect("get").is_some(), "TTL cleanup keeps queued runs");
    let queued =
        store.list(RunFilter { status: Some(RunStatus::Queued), ..RunFilter::default() }).await.expect("list queued");
    assert!(queued.runs.iter().any(|r| r.run_id == id), "queued runs can be listed by status");
}

/// Small deterministic generator (xorshift64*), so failures are reproducible by seed.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, bound: u64) -> u64 {
        if bound == 0 { 0 } else { self.next() % bound }
    }
}

/// Property: whatever mix of retried, overlapping and lost-reply appends a writer
/// sends, the store ends with every event exactly once, in order, at its index.
///
/// Each of `seeds` cases writes 1–80 events in random batches of 1–8; a reply is
/// "lost" a third of the time (the writer resends from its last acknowledged
/// index), and writers sometimes resend batches that start before it (stale
/// retries). Gaps and altered retries are checked to be refused without effect.
pub async fn append_retry_property(store: &dyn RunStore, seeds: u64) {
    for seed in 1..=seeds {
        let run_id = format!("conformance-prop-{seed}");
        store.create(record(&run_id)).await.expect("create");
        let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
        let total = 1 + rng.below(80);
        let events = numbered_events(0..total);
        let at = |i: u64| usize::try_from(i).expect("small index");
        let (mut acked, mut stored) = (0u64, 0u64);
        while acked < total {
            let rewind = rng.below(acked.min(4) + 1);
            let start = acked - rewind;
            let len = (1 + rng.below(8)).min(total - start);
            let batch = &events[at(start)..at(start + len)];
            let count = store
                .append_events(&run_id, start, batch)
                .await
                .unwrap_or_else(|e| panic!("seed {seed}: append at {start} (+{len}) failed: {e}"));
            stored = stored.max(start + len);
            assert_eq!(count, stored, "seed {seed}: event count after append at {start} (+{len})");
            if rng.below(3) != 0 {
                acked = count;
            }
            if rng.below(10) == 0 {
                let gap = store.append_events(&run_id, stored + 1, &events[at(start)..at(start + 1)]).await;
                assert!(matches!(gap, Err(StoreError::Conflict(_))), "seed {seed}: a gap must be refused");
                let altered = vec![numbered_event(start, "altered")];
                let altered = store.append_events(&run_id, start, &altered).await;
                assert!(
                    matches!(altered, Err(StoreError::Conflict(_))),
                    "seed {seed}: an altered retry must be refused"
                );
            }
        }
        let got = store.events(&run_id, 0).await.expect("events");
        assert_eq!(texts(&got), texts(&events), "seed {seed}: every event exactly once, in order");
        let from = rng.below(total);
        let tail = store.events(&run_id, from).await.expect("events from");
        assert_eq!(texts(&tail), texts(&events[at(from)..]), "seed {seed}: events from {from}");
        assert_eq!(store.get(&run_id).await.expect("get").expect("exists").event_count, total);
        store.delete(&run_id).await.expect("delete");
    }
}

/// Run every [`CoordinationStore`] check.
pub async fn coordination_suite(store: &dyn CoordinationStore) {
    let key = "conformance/d/shop/markers/a";
    assert_eq!(store.create_if_absent(key, b"one".to_vec()).await.expect("create"), 1, "versions start at 1");
    assert!(
        matches!(store.create_if_absent(key, b"again".to_vec()).await, Err(StoreError::Conflict(_))),
        "create_if_absent of an existing key is a conflict"
    );
    let got = store.get_object(key).await.expect("get").expect("created key exists");
    assert_eq!((got.value.as_slice(), got.version), (b"one".as_slice(), 1), "read after write");

    assert_eq!(store.compare_and_swap(key, 1, b"two".to_vec()).await.expect("cas"), 2, "cas bumps the version");
    assert!(
        matches!(store.compare_and_swap(key, 1, b"stale".to_vec()).await, Err(StoreError::Precondition(_))),
        "cas with an old version is refused"
    );
    assert_eq!(store.get_object(key).await.expect("get").expect("exists").value, b"two");
    assert!(
        matches!(store.compare_and_swap("conformance/missing", 1, Vec::new()).await, Err(StoreError::NotFound(_))),
        "cas of a missing key is not found"
    );

    let (a, b) =
        join(store.compare_and_swap(key, 2, b"a".to_vec()), store.compare_and_swap(key, 2, b"b".to_vec())).await;
    assert_eq!(usize::from(a.is_ok()) + usize::from(b.is_ok()), 1, "exactly one of two concurrent cas wins");
    let race = "conformance/d/shop/markers/race";
    let (a, b) = join(store.create_if_absent(race, b"a".to_vec()), store.create_if_absent(race, b"b".to_vec())).await;
    assert_eq!(usize::from(a.is_ok()) + usize::from(b.is_ok()), 1, "exactly one of two concurrent creates wins");

    store.create_if_absent("conformance/d/shopping/markers/x", Vec::new()).await.expect("create");
    store.create_if_absent("conformance/d/shop/cron/b", Vec::new()).await.expect("create");
    let listed: Vec<String> =
        store.list_objects("conformance/d/shop/").await.expect("list").into_iter().map(|(k, _)| k).collect();
    assert_eq!(
        listed,
        ["conformance/d/shop/cron/b", "conformance/d/shop/markers/a", "conformance/d/shop/markers/race"],
        "list returns the prefix's keys in ascending order"
    );

    assert!(
        matches!(store.delete_object(key, 1).await, Err(StoreError::Precondition(_))),
        "delete with an old version is refused"
    );
    assert!(store.delete_object(key, 3).await.expect("delete"), "delete at the current version");
    assert!(!store.delete_object(key, 3).await.expect("delete again"), "deleting a missing key returns false");
    assert!(store.get_object(key).await.expect("get").is_none(), "a deleted key is gone");
    assert_eq!(store.create_if_absent(key, Vec::new()).await.expect("recreate"), 1, "a recreated key starts over");

    for bad in ["", "conformance/../x", "conformance//x", "conformance/a b"] {
        assert!(matches!(store.get_object(bad).await, Err(StoreError::InvalidId(_))), "invalid key `{bad}`");
    }
    let big = vec![0u8; MAX_VALUE_BYTES + 1];
    assert!(store.create_if_absent("conformance/big", big).await.is_err(), "oversized values are refused");
}
