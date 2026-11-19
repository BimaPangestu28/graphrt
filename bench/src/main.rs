//! graphrt benchmark harness.
//!
//! Every section prints one JSON object on stdout; `bench/run.sh` combines
//! them into `bench/results/<date>-<host>.json`.
//!
//! ```text
//! graphrt-bench engine          # per-run / per-node latency of graph shapes
//! graphrt-bench concurrency     # runs/sec with a mock model (0 ms and 50 ms)
//! graphrt-bench streaming       # time to first answer chunk vs raw model stream
//! graphrt-bench startup         # parse + build time for small / large graphs
//! graphrt-bench pool            # bytes allocated by a variable-pool clone
//! graphrt-bench mem-paused N    # peak RSS holding N paused runs (+ snapshot size)
//! graphrt-bench mem-inflight N  # peak RSS with N runs in flight (50 ms model)
//! ```
//!
//! All model calls use an in-process mock: numbers measure the runtime, not any
//! model provider.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use futures::StreamExt;
use futures::future::BoxFuture;
use graphrt_core::Workflow;
use graphrt_core::engine::{Engine, Limits, RunInput};
use graphrt_core::event::Event;
use graphrt_core::graph::Graph;
use graphrt_core::node::{Node, NodeContext, NodeOutput, NodeResult, Registry, Services};
use graphrt_core::pool::VariablePool;
use graphrt_core::response::ResponseAssembler;
use graphrt_core::value::Value;
use graphrt_core::{IndexMap, Snapshot};
use graphrt_llm::types::{ChatDelta, ChatModel, ChatRequest, ChatStream, FinishReason, Message};
use graphrt_llm::{LlmError, ModelRegistry};
use serde_json::{Value as Json, json};

// ---------------------------------------------------------------------------
// Allocation counter (used by the `pool` section only). It is off unless a
// section turns it on: counting every allocation on one shared atomic makes all
// cores contend on a single cache line and would distort the concurrency numbers.
// ---------------------------------------------------------------------------

struct Counting;

static ALLOCATED: AtomicUsize = AtomicUsize::new(0);
static COUNTING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

// SAFETY: forwards every call to the system allocator unchanged; only counts bytes.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if COUNTING.load(Ordering::Relaxed) {
            ALLOCATED.fetch_add(layout.size(), Ordering::Relaxed);
        }
        // SAFETY: same contract as the caller's.
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: same contract as the caller's.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

// ---------------------------------------------------------------------------
// Mock model and no-op node.
// ---------------------------------------------------------------------------

/// Streams `chunks` short text deltas after waiting `delay` (simulated model latency).
#[derive(Debug)]
struct BenchModel {
    delay: Duration,
    chunks: usize,
}

impl ChatModel for BenchModel {
    fn chat(&self, _request: ChatRequest) -> BoxFuture<'_, Result<ChatStream, LlmError>> {
        let delay = self.delay;
        let chunks = self.chunks;
        Box::pin(async move {
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            let mut deltas: Vec<Result<ChatDelta, LlmError>> =
                (0..chunks).map(|i| Ok(ChatDelta::Text(format!("tok{i} ")))).collect();
            deltas.push(Ok(ChatDelta::Finish(FinishReason::Stop)));
            let stream: ChatStream = Box::pin(futures::stream::iter(deltas));
            Ok(stream)
        })
    }
}

/// A node that does nothing: isolates scheduling overhead from node work.
#[derive(Debug)]
struct Noop;

impl Node for Noop {
    fn run(&self, _ctx: NodeContext) -> BoxFuture<'_, NodeResult> {
        Box::pin(async { Ok(NodeOutput::new().with("output", 1).into()) })
    }
}

fn registry() -> Registry {
    let mut r = Registry::with_builtins();
    graphrt_llm::register(&mut r);
    r.register("noop", |_| Ok(Arc::new(Noop) as Arc<dyn Node>));
    r
}

