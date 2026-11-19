# graphrt benchmarks — 2026-11-19-Bima-Pangestu

arm, 11 logical CPUs, None GiB RAM, Darwin 25.6.0, rustc 1.95.0 (59807616e 2026-04-14), commit d7a86e9

## Engine overhead (sequential runs, current-thread runtime)

| Graph | Nodes | p50 ms | p95 ms | µs / node (p50) |
|---|---:|---:|---:|---:|
| chain_noop_10 | 12 | 0.007 | 0.017 | 0.60 |
| chain_template_10 | 12 | 0.012 | 0.017 | 1.01 |
| chain_noop_100 | 102 | 0.047 | 0.050 | 0.46 |
| chain_template_100 | 102 | 0.082 | 0.087 | 0.80 |
| chain_noop_1000 | 1002 | 0.353 | 0.355 | 0.35 |
| chain_template_1000 | 1002 | 0.790 | 0.806 | 0.79 |
| fan_out_in_100 | 103 | 0.045 | 0.045 | 0.43 |
| fan_out_in_1000 | 1003 | 0.471 | 0.477 | 0.47 |
| branches_100 | 602 | 0.248 | 0.252 | 0.41 |
| parallel_iteration_1000 | 2003 | 1.853 | 1.863 | 0.93 |
| loop10_x_iteration10 | 223 | 0.206 | 0.208 | 0.92 |

## Allocations (sequential runs, counted by the harness allocator)

| Graph | Nodes | Allocations / run | Bytes / run | Allocations / node |
|---|---:|---:|---:|---:|
| chain_noop_100 | 102 | 420 | 159964 | 4.1 |
| chain_template_100 | 102 | 1117 | 825737 | 11.0 |
| fan_out_in_100 | 103 | 561 | 206606 | 5.4 |
| llm_10_chunks | 3 | 88 | 26126 | 29.3 |

## Concurrent runs (multi-thread runtime, mock model, 10 streamed chunks)

| Model delay | Concurrent runs | Runs / s | Ideal runs / s |
|---:|---:|---:|---:|
| 0 ms | 1 | 60,294 | - |
| 0 ms | 10 | 171,117 | - |
| 0 ms | 100 | 251,947 | - |
| 0 ms | 1000 | 289,094 | - |
| 50 ms | 1 | 16 | 20 |
| 50 ms | 10 | 190 | 200 |
| 50 ms | 100 | 1,754 | 2,000 |
| 50 ms | 1000 | 15,379 | 20,000 |
| 50 ms | 10000 | 108,629 | 200,000 |

## Streaming

Time to first answer chunk through engine + ResponseAssembler: p50 8.1 µs (p95 10.2 µs); raw mock stream first delta: 2.3 µs; overhead ≈ 5.8 µs.

## Memory

| Scenario | Runs | Peak RSS MiB | Snapshot bytes (avg) |
|---|---:|---:|---:|
| paused (snapshots held) | 1 | n/a (Linux only) | 576 |
| in flight (200 ms model) | 1 | n/a (Linux only) | - |
| paused (snapshots held) | 100 | n/a (Linux only) | 581 |
| in flight (200 ms model) | 100 | n/a (Linux only) | - |
| paused (snapshots held) | 1000 | n/a (Linux only) | 585 |
| in flight (200 ms model) | 1000 | n/a (Linux only) | - |
| paused (snapshots held) | 10000 | n/a (Linux only) | 590 |
| in flight (200 ms model) | 10000 | n/a (Linux only) | - |

Variable pool clone (100 namespaces x 10 string variables): 16 bytes allocated, 3.8 ns; first write after a clone copies one namespace (1544 bytes).

## Startup

- Parse + build, 3-node YAML: p50 45 µs
- Parse + build, 1,002-node JSON: p50 7.71 ms
- Node registry construction: 3 µs
- CLI `graphrt version`: p50 3.9 ms
- CLI `graphrt run hello.yaml`: p50 4.9 ms (process start to exit)

## Sizes

| Build | Size |
|---|---:|
| core-only | 2.45 MiB |
| default | 13.01 MiB |
