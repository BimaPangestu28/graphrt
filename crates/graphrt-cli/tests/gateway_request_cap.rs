//! `preempt_after_ms` of `POST /internal/v1/runs`: the agent preempts the
//! execution itself shortly before the platform's request cap, so the stream
//! ends with a resumable `run_paused` (kind `external`, `request_cap`) instead
//! of the platform killing the request mid-node.
#![cfg(all(feature = "trust-gateway", feature = "http"))]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use graphrt_cli::gateway_agent::signature::{self, GatewayKey, Signed};
use graphrt_cli::gateway_agent::{GatewayAgent, GatewayAgentConfig};
use graphrt_cli::serve::{build_state, discover, router};
use graphrt_cli::setup::ServiceOptions;
use graphrt_core::engine::Limits;
use http_body_util::BodyExt;
use serde_json::{Value as Json, json};
use tower::ServiceExt;

const SECRET: [u8; 32] = [0x24; 32];

/// An upstream that never answers in time (a slow model call).
async fn hanging_upstream() -> u16 {
    let app = axum::Router::new().route(
        "/work",
        axum::routing::get(|| async {
            tokio::time::sleep(Duration::from_secs(120)).await;
            "late"
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app).await });
    port
}

async fn app(port: u16) -> axum::Router {
    let dir = std::env::temp_dir().join(format!("graphrt-gw-request-cap-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("work.yaml"),
        format!(
            r#"version: "1"
name: work
graph:
  nodes:
    - {{ id: start, type: start, variables: [] }}
    - id: call
      type: http-request
      method: get
      url: "http://127.0.0.1:{port}/work"
      timeout: {{ connect: 5, read: 300 }}
    - {{ id: end, type: end, outputs: [{{ variable: body, value_selector: [call, body] }}] }}
  edges:
    - {{ source: start, target: call }}
    - {{ source: call, target: end }}
"#
        ),
    )
    .unwrap();
    let files = discover(&dir).expect("discover");
    let options = ServiceOptions {
        allow_private_network: true,
        plugins: graphrt_cli::extensions::PluginOptions {
            dirs: Vec::new(),
            no_default: true,
            trusted: Vec::new(),
            pool: Default::default(),
            allow_private_network: false,
        },
        ..ServiceOptions::default()
    };
    let state = build_state(&files, &options, Limits::default(), None).await.expect("state");
    let config = GatewayAgentConfig {
        deployment: "shop".to_owned(),
        keys: vec![GatewayKey::new("k1", &SECRET).unwrap()],
        reclaim_grace: Duration::from_secs(60),
        reattach_window: Duration::from_millis(50),
        max_body_bytes: 1 << 20,
    };
    router(Arc::new(state.with_gateway_agent(Some(Arc::new(GatewayAgent::new(config))))), 1 << 20)
}

fn signed_run(body: &Json) -> Request<Body> {
    let bytes = serde_json::to_vec(body).unwrap();
    let signed = Signed {
        deployment: "shop",
        method: "POST",
        path: "/internal/v1/runs",
        run_id: "cap-1",
        epoch: "1",
        body: &bytes,
    };
    let header = signature::sign(&GatewayKey::new("k1", &SECRET).unwrap(), signature::unix_now(), &signed);
    Request::post("/internal/v1/runs")
        .header("content-type", "application/json")
        .header("graphrt-protocol", "1")
        .header("graphrt-sig", header)
        .header("graphrt-deployment", "shop")
        .header("graphrt-run-id", "cap-1")
        .header("graphrt-epoch", "1")
        .body(Body::from(bytes))
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_run_is_preempted_before_the_request_cap_with_a_resumable_snapshot() {
    let app = app(hanging_upstream().await).await;
    let started = std::time::Instant::now();
    let response = app.oneshot(signed_run(&json!({ "workflow": "work", "preempt_after_ms": 300 }))).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = tokio::time::timeout(Duration::from_secs(20), response.into_body().collect())
        .await
        .expect("stream ends at the cap")
        .unwrap();
    let text = String::from_utf8_lossy(&body.to_bytes()).into_owned();
    assert!(started.elapsed() < Duration::from_secs(10), "preempted, not waiting for the hanging node");
    let paused = text
        .split("\n\n")
        .find(|frame| frame.contains("event: run_paused"))
        .unwrap_or_else(|| panic!("no run_paused in {text}"));
    let data: Json =
        serde_json::from_str(paused.lines().find_map(|line| line.strip_prefix("data: ")).unwrap()).unwrap();
    assert_eq!(data["reasons"][0]["kind"], "external");
    assert_eq!(data["reasons"][0]["data"], "request_cap");
    assert!(data.get("snapshot").is_some(), "the gateway needs the snapshot to re-dispatch");
}
