# graphrt benchmarks — 2026-11-21-Bima-Pangestu

arm, 11 logical CPUs, None GiB RAM, Darwin 25.6.0, rustc 1.95.0 (59807616e 2026-04-14), commit 2e8d9f8

## Engine overhead (sequential runs, current-thread runtime)

| Graph | Nodes | p50 ms | p95 ms | µs / node (p50) |
|---|---:|---:|---:|---:|
| chain_noop_10 | 12 | 0.011 | 0.026 | 0.92 |
| chain_template_10 | 12 | 0.014 | 0.021 | 1.16 |
| chain_noop_100 | 102 | 0.045 | 0.052 | 0.44 |
| chain_template_100 | 102 | 0.081 | 0.089 | 0.79 |
| chain_noop_1000 | 1002 | 0.367 | 0.380 | 0.37 |
| chain_template_1000 | 1002 | 0.798 | 0.827 | 0.80 |
| fan_out_in_100 | 103 | 0.045 | 0.048 | 0.43 |
| fan_out_in_1000 | 1003 | 0.484 | 0.505 | 0.48 |
| branches_100 | 602 | 0.248 | 0.260 | 0.41 |
| parallel_iteration_1000 | 2003 | 1.849 | 1.891 | 0.92 |
| loop10_x_iteration10 | 223 | 0.210 | 0.228 | 0.94 |

## Allocations (sequential runs, counted by the harness allocator)

| Graph | Nodes | Allocations / run | Bytes / run | Allocations / node |
|---|---:|---:|---:|---:|
| chain_noop_100 | 102 | 421 | 159972 | 4.1 |
| chain_template_100 | 102 | 1118 | 825745 | 11.0 |
| fan_out_in_100 | 103 | 562 | 206614 | 5.5 |
| llm_10_chunks | 3 | 89 | 26150 | 29.7 |

## Concurrent runs (multi-thread runtime, mock model, 10 streamed chunks)

| Model delay | Concurrent runs | Runs / s | Ideal runs / s |
|---:|---:|---:|---:|
| 0 ms | 1 | 57,678 | - |
| 0 ms | 10 | 185,874 | - |
| 0 ms | 100 | 266,488 | - |
| 0 ms | 1000 | 312,014 | - |
| 50 ms | 1 | 18 | 20 |
| 50 ms | 10 | 184 | 200 |
| 50 ms | 100 | 1,844 | 2,000 |
| 50 ms | 1000 | 15,583 | 20,000 |
| 50 ms | 10000 | 108,330 | 200,000 |

## Streaming

Time to first answer chunk through engine + ResponseAssembler: p50 8.0 µs (p95 9.8 µs); raw mock stream first delta: 2.2 µs; overhead ≈ 5.8 µs.

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

Variable pool clone (100 namespaces x 10 string variables): 16 bytes allocated, 3.7 ns; first write after a clone copies one namespace (1544 bytes).

## Startup

- Parse + build, 3-node YAML: p50 41 µs
- Parse + build, 1,002-node JSON: p50 6.41 ms
- Node registry construction: 5 µs
- CLI `graphrt version`: p50 5.3 ms
- CLI `graphrt run hello.yaml`: p50 5.4 ms (process start to exit)

## Sizes

| Build | Size |
|---|---:|
| core-only | 2.47 MiB |
| default | 13.27 MiB |
