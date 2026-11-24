//! Client SSE streams under load (plan §4.2): slow clients, disconnects and
//! 1,000 concurrent streams through one gateway, over the lean agent stream
//! client (the fake agents listen on loopback `http://`).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

mod common;

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use common::{API_KEY, DEPLOYMENT, Options, TestGateway, start_gateway};
use graphrt_core::store::{RunStatus, RunStore as _};
use graphrt_gateway::pool::PoolConfig;
use graphrt_gateway::quota::Quotas;
use serde_json::{Value as Json, json};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;

fn runs() -> String {
    format!("/v1/deployments/{DEPLOYMENT}/runs")
}

/// Opens a streaming run on a raw socket and reads until its `run_started`
/// frame: returns the socket (unread from there on) and the run id.
async fn open_raw_stream(gateway: &TestGateway, inputs: Json) -> (TcpStream, String) {
    let address = gateway.url.trim_start_matches("http://").to_owned();
    let mut socket = TcpStream::connect(&address).await.unwrap();
    let body = json!({ "stream": true, "inputs": inputs }).to_string();
    let request = format!(
        "POST {} HTTP/1.1\r\nhost: {address}\r\nx-api-key: {API_KEY}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
        runs(),
        body.len()
    );
    socket.write_all(request.as_bytes()).await.unwrap();
    let mut received = Vec::new();
    let mut chunk = [0u8; 512];
    let marker = b"\"run_id\":\"";
    loop {
        let read = socket.read(&mut chunk).await.unwrap();
        assert!(read > 0, "stream ended early: {}", String::from_utf8_lossy(&received));
        received.extend_from_slice(&chunk[..read]);
        let text = String::from_utf8_lossy(&received);
        if let Some(start) =
            text.find("event: run_started").and_then(|at| text[at..].find("\"run_id\":\"").map(|i| at + i))
        {
            let rest = &text[start + marker.len()..];
            if let Some(end) = rest.find('"') {
                return (socket, rest[..end].to_owned());
            }
        }
    }
}

fn engine_ids(frames: &[(Option<String>, String, Json)]) -> Vec<u64> {
    frames
        .iter()
        .filter_map(|(id, _, _)| id.as_deref())
        .filter(|id| !id.contains('.'))
        .map(|id| id.parse().unwrap())
        .collect()
}

async fn wait_for(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// A client that stops reading (a phone on a train) holds no more than its
/// bounded buffers: the run, the agent and the other clients of the run keep
/// going, and the slow client is cut off and can resume from the store.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn slow_client_never_stalls_the_run_or_other_clients() {
    let gateway = start_gateway(Options::default()).await;
    // 160 x 64 KiB response chunks: ~10 MiB, far more than socket buffers hold.
    let events = 160u64;
    let inputs = json!({ "events": events, "delay_ms": 1, "pad_bytes": 64 * 1024 });
    let (slow, run_id) = open_raw_stream(&gateway, inputs).await;
    let started = Instant::now();
    let fast = gateway.sse(&format!("{}/{run_id}/events", runs()), None, None).await;
    let elapsed = started.elapsed();
    let ids = engine_ids(&fast);
    assert_eq!(ids, (0..events + 2).collect::<Vec<_>>(), "gapless, in order");
    assert_eq!(fast.last().unwrap().1, "run_succeeded");
    assert!(elapsed < Duration::from_secs(15), "the fast client was held back: {elapsed:?}");
    let record = gateway.wait_settled(&run_id).await;
    assert_eq!(record["status"], "succeeded");
    assert_eq!(record["event_count"], events + 2);
    // The slow client still holds its socket; once it goes, so does its stream.
    drop(slow);
    let metrics = &gateway.gateway.metrics;
    wait_for("the client streams to close", || metrics.client_streams.load(Ordering::Relaxed) == 0).await;
    // It resumes where it left off from the store, without gaps.
    let resumed = gateway.sse(&format!("{}/{run_id}/events", runs()), None, Some("0")).await;
    assert_eq!(engine_ids(&resumed), (1..events + 2).collect::<Vec<_>>());
}

/// A client that disconnects mid-stream ends only its own stream: the run
/// finishes and is stored in full.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn client_disconnect_mid_stream_keeps_the_run() {
    let gateway = start_gateway(Options::default()).await;
    let events = 20u64;
    let (socket, run_id) = open_raw_stream(&gateway, json!({ "events": events, "delay_ms": 10 })).await;
    drop(socket);
    let record = gateway.wait_settled(&run_id).await;
    assert_eq!(record["status"], "succeeded");
    assert_eq!(record["event_count"], events + 2);
    let stored = gateway.gateway.store.get(&run_id).await.unwrap().unwrap();
    assert_eq!(stored.status, RunStatus::Succeeded);
    let metrics = &gateway.gateway.metrics;
    wait_for("the client stream to close", || metrics.client_streams.load(Ordering::Relaxed) == 0).await;
    // The agent connection of the finished stream is kept for the next run.
    wait_for("the agent connection to be kept", || gateway.gateway.agents.idle_stream_connections() == 1).await;
    let (status, record) = gateway.post(&runs(), json!({ "inputs": { "text": "again" } })).await;
    assert_eq!((status, record["status"].as_str()), (200, Some("succeeded")));
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
    let started = Instant::now();
    let tasks: Vec<_> = (0..streams)
        .map(|_| {
            let (url, body) = (gateway.url.clone(), json!({ "stream": true, "inputs": inputs.clone() }));
            // One HTTP client per stream: one connection per stream, as browsers do.
            tokio::spawn(async move {
                let client = reqwest::Client::new();
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