fn services(delay_ms: u64, chunks: usize) -> Services {
    let model = Arc::new(BenchModel { delay: Duration::from_millis(delay_ms), chunks });
    Services::new().with(ModelRegistry::new().with("bench", model))
}

fn engine(doc: Json, services: Services) -> Engine {
    let graph = Graph::build(Workflow::from_value(doc).expect("document"), &registry()).expect("graph");
    Engine::new(Arc::new(graph))
        .with_services(services)
        .with_limits(Limits { max_steps: 10_000_000, ..Limits::default() })
}

// ---------------------------------------------------------------------------
// Graph shapes.
// ---------------------------------------------------------------------------

fn start(vars: Json) -> Json {
    json!({"id": "start", "type": "start", "variables": vars})
}

fn edge(a: &str, b: &str) -> Json {
    json!({"source": a, "target": b})
}

/// `start -> n0 -> ... -> n(len-1) -> end`; `kind` is `noop` or `template`.
fn chain(len: usize, kind: &str) -> Json {
    let mut nodes = vec![start(json!([{"variable": "x", "type": "string"}]))];
    let mut edges = Vec::new();
    let mut prev = "start".to_owned();
    let mut prev_field = "x";
    for i in 0..len {
        let id = format!("n{i}");
        let node = if kind == "template" {
            json!({"id": id, "type": "template-transform", "template": "{{ v }}!",
                   "variables": [{"variable": "v", "value_selector": [prev, prev_field]}]})
        } else {
            json!({"id": id, "type": "noop"})
        };
        nodes.push(node);
        edges.push(edge(&prev, &id));
        prev = id;
        prev_field = "output";
    }
    nodes.push(json!({"id": "end", "type": "end", "outputs": [{"variable": "out", "value_selector": [prev, prev_field]}]}));
    edges.push(edge(&prev, "end"));
    json!({"graph": {"nodes": nodes, "edges": edges}})
}

/// `start -> {b0..b(width-1)} -> join -> end` (fan-out / fan-in).
fn fan(width: usize) -> Json {
    let mut nodes = vec![start(json!([])), json!({"id": "join", "type": "noop"})];
    let mut edges = Vec::new();
    for i in 0..width {
        let id = format!("b{i}");
        nodes.push(json!({"id": id, "type": "noop"}));
        edges.push(edge("start", &id));
        edges.push(edge(&id, "join"));
    }
    nodes.push(json!({"id": "end", "type": "end", "outputs": []}));
    edges.push(edge("join", "end"));
    json!({"graph": {"nodes": nodes, "edges": edges}})
}

/// `depth` if-else nodes in a row; each skips a 3-node side path (skip propagation).
fn branches(depth: usize) -> Json {
    let mut nodes = vec![start(json!([{"variable": "flag", "type": "number"}]))];
    let mut edges = Vec::new();
    let mut prev = "start".to_owned();
    for i in 0..depth {
        let cond = format!("c{i}");
        nodes.push(json!({"id": cond, "type": "if-else", "cases": [{"case_id": "yes", "logical_operator": "and",
            "conditions": [{"variable_selector": ["start", "flag"], "comparison_operator": ">", "value": "0"}]}]}));
        edges.push(edge(&prev, &cond));
        let taken = format!("t{i}");
        nodes.push(json!({"id": taken, "type": "noop"}));
        edges.push(json!({"source": cond, "target": taken, "sourceHandle": "yes"}));
        let mut side_prev = cond.clone();
        for s in 0..3 {
            let side = format!("s{i}_{s}");
            nodes.push(json!({"id": side, "type": "noop"}));
            if s == 0 {
                edges.push(json!({"source": side_prev, "target": side, "sourceHandle": "false"}));
            } else {
                edges.push(edge(&side_prev, &side));
            }
            side_prev = side;
        }
        let join = format!("j{i}");
        nodes.push(json!({"id": join, "type": "noop"}));
        edges.push(edge(&taken, &join));
        edges.push(edge(&side_prev, &join));
        prev = join;
    }
    nodes.push(json!({"id": "end", "type": "end", "outputs": []}));
    edges.push(edge(&prev, "end"));
    json!({"graph": {"nodes": nodes, "edges": edges}})
}

