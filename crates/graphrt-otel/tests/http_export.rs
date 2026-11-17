//! OTLP/HTTP exporter against an in-test collector.
#![cfg(feature = "http")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use graphrt_otel::http::ExportSettings;
use graphrt_otel::span::now_unix_nanos;
use graphrt_otel::{Metrics, OtelConfig, OtlpHttpExporter, Span, SpanExporter, TraceId};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

#[derive(Debug, Clone)]
struct Request {
    path: String,
    headers: String,
    body: serde_json::Value,
}

/// Collector answering each request with the next status from `statuses` (then 200).
async fn collector(statuses: Vec<u16>) -> (String, Arc<Mutex<Vec<Request>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    tokio::spawn(async move {
        let mut statuses = statuses.into_iter();
        loop {
            let Ok((mut sock, _)) = listener.accept().await else { return };
            let mut buf = Vec::new();
            let mut chunk = [0u8; 8192];
            let (head, body) = loop {
                let n = sock.read(&mut chunk).await.unwrap_or(0);
                if n == 0 {
                    break (String::new(), Vec::new());
                }
                buf.extend_from_slice(&chunk[..n]);
                if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&buf[..pos]).to_string();
                    let len = head
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().to_owned())
                        })
                        .and_then(|v| v.parse::<usize>().ok())
                        .unwrap_or(0);
                    let mut body = buf[pos + 4..].to_vec();
                    while body.len() < len {
                        let n = sock.read(&mut chunk).await.unwrap_or(0);
                        if n == 0 {
                            break;
                        }
                        body.extend_from_slice(&chunk[..n]);
                    }
                    break (head, body);
                }
            };
            let path = head.split_whitespace().nth(1).unwrap_or_default().to_owned();
            log.lock().unwrap().push(Request {
                path,
                headers: head.to_ascii_lowercase(),
                body: serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null),
            });
            let status = statuses.next().unwrap_or(200);
            let reason = if status == 200 { "OK" } else { "Service Unavailable" };
            let response = format!(
                "HTTP/1.1 {status} {reason}\r\ncontent-length: 2\r\nretry-after: 0\r\nconnection: close\r\n\r\n{{}}"
            );
            let _ = sock.write_all(response.as_bytes()).await;
        }
    });
    (format!("http://{addr}"), seen)
}

fn config(endpoint: &str) -> OtelConfig {
    let vars = vec![
        ("OTEL_EXPORTER_OTLP_ENDPOINT".to_owned(), endpoint.to_owned()),
        ("OTEL_EXPORTER_OTLP_HEADERS".to_owned(), "x-api-key=secret-key".to_owned()),
        ("OTEL_SERVICE_NAME".to_owned(), "graphrt-test".to_owned()),
    ];
    OtelConfig::from_vars(&vars).unwrap()
}

fn span(name: &str) -> Span {
    let mut s = Span::new(TraceId::random(), None, name, now_unix_nanos());
    s.end_unix_nano = s.start_unix_nano + 1_000;
    s
}

fn settings() -> ExportSettings {
    ExportSettings {
        batch_size: 2,
        queue_capacity: 64,
        flush_interval: Duration::from_secs(60),
        metrics_interval: Duration::from_secs(60),
        timeout: Duration::from_secs(5),
        max_retries: 2,
        initial_backoff: Duration::from_millis(10),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn batches_spans_and_sends_headers() {
    let (endpoint, seen) = collector(vec![]).await;
    let metrics = Arc::new(Metrics::new());
    metrics.record_run("wf", "succeeded", 5);
    let exporter = OtlpHttpExporter::start_with(&config(&endpoint), Some(metrics), settings()).unwrap();
    for i in 0..5 {
        exporter.export(span(&format!("s{i}")));
    }
    exporter.shutdown().await;
    let requests = seen.lock().unwrap().clone();
    let traces: Vec<&Request> = requests.iter().filter(|r| r.path == "/v1/traces").collect();
    assert_eq!(traces.len(), 3, "5 spans in batches of 2");
    let total: usize = traces
        .iter()
        .map(|r| r.body["resourceSpans"][0]["scopeSpans"][0]["spans"].as_array().map_or(0, Vec::len))
        .sum();
    assert_eq!(total, 5);
    assert!(traces[0].headers.contains("x-api-key: secret-key"));
    assert!(traces[0].headers.contains("content-type: application/json"));
    assert_eq!(traces[0].body["resourceSpans"][0]["resource"]["attributes"][0]["value"]["stringValue"], "graphrt-test");
    let metric_requests: Vec<&Request> = requests.iter().filter(|r| r.path == "/v1/metrics").collect();
    assert_eq!(metric_requests.len(), 1);
    assert_eq!(metric_requests[0].body["resourceMetrics"][0]["scopeMetrics"][0]["metrics"][0]["name"], "graphrt.runs");
    assert_eq!(exporter.exported(), 5);
    assert_eq!(exporter.dropped(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retries_on_unavailable() {
    let (endpoint, seen) = collector(vec![503, 503]).await;
    let exporter = OtlpHttpExporter::start_with(&config(&endpoint), None, settings()).unwrap();
    exporter.export(span("a"));
    exporter.flush().await;
    assert_eq!(seen.lock().unwrap().len(), 3, "two 503s, then success");
    assert_eq!(exporter.exported(), 1);
    assert_eq!(exporter.failed(), 0);

    let (endpoint, seen) = collector(vec![503, 503, 503, 503]).await;
    let exporter = OtlpHttpExporter::start_with(&config(&endpoint), None, settings()).unwrap();
    exporter.export(span("b"));
    exporter.flush().await;
    assert_eq!(seen.lock().unwrap().len(), 3, "initial attempt + 2 retries");
    assert_eq!(exporter.failed(), 1);
}

#[tokio::test]
async fn full_queue_drops_instead_of_blocking() {
    let (endpoint, _seen) = collector(vec![]).await;
    let settings = ExportSettings { queue_capacity: 4, ..settings() };
    let exporter = OtlpHttpExporter::start_with(&config(&endpoint), None, settings).unwrap();
    // Current-thread runtime: the worker cannot drain while this loop runs.
    for i in 0..10 {
        exporter.export(span(&format!("s{i}")));
    }
    assert_eq!(exporter.dropped(), 6);
    exporter.shutdown().await;
    assert_eq!(exporter.exported(), 4);
}

#[test]
fn start_outside_tokio_is_an_error() {
    let err = OtlpHttpExporter::start(&OtelConfig::default(), None).unwrap_err();
    assert_eq!(err, graphrt_otel::ExportError::NoRuntime);
}
