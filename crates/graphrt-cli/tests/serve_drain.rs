//! Shutdown drain of `graphrt serve`: runs pause (never fail) on SIGTERM and
//! complete after a restart.
#![cfg(all(feature = "serve", feature = "http"))]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use graphrt_cli::drain::{DRAINED, drain};
use graphrt_cli::serve::{AppState, build_state, discover, resume_drained_runs, router};
use graphrt_cli::setup::ServiceOptions;
use graphrt_cli::store::FileStore;
use graphrt_core::engine::Limits;
use graphrt_core::store::RunStore;
use http_body_util::BodyExt;
use serde_json::{Value as Json, json};
use tower::ServiceExt;

/// An upstream the workflow calls: it hangs (like a slow LLM call) while `hang`
/// is set and answers at once otherwise. Counts requests.
#[derive(Debug, Default)]
struct Upstream {
    hang: AtomicBool,
    calls: AtomicUsize,
}

async fn start_upstream() -> (Arc<Upstream>, u16) {
    let upstream = Arc::new(Upstream { hang: AtomicBool::new(true), calls: AtomicUsize::new(0) });
    let shared = Arc::clone(&upstream);
    let app = axum::Router::new().route(
        "/work",
        axum::routing::get(move || {
            let upstream = Arc::clone(&shared);
            async move {
                upstream.calls.fetch_add(1, Ordering::SeqCst);
                if upstream.hang.load(Ordering::SeqCst) {
                    tokio::time::sleep(Duration::from_secs(120)).await;
                }
                "done"
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app).await });
    (upstream, port)
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("graphrt-serve-drain-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A workflow that calls the upstream once, then ends with its body.
fn workflows_dir(tag: &str, port: u16) -> PathBuf {
    let dir = temp_dir(&format!("wf-{tag}"));
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
    dir
}

async fn state_with(workflows: &Path, store: Arc<dyn RunStore>) -> Arc<AppState> {
    let files = discover(workflows).expect("discover");
    let options = ServiceOptions {
        allow_private_network: true,
        plugins: graphrt_cli::extensions::PluginOptions { dirs: Vec::new(), no_default: true, trusted: Vec::new() },
        ..ServiceOptions::default()
    };
    Arc::new(build_state(&files, &options, Limits::default(), None).await.expect("state").with_store(store))
}

fn post(uri: &str, body: Json) -> Request<Body> {
    Request::post(uri).header("content-type", "application/json").body(Body::from(body.to_string())).expect("request")
}

async fn send(app: &axum::Router, req: Request<Body>) -> (StatusCode, axum::http::HeaderMap, String) {
    let res = app.clone().oneshot(req).await.expect("response");
    let status = res.status();
    let headers = res.headers().clone();
    let body = res.into_body().collect().await.expect("body").to_bytes();
    (status, headers, String::from_utf8_lossy(&body).into_owned())
}

async fn get_json(app: &axum::Router, uri: &str) -> Json {
    let (status, _, body) = send(app, Request::get(uri).body(Body::empty()).unwrap()).await;
    assert_eq!(status, StatusCode::OK, "{uri}: {body}");
    serde_json::from_str(&body).expect("json")
}

async fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    for _ in 0..1000 {
        if done() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for {what}");
}

/// Poll a run until it is no longer executing.
async fn wait_settled(app: &axum::Router, run_id: &str) -> Json {
    for _ in 0..1000 {
        let run = get_json(app, &format!("/v1/runs/{run_id}")).await;
        if run["status"] != "running" && run["active"] == false {
            return run;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("run {run_id} did not settle");
}

fn run_id_of_sse(body: &str) -> String {
    let start = body.find("\"run_id\":\"").expect("run_started in the stream") + "\"run_id\":\"".len();
    body[start..].split('"').next().unwrap().to_owned()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sigterm_drain_pauses_async_and_streaming_runs_and_a_restart_completes_them() {
    let (upstream, port) = start_upstream().await;
    let wf = workflows_dir("mix", port);
    let store_dir = temp_dir("store-mix");
    let state = state_with(&wf, Arc::new(FileStore::open(&store_dir).unwrap())).await;
    let app = router(Arc::clone(&state), 1 << 20);

    // Five async runs and five streaming runs, all waiting on the hanging upstream.
    let mut async_ids = Vec::new();
    for _ in 0..5 {
        let (status, _, body) = send(&app, post("/v1/workflows/work/runs", json!({ "async": true }))).await;
        assert_eq!(status, StatusCode::ACCEPTED, "{body}");
        async_ids.push(serde_json::from_str::<Json>(&body).unwrap()["run_id"].as_str().unwrap().to_owned());
    }
    let mut streams = Vec::new();
    for _ in 0..5 {
        let app = app.clone();
        streams.push(tokio::spawn(async move {
            let (status, _, body) = send(&app, post("/v1/workflows/work/runs", json!({ "stream": true }))).await;
            assert_eq!(status, StatusCode::OK);
            body
        }));
    }
    wait_until("10 upstream calls", || upstream.calls.load(Ordering::SeqCst) == 10).await;
    assert_eq!(state.active_runs(), 10);

    // SIGTERM: drain with a 3 s deadline (preempting at about 2 s).
    let timeout = Duration::from_secs(3);
    let began = Instant::now();
    let draining = {
        let state = Arc::clone(&state);
        tokio::spawn(async move { drain(&state, timeout, "SIGTERM", None).await })
    };
    wait_until("drain start", || state.is_draining()).await;
    // New runs are refused with 503 + Retry-After; readiness fails, liveness does not.
    let (status, headers, _) = send(&app, post("/v1/workflows/work/runs", json!({ "async": true }))).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(headers.get("retry-after").and_then(|v| v.to_str().ok()), Some("5"));
    let (ready, _, _) = send(&app, Request::get("/readyz").body(Body::empty()).unwrap()).await;
    assert_eq!(ready, StatusCode::SERVICE_UNAVAILABLE);
    let (health, _, _) = send(&app, Request::get("/healthz").body(Body::empty()).unwrap()).await;
    assert_eq!(health, StatusCode::OK);

    let report = draining.await.unwrap();
    let elapsed = began.elapsed();

    assert!(elapsed <= timeout + Duration::from_millis(250), "drain took {elapsed:?}");
    assert_eq!((report.runs, report.paused, report.failed, report.unfinished), (10, 10, 0, 0), "{report}");
    assert!(report.preempted, "the hanging nodes had to be preempted");
    assert_eq!(state.active_runs(), 0);

    // Streaming callers got `run_paused` with the drained reason and the snapshot.
    let mut stream_ids = Vec::new();
    for stream in streams {
        let body = stream.await.unwrap();
        assert!(body.contains("event: run_paused"), "{body}");
        assert!(body.contains(&format!("\"kind\":\"{DRAINED}\"")), "{body}");
        assert!(body.contains("\"snapshot\""), "{body}");
        stream_ids.push(run_id_of_sse(&body));
    }
    // Every run is stored as paused (drained), none as failed.
    for id in async_ids.iter().chain(&stream_ids) {
        let run = get_json(&app, &format!("/v1/runs/{id}")).await;
        assert_eq!(run["status"], "paused", "{run}");
        assert_eq!(run["pause_reasons"][0]["kind"], DRAINED, "{run}");
    }
    for id in &async_ids {
        let run = get_json(&app, &format!("/v1/runs/{id}")).await;
        assert_eq!(run["pause_reasons"][0]["data"]["resume"], "auto");
    }
    for id in &stream_ids {
        let run = get_json(&app, &format!("/v1/runs/{id}")).await;
        assert_eq!(run["pause_reasons"][0]["data"]["resume"], "client");
    }
    drop(app);
    drop(state);

    // Restart on the same store, with the upstream answering now.
    upstream.hang.store(false, Ordering::SeqCst);
    let state = state_with(&wf, Arc::new(FileStore::open(&store_dir).unwrap())).await;
    assert_eq!(state.recover().await.unwrap(), 0, "nothing was left running");
    let app = router(Arc::clone(&state), 1 << 20);
    // Async runs resume by themselves; streaming callers resume theirs.
    assert_eq!(resume_drained_runs(Arc::clone(&state)).await, 5);
    for id in &stream_ids {
        let (status, _, body) = send(&app, post(&format!("/v1/runs/{id}/resume"), json!({}))).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(serde_json::from_str::<Json>(&body).unwrap()["status"], "succeeded", "{body}");
    }
    for id in &async_ids {
        let run = wait_settled(&app, id).await;
        assert_eq!(run["status"], "succeeded", "{run}");
        assert_eq!(run["outputs"]["body"], "done");
    }
    // The preempted node ran again once per run (at-least-once).
    assert_eq!(upstream.calls.load(Ordering::SeqCst), 20);
    let _ = std::fs::remove_dir_all(&wf);
    let _ = std::fs::remove_dir_all(&store_dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn drain_without_active_runs_returns_at_once() {
    let (_upstream, port) = start_upstream().await;
    let wf = workflows_dir("idle", port);
    let state = state_with(&wf, Arc::new(graphrt_cli::store::MemoryStore::new())).await;
    let began = Instant::now();
    let report = drain(&state, Duration::from_secs(5), "SIGTERM", None).await;
    assert_eq!(report.runs, 0);
    assert!(began.elapsed() < Duration::from_millis(500));
    let _ = std::fs::remove_dir_all(&wf);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn blocking_callers_get_a_paused_result() {
    let (upstream, port) = start_upstream().await;
    let wf = workflows_dir("blocking", port);
    let state = state_with(&wf, Arc::new(graphrt_cli::store::MemoryStore::new())).await;
    let app = router(Arc::clone(&state), 1 << 20);
    let call = {
        let app = app.clone();
        tokio::spawn(async move { send(&app, post("/v1/workflows/work/runs", json!({}))).await })
    };
    wait_until("the upstream call", || upstream.calls.load(Ordering::SeqCst) == 1).await;
    let report = drain(&state, Duration::from_millis(600), "SIGINT", None).await;
    assert_eq!((report.paused, report.failed), (1, 0), "{report}");
    let (status, _, body) = call.await.unwrap();
    assert_eq!(status, StatusCode::OK);
    let result: Json = serde_json::from_str(&body).unwrap();
    assert_eq!(result["status"], "paused", "{body}");
    assert_eq!(result["reasons"][0]["kind"], DRAINED);
    assert_eq!(result["reasons"][0]["data"]["cause"], "SIGINT");
    let _ = std::fs::remove_dir_all(&wf);
}

/// The real binary: SIGTERM drains and exits 0 before the deadline, the run is
/// stored as paused, and the next start resumes it to completion.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_binary_drains_on_sigterm_and_resumes_after_restart() {
    let (upstream, port) = start_upstream().await;
    let wf = workflows_dir("binary", port);
    let store_dir = temp_dir("store-binary");
    let addr = free_addr();
    let mut server = spawn_server(&wf, &store_dir, &addr);
    wait_healthy(&addr).await;
    let (status, body) = http(&addr, "POST", "/v1/workflows/work/runs", r#"{"async": true}"#);
    assert_eq!(status, 202, "{body}");
    let run_id = serde_json::from_str::<Json>(&body).unwrap()["run_id"].as_str().unwrap().to_owned();
    wait_until("the upstream call", || upstream.calls.load(Ordering::SeqCst) == 1).await;

    let began = Instant::now();
    let killed = std::process::Command::new("kill").args(["-TERM", &server.id().to_string()]).status().unwrap();
    assert!(killed.success());
    let exit = wait_exit(&mut server, Duration::from_secs(5)).await;
    assert!(exit.success(), "exit status {exit:?}");
    // --drain-timeout 1s, plus process start-up noise.
    assert!(began.elapsed() < Duration::from_millis(1800), "took {:?}", began.elapsed());
    let record: Json = serde_json::from_str(
        &std::fs::read_to_string(store_dir.join("runs").join(&run_id).join("record.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(record["status"], "paused", "{record}");
    assert_eq!(record["pause_reasons"][0]["data"]["cause"], "SIGTERM", "{record}");
    assert!(store_dir.join("runs").join(&run_id).join("snapshot.json").is_file());

    upstream.hang.store(false, Ordering::SeqCst);
    let mut server = spawn_server(&wf, &store_dir, &addr);
    wait_healthy(&addr).await;
    let mut settled = Json::Null;
    for _ in 0..500 {
        let (_, body) = http(&addr, "GET", &format!("/v1/runs/{run_id}"), "");
        settled = serde_json::from_str(&body).unwrap_or(Json::Null);
        if settled["status"] == "succeeded" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(settled["status"], "succeeded", "{settled}");
    let _ = std::process::Command::new("kill").args(["-INT", &server.id().to_string()]).status();
    let exit = wait_exit(&mut server, Duration::from_secs(5)).await;
    assert!(exit.success(), "SIGINT exit {exit:?}");
    let _ = std::fs::remove_dir_all(&wf);
    let _ = std::fs::remove_dir_all(&store_dir);
}

#[cfg(unix)]
fn free_addr() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().to_string()
}

#[cfg(unix)]
fn spawn_server(workflows: &Path, store: &Path, addr: &str) -> std::process::Child {
    std::process::Command::new(env!("CARGO_BIN_EXE_graphrt"))
        .args(["serve", workflows.to_str().unwrap(), "--addr", addr, "--store", store.to_str().unwrap()])
        .args(["--drain-timeout", "1s", "--allow-private-network", "--no-default-plugins"])
        .env_remove("GRAPHRT_API_KEY")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn graphrt serve")
}

#[cfg(unix)]
async fn wait_healthy(addr: &str) {
    for _ in 0..1000 {
        if std::net::TcpStream::connect(addr).is_ok() && http(addr, "GET", "/healthz", "").0 == 200 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("server on {addr} did not become healthy");
}

#[cfg(unix)]
async fn wait_exit(child: &mut std::process::Child, limit: Duration) -> std::process::ExitStatus {
    let began = Instant::now();
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        if began.elapsed() > limit {
            let _ = child.kill();
            panic!("graphrt serve did not exit within {limit:?}");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// A minimal HTTP/1.1 request (the test has no HTTP client dependency).
#[cfg(unix)]
fn http(addr: &str, method: &str, path: &str, body: &str) -> (u16, String) {
    let Ok(mut stream) = std::net::TcpStream::connect(addr) else { return (0, String::new()) };
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    if stream.write_all(request.as_bytes()).is_err() {
        return (0, String::new());
    }
    let mut response = String::new();
    let _ = stream.read_to_string(&mut response);
    let status = response.split(' ').nth(1).and_then(|code| code.parse().ok()).unwrap_or(0);
    let body = response.split_once("\r\n\r\n").map(|(_, body)| body.to_owned()).unwrap_or_default();
    (status, body)
}

/// A store whose writes are serialised and slow, like [`FileStore`] (one store
/// lock, several fsyncs per write) on a disk busy with other work.
#[derive(Debug)]
struct SlowStore {
    inner: graphrt_cli::store::MemoryStore,
    lock: tokio::sync::Mutex<()>,
    write: Duration,
}

impl SlowStore {
    async fn slowly<T>(&self, write: impl std::future::Future<Output = T>) -> T {
        let _serialised = self.lock.lock().await;
        tokio::time::sleep(self.write).await;
        write.await
    }
}

impl RunStore for SlowStore {
    fn create(
        &self,
        record: graphrt_core::store::RunRecord,
    ) -> futures::future::BoxFuture<'_, graphrt_core::store::StoreResult<()>> {
        Box::pin(self.slowly(self.inner.create(record)))
    }
    fn put(
        &self,
        record: graphrt_core::store::RunRecord,
    ) -> futures::future::BoxFuture<'_, graphrt_core::store::StoreResult<()>> {
        Box::pin(self.slowly(self.inner.put(record)))
    }
    fn put_if(
        &self,
        record: graphrt_core::store::RunRecord,
        expected_epoch: u64,
    ) -> futures::future::BoxFuture<'_, graphrt_core::store::StoreResult<()>> {
        Box::pin(self.slowly(self.inner.put_if(record, expected_epoch)))
    }
    fn get<'a>(
        &'a self,
        run_id: &'a str,
    ) -> futures::future::BoxFuture<'a, graphrt_core::store::StoreResult<Option<graphrt_core::store::RunRecord>>> {
        self.inner.get(run_id)
    }
    fn list(
        &self,
        filter: graphrt_core::store::RunFilter,
    ) -> futures::future::BoxFuture<'_, graphrt_core::store::StoreResult<graphrt_core::store::RunPage>> {
        self.inner.list(filter)
    }
    fn delete<'a>(&'a self, run_id: &'a str) -> futures::future::BoxFuture<'a, graphrt_core::store::StoreResult<bool>> {
        Box::pin(self.slowly(self.inner.delete(run_id)))
    }
    fn append_events<'a>(
        &'a self,
        run_id: &'a str,
        first_index: u64,
        events: &'a [graphrt_core::store::StoredEvent],
    ) -> futures::future::BoxFuture<'a, graphrt_core::store::StoreResult<u64>> {
        Box::pin(self.slowly(self.inner.append_events(run_id, first_index, events)))
    }
    fn events<'a>(
        &'a self,
        run_id: &'a str,
        from: u64,
    ) -> futures::future::BoxFuture<'a, graphrt_core::store::StoreResult<Vec<graphrt_core::store::StoredEvent>>> {
        self.inner.events(run_id, from)
    }
    fn save_snapshot<'a>(
        &'a self,
        run_id: &'a str,
        snapshot: &'a graphrt_core::Snapshot,
    ) -> futures::future::BoxFuture<'a, graphrt_core::store::StoreResult<()>> {
        Box::pin(self.slowly(self.inner.save_snapshot(run_id, snapshot)))
    }
    fn load_snapshot<'a>(
        &'a self,
        run_id: &'a str,
    ) -> futures::future::BoxFuture<'a, graphrt_core::store::StoreResult<Option<graphrt_core::Snapshot>>> {
        self.inner.load_snapshot(run_id)
    }
    fn claim_resume<'a>(
        &'a self,
        run_id: &'a str,
        now_ms: u64,
    ) -> futures::future::BoxFuture<'a, graphrt_core::store::StoreResult<graphrt_core::store::RunRecord>> {
        Box::pin(self.slowly(self.inner.claim_resume(run_id, now_ms)))
    }
    fn recover_interrupted(
        &self,
        now_ms: u64,
    ) -> futures::future::BoxFuture<'_, graphrt_core::store::StoreResult<usize>> {
        self.inner.recover_interrupted(now_ms)
    }
    fn purge_before(&self, cutoff_ms: u64) -> futures::future::BoxFuture<'_, graphrt_core::store::StoreResult<usize>> {
        self.inner.purge_before(cutoff_ms)
    }
}