/// Parallel iteration over the `items` input with a one-node body.
fn iteration(parallel: usize) -> Json {
    json!({"graph": {
        "nodes": [
            start(json!([{"variable": "items", "type": "json"}])),
            {"id": "it", "type": "iteration", "iterator_selector": ["start", "items"],
             "output_selector": ["body", "output"], "is_parallel": true, "parallel_nums": parallel},
            {"id": "s", "type": "iteration-start", "parentId": "it"},
            {"id": "body", "type": "noop", "parentId": "it"},
            {"id": "end", "type": "end", "outputs": [{"variable": "out", "value_selector": ["it", "output"]}]}
        ],
        "edges": [edge("start", "it"), edge("s", "body"), edge("it", "end")]
    }})
}

/// A loop of `rounds` rounds, each running a parallel iteration over `items`.
fn nested(rounds: usize) -> Json {
    json!({"graph": {
        "nodes": [
            start(json!([{"variable": "items", "type": "json"}])),
            {"id": "lp", "type": "loop", "loop_count": rounds, "loop_variables": [],
             "logical_operator": "and", "break_conditions": []},
            {"id": "ls", "type": "loop-start", "parentId": "lp"},
            {"id": "it", "type": "iteration", "parentId": "lp", "iterator_selector": ["start", "items"],
             "output_selector": ["body", "output"], "is_parallel": true, "parallel_nums": 10},
            {"id": "is", "type": "iteration-start", "parentId": "it"},
            {"id": "body", "type": "noop", "parentId": "it"},
            {"id": "end", "type": "end", "outputs": []}
        ],
        "edges": [edge("start", "lp"), edge("ls", "it"), edge("is", "body"), edge("lp", "end")]
    }})
}

/// `start -> llm -> answer` with the bench model.
fn llm_graph() -> Json {
    json!({"graph": {
        "nodes": [
            start(json!([])),
            {"id": "llm", "type": "llm", "model": {"provider": "bench", "name": "m"},
             "prompt_template": [{"role": "user", "text": "{{#sys.query#}}"}]},
            {"id": "answer", "type": "answer", "answer": "{{#llm.text#}}"}
        ],
        "edges": [edge("start", "llm"), edge("llm", "answer")]
    }})
}

/// `start -> human-input` (pauses every run).
fn approval_graph() -> Json {
    json!({"graph": {
        "nodes": [
            start(json!([{"variable": "topic", "type": "text-input"}])),
            {"id": "ask", "type": "human-input", "prompt": "Approve {{#start.topic#}}?",
             "inputs": [{"name": "comment", "type": "text"}], "actions": [{"id": "ok"}, {"id": "no"}]},
            {"id": "yes", "type": "answer", "answer": "approved: {{#ask.comment#}}"},
            {"id": "nope", "type": "answer", "answer": "rejected"}
        ],
        "edges": [edge("start", "ask"),
                  {"source": "ask", "target": "yes", "sourceHandle": "ok"},
                  {"source": "ask", "target": "nope", "sourceHandle": "no"}]
    }})
}

// ---------------------------------------------------------------------------
// Statistics.
// ---------------------------------------------------------------------------

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn us(d: Duration) -> f64 {
    d.as_secs_f64() * 1_000_000.0
}

fn percentile(sorted: &[Duration], p: f64) -> Duration {
    if sorted.is_empty() {
        return Duration::ZERO;
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss, clippy::cast_precision_loss)]
    let idx = ((sorted.len() - 1) as f64 * p).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

