//! `graphrt run` with OpenTelemetry enabled.
#![cfg(feature = "otel")]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::indexing_slicing)]

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex};

fn hello() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/hello.yaml")
}

fn graphrt() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_graphrt"));
    for var in [
        "OTEL_EXPORTER_OTLP_ENDPOINT",
        "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT",
        "OTEL_SDK_DISABLED",
        "OTEL_INSTRUMENTATION_GENAI_CAPTURE_CONTENT",
        "GRAPHRT_OTEL_CAPTURE_CONTENT",
        "GRAPHRT_OTEL_PRESET",
        "OTEL_EXPORTER_OTLP_HEADERS",
        "LANGFUSE_HOST",
        "LANGFUSE_BASE_URL",
    ] {
        cmd.env_remove(var);
    }
    cmd.env("HOME", std::env::temp_dir()).env_remove("GRAPHRT_PLUGIN_PATH");
    cmd
}

#[test]
fn otel_stdout_prints_span_lines_on_stderr() {
    let out = graphrt()
        .args(["run"])
        .arg(hello())
        .args(["--input", "name=Ada", "--otel-stdout", "--no-default-plugins"])
        .output()
        .expect("spawn graphrt");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    // Stdout still carries only the run result.
    assert!(String::from_utf8_lossy(&out.stdout).contains("Hello, Ada."));
    let spans: Vec<serde_json::Value> = String::from_utf8_lossy(&out.stderr)
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|v| v.get("spanId").is_some())
        .collect();
    let root = spans.iter().find(|s| s["name"] == "invoke_workflow hello").expect("root span");
    assert_eq!(spans.len(), 4, "root + start/greet/end");
    for span in spans.iter().filter(|s| s["name"] != "invoke_workflow hello") {
        assert_eq!(span["traceId"], root["traceId"]);
        assert_eq!(span["parentSpanId"], root["spanId"]);
    }
}

#[test]
fn run_context_flags_and_content_capture_reach_the_spans() {
    let out = graphrt()
        .args(["run"])
        .arg(hello())
        .args(["--input", "name=Ada", "--otel-stdout", "--no-default-plugins"])
        .args(["--session-id", "s-1", "--user-id", "u-1", "--tag", "nightly", "--metadata", "suite=smoke"])
        .env("OTEL_INSTRUMENTATION_GENAI_CAPTURE_CONTENT", "SPAN_ONLY")
        .output()
        .expect("spawn graphrt");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let spans: Vec<serde_json::Value> = String::from_utf8_lossy(&out.stderr)
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|v| v.get("spanId").is_some())
        .collect();
    let attr = |span: &serde_json::Value, key: &str| {
        span["attributes"].as_array().unwrap().iter().find(|a| a["key"] == key).map(|a| a["value"].clone())
    };
    assert!(!spans.is_empty());
    for span in &spans {
        assert_eq!(attr(span, "session.id"), Some(serde_json::json!({"stringValue": "s-1"})), "{span}");
        assert_eq!(attr(span, "langfuse.trace.metadata.suite"), Some(serde_json::json!({"stringValue": "smoke"})));
    }
    let root = spans.iter().find(|s| s["name"] == "invoke_workflow hello").expect("root span");
    let tags = attr(root, "langfuse.trace.tags").unwrap();
    assert_eq!(tags["arrayValue"]["values"][0]["stringValue"], "nightly");
    let input = attr(root, "langfuse.observation.input").unwrap();
    assert!(input["stringValue"].as_str().unwrap().contains("Ada"), "{input}");
}

/// A fake OTLP collector on a local port: records every request (head and body), answers 200.
fn collector() -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let requests = Arc::new(Mutex::new(Vec::<String>::new()));
    let seen = requests.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let mut buf = Vec::new();
            let mut chunk = [0u8; 8192];
            loop {
                let n = stream.read(&mut chunk).unwrap_or(0);
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
                let text = String::from_utf8_lossy(&buf).to_string();
                if let Some(pos) = text.find("\r\n\r\n") {
                    let len = text
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().to_owned())
                        })
                        .and_then(|v| v.parse::<usize>().ok())
                        .unwrap_or(0);
                    if buf.len() >= pos + 4 + len {
                        break;
                    }
                }
            }
            seen.lock().unwrap().push(String::from_utf8_lossy(&buf).to_string());
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}");
        }
    });
    (format!("http://{addr}"), requests)
}

fn path_of(request: &str) -> &str {
    request.split_whitespace().nth(1).unwrap_or_default()
}

#[test]
fn otlp_endpoint_variable_enables_export() {
    let (url, requests) = collector();
    let out = graphrt()
        .args(["run"])
        .arg(hello())
        .args(["--input", "name=Ada", "--no-default-plugins"])
        .env("OTEL_EXPORTER_OTLP_ENDPOINT", url)
        .output()
        .expect("spawn graphrt");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let paths: Vec<String> = requests.lock().unwrap().iter().map(|r| path_of(r).to_owned()).collect();
    assert!(paths.iter().any(|p| p == "/v1/traces"), "{paths:?}");
    assert!(paths.iter().any(|p| p == "/v1/metrics"), "{paths:?}");
}

#[test]
fn langfuse_preset_sends_its_headers_from_its_variables() {
    let (url, requests) = collector();
    let out = graphrt()
        .args(["run"])
        .arg(hello())
        .args(["--input", "name=Ada", "--no-default-plugins", "--otel-preset", "langfuse"])
        .env("LANGFUSE_BASE_URL", &url)
        .env("LANGFUSE_PUBLIC_KEY", "pk-lf-1")
        .env("LANGFUSE_SECRET_KEY", "sk-lf-2")
        .output()
        .expect("spawn graphrt");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let requests = requests.lock().unwrap().clone();
    let traces = requests.iter().find(|r| path_of(r) == "/api/public/otel/v1/traces").expect("trace export");
    let head = traces.to_ascii_lowercase();
    assert!(head.contains("authorization: basic cgstbgytmtpzay1szi0y"), "{head}");
    assert!(head.contains("x-langfuse-ingestion-version: 4"), "{head}");
    assert!(!requests.iter().any(|r| path_of(r).ends_with("/v1/metrics")), "Langfuse takes no OTLP metrics");

    let missing = graphrt()
        .args(["run"])
        .arg(hello())
        .args(["--input", "name=Ada", "--no-default-plugins"])
        .env("GRAPHRT_OTEL_PRESET", "langfuse")
        .env("LANGFUSE_PUBLIC_KEY", "pk-lf-1")
        .env_remove("LANGFUSE_SECRET_KEY")
        .output()
        .expect("spawn graphrt");
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("LANGFUSE_SECRET_KEY"));
}
