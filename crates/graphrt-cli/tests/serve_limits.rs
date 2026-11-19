//! `graphrt serve` admission control: `--max-active-runs`.
#![cfg(all(feature = "serve", feature = "code"))]
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::PathBuf;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use graphrt_cli::serve::{build_state, discover, router};
use graphrt_cli::setup::ServiceOptions;
use graphrt_core::engine::Limits;
use tower::ServiceExt;

/// A workflow whose code node keeps the run busy for a while.
fn workflows_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("graphrt-serve-limits-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("slow.yaml"),
        r#"version: "1"
name: slow
graph:
  nodes:
    - { id: start, type: start, variables: [] }
    - id: wait
      type: code
      code: "function main() { const t = Date.now() + 1500; while (Date.now() < t) {} return { ok: true }; }"
    - { id: end, type: end, outputs: [{ variable: ok, value_selector: [wait, ok] }] }
  edges:
    - { source: start, target: wait }
    - { source: wait, target: end }
"#,
    )
    .unwrap();
    dir
}

#[tokio::test]
async fn runs_beyond_the_active_limit_are_refused() {
    let dir = workflows_dir();
    let files = discover(&dir).expect("discover");
    let options = ServiceOptions {
        plugins: graphrt_cli::extensions::PluginOptions { dirs: Vec::new(), no_default: true, trusted: Vec::new() },
        ..ServiceOptions::default()
    };
    let state = build_state(&files, &options, Limits::default(), None).await.expect("state").with_max_active_runs(1);
    let app = router(Arc::new(state), 1 << 20);
    let start = || {
        Request::post("/v1/workflows/slow/runs")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"inputs": {}, "async": true}"#))
            .expect("request")
    };

    let first = app.clone().oneshot(start()).await.expect("response");
    assert_eq!(first.status(), StatusCode::ACCEPTED);
    let second = app.clone().oneshot(start()).await.expect("response");
    assert_eq!(second.status(), StatusCode::TOO_MANY_REQUESTS);

    // Once the first run finishes, new runs are admitted again.
    let mut admitted = false;
    for _ in 0..100 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let response = app.clone().oneshot(start()).await.expect("response");
        if response.status() == StatusCode::ACCEPTED {
            admitted = true;
            break;
        }
    }
    assert!(admitted, "a run should be admitted after the first one finished");
    let _ = std::fs::remove_dir_all(&dir);
}