/// Run `iters` sequential runs (after warm-up) and report latency percentiles.
fn latency(rt: &tokio::runtime::Runtime, e: &Engine, iters: usize, nodes: usize, input: &dyn Fn() -> RunInput) -> Json {
    for _ in 0..3.min(iters) {
        let end = rt.block_on(e.run_to_end(input()));
        assert!(matches!(end, Event::RunSucceeded { .. }), "warm-up failed: {end:?}");
    }
    let mut samples = Vec::with_capacity(iters);
    for _ in 0..iters {
        let t = Instant::now();
        let end = rt.block_on(e.run_to_end(input()));
        samples.push(t.elapsed());
        assert!(matches!(end, Event::RunSucceeded { .. }), "run failed: {end:?}");
    }
    samples.sort();
    let p50 = percentile(&samples, 0.5);
    let p95 = percentile(&samples, 0.95);
    #[allow(clippy::cast_precision_loss)]
    let per_node = us(p50) / nodes.max(1) as f64;
    json!({"iterations": iters, "nodes": nodes, "p50_ms": ms(p50), "p95_ms": ms(p95), "p50_us_per_node": per_node})
}

fn items(n: usize) -> Value {
    Value::from_json(json!((0..n).collect::<Vec<_>>()))
}

// ---------------------------------------------------------------------------
// Sections.
// ---------------------------------------------------------------------------

fn section_engine() -> Json {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
    let mut out = serde_json::Map::new();
    let x = || RunInput::new().input("x", "hello");
    for (len, iters) in [(10, 2000), (100, 500), (1000, 50)] {
        for kind in ["noop", "template"] {
            let e = engine(chain(len, kind), Services::new());
            out.insert(format!("chain_{kind}_{len}"), latency(&rt, &e, iters, len + 2, &x));
        }
    }
    for (width, iters) in [(100, 300), (1000, 30)] {
        let e = engine(fan(width), Services::new());
        out.insert(format!("fan_out_in_{width}"), latency(&rt, &e, iters, width + 3, &RunInput::new));
    }
    let e = engine(branches(100), Services::new());
    out.insert(
        "branches_100".into(),
        latency(&rt, &e, 300, 100 * 6 + 2, &|| RunInput::new().input("flag", 1)),
    );
    let e = engine(iteration(100), Services::new());
    let thousand = items(1000);
    out.insert(
        "parallel_iteration_1000".into(),
        latency(&rt, &e, 30, 1000 * 2 + 3, &|| RunInput::new().input("items", thousand.clone())),
    );
    let e = engine(nested(10), Services::new());
    let ten = items(10);
    out.insert(
        "loop10_x_iteration10".into(),
        latency(&rt, &e, 200, 10 * (2 + 10 * 2) + 3, &|| RunInput::new().input("items", ten.clone())),
    );
    Json::Object(out)
}

fn section_concurrency() -> Json {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("runtime");
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    let mut rows = Vec::new();
    for delay_ms in [0u64, 50] {
        let e = Arc::new(engine(llm_graph(), services(delay_ms, 10)));
        let levels: &[usize] = if delay_ms == 0 { &[1, 10, 100, 1000] } else { &[1, 10, 100, 1000, 10_000] };
        for &concurrency in levels {
            // Warm-up.
            rt.block_on(e.run_to_end(RunInput::new().sys("query", "hi")));
            let rounds = if delay_ms == 0 { 20 } else { 3 };
            let t = Instant::now();
            for _ in 0..rounds {
                rt.block_on(async {
                    let handles: Vec<_> = (0..concurrency)
                        .map(|_| {
                            let e = Arc::clone(&e);
                            tokio::spawn(async move { e.run_to_end(RunInput::new().sys("query", "hi")).await })
                        })
                        .collect();
                    for h in handles {
                        let end = h.await.expect("join");
                        assert!(matches!(end, Event::RunSucceeded { .. }), "{end:?}");
                    }
                });
            }
            let elapsed = t.elapsed();
            #[allow(clippy::cast_precision_loss)]
            let runs_per_sec = (concurrency * rounds) as f64 / elapsed.as_secs_f64();
            #[allow(clippy::cast_precision_loss)]
            let ideal = if delay_ms == 0 { Json::Null } else { json!(concurrency as f64 / (delay_ms as f64 / 1000.0)) };
            rows.push(json!({"model_delay_ms": delay_ms, "concurrent_runs": concurrency,
                             "runs_per_sec": runs_per_sec, "ideal_runs_per_sec": ideal,
                             "batch_ms": ms(elapsed) / rounds as f64}));
        }
    }
    json!({"worker_threads": threads, "graph": "start -> llm (10 streamed chunks) -> answer", "rows": rows})
}

