# graphrt benchmarks — 2026-11-17-bimapangestu

13th Gen Intel(R) Core(TM) i7-13700, 24 logical CPUs, 61.6 GiB RAM, Linux 7.0.0-43-generic, rustc 1.95.0 (59807616e 2026-04-14), commit 72f1db6

## Engine overhead (sequential runs, current-thread runtime)

| Graph | Nodes | p50 ms | p95 ms | µs / node (p50) |
|---|---:|---:|---:|---:|
| chain_noop_10 | 12 | 0.017 | 0.026 | 1.45 |
| chain_template_10 | 12 | 0.025 | 0.028 | 2.12 |
| chain_noop_100 | 102 | 0.227 | 0.238 | 2.22 |
| chain_template_100 | 102 | 0.308 | 0.324 | 3.02 |
| chain_noop_1000 | 1002 | 9.197 | 9.315 | 9.18 |
| chain_template_1000 | 1002 | 10.154 | 10.277 | 10.13 |
| fan_out_in_100 | 103 | 0.291 | 0.304 | 2.82 |
| fan_out_in_1000 | 1003 | 15.003 | 15.329 | 14.96 |
| branches_100 | 602 | 1.394 | 1.427 | 2.32 |
| parallel_iteration_1000 | 2003 | 3.587 | 3.617 | 1.79 |
| loop10_x_iteration10 | 223 | 0.421 | 0.440 | 1.89 |

## Concurrent runs (multi-thread runtime, mock model, 10 streamed chunks)

| Model delay | Concurrent runs | Runs / s | Ideal runs / s |
|---:|---:|---:|---:|
| 0 ms | 1 | 37,351 | - |
| 0 ms | 10 | 117,475 | - |
| 0 ms | 100 | 188,874 | - |
| 0 ms | 1000 | 191,052 | - |
| 50 ms | 1 | 20 | 20 |
| 50 ms | 10 | 195 | 200 |
| 50 ms | 100 | 1,931 | 2,000 |
| 50 ms | 1000 | 18,104 | 20,000 |
| 50 ms | 10000 | 110,435 | 200,000 |

## Streaming

Time to first answer chunk through engine + ResponseAssembler: p50 13.6 µs (p95 21.8 µs); raw mock stream first delta: 1.9 µs; overhead ≈ 11.7 µs.

## Memory

| Scenario | Runs | Peak RSS MiB | Snapshot bytes (avg) |
|---|---:|---:|---:|
| paused (snapshots held) | 1 | 6.5 | 576 |
| in flight (200 ms model) | 1 | 6.4 | - |
| paused (snapshots held) | 100 | 7.3 | 581 |
| in flight (200 ms model) | 100 | 7.7 | - |
| paused (snapshots held) | 1000 | 9.2 | 585 |
| in flight (200 ms model) | 1000 | 16.5 | - |
| paused (snapshots held) | 10000 | 41.6 | 590 |
| in flight (200 ms model) | 10000 | 113.1 | - |

Variable pool clone (100 namespaces x 10 string variables): 8 bytes allocated, 9.6 ns; first write after a clone copies one namespace (4272 bytes).

## Startup

- Parse + build, 3-node YAML: p50 37 µs
- Parse + build, 1,002-node JSON: p50 4.67 ms
- Node registry construction: 644 µs
- CLI `graphrt version`: p50 1.4 ms
- CLI `graphrt run hello.yaml`: p50 8.3 ms (process start to exit)

## Sizes

| Build | Size |
|---|---:|
| core-only | 2.96 MiB |
| default | 12.51 MiB |
| docker-image | 24.36 MiB |
