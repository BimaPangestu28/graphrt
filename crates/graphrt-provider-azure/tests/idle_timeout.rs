//! C6: long SSE streams through an ingress with an idle timeout.
//!
//! `IdleProxy` stands in for the Container Apps ingress: it forwards TCP both
//! ways and cuts the connection after `idle` without a byte in either
//! direction. A fake agent streams events with long gaps, optionally with SSE
//! comment heartbeats (`: hb`) in between, scaled down (idle 300 ms instead of
//! 240 s, heartbeat 100 ms instead of 15 s: same 1:3 bound the gateway checks
//! with `heartbeat_keeps_stream_open`, which needs ≥ 2 heartbeats per idle window).
//!
//! This proves the mechanism; whether Azure's standard 240 s limit is idle or
//! total is proven only by `c06_real_azure_stream_outlives_240s` (ignored,
//! needs a deployed app).
#![cfg(feature = "provider-aca")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use axum::body::{Body, Bytes};
use axum::extract::Query;
use graphrt_provider_azure::aca::{AcaConfig, IngressMode, heartbeat_keeps_stream_open};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

fn epoch() -> Instant {
    static START: OnceLock<Instant> = OnceLock::new();
    *START.get_or_init(Instant::now)
}

fn micros_now() -> u64 {
    epoch().elapsed().as_micros() as u64
}

#[derive(serde::Deserialize)]
struct StreamParams {
    events: u64,
    gap_ms: u64,
    /// 0 = no heartbeats.
    heartbeat_ms: u64,
    payload: usize,
}

/// Fake agent: `events` events `gap_ms` apart, heartbeats every `heartbeat_ms`.
async fn stream(Query(params): Query<StreamParams>) -> axum::response::Response {
    let state = (0u64, params, None::<tokio::time::Instant>);
    let body = futures::stream::unfold(state, |(index, params, next_event_at)| async move {
        if index >= params.events {
            return None;
        }
        let now = tokio::time::Instant::now();
        let deadline = next_event_at.unwrap_or(now + Duration::from_millis(params.gap_ms));
        let beat = Duration::from_millis(params.heartbeat_ms);
        if params.heartbeat_ms > 0 && now + beat < deadline {
            tokio::time::sleep(beat).await;
            // Flushed as its own chunk, like the agent's heartbeat timer.
            return Some((
                Ok::<_, std::convert::Infallible>(Bytes::from_static(b": hb\n\n")),
                (index, params, Some(deadline)),
            ));
        }
        tokio::time::sleep_until(deadline).await;
        let padding = "x".repeat(params.payload);
        let event = format!("id: {index}\ndata: {{\"sent_us\":{},\"pad\":\"{padding}\"}}\n\n", micros_now());
        Some((Ok(Bytes::from(event)), (index + 1, params, None)))
    });
    axum::response::Response::builder()
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .body(Body::from_stream(body))
        .unwrap()
}

async fn start_agent() -> SocketAddr {
    let router = axum::Router::new().route("/stream", axum::routing::get(stream));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    address
}

/// TCP proxy that drops a connection after `idle` without traffic.
async fn start_idle_proxy(upstream: SocketAddr, idle: Duration) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (client, _) = listener.accept().await.unwrap();
            tokio::spawn(proxy_connection(client, upstream, idle));
        }
    });
    address
}

async fn proxy_connection(client: TcpStream, upstream: SocketAddr, idle: Duration) {
    let server = TcpStream::connect(upstream).await.unwrap();
    let last_activity = Arc::new(AtomicU64::new(micros_now()));
    let (client_read, client_write) = client.into_split();
    let (server_read, server_write) = server.into_split();
    let upstream_copy = tokio::spawn(copy_with_activity(client_read, server_write, last_activity.clone()));
    let downstream_copy = tokio::spawn(copy_with_activity(server_read, client_write, last_activity.clone()));
    loop {
        tokio::time::sleep(Duration::from_millis(10)).await;
        if upstream_copy.is_finished() && downstream_copy.is_finished() {
            return;
        }
        let idle_for = Duration::from_micros(micros_now().saturating_sub(last_activity.load(Ordering::SeqCst)));
        if idle_for >= idle {
            // Like an ingress idle timeout: both directions are closed.
            upstream_copy.abort();
            downstream_copy.abort();
            return;
        }
    }
}

async fn copy_with_activity(
    mut from: tokio::net::tcp::OwnedReadHalf,
    mut to: tokio::net::tcp::OwnedWriteHalf,
    last_activity: Arc<AtomicU64>,
) {
    let mut buffer = vec![0u8; 16 * 1024];
    while let Ok(read) = from.read(&mut buffer).await {
        if read == 0 {
            break;
        }
        last_activity.store(micros_now(), Ordering::SeqCst);
        if to.write_all(&buffer[..read]).await.is_err() {
            break;
        }
    }
    let _ = to.shutdown().await;
}