fn section_streaming() -> Json {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
    let e = engine(llm_graph(), services(0, 50));
    let graph = Arc::clone(e.graph());
    let model = BenchModel { delay: Duration::ZERO, chunks: 50 };
    let iters = 2000;
    let mut direct = Vec::with_capacity(iters);
    let mut through = Vec::with_capacity(iters);
    for _ in 0..iters {
        let t = Instant::now();
        let first = rt.block_on(async {
            let mut s = model.chat(ChatRequest::new("m", vec![Message::user("hi")])).await.expect("chat");
            s.next().await
        });
        direct.push(t.elapsed());
        assert!(first.is_some());

        let t = Instant::now();
        let got = rt.block_on(async {
            let mut assembler = ResponseAssembler::new(&graph);
            let mut stream = e.run(RunInput::new().sys("query", "hi"));
            let mut first = None;
            while let Some(ev) = stream.next().await {
                if first.is_none() && assembler.push(&ev).iter().any(|c| !c.text.is_empty()) {
                    first = Some(t.elapsed());
                }
            }
            first
        });
        through.push(got.expect("an answer chunk"));
    }
    direct.sort();
    through.sort();
    json!({
        "iterations": iters,
        "direct_model_first_delta_p50_us": us(percentile(&direct, 0.5)),
        "engine_assembler_first_chunk_p50_us": us(percentile(&through, 0.5)),
        "engine_assembler_first_chunk_p95_us": us(percentile(&through, 0.95)),
        "overhead_p50_us": us(percentile(&through, 0.5)) - us(percentile(&direct, 0.5)),
    })
}

fn section_startup() -> Json {
    let hello = r#"
version: "1"
graph:
  nodes:
    - { id: start, type: start, variables: [{ variable: name, type: string }] }
    - { id: greet, type: template-transform, template: "Hello {{ name }}", variables: [{ variable: name, value_selector: [start, name] }] }
    - { id: end, type: end, outputs: [{ variable: greeting, value_selector: [greet, output] }] }
  edges:
    - { source: start, target: greet }
    - { source: greet, target: end }
"#;
    let large = serde_json::to_string(&chain(1000, "template")).expect("json");
    let reg = registry();
    let measure = |text: &str, iters: usize| -> Json {
        let mut samples = Vec::with_capacity(iters);
        for _ in 0..iters {
            let t = Instant::now();
            let g = Graph::from_text(text, &reg).expect("graph");
            samples.push(t.elapsed());
            std::hint::black_box(g);
        }
        samples.sort();
        json!({"p50_ms": ms(percentile(&samples, 0.5)), "p95_ms": ms(percentile(&samples, 0.95)), "bytes": text.len()})
    };
    let reg_t = Instant::now();
    let r = registry();
    let registry_us = us(reg_t.elapsed());
    std::hint::black_box(r);
    json!({
        "registry_build_us": registry_us,
        "parse_build_small_yaml_3_nodes": measure(hello, 2000),
        "parse_build_large_json_1002_nodes": measure(&large, 50),
    })
}

