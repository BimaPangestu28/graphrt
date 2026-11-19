# graphrt benchmarks — 2026-11-19-Bima-Pangestu

arm, 11 logical CPUs, None GiB RAM, Darwin 25.6.0, rustc 1.95.0 (59807616e 2026-04-14), commit 1daba78

## Engine overhead (sequential runs, current-thread runtime)

| Graph | Nodes | p50 ms | p95 ms | µs / node (p50) |
|---|---:|---:|---:|---:|
| chain_noop_10 | 12 | 0.012 | 0.025 | 1.03 |
| chain_template_10 | 12 | 0.018 | 0.022 | 1.47 |
| chain_noop_100 | 102 | 0.080 | 0.086 | 0.79 |
| chain_template_100 | 102 | 0.142 | 0.162 | 1.39 |
| chain_noop_1000 | 1002 | 0.806 | 0.857 | 0.80 |
| chain_template_1000 | 1002 | 1.741 | 2.116 | 1.74 |
| fan_out_in_100 | 103 | 0.087 | 0.204 | 0.85 |
| fan_out_in_1000 | 1003 | 1.039 | 1.132 | 1.04 |
| branches_100 | 602 | 0.463 | 0.894 | 0.77 |
| parallel_iteration_1000 | 2003 | 3.053 | 6.981 | 1.52 |
| loop10_x_iteration10 | 223 | 0.298 | 0.334 | 1.33 |

## Concurrent runs (multi-thread runtime, mock model, 10 streamed chunks)

| Model delay | Concurrent runs | Runs / s | Ideal runs / s |
|---:|---:|---:|---:|
| 0 ms | 1 | 49,231 | - |
| 0 ms | 10 | 131,760 | - |
| 0 ms | 100 | 210,418 | - |
| 0 ms | 1000 | 205,140 | - |
| 50 ms | 1 | 19 | 20 |
| 50 ms | 10 | 191 | 200 |
| 50 ms | 100 | 1,863 | 2,000 |
| 50 ms | 1000 | 18,046 | 20,000 |
| 50 ms | 10000 | 109,246 | 200,000 |

## Streaming

Time to first answer chunk through engine + ResponseAssembler: p50 12.0 µs (p95 29.7 µs); raw mock stream first delta: 2.4 µs; overhead ≈ 9.7 µs.

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

Variable pool clone (100 namespaces x 10 string variables): 8 bytes allocated, 4.1 ns; first write after a clone copies one namespace (1504 bytes).

## Startup

- Parse + build, 3-node YAML: p50 39 µs
- Parse + build, 1,002-node JSON: p50 5.57 ms
- Node registry construction: 5 µs
