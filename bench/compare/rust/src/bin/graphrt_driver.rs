//! In-process graphrt driver, the counterpart of `python/driver.py`.
//!
//! ```text
//! graphrt-driver seq  <workflow.yaml> <iterations>            # warm sequential latency
//! graphrt-driver conc <workflow.yaml> <concurrency> <total>   # throughput, C runs in flight
//! ```
//!
//! The workflow is parsed and compiled once (like a compiled LangGraph graph is
//! reused); each run gets the start input `x = "hello"`. Model providers come
//! from the environment exactly as in `graphrt run` (`OPENAI_BASE_URL`,
//! `OPENAI_API_KEY`). `seq` uses a current-thread runtime; `conc` uses a
//! multi-thread runtime with `GRAPHRT_DRIVER_WORKERS` threads (default: one per
//! CPU). Prints one JSON object on stdout.
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use graphrt_core::Workflow;
use graphrt_core::engine::{Engine, Limits, RunInput};
use graphrt_core::event::Event;
use graphrt_core::graph::Graph;
use graphrt_core::node::{Registry, Services};
use graphrt_llm::ModelRegistry;
use serde_json::{Value as Json, json};

const WARMUP_RUNS: usize = 20;
const THROUGHPUT_ROUNDS: usize = 3;

fn build_engine(path: &str) -> Engine {
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("cannot read {path}: {e}"));
    let workflow = Workflow::parse(&text).unwrap_or_else(|e| panic!("{path}: {e}"));
    let mut registry = Registry::with_builtins();
    graphrt_llm::register(&mut registry);
    let graph = Graph::build(workflow, &registry).unwrap_or_else(|e| panic!("{path}: {e}"));
    let models = ModelRegistry::from_env().unwrap_or_else(|e| panic!("model configuration: {e}"));
    Engine::new(Arc::new(graph))
        .with_services(Services::new().with(models))
        .with_limits(Limits { max_steps: 10_000_000, ..Limits::default() })
}

async fn run_once(engine: &Engine) {
    let end = engine.run_to_end(RunInput::new().input("x", "hello")).await;
    assert!(matches!(end, Event::RunSucceeded { .. }), "run failed: {end:?}");
}

fn summarize_ms(mut samples: Vec<f64>) -> Json {
    samples.sort_by(f64::total_cmp);
    let at = |q: f64| samples[((samples.len() - 1) as f64 * q).round() as usize];
    json!({
        "runs": samples.len(),
        "median_ms": at(0.5), "p25_ms": at(0.25), "p75_ms": at(0.75),
        "min_ms": samples[0], "max_ms": samples[samples.len() - 1],
    })
}

fn sequential(path: &str, iterations: usize) -> Json {
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
    let engine = build_engine(path);
    runtime.block_on(async {
        for _ in 0..WARMUP_RUNS {
            run_once(&engine).await;
        }
        let mut samples = Vec::with_capacity(iterations);
        for _ in 0..iterations {
            let started = Instant::now();
            run_once(&engine).await;
            samples.push(started.elapsed().as_secs_f64() * 1000.0);
        }
        summarize_ms(samples)
    })
}

/// Keeps `concurrency` runs in flight until `total` runs have finished; returns runs/s.
async fn throughput_round(engine: &Arc<Engine>, concurrency: usize, total: usize) -> f64 {
    let next = Arc::new(AtomicUsize::new(0));
    let started = Instant::now();
    let workers: Vec<_> = (0..concurrency)
        .map(|_| {
            let engine = Arc::clone(engine);
            let next = Arc::clone(&next);
            tokio::spawn(async move {
                while next.fetch_add(1, Ordering::Relaxed) < total {
                    run_once(&engine).await;
                }
            })
        })
        .collect();
    for worker in workers {
        worker.await.expect("worker");
    }
    total as f64 / started.elapsed().as_secs_f64()
}

fn concurrent(path: &str, concurrency: usize, total: usize) -> Json {
    let mut builder = tokio::runtime::Builder::new_multi_thread();
    if let Some(workers) = std::env::var("GRAPHRT_DRIVER_WORKERS").ok().and_then(|v| v.parse().ok()) {
        builder.worker_threads(workers);
    }
    let runtime = builder.enable_all().build().expect("runtime");
    let engine = Arc::new(build_engine(path));
    runtime.block_on(async {
        throughput_round(&engine, concurrency, concurrency.max(WARMUP_RUNS)).await;
        let mut rounds = Vec::with_capacity(THROUGHPUT_ROUNDS);
        for _ in 0..THROUGHPUT_ROUNDS {
            rounds.push(throughput_round(&engine, concurrency, total).await);
        }
        rounds.sort_by(f64::total_cmp);
        json!({
            "concurrency": concurrency, "total_runs": total,
            "runs_per_sec_median": rounds[rounds.len() / 2],
            "runs_per_sec_min": rounds[0], "runs_per_sec_max": rounds[rounds.len() - 1],
        })
    })
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let number = |i: usize| -> usize {
        args.get(i).and_then(|v| v.parse().ok()).unwrap_or_else(|| panic!("argument {i} must be a number"))
    };
    let result = match args.first().map(String::as_str) {
        Some("seq") if args.len() == 3 => sequential(&args[1], number(2)),
        Some("conc") if args.len() == 4 => concurrent(&args[1], number(2), number(3)),
        _ => {
            eprintln!("usage: graphrt-driver seq <workflow> <iterations> | conc <workflow> <concurrency> <total>");
            std::process::exit(2);
        }
    };
    println!("{result}");
}
