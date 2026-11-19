# Benchmarks

These numbers describe the **runtime itself**: scheduling, variable handling,
streaming, memory and startup. Every model call goes to an in-process mock, so
nothing here measures a model provider, the network or a real tool. The
[comparison with Python agent stacks](#graphrt-vs-python-agent-stacks) at the
end is the exception: both sides call a local fake HTTP server there.

Reproduce on your own machine:

```sh
bench/run.sh                # ~2–3 minutes on a warm build cache
bench/run.sh --docker       # also builds the container image and records its size
bench/run.sh --skip-sizes   # skip the min-size CLI builds
```

Results land in `bench/results/<date>-<host>.json` (raw) and `.md` (tables).
The harness is a standalone crate in `bench/` (not a workspace member), built
with `--release`, thin LTO and one codegen unit.

## Machine

13th Gen Intel Core i7-13700 (24 logical CPUs), 61.6 GiB RAM, Linux 7.0,
rustc 1.95.0, commit `72f1db6`, measured 2026-11-17. Raw data:
[`bench/results/2026-11-17-bimapangestu.json`](../bench/results/2026-11-17-bimapangestu.json).

## Headline numbers

| What | Result |
| --- | --- |
| Scheduling overhead per node (flat from 10 to 1,000+ nodes per scope) | ~1.5–2.5 µs |
| 100-node chain, end to end | 0.15 ms |
| 1,000-item parallel iteration | 3.6 ms |
| Time to first streamed answer chunk (engine + response assembler) | ~12 µs over the raw model stream |
| Runs/s with an instant mock model (multi-thread runtime) | ~190,000 |
| 1,000 concurrent runs waiting on a 50 ms model | 18,100 runs/s (ideal 20,000) |
| Memory per in-flight run | ~10 KiB |
| Memory per paused run held in memory / snapshot size | ~3.5 KiB / ~0.6 KB JSON |
| Cloning a variable pool (1,000 variables) | 8 bytes, ~10 ns |
| `graphrt run hello.yaml`, process start to exit | 8.3 ms |
| Binary: core only / everything | 2.96 MiB / 12.51 MiB |
| Container image (distroless, default features) | 24.4 MiB |

## Engine overhead

Sequential runs on a current-thread Tokio runtime after warm-up. `noop` nodes do
no work, so their time is pure scheduling; `template` nodes render a minijinja
template that reads the previous node's output.

| Graph | Nodes | p50 ms | p95 ms | µs / node (p50) |
|---|---:|---:|---:|---:|
| chain, noop, 10 | 12 | 0.018 | 0.028 | 1.53 |
| chain, template, 10 | 12 | 0.026 | 0.026 | 2.14 |
| chain, noop, 100 | 102 | 0.150 | 0.157 | 1.47 |
| chain, template, 100 | 102 | 0.228 | 0.237 | 2.23 |
| chain, noop, 1,000 | 1,002 | 1.433 | 1.482 | 1.43 |
| chain, template, 1,000 | 1,002 | 2.327 | 2.367 | 2.32 |
| fan-out / fan-in, 100 branches | 103 | 0.170 | 0.177 | 1.65 |
| fan-out / fan-in, 1,000 branches | 1,003 | 2.056 | 2.102 | 2.05 |
| 100 if-else nodes, each skipping a 3-node path | 602 | 0.695 | 0.715 | 1.15 |
| parallel iteration, 1,000 items (100 at a time) | 2,003 | 3.781 | 3.807 | 1.89 |
| loop of 10 rounds × parallel iteration of 10 | 223 | 0.443 | 0.459 | 1.99 |

"Nodes" counts node executions, including container bodies.

Per-node cost stays flat as a scope grows: the 1,000-node chain and the
1,000-branch fan-out/fan-in cost about the same per node as their 100-node
versions. A regression check guards this: `bench/run.sh` runs
`cargo test --release -p graphrt-core --test engine_scaling -- --ignored`,
which fails if per-node cost (including response assembly) grows more than
2.5× from 100 to 1,000 nodes.

## Concurrency

Graph: `start -> llm -> answer`, where the mock model streams 10 text chunks.
Runs are spawned on a multi-thread Tokio runtime (24 workers). With a 50 ms
model delay, the ideal throughput is `concurrent runs / 0.05 s`.

| Model delay | Concurrent runs | Runs / s | Ideal runs / s |
|---:|---:|---:|---:|
| 0 ms | 1 | 37,351 | – |
| 0 ms | 10 | 117,475 | – |
| 0 ms | 100 | 188,874 | – |
| 0 ms | 1,000 | 191,052 | – |
| 50 ms | 1 | 20 | 20 |
| 50 ms | 10 | 195 | 200 |
| 50 ms | 100 | 1,931 | 2,000 |
| 50 ms | 1,000 | 18,104 | 20,000 |
| 50 ms | 10,000 | 110,435 | 200,000 |

Up to 1,000 concurrent runs the runtime adds close to nothing on top of model
latency. At 10,000 runs per batch the per-run CPU cost (engine work, task
spawning, event delivery) becomes visible and a batch takes ~90 ms instead of
50 ms.

## Streaming

Time from starting a run to the first non-empty answer chunk produced by
`ResponseAssembler` (engine, LLM node and answer node in between), versus the
first delta of the same mock stream read directly:

- engine + assembler: p50 13.6 µs, p95 21.8 µs
- raw mock stream: p50 1.9 µs
- added latency: ~12 µs

## Memory

Each measurement runs in a fresh process; "peak RSS" is the process high-water
mark (`VmHWM`), which includes a ~6 MiB baseline (runtime, allocator, binary).

| Scenario | Runs | Peak RSS MiB | Snapshot JSON bytes (avg) |
|---|---:|---:|---:|
| paused runs, snapshots kept in memory | 1 | 6.5 | 576 |
| paused runs, snapshots kept in memory | 100 | 7.3 | 581 |
| paused runs, snapshots kept in memory | 1,000 | 9.2 | 585 |
| paused runs, snapshots kept in memory | 10,000 | 41.6 | 590 |
| runs in flight (200 ms model) | 1 | 6.4 | – |
| runs in flight (200 ms model) | 100 | 7.7 | – |
| runs in flight (200 ms model) | 1,000 | 16.5 | – |
| runs in flight (200 ms model) | 10,000 | 113.1 | – |

The paused graph is `start -> human-input -> answer`; one snapshot is resumed
afterwards to check that it is usable.

Variable pools are reference-counted per namespace: cloning a pool with 100
namespaces × 10 string variables allocates 8 bytes and takes ~10 ns. The first
write after a clone copies only the namespace being written (4,272 bytes here).

## Startup

| What | p50 |
| --- | ---: |
| Parse + build a 3-node YAML workflow | 37 µs |
| Parse + build a 1,002-node JSON workflow | 4.7 ms |
| Build the node registry (all built-in kinds) | 0.6 ms |
| `graphrt version` (process start to exit) | 1.4 ms |
| `graphrt run examples/hello.yaml` (process start to exit) | 8.3 ms |

CLI timings are 50 runs each after 3 warm-up runs, measured with Python's
`time.perf_counter` around `subprocess.run`.

## Sizes

`cargo build -p graphrt-cli --profile min-size` (opt-level `z`, fat LTO,
stripped, unwinding kept):

| Build | Size |
| --- | ---: |
| core only (`--no-default-features`) | 2.96 MiB |
| default (LLM, HTTP, JS code, Python runner, MCP, documents, RAG, plugins, WASM, server) | 12.51 MiB |
| container image (`Dockerfile`, distroless static, default features) | 24.36 MiB |

## Caveats

- **Mock models.** Model calls return immediately (or after a fixed sleep) from
  memory. Real providers add network latency and parsing; the runtime's share of
  an actual agent run is typically well under 1%.
- **One machine.** Results come from a single desktop CPU with other workloads
  idle but not isolated (no CPU pinning, frequency scaling on). Expect
  differences on laptops, small VMs and phones; rerun `bench/run.sh` there.
- **Sequential latency vs throughput.** Engine-overhead numbers are single runs
  on a current-thread runtime; the concurrency table uses a multi-thread runtime.
- **Not covered here:** HTTP, code sandbox, document extraction, retrieval and
  plugin process latency. Those depend on external systems and are better
  measured end to end in a target deployment.

## graphrt vs Python agent stacks

The same graph shapes in graphrt, [LangGraph](https://github.com/langchain-ai/langgraph)
(with `langchain-openai` for the model node) and plain LangChain LCEL runnables,
all calling one local fake OpenAI-compatible HTTP server that streams 10 deltas.
Reproduce with `bench/compare/run.sh --docker`; the scripts, both sides' graph
definitions and the fairness rules are in [`bench/compare/`](../bench/compare/README.md).
Raw data: [`bench/results/2026-11-19-Bima-Pangestu-vs-python.json`](../bench/results/2026-11-19-Bima-Pangestu-vs-python.json)
([tables](../bench/results/2026-11-19-Bima-Pangestu-vs-python.md)).

**Setup.** Apple M3 Pro laptop (11 cores, 18 GiB), macOS 26 (Darwin 25.6),
rustc 1.95.0, graphrt at commit `90d4db0` (`main` with the unreleased 0.3.0
work; the binary still reports 0.2.0), `min-size` profile, default features.
Python 3.11.11, langgraph 1.2.23, langchain-core 1.8.3, langchain-openai
1.8.3, openai 3.55.1 (exact pins in `bench/compare/requirements.txt`). Linux
rows: Docker Desktop's arm64 VM on the same laptop, container limited to
1 CPU and 1 GiB, Python 3.11.18 in `python:3.11-slim`, static musl graphrt.
The laptop was in normal use during the run (shared and noisy): differences
under ~20% are noise.

### Results

Medians; brackets are p25–p75 (throughput: min–max of 3 rounds).

| Metric | graphrt | LangGraph | LCEL |
| --- | ---: | ---: | ---: |
| Cold start, 3-node LLM chain, process start to exit (macOS) | 5.3 ms (5.2–5.6) | 728 ms (706–758) | 619 ms (614–624) |
| Cold start, 3-node LLM chain (Linux, 1 CPU) | 6.5 ms (5.8–10.3) | 803 ms (689–913) | 534 ms (527–554) |
| Cold start, 1,000-node passthrough chain (macOS) | 52 ms (52–52) | 734 ms (686–770) | 552 ms (539–619) |
| Cold start, 1,000-node passthrough chain (Linux, 1 CPU) | 100 ms (83–107) | 1,221 ms (1,136–1,292) | 381 ms (358–411) |
| Peak RSS, 3-node LLM chain, one run (macOS / Linux) | 12.7 / 8.8 MiB | 101 / 96 MiB | 94 / 90 MiB |
| Warm run, 3-node LLM chain, 0 ms server latency | 0.069 ms | 3.09 ms | 2.98 ms |
| Orchestration per node, 1,000-node chain (warm, marginal) | 0.37 µs | 215 µs (sync `invoke`: 136 µs) | 180 µs |
| Orchestration per node, 1,000-way fan-out / fan-in (warm, marginal) | 0.50 µs | 106 µs (sync: 112 µs) | 98 µs |
| Throughput, 50 ms server latency, 1 run in flight (ideal 20/s) | 19/s | 15/s | 16/s |
| Throughput, 50 ms latency, 100 in flight (ideal 2,000/s) | 1,821/s (1 thread: 1,837/s) | 308/s | 338/s |
| Throughput, 50 ms latency, 1,000 in flight (ideal 20,000/s) | 17,248/s (1 thread: 16,498/s) | 213/s | 239/s |
| Throughput, 0 ms latency, 100 in flight (CPU-bound) | 74,461/s (1 thread: 24,203/s) | 281/s | 289/s |
| Peak RSS, 1,000 runs in flight | 81 MiB (1 thread: 76 MiB) | 223 MiB | 185 MiB |
| Install footprint | 13.0 MiB binary (4.3 MiB with only the `llm` feature), no runtime | 88.5 MiB in 43 distributions, plus a Python interpreter (LCEL alone needs a subset) | – |
| Container image | 16.8 MiB (distroless static) | 245 MiB (`python:3.11-slim` + requirements) | – |

The import of `langgraph` and `langchain_openai` alone takes 641 ms (macOS)
and 546 ms (Linux) and accounts for nearly all of Python's cold start;
`python -c pass` takes 10 ms / 6 ms. The full per-shape tables (10-node
shapes, sync LangGraph, every concurrency level) are in the results file.

### Methodology

- **Cold start**: `graphrt run <workflow> --no-default-plugins
  --allow-private-network --output json` vs `python driver.py once <framework>
  <shape>` (build the graph, run it once with `asyncio.run(graph.ainvoke(...))`,
  print). hyperfine, `--shell=none`, 3 warm-up runs (warm page cache and
  `.pyc` files), 30 measured runs.
- **Peak RSS**: `/usr/bin/time -l` (macOS) and GNU `/usr/bin/time -v`
  (Linux), median of 5 runs; the throughput rows wrap each driver process.
- **Per-node overhead**: graph compiled once, 20 warm-up runs, then sequential
  runs in one process (graphrt: current-thread Tokio runtime; Python: one
  asyncio loop). Passthrough nodes forward one string (graphrt:
  `variable-aggregator`; LangGraph: a node returning `{"x": state["x"]}`;
  LCEL: `RunnableLambda`). "Marginal" is (t₁,₀₀₀ − t₁₀) / 990, which drops fixed
  per-run cost. graphrt also runs its `start`/`end` (and `join`) nodes, which
  the Python graphs do not have.
- **Throughput**: the 3-node LLM chain with C runs kept in flight by C worker
  tasks/coroutines, one shared compiled graph and one shared HTTP client on
  each side. Python runs on one asyncio event loop, as LangGraph is normally
  served; graphrt is reported on all cores and on one worker thread, which is
  the like-for-like comparison with one CPython thread.
- **Fairness**: same fake server, same latency and response bytes, streaming
  on both sides; Python uses async nodes and `ainvoke`, a compiled graph
  reused across runs, no checkpointer, no debug, `max_retries=0`, LangSmith
  tracing explicitly off (`LANGCHAIN_TRACING_V2=false`,
  `LANGSMITH_TRACING=false`, API keys removed). For CPU-only shapes we also ran
  sync LangGraph (`invoke`, sync nodes), which is faster on the chain and is
  shown next to the async number.

### Where graphrt does not win, or wins by little

- **Model-bound work.** With one run in flight and a 50 ms model, all three
  stacks are within ~20% of the ideal 20 runs/s: Python's ~3 ms per call is
  small next to 50 ms and negligible next to real provider latency (hundreds
  of ms to seconds). The large gaps above are orchestration and process
  overhead, not end-to-end agent latency.
- **Cold start of large graphs.** graphrt's cold start grows from 5 ms to
  52 ms (100 ms on one Linux CPU) for a 1,000-node workflow: the CLI parses
  and validates the document with full diagnostics before running it, while a
  warm run of the same graph takes 0.38 ms. On one Linux CPU, LCEL's cold
  start for that shape (381 ms) is only 3.8× slower.
- **Per-run memory under load.** Most of Python's RSS is the ~90 MiB
  interpreter + import baseline; per in-flight run, graphrt (~70 KiB at 1,000
  in flight, including HTTP buffers) and Python (~120 KiB for LangGraph) are
  the same order of magnitude.

### Caveats

- One laptop, in use during the run; no CPU pinning. The Linux numbers come
  from a VM on the same laptop, not from a small device.
- The fake server answers on loopback in microseconds; real networks and
  providers dominate real runs.
- Python was measured with the stock asyncio loop on CPython 3.11. uvloop,
  newer CPython versions, PyPy and multi-process deployments (several
  uvicorn workers) would raise Python's throughput; they were not measured.
- Only these two Python stacks and these shapes were measured: no tools,
  checkpointing or persistence on either side.
- graphrt's in-process numbers come from `bench/compare/rust` (same engine
  and OpenAI provider crates as the CLI, release profile with thin LTO), not
  from the `graphrt` binary.
