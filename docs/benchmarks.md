# Benchmarks

These numbers describe the **runtime itself**: scheduling, variable handling,
streaming, memory and startup. Every model call goes to an in-process mock, so
nothing here measures a model provider, the network or a real tool.

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