/// Read an SSE stream: event ids in order, and per-event delivery delay.
async fn read_events(url: &str) -> (Vec<u64>, Vec<Duration>, bool) {
    let client = graphrt_provider_azure::http::build_client(Duration::from_secs(600)).unwrap();
    let mut response = client.get(url).send().await.unwrap();
    let mut buffer = String::new();
    let (mut ids, mut delays) = (Vec::new(), Vec::new());
    let completed = loop {
        match response.chunk().await {
            Ok(Some(chunk)) => buffer.push_str(&String::from_utf8_lossy(&chunk)),
            Ok(None) => break true,
            Err(_) => break false,
        }
        while let Some(end) = buffer.find("\n\n") {
            let event: String = buffer.drain(..end + 2).collect();
            let Some(id) = event.lines().find_map(|line| line.strip_prefix("id: ")) else { continue };
            ids.push(id.parse().unwrap());
            let data = event.lines().find_map(|line| line.strip_prefix("data: ")).unwrap();
            let sent: serde_json::Value = serde_json::from_str(data).unwrap();
            let sent_us = sent["sent_us"].as_u64().unwrap();
            delays.push(Duration::from_micros(micros_now().saturating_sub(sent_us)));
        }
    };
    (ids, delays, completed)
}

fn p95(samples: &mut [Duration]) -> Duration {
    samples.sort();
    samples[((samples.len() as f64 * 0.95).ceil() as usize).saturating_sub(1).min(samples.len() - 1)]
}

#[tokio::test]
async fn c06_heartbeats_keep_a_stream_alive_past_the_idle_timeout() {
    epoch();
    let agent = start_agent().await;
    let proxy = start_idle_proxy(agent, Duration::from_millis(300)).await;
    // 4 events 400 ms apart: every gap is longer than the idle timeout.
    let url = format!("http://{proxy}/stream?events=4&gap_ms=400&heartbeat_ms=100&payload=16");
    let started = Instant::now();
    let (ids, _, completed) = read_events(&url).await;
    assert_eq!(ids, vec![0, 1, 2, 3], "no event lost or duplicated");
    assert!(completed, "stream ended normally, not cut");
    assert!(started.elapsed() > Duration::from_millis(1200), "stream outlived the idle timeout 4x");
}

#[tokio::test]
async fn c06_without_heartbeats_the_idle_timeout_cuts_the_stream() {
    epoch();
    let agent = start_agent().await;
    let proxy = start_idle_proxy(agent, Duration::from_millis(300)).await;
    let url = format!("http://{proxy}/stream?events=4&gap_ms=400&heartbeat_ms=0&payload=16");
    let (ids, _, completed) = read_events(&url).await;
    assert!(!completed && ids.len() < 4, "the fake ingress must cut a silent stream: {ids:?}");
}

#[tokio::test]
async fn c06_proxy_adds_under_50ms_p95_and_does_not_buffer() {
    epoch();
    let agent = start_agent().await;
    let proxy = start_idle_proxy(agent, Duration::from_secs(5)).await;
    let query = "stream?events=60&gap_ms=15&heartbeat_ms=0&payload=1000";
    let (direct_ids, mut direct, _) = read_events(&format!("http://{agent}/{query}")).await;
    let (proxied_ids, mut proxied, _) = read_events(&format!("http://{proxy}/{query}")).await;
    assert_eq!(direct_ids.len(), 60);
    assert_eq!(proxied_ids, direct_ids);
    let (direct_p95, proxied_p95) = (p95(&mut direct), p95(&mut proxied));
    eprintln!("C6 delivery delay p95: direct {direct_p95:?}, through idle proxy {proxied_p95:?}");
    assert!(proxied_p95.saturating_sub(direct_p95) <= Duration::from_millis(50));
    // Buffering of ≥ 1 chunk would delay an event by at least one gap (15 ms) on every event.
    // `p95` sorted the delays: the last ones are the largest.
    let late = proxied.iter().filter(|delay| **delay >= Duration::from_millis(15)).count();
    let largest: Vec<Duration> = proxied.iter().rev().take(5).copied().collect();
    assert!(
        proxied.iter().max().unwrap() < &Duration::from_millis(50),
        "events must arrive as they are sent: {late} of {} events 15 ms or more late (buffering delays most, \
         a busy runner one or two), the largest delays {largest:?}, direct p95 {direct_p95:?}",
        proxied.len()
    );
}

#[test]
fn c06_plan_heartbeat_fits_every_ingress_mode() {
    let mut config = AcaConfig::new("sub", "rg");
    for mode in [
        IngressMode::Standard,
        IngressMode::StandardIdleVerified,
        IngressMode::Premium { request_idle_timeout_minutes: 4 },
    ] {
        config.ingress = mode;
        assert!(heartbeat_keeps_stream_open(Duration::from_secs(15), &config.capabilities()), "{mode:?}");
    }
}

/// Real Azure: an SSE stream through a deployed container app's ingress for
/// more than 240 s, with data gaps longer than 240 s covered by heartbeats.
/// `GRAPHRT_AZURE_C6_URL` must be such a stream (e.g. a workflow that sleeps
/// 300 s between two events on an agent with 15 s heartbeats). Passing proves
/// the standard ingress limit is idle-only (`IngressMode::StandardIdleVerified`).
#[tokio::test]
#[ignore = "needs a deployed Azure Container App (GRAPHRT_AZURE_C6_URL)"]
async fn c06_real_azure_stream_outlives_240s() {
    let Ok(url) = std::env::var("GRAPHRT_AZURE_C6_URL") else { return };
    let started = Instant::now();
    let client = graphrt_provider_azure::http::build_client(Duration::from_secs(600)).unwrap();
    let mut response = client.get(url).send().await.unwrap();
    let mut events = 0;
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                events += String::from_utf8_lossy(&chunk).lines().filter(|line| line.starts_with("id: ")).count()
            }
            Ok(None) => break,
            Err(error) => panic!("stream cut after {:?} ({events} events): {error}", started.elapsed()),
        }
    }
    assert!(started.elapsed() > Duration::from_secs(270), "stream must outlive the 240 s limit");
}
