//! Latency and throughput of `S3Store` against a real object store, for the
//! numbers in docs/files-and-images-design.md.
//!
//! ```sh
//! export AWS_ACCESS_KEY_ID=... AWS_SECRET_ACCESS_KEY=...
//! GRAPHRT_S3_TEST_ENDPOINT=http://127.0.0.1:9000 GRAPHRT_S3_TEST_BUCKET=bench \
//!   cargo run --release -p graphrt-files --features s3 --example s3_bench
//! ```

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    clippy::cast_precision_loss,
    clippy::indexing_slicing
)]

use std::sync::Arc;
use std::time::Instant;

use graphrt_core::tool::{ToolCallContext, ToolRuntime};
use graphrt_files::{FileStore, FileTools, S3Config, S3Store, Workspace, WorkspaceLimits, WorkspacePath};
use serde_json::json;

const MIB: usize = 1024 * 1024;

fn path(raw: &str) -> WorkspacePath {
    WorkspacePath::parse(raw).unwrap()
}

/// Median of `runs` timings of `work`, in milliseconds.
async fn median_ms<F, Fut>(runs: usize, mut work: F) -> f64
where
    F: FnMut() -> Fut,
    Fut: Future<Output = ()>,
{
    let mut samples = Vec::with_capacity(runs);
    for _ in 0..runs {
        let started = Instant::now();
        work().await;
        samples.push(started.elapsed().as_secs_f64() * 1000.0);
    }
    samples.sort_by(f64::total_cmp);
    samples[samples.len() / 2]
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() {
    let endpoint = std::env::var("GRAPHRT_S3_TEST_ENDPOINT").ok();
    let bucket = std::env::var("GRAPHRT_S3_TEST_BUCKET").expect("set GRAPHRT_S3_TEST_BUCKET");
    let region = std::env::var("AWS_REGION").unwrap_or_else(|_| "us-east-1".into());
    let config = S3Config {
        endpoint,
        prefix: Some(format!("graphrt-bench-{}", std::process::id())),
        allow_http: true,
        ..S3Config::new(bucket, region)
    };
    let store = Arc::new(S3Store::from_env(&config).unwrap());
    let workspace = Workspace::new(Arc::clone(&store) as Arc<dyn FileStore>, WorkspaceLimits::default());
    let tools = FileTools::new(workspace.clone());

    println!("| What | Result |\n| --- | --- |");
    let small = path("small.txt");
    let ms = median_ms(50, || async {
        store.write(&small, b"hello\n".to_vec(), true).await.unwrap();
    })
    .await;
    println!("| Write a small object | {ms:.2} ms |");
    let ms = median_ms(50, || async {
        store.stat(&small).await.unwrap();
    })
    .await;
    println!("| Stat | {ms:.2} ms |");
    let ms = median_ms(50, || async {
        store.read(&small, 0, 1024).await.unwrap();
    })
    .await;
    println!("| Read a small object | {ms:.2} ms |");

    let big = path("big.bin");
    let bytes: Vec<u8> = (0..16 * MIB).map(|i| (i % 251) as u8).collect();
    let ms = median_ms(5, || async {
        store.write(&big, bytes.clone(), true).await.unwrap();
    })
    .await;
    println!("| Write 16 MiB (hash, sign, upload) | {ms:.0} ms ({:.0} MiB/s) |", 16.0 / (ms / 1000.0));
    let ms = median_ms(5, || async {
        assert_eq!(store.read(&big, 0, 64 * MIB as u64).await.unwrap().len(), 16 * MIB);
    })
    .await;
    println!("| Read 16 MiB | {ms:.0} ms ({:.0} MiB/s) |", 16.0 / (ms / 1000.0));
    let ms = median_ms(20, || async {
        store.read(&big, 8 * MIB as u64, 256 * 1024).await.unwrap();
    })
    .await;
    println!("| Read a 256 KiB window in the middle of 16 MiB | {ms:.2} ms |");

    for index in 0..200 {
        store.write(&path(&format!("many/{index:04}.txt")), b"line one\nneedle here\n".to_vec(), true).await.unwrap();
    }
    let ms = median_ms(10, || async {
        assert_eq!(store.list(Some(&path("many")), 1000).await.unwrap().entries.len(), 200);
    })
    .await;
    println!("| List 200 objects | {ms:.2} ms |");
    let ms = median_ms(5, || async {
        let call = tools.call("search", json!({"query": "needle", "path": "many"}), ToolCallContext::empty());
        assert!(!call.await.unwrap().is_error);
    })
    .await;
    println!("| `files/search` across 200 small objects | {ms:.0} ms |");
    let ms = median_ms(10, || async {
        let call = tools.call("read", json!({"path": "many/0007.txt"}), ToolCallContext::empty());
        assert!(!call.await.unwrap().is_error);
    })
    .await;
    println!("| `files/read`, small file | {ms:.2} ms |");

    let listing = store.list(None, 10_000).await.unwrap();
    for entry in listing.entries {
        store.delete(&entry.path).await.unwrap();
    }
}
