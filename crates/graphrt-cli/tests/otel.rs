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
    for var in ["OTEL_EXPORTER_OTLP_ENDPOINT", "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT", "OTEL_SDK_DISABLED"] {
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
fn otlp_endpoint_variable_enables_export() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let paths = Arc::new(Mutex::new(Vec::<String>::new()));
    let seen = paths.clone();
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
            let text = String::from_utf8_lossy(&buf).to_string();
            seen.lock().unwrap().push(text.split_whitespace().nth(1).unwrap_or_default().to_owned());
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}");
        }
    });
    let out = graphrt()
        .args(["run"])
        .arg(hello())
        .args(["--input", "name=Ada", "--no-default-plugins"])
        .env("OTEL_EXPORTER_OTLP_ENDPOINT", format!("http://{addr}"))
        .output()
        .expect("spawn graphrt");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let paths = paths.lock().unwrap().clone();
    assert!(paths.iter().any(|p| p == "/v1/traces"), "{paths:?}");
    assert!(paths.iter().any(|p| p == "/v1/metrics"), "{paths:?}");
}
