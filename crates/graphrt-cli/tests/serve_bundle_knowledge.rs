//! `graphrt serve` runs a bundle's knowledge-retrieval steps on the knowledge
//! config and passages that came in the bundle, as `graphrt run` does.
#![cfg(all(feature = "serve", feature = "bundle", feature = "rag"))]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::indexing_slicing)]

use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use graphrt_cli::serve::{build_state, discover, router};
use graphrt_cli::setup::ServiceOptions;
use graphrt_core::engine::Limits;
use http_body_util::BodyExt;
use tower::ServiceExt;

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

#[tokio::test]
async fn served_bundles_search_their_own_knowledge() {
    let dir = std::env::temp_dir().join(format!("graphrt-serve-bundle-kb-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let out = dir.join("policies.grt");
    // Store paths in the example knowledge config are relative to the repository root.
    let built = Command::new(env!("CARGO_BIN_EXE_graphrt"))
        .current_dir(root())
        .args(["bundle", "build", "examples/evals/retrieval/retrieve.yaml", "--knowledge"])
        .arg("examples/evals/retrieval/knowledge.yaml")
        .arg("-o")
        .arg(&out)
        .env("HOME", &dir)
        .output()
        .unwrap();
    assert!(built.status.success(), "{}", String::from_utf8_lossy(&built.stderr));

    let options = ServiceOptions {
        plugins: graphrt_cli::extensions::PluginOptions {
            dirs: Vec::new(),
            no_default: true,
            trusted: Vec::new(),
            pool: Default::default(),
            allow_private_network: false,
        },
        ..ServiceOptions::default()
    };
    assert!(options.knowledge.is_none(), "the host gives no knowledge config");
    let files = discover(&dir).unwrap();
    let state = build_state(&files, &options, Limits::default(), None).await.unwrap();
    let app = router(Arc::new(state), 1 << 20);
    let request = Request::post("/v1/workflows/policies/runs")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::json!({ "inputs": { "question": "How long do refunds take?" } }).to_string()))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    let status = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let body = String::from_utf8_lossy(&body);
    assert_eq!(status, StatusCode::OK, "{body}");
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["status"], "succeeded", "{body}");
    let result = json["outputs"]["result"].to_string().to_lowercase();
    assert!(result.contains("refund"), "the bundled passages are searched: {result}");
    let _ = std::fs::remove_dir_all(&dir);
}
