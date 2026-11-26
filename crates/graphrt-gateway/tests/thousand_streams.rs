//! 1,000 concurrent client SSE streams through one gateway (plan §4.2), over
//! the lean agent stream client (the fake agents listen on loopback `http://`).
//!
//! A test binary of its own: it bounds the gap between chunks, and `cargo test`
//! runs the tests of one binary side by side. Next to the other stream tests
//! it would share the CPUs of a CI runner with their runtimes.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

mod common;

use std::time::{Duration, Instant};

use common::{API_KEY, DEPLOYMENT, Options, start_gateway};
use graphrt_gateway::pool::PoolConfig;
use graphrt_gateway::quota::Quotas;
use serde_json::json;

fn runs() -> String {
    format!("/v1/deployments/{DEPLOYMENT}/runs")
}

async fn wait_for(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// The soft open-file limit where it can be read without `unsafe` (Linux).
fn open_file_limit() -> Option<u64> {
    let limits = std::fs::read_to_string("/proc/self/limits").ok()?;
    let line = limits.lines().find(|line| line.starts_with("Max open files"))?;
    line.split_whitespace().nth(3)?.parse().ok()
}

/// 1,000 concurrent streaming runs through one gateway (the 64 MiB target of
/// plan §4.2 is measured on the release binaries by bench/gateway/gwbench.py):
/// every stream complete and gapless, no stream stalled.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn thousand_concurrent_streams() {
    // Each stream holds 4 sockets in this process: test client, gateway (both
    // ends) and fake agent. Fewer streams where the open-file limit is lower.
    let streams = open_file_limit()
        .map_or(1000, |limit| 1000usize.min(usize::try_from(limit.saturating_sub(256) / 4).unwrap_or(1000)));
    if streams < 1000 {
        eprintln!("open-file limit {:?}: running {streams} streams instead of 1,000", open_file_limit());
    }
    let pool = PoolConfig {
        max_active_runs: 250,
        min_replicas: 4,
        queue_timeout: Duration::from_secs(60),
        quotas: Quotas { max_replicas: 4, max_queued: 4096, ..Quotas::default() },
        ..PoolConfig::default()
    };
    let gateway = start_gateway(Options { pool, ..Options::default() }).await;
    let events = 5u64;
    let interval_ms = 200u64;
    let inputs = json!({ "events": events, "delay_ms": interval_ms });
    // One HTTP client per stream: one connection per stream, as browsers do.
    // Built before the streams start and off the runtime: each one costs about
    // 13 ms of CPU in a debug build, and building them inside the streams kept
    // the gateway and the agents of this process from running for seconds.
    let clients = tokio::task::spawn_blocking(move || (0..streams).map(|_| reqwest::Client::new()).collect::<Vec<_>>())
        .await
        .unwrap();
    let started = Instant::now();
    let tasks: Vec<_> = clients
        .into_iter()
        .map(|client| {
            let (url, body) = (gateway.url.clone(), json!({ "stream": true, "inputs": inputs.clone() }));
            tokio::spawn(async move {
                let response = client
                    .post(format!("{url}{}", runs()))
                    .header("x-api-key", API_KEY)
                    .json(&body)
                    .send()
                    .await
                    .unwrap();
                let mut response = response;
                let mut parser = graphrt_gateway::sse::Parser::default();
                let (mut frames, mut last, mut max_gap) = (Vec::new(), Instant::now(), Duration::ZERO);
                while let Some(chunk) = response.chunk().await.unwrap() {
                    let now = Instant::now();
                    max_gap = max_gap.max(now - last);
                    last = now;
                    frames.extend(parser.feed(&chunk).unwrap());
                }
                (frames, max_gap)
            })
        })
        .collect();
    let mut worst_gap = Duration::ZERO;
    for task in tasks {
        let (frames, max_gap) = task.await.unwrap();
        let ids: Vec<u64> = frames.iter().filter_map(|frame| frame.event_index()).collect();
        assert_eq!(ids, (0..events + 2).collect::<Vec<_>>(), "gapless, in order");
        assert_eq!(frames.last().unwrap().event, "run_succeeded");
        worst_gap = worst_gap.max(max_gap);
    }
    let elapsed = started.elapsed();
    // A stall is a gap well above the agent's own pace between chunks.
    assert!(worst_gap < Duration::from_millis(interval_ms * 10), "a stream stalled for {worst_gap:?}");
    eprintln!("{streams} streams in {elapsed:?}, worst gap between chunks {worst_gap:?}");
    let agents = &gateway.gateway.agents;
    assert!(agents.idle_stream_connections() <= 4 * graphrt_gateway::stream_conn::MAX_IDLE_PER_AGENT);
    wait_for("finished streams to give their agent connections back", || agents.idle_stream_connections() > 0).await;
}
