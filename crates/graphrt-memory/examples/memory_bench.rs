//! What long term memory costs: a write with fsync, the first load of a
//! scope, and recall from a cached scope, at the documented limits.
//!
//! ```sh
//! cargo run --release -p graphrt-memory --example memory_bench
//! ```

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::print_stdout, clippy::indexing_slicing)]

use std::time::{Duration, Instant};

use graphrt_core::memory::{MemoryBank, MemoryKind, MemoryQuery, MemoryScope, MemorySource, MemoryTrust, NewMemory};
use graphrt_memory::{Limits, MemoryStore};

const WORDS: [&str; 24] = [
    "invoice",
    "pdf",
    "monthly",
    "jakarta",
    "bandung",
    "coffee",
    "tea",
    "shipping",
    "refund",
    "plan",
    "gold",
    "reseller",
    "order",
    "chair",
    "black",
    "office",
    "friday",
    "email",
    "phone",
    "billing",
    "address",
    "warehouse",
    "discount",
    "contract",
];

fn note(n: usize) -> NewMemory {
    let words: Vec<&str> = (0..12).map(|k| WORDS[(n * 7 + k * 5) % WORDS.len()]).collect();
    let source = MemorySource { actor: "agent".into(), ..MemorySource::default() };
    NewMemory::new(format!("note {n}: {}", words.join(" ")), MemoryTrust::UserStated, source).kind(MemoryKind::Fact)
}

fn percentile(mut samples: Vec<Duration>, p: f64) -> Duration {
    samples.sort();
    let at = ((samples.len() as f64 - 1.0) * p).round() as usize;
    samples[at]
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let root = std::env::temp_dir().join(format!("graphrt-memory-bench-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let store = MemoryStore::open(&root).with_limits(Limits { user_notes: 500, ..Limits::default() });
    let scope = MemoryScope::user(None, "bench", "user-1");

    let mut writes = Vec::new();
    for n in 0..500 {
        let started = Instant::now();
        store.remember(&scope, note(n)).await.unwrap();
        writes.push(started.elapsed());
    }
    println!(
        "remember with fsync, 500 notes into one scope: p50 {:?}, p95 {:?}",
        percentile(writes.clone(), 0.5),
        percentile(writes, 0.95)
    );
    let bytes = std::fs::metadata(root.join("_/bench/users")).map(|_| ()).ok();
    let _ = bytes;

    // A new store reads the scope from disk on first use.
    let mut loads = Vec::new();
    for _ in 0..20 {
        let fresh = MemoryStore::open(&root);
        let started = Instant::now();
        fresh.recall(&scope, &MemoryQuery::new("invoice pdf")).await.unwrap();
        loads.push(started.elapsed());
    }
    println!("first load and recall of a scope of 500 notes: p50 {:?}", percentile(loads, 0.5));

    let mut recalls = Vec::new();
    for n in 0..200 {
        let query = format!("{} {}", WORDS[n % WORDS.len()], WORDS[(n * 3) % WORDS.len()]);
        let started = Instant::now();
        let hits = store.recall(&scope, &MemoryQuery::new(query)).await.unwrap();
        recalls.push(started.elapsed());
        assert!(!hits.is_empty());
    }
    println!(
        "recall by keyword, 500 notes, scope cached: p50 {:?}, p95 {:?}",
        percentile(recalls.clone(), 0.5),
        percentile(recalls, 0.95)
    );
    let _ = std::fs::remove_dir_all(&root);
}
