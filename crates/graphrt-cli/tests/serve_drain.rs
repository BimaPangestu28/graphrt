//! Shutdown drain of `graphrt serve`: runs pause (never fail) on SIGTERM and
//! complete after a restart.
#![cfg(all(feature = "serve", feature = "http"))]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]

#[cfg(unix)]
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use graphrt_cli::drain::{DRAINED, drain, preempt_at, reserve_per_run};
use graphrt_cli::serve::{AppState, build_state, discover, resume_drained_runs, router};
use graphrt_cli::setup::ServiceOptions;
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
    /// When each hanging request was given up by its caller (a preempted node
    /// drops its connection), for the diagnostics of a failed drain.
    dropped: Mutex<Vec<Instant>>,
}

/// Records in [`Upstream::dropped`] that a request ended without an answer.
struct DroppedCall {
    upstream: Arc<Upstream>,
    answered: bool,
}

impl Drop for DroppedCall {
    fn drop(&mut self) {
        if !self.answered
            && let Ok(mut dropped) = self.upstream.dropped.lock()
        {
            dropped.push(Instant::now());
        }
    }
}

/// What happened when during a drain, printed when its outcome is not the
/// expected one: without it a failure on a CI runner says only how many runs
/// did not finish, not which stage took the time.
struct DrainTimeline {
    began: Instant,
    /// The store's write latency estimate when the drain began.
    write_estimate: Duration,
    /// When the drain is expected to preempt in-flight nodes, from that estimate.
    expected_preempt: Duration,
    runs: Vec<Arc<graphrt_cli::store::live::ActiveRun>>,
    /// When each run was first seen done, by its place in `runs`.
    done_at: Arc<Mutex<Vec<Option<Duration>>>>,
    watcher: tokio::task::JoinHandle<()>,
}

