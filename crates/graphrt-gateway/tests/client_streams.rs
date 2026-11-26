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

/// Decodes an HTTP/1.1 response with a chunked body: (head, body).
fn dechunk(raw: &[u8]) -> (String, Vec<u8>) {
    let split = raw.windows(4).position(|window| window == b"\r\n\r\n").expect("response head");
    let head = String::from_utf8_lossy(&raw[..split]).into_owned();
    let mut rest = &raw[split + 4..];
    let mut body = Vec::new();
    loop {
        let line_end = rest.windows(2).position(|window| window == b"\r\n").expect("chunk size line");
        let size = usize::from_str_radix(std::str::from_utf8(&rest[..line_end]).unwrap().trim(), 16).unwrap();
        rest = &rest[line_end + 2..];
        if size == 0 {
            return (head, body);
        }
        body.extend_from_slice(&rest[..size]);
        rest = &rest[size + 2..];
    }
}

/// A client that reads slowly falls behind the live broadcast while the run
/// finishes: once it reads again it still gets every event up to the terminal
/// one (from the replay ring), never a stream that just stops (a cut-off answer
/// a client cannot tell from a short one).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lagging_client_of_a_finished_run_gets_the_whole_stream() {
    let gateway = start_gateway(Options::default()).await;
    // 200 x 64 KiB chunks (402 frames, within the 512-frame ring): far more than
    // the socket buffers and the client's 64-frame buffer hold.
    let events = 200u64;
    let inputs = json!({ "events": events, "delay_ms": 0, "pad_bytes": 64 * 1024 });
    let address: std::net::SocketAddr = gateway.url.trim_start_matches("http://").parse().unwrap();
    let socket = tokio::net::TcpSocket::new_v4().unwrap();
    socket.set_recv_buffer_size(4096).unwrap();
    let mut socket = socket.connect(address).await.unwrap();
    let body = json!({ "stream": true, "inputs": inputs }).to_string();
    let request = format!(
        "POST {} HTTP/1.1\r\nhost: {address}\r\nx-api-key: {API_KEY}\r\nconnection: close\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
        runs(),
        body.len()
    );
    socket.write_all(request.as_bytes()).await.unwrap();
    // Read nothing until the run is over: the client stream lags behind the broadcast.
    let metrics = &gateway.gateway.metrics;
    wait_for("the run to finish", || metrics.runs_finished.load(Ordering::Relaxed) == 1).await;
    let mut raw = Vec::new();
    tokio::time::timeout(Duration::from_secs(30), socket.read_to_end(&mut raw)).await.unwrap().unwrap();
    let (head, body) = dechunk(&raw);
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    let frames = graphrt_gateway::sse::Parser::default().feed(&body).unwrap();
    let ids: Vec<u64> = frames.iter().filter_map(|frame| frame.event_index()).collect();
    assert_eq!(ids, (0..events + 2).collect::<Vec<_>>(), "gapless, in order, up to the terminal event");
    assert_eq!(frames.iter().filter(|frame| frame.event == "response").count() as u64, events);
    assert_eq!(frames.last().unwrap().event, "run_succeeded");
}

/// A run the gateway fails itself (here: its executor vanished without a
/// terminal event) still ends every client stream with `run_failed`, live and
/// on a reconnect, instead of a stream that just stops.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_run_the_gateway_fails_ends_its_streams_with_run_failed() {
    let gateway = start_gateway(Options { reclaim_grace: Duration::from_millis(300), ..Options::default() }).await;
    let body = json!({ "stream": true, "inputs": { "mode": "vanish", "events": 3 } });
    let live = gateway.sse(&runs(), Some(body), None).await;
    let (id, event, data) = live.last().unwrap();
    assert_eq!((id.as_deref(), event.as_str()), (None, "run_failed"), "{live:?}");
    assert_eq!(data["error_type"], "owner_lost");
    let ids = engine_ids(&live);
    assert_eq!(ids, (0..ids.len() as u64).collect::<Vec<_>>(), "the agents' events first, gapless");
    let run_id =
        live.iter().find(|(_, event, _)| event == "run_started").unwrap().2["run_id"].as_str().unwrap().to_owned();
    let last = ids.last().unwrap().to_string();
    for last_event_id in [None, Some(last.as_str())] {
        let replayed = gateway.sse(&format!("{}/{run_id}/events", runs()), None, last_event_id).await;
        assert_eq!(replayed.last().unwrap().1, "run_failed", "{last_event_id:?}: {replayed:?}");
    }
    // A finished run that has its own terminal event gets nothing made up.
    let ok = gateway.sse(&runs(), Some(json!({ "stream": true, "inputs": { "events": 1 } })), None).await;
    assert_eq!(ok.iter().filter(|(_, event, _)| event.starts_with("run_s")).count(), 2, "{ok:?}");
}