/// Ten runs stopped at once each store their final state through one store
/// lock: on a slow disk that takes longer than a fixed reserve, so the drain
/// must preempt early enough for the store it actually has (no run left
/// `running` at the deadline, which a restart would mark failed).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn drain_leaves_time_for_a_slow_serialised_store() {
    let (upstream, port) = start_upstream().await;
    let wf = workflows_dir("slow-store", port);
    let store = SlowStore {
        inner: graphrt_cli::store::MemoryStore::new(),
        lock: tokio::sync::Mutex::new(()),
        write: Duration::from_millis(40),
    };
    let state = state_with(&wf, Arc::new(store)).await;
    let app = router(Arc::clone(&state), 1 << 20);
    for _ in 0..10 {
        let (status, _, body) = send(&app, post("/v1/workflows/work/runs", json!({ "async": true }))).await;
        assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    }
    wait_until("10 upstream calls", || upstream.calls.load(Ordering::SeqCst) == 10).await;

    let timeout = Duration::from_secs(3);
    let report = drain(&state, timeout, "SIGTERM", None).await;
    assert!(report.elapsed <= timeout + Duration::from_millis(250), "{report}");
    assert_eq!((report.runs, report.paused, report.failed, report.unfinished), (10, 10, 0, 0), "{report}");
    assert!(report.preempted, "{report}");
    let _ = std::fs::remove_dir_all(&wf);
}