impl DrainTimeline {
    /// Start watching the runs that are active now. Call it just before the drain.
    fn start(state: &Arc<AppState>, timeout: Duration) -> Self {
        let began = Instant::now();
        let runs = state.active_list();
        let write_estimate = state.store_write_estimate();
        let expected_preempt = preempt_at(timeout, runs.len(), reserve_per_run(write_estimate));
        let done_at = Arc::new(Mutex::new(vec![None; runs.len()]));
        let watcher = {
            let (runs, done_at) = (runs.clone(), Arc::clone(&done_at));
            tokio::spawn(async move {
                loop {
                    let mut all_done = true;
                    for (index, run) in runs.iter().enumerate() {
                        let mut seen = done_at.lock().unwrap();
                        if seen[index].is_none() {
                            if run.is_done() {
                                seen[index] = Some(began.elapsed());
                            } else {
                                all_done = false;
                            }
                        }
                    }
                    if all_done {
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
        };
        Self { began, write_estimate, expected_preempt, runs, done_at, watcher }
    }

    /// The timeline as text. Times are from the start of the drain.
    async fn describe(&self, state: &AppState, upstream: &Upstream, drain_returned: Duration) -> String {
        self.watcher.abort();
        let ms = |d: Duration| format!("{:.0} ms", d.as_secs_f64() * 1000.0);
        let mut lines = vec![
            format!("drain returned after {}", ms(drain_returned)),
            format!(
                "store write estimate: {} at the start, {} now",
                ms(self.write_estimate),
                ms(state.store_write_estimate())
            ),
            format!("preemption expected at {} for {} run(s)", ms(self.expected_preempt), self.runs.len()),
        ];
        let dropped: Vec<String> =
            upstream.dropped.lock().unwrap().iter().map(|at| ms(at.saturating_duration_since(self.began))).collect();
        lines.push(format!(
            "upstream calls given up by their node (preempted): {} of {} [{}]",
            dropped.len(),
            upstream.calls.load(Ordering::SeqCst),
            dropped.join(", ")
        ));
        lines.push(format!("runs still active: {}", state.active_runs()));
        let done_at = self.done_at.lock().unwrap().clone();
        for (run, done) in self.runs.iter().zip(done_at) {
            let stored = match state.store().get(&run.run_id).await {
                Ok(Some(record)) => format!("{:?} with {} event(s)", record.status, record.event_count),
                Ok(None) => "no record".to_owned(),
                Err(error) => format!("store error: {error}"),
            };
            lines.push(format!(
                "  {} ({}): done {}, ended as {:?}, stored as {stored}",
                run.run_id,
                if run.is_detached() { "async" } else { "stream" },
                done.map_or_else(|| "never".to_owned(), ms),
                run.terminal_kind(),
            ));
        }
        lines.join("\n")
    }
}

async fn start_upstream() -> (Arc<Upstream>, u16) {
    let upstream =
        Arc::new(Upstream { hang: AtomicBool::new(true), calls: AtomicUsize::new(0), dropped: Mutex::default() });
    let shared = Arc::clone(&upstream);
    let app = axum::Router::new().route(
        "/work",
        axum::routing::get(move || {
            let upstream = Arc::clone(&shared);
            async move {
                upstream.calls.fetch_add(1, Ordering::SeqCst);
                let mut call = DroppedCall { upstream: Arc::clone(&upstream), answered: false };
                if upstream.hang.load(Ordering::SeqCst) {
                    tokio::time::sleep(Duration::from_secs(120)).await;
                }
                call.answered = true;
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
        plugins: graphrt_cli::extensions::PluginOptions {
            dirs: Vec::new(),
            no_default: true,
            trusted: Vec::new(),
            pool: Default::default(),
            allow_private_network: false,
        },
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

/// How long each store write takes once the drain began, in
/// [`sigterm_drain_pauses_async_and_streaming_runs_and_a_restart_completes_them`]:
/// a disk that wrote in 3 ms before the drain and is busy during it, like the
/// CI runners whose ten runs' final writes took over 2.5 s. The ten pending
/// event flushes and ten final writes take 1.6 s; with three writes per final
/// state (events, snapshot and record one by one) the drain needed 3.2 s and
/// left every run unfinished at its 3 s deadline.
const BUSY_DISK_WRITE: Duration = Duration::from_millis(80);

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sigterm_drain_pauses_async_and_streaming_runs_and_a_restart_completes_them() {
    let (upstream, port) = start_upstream().await;
    let wf = workflows_dir("mix", port);
    // Writes are fast until the drain begins and slow during it: the latency
    // estimate the drain starts from is too low for the writes it waits for.
    let memory = Arc::new(graphrt_cli::store::MemoryStore::new());
    let store = Arc::new(SlowStore::new(Arc::clone(&memory), Duration::ZERO));
    let state = state_with(&wf, Arc::clone(&store) as Arc<dyn RunStore>).await;
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
    let timeline = DrainTimeline::start(&state, timeout);
    store.set_write(BUSY_DISK_WRITE);
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
    // Printed with a failed assertion below: which stage of the drain took the time.
    let timeline = timeline.describe(&state, &upstream, elapsed).await;

    assert!(elapsed <= timeout + Duration::from_millis(250), "drain took {elapsed:?}\n{timeline}");
    assert_eq!((report.runs, report.paused, report.failed, report.unfinished), (10, 10, 0, 0), "{report}\n{timeline}");
    assert!(report.preempted, "the hanging nodes had to be preempted\n{timeline}");
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
    let state = state_with(&wf, memory).await;
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
    let exit = wait_exit(&mut server, DRAIN_TIMEOUT + Duration::from_secs(5)).await;
    assert!(exit.success(), "exit status {exit:?}");
    // Within --drain-timeout, plus process start-up noise.
    assert!(began.elapsed() < DRAIN_TIMEOUT + Duration::from_millis(800), "took {:?}", began.elapsed());
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
    let exit = wait_exit(&mut server, DRAIN_TIMEOUT + Duration::from_secs(5)).await;
    assert!(exit.success(), "SIGINT exit {exit:?}");
    let _ = std::fs::remove_dir_all(&wf);
    let _ = std::fs::remove_dir_all(&store_dir);
}

/// `--drain-timeout` of the binary. The run waits on a hanging call, so it is
/// preempted a quarter of the timeout before the end (at least 0.5 s): with
/// 1 s, cancelling and storing it on a CI runner busy with other test binaries
/// sometimes took longer than that, and the run was left `running`.
#[cfg(unix)]
const DRAIN_TIMEOUT: Duration = Duration::from_secs(4);

#[cfg(unix)]
fn free_addr() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().to_string()
}

#[cfg(unix)]
fn spawn_server(workflows: &Path, store: &Path, addr: &str) -> std::process::Child {
    std::process::Command::new(env!("CARGO_BIN_EXE_graphrt"))
        .args(["serve", workflows.to_str().unwrap(), "--addr", addr, "--store", store.to_str().unwrap()])
        .args(["--drain-timeout", &format!("{}ms", DRAIN_TIMEOUT.as_millis())])
        .args(["--allow-private-network", "--no-default-plugins"])
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

/// A store whose writes are serialised and slow, like `FileStore` (one store
/// lock, several fsyncs per write) on a disk busy with other work. Reads are
/// not slowed, so a test sees at once what was stored.
#[derive(Debug)]
struct SlowStore {
    inner: Arc<graphrt_cli::store::MemoryStore>,
    lock: tokio::sync::Mutex<()>,
    /// Duration of each write, in microseconds (changed while the test runs).
    write_micros: AtomicU64,
}

impl SlowStore {
    fn new(inner: Arc<graphrt_cli::store::MemoryStore>, write: Duration) -> Self {
        let micros = u64::try_from(write.as_micros()).unwrap();
        Self { inner, lock: tokio::sync::Mutex::new(()), write_micros: AtomicU64::new(micros) }
    }

    /// From now on each write takes `write`.
    fn set_write(&self, write: Duration) {
        self.write_micros.store(u64::try_from(write.as_micros()).unwrap(), Ordering::SeqCst);
    }

    /// Run `write` once the store lock is free and the write's duration passed.
    /// The memory store writes when called, so the call is made at the end.
    async fn slowly<T, F: std::future::Future<Output = T>>(&self, write: impl FnOnce() -> F) -> T {
        let _serialised = self.lock.lock().await;
        let micros = self.write_micros.load(Ordering::SeqCst);
        if micros > 0 {
            tokio::time::sleep(Duration::from_micros(micros)).await;
        }
        write().await
    }
}

impl RunStore for SlowStore {
    fn create(
        &self,
        record: graphrt_core::store::RunRecord,
    ) -> futures::future::BoxFuture<'_, graphrt_core::store::StoreResult<()>> {
        Box::pin(self.slowly(move || self.inner.create(record)))
    }
    fn put(
        &self,
        record: graphrt_core::store::RunRecord,
    ) -> futures::future::BoxFuture<'_, graphrt_core::store::StoreResult<()>> {
        Box::pin(self.slowly(move || self.inner.put(record)))
    }
    fn put_if(
        &self,
        record: graphrt_core::store::RunRecord,
        expected_epoch: u64,
    ) -> futures::future::BoxFuture<'_, graphrt_core::store::StoreResult<()>> {
        Box::pin(self.slowly(move || self.inner.put_if(record, expected_epoch)))
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
        Box::pin(self.slowly(move || self.inner.delete(run_id)))
    }
    fn append_events<'a>(
        &'a self,
        run_id: &'a str,
        first_index: u64,
        events: &'a [graphrt_core::store::StoredEvent],
    ) -> futures::future::BoxFuture<'a, graphrt_core::store::StoreResult<u64>> {
        Box::pin(self.slowly(move || self.inner.append_events(run_id, first_index, events)))
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
        Box::pin(self.slowly(move || self.inner.save_snapshot(run_id, snapshot)))
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
        Box::pin(self.slowly(move || self.inner.claim_resume(run_id, now_ms)))
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
    /// One write, like `FileStore`, which takes its lock once for a final state.
    fn finish<'a>(
        &'a self,
        write: graphrt_core::store::FinalWrite<'a>,
    ) -> futures::future::BoxFuture<'a, graphrt_core::store::StoreResult<()>> {
        Box::pin(self.slowly(move || self.inner.finish(write)))
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
    let store = SlowStore::new(Arc::new(graphrt_cli::store::MemoryStore::new()), Duration::from_millis(40));
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

/// How long each store write takes in the tests below: far longer than a node,
/// so a run whose engine waits for its event writes is plainly slower.
const SLOW_DISK_WRITE: Duration = Duration::from_millis(400);

/// A run's event writes go to the store while its engine goes on: on a slow
/// disk, a node that hangs is preempted at once, not after the event write
/// that started 250 ms after the run's last event.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_slow_event_write_does_not_hold_up_preemption() {
    let (upstream, port) = start_upstream().await;
    let wf = workflows_dir("slow-preempt", port);
    let store = Arc::new(SlowStore::new(Arc::new(graphrt_cli::store::MemoryStore::new()), Duration::ZERO));
    let state = state_with(&wf, Arc::clone(&store) as Arc<dyn RunStore>).await;
    let app = router(Arc::clone(&state), 1 << 20);
    let (status, _, body) = send(&app, post("/v1/workflows/work/runs", json!({ "async": true }))).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    wait_until("the upstream call", || upstream.calls.load(Ordering::SeqCst) == 1).await;
    let run = state.active_list().remove(0);

    // The run hangs in its node; the batch of its first events is written 250 ms
    // after the first of them, on a disk that now takes 2 s per write.
    store.set_write(SLOW_DISK_WRITE * 5);
    tokio::time::sleep(Duration::from_millis(400)).await;
    let preempted = Instant::now();
    run.handle().preempt("test");
    while run.terminal_kind().is_none() && preempted.elapsed() < Duration::from_secs(10) {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let took = preempted.elapsed();
    assert_eq!(run.terminal_kind(), Some("paused"));
    assert!(took < Duration::from_secs(1), "the run paused {took:?} after its preemption");
    store.set_write(Duration::ZERO);
    let _ = std::fs::remove_dir_all(&wf);
}

/// A workflow of `length` quick nodes in a line (three events each).
fn chain_dir(tag: &str, length: usize) -> PathBuf {
    let dir = temp_dir(&format!("wf-{tag}"));
    let mut nodes = String::from("    - { id: start, type: start, variables: [] }\n");
    let mut edges = String::new();
    let mut previous = "start".to_owned();
    for index in 0..length {
        nodes.push_str(&format!(
            "    - {{ id: p{index}, type: template-transform, template: \"{index}\", variables: [] }}\n"
        ));
        edges.push_str(&format!("    - {{ source: {previous}, target: p{index} }}\n"));
        previous = format!("p{index}");
    }
    nodes.push_str(&format!(
        "    - {{ id: end, type: end, outputs: [{{ variable: out, value_selector: [{previous}, output] }}] }}\n"
    ));
    edges.push_str(&format!("    - {{ source: {previous}, target: end }}\n"));
    std::fs::write(
        dir.join("work.yaml"),
        format!("version: \"1\"\nname: work\ngraph:\n  nodes:\n{nodes}  edges:\n{edges}"),
    )
    .unwrap();
    dir
}

/// A streaming run of many quick nodes gets its events as fast as its nodes
/// produce them: the batches written meanwhile (one per 32 events, 400 ms
/// each) do not slow the run down. Its terminal event comes before its final
/// write; only the record's first write (`create`) is waited for.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_slow_store_does_not_slow_a_streaming_run_down() {
    let wf = chain_dir("slow-chain", 100);
    let store = Arc::new(SlowStore::new(Arc::new(graphrt_cli::store::MemoryStore::new()), SLOW_DISK_WRITE));
    let state = state_with(&wf, Arc::clone(&store) as Arc<dyn RunStore>).await;
    let app = router(Arc::clone(&state), 1 << 20);
    let began = Instant::now();
    let (status, _, body) = send(&app, post("/v1/workflows/work/runs", json!({ "stream": true }))).await;
    let took = began.elapsed();
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("run_succeeded"), "{body}");
    // The record's create plus at most one more write; waiting for each of the
    // ten event batches took over 4 s.
    assert!(took < SLOW_DISK_WRITE * 3, "the run took {took:?}");

    // Every event is stored once the run is done.
    let run_id = run_id_of_sse(&body);
    wait_until("the run is stored", || state.active_runs() == 0).await;
    let record = store.inner.get(&run_id).await.unwrap().unwrap();
    let streamed = u64::try_from(body.matches("\nevent: ").count() + usize::from(body.starts_with("event: "))).unwrap();
    assert_eq!(record.event_count, streamed, "{body}");
    assert_eq!(store.inner.events(&run_id, 0).await.unwrap().len() as u64, streamed);
    let _ = std::fs::remove_dir_all(&wf);
}