fn section_pool() -> Json {
    COUNTING.store(true, Ordering::Relaxed);
    let mut pool = VariablePool::new();
    for n in 0..100 {
        for v in 0..10 {
            pool.insert(&format!("node{n}"), &format!("var{v}"), Value::from(format!("value {n}/{v} with some text")));
        }
    }
    let before = ALLOCATED.load(Ordering::Relaxed);
    let clones: Vec<VariablePool> = (0..1000).map(|_| pool.clone()).collect();
    let after = ALLOCATED.load(Ordering::Relaxed);
    std::hint::black_box(&clones);
    let mut one = pool.clone();
    let before_write = ALLOCATED.load(Ordering::Relaxed);
    one.insert("node0", "var0", Value::from("changed"));
    let after_write = ALLOCATED.load(Ordering::Relaxed);
    let t = Instant::now();
    for _ in 0..100_000 {
        std::hint::black_box(pool.clone());
    }
    #[allow(clippy::cast_precision_loss)]
    let clone_ns = t.elapsed().as_nanos() as f64 / 100_000.0;
    json!({
        "pool": "100 namespaces x 10 string variables",
        "bytes_allocated_per_clone": (after - before) / 1000,
        "bytes_allocated_by_first_write_after_clone": after_write - before_write,
        "clone_ns": clone_ns,
    })
}

fn rss_kib(field: &str) -> u64 {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    status
        .lines()
        .find(|l| l.starts_with(field))
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

fn section_mem_paused(n: usize) -> Json {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("runtime");
    let e = Arc::new(engine(approval_graph(), Services::new()));
    let base = rss_kib("VmRSS:");
    let snapshots: Vec<Snapshot> = rt.block_on(async {
        let handles: Vec<_> = (0..n)
            .map(|i| {
                let e = Arc::clone(&e);
                tokio::spawn(async move {
                    match e.run_to_end(RunInput::new().input("topic", format!("item {i}"))).await {
                        Event::RunPaused { snapshot, .. } => *snapshot,
                        other => panic!("expected pause, got {other:?}"),
                    }
                })
            })
            .collect();
        let mut out = Vec::with_capacity(n);
        for h in handles {
            out.push(h.await.expect("join"));
        }
        out
    });
    let json_bytes: usize = snapshots.iter().map(|s| s.to_json().map(|t| t.len()).unwrap_or(0)).sum();
    // Resume one to prove the snapshots are usable.
    let mut inputs = IndexMap::new();
    inputs.insert("ask".to_owned(), Value::from_json(json!({"action": "ok", "values": {"comment": "fine"}})));
    let resumed = rt.block_on(e.run_to_end(RunInput::new().resume(snapshots[0].clone(), inputs)));
    assert!(matches!(resumed, Event::RunSucceeded { .. }), "{resumed:?}");
    let peak = rss_kib("VmHWM:");
    std::hint::black_box(&snapshots);
    json!({"paused_runs": n, "baseline_rss_kib": base, "peak_rss_kib": peak,
           "snapshot_json_bytes_avg": json_bytes / n.max(1)})
}

fn section_mem_inflight(n: usize) -> Json {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("runtime");
    let e = Arc::new(engine(llm_graph(), services(200, 10)));
    let base = rss_kib("VmRSS:");
    let t = Instant::now();
    rt.block_on(async {
        let handles: Vec<_> = (0..n)
            .map(|_| {
                let e = Arc::clone(&e);
                tokio::spawn(async move { e.run_to_end(RunInput::new().sys("query", "hi")).await })
            })
            .collect();
        for h in handles {
            let end = h.await.expect("join");
            assert!(matches!(end, Event::RunSucceeded { .. }), "{end:?}");
        }
    });
    let peak = rss_kib("VmHWM:");
    json!({"inflight_runs": n, "model_delay_ms": 200, "baseline_rss_kib": base, "peak_rss_kib": peak,
           "wall_ms": ms(t.elapsed())})
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let section = args.get(1).map(String::as_str).unwrap_or("");
    let n: usize = args.get(2).and_then(|v| v.parse().ok()).unwrap_or(1);
    let result = match section {
        "engine" => section_engine(),
        "concurrency" => section_concurrency(),
        "streaming" => section_streaming(),
        "startup" => section_startup(),
        "pool" => section_pool(),
        "mem-paused" => section_mem_paused(n),
        "mem-inflight" => section_mem_inflight(n),
        other => {
            eprintln!("unknown section `{other}`; see the module docs");
            std::process::exit(2);
        }
    };
    println!("{}", serde_json::to_string(&result).expect("json"));
}
