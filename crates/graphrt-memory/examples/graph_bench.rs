//! What the memory graph costs at its limits: 2 000 entities and 10 000
//! relations, a two hop search through recall.
//!
//! ```sh
//! cargo run --release -p graphrt-memory --features graph --example graph_bench
//! ```

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::print_stdout, clippy::indexing_slicing)]

use std::time::{Duration, Instant};

use graphrt_core::memory::{
    ExtractedFacts, MemoryBank, MemoryQuery, MemoryScope, MemorySource, MemoryTrust, NewMemory,
};
use graphrt_memory::{Limits, MemoryStore};
use serde_json::json;

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let root = std::env::temp_dir().join(format!("graphrt-graph-bench-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let store = MemoryStore::open(&root).with_limits(Limits {
        scope_bytes: 64 * 1024 * 1024,
        user_notes: 500,
        ..Limits::default()
    });
    let scope = MemoryScope::user(None, "bench", "user-1");
    let source = MemorySource { actor: "agent".into(), ..MemorySource::default() };
    let started = Instant::now();
    // 500 notes, 20 relations each among 2 000 entities.
    for n in 0..500_usize {
        let note = store
            .remember(
                &scope,
                NewMemory::new(format!("note {n} about entity{n}"), MemoryTrust::UserStated, source.clone()),
            )
            .await
            .unwrap();
        let relations: Vec<_> = (0..20)
            .map(|k| {
                let a = (n * 4 + k) % 2_000;
                let b = (n * 7 + k * 13 + 1) % 2_000;
                json!({"source": format!("entity{a}"), "relation": format!("rel_{}", k % 9), "target": format!("entity{b}")})
            })
            .collect();
        let facts: ExtractedFacts = serde_json::from_value(json!({"relations": relations})).unwrap();
        store.link(&scope, &note.id, facts).await.unwrap();
    }
    println!("built 500 notes and 10 000 relations in {:?}", started.elapsed());
    let mut times: Vec<Duration> = Vec::new();
    for n in 0..50 {
        let query = MemoryQuery::new(format!("entity{}", n * 37 % 2_000));
        let started = Instant::now();
        let hits = store.recall(&scope, &query).await.unwrap();
        times.push(started.elapsed());
        assert!(!hits.is_empty());
    }
    times.sort();
    println!("recall with a two hop graph search: p50 {:?}, p95 {:?}", times[25], times[47]);
    let _ = std::fs::remove_dir_all(&root);
}
