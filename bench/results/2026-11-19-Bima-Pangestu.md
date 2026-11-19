# graphrt benchmarks — 2026-11-19-Bima-Pangestu

arm, 11 logical CPUs, None GiB RAM, Darwin 25.6.0, rustc 1.95.0 (59807616e 2026-04-14), commit bfbe877

## Engine overhead (sequential runs, current-thread runtime)

| Graph | Nodes | p50 ms | p95 ms | µs / node (p50) |
|---|---:|---:|---:|---:|
| chain_noop_10 | 12 | 0.020 | 0.030 | 1.63 |
| chain_template_10 | 12 | 0.021 | 0.026 | 1.72 |
| chain_noop_100 | 102 | 0.088 | 0.093 | 0.86 |
| chain_template_100 | 102 | 0.148 | 0.149 | 1.45 |
| chain_noop_1000 | 1002 | 0.876 | 0.892 | 0.87 |
| chain_template_1000 | 1002 | 1.657 | 1.746 | 1.65 |
| fan_out_in_100 | 103 | 0.099 | 0.104 | 0.96 |
| fan_out_in_1000 | 1003 | 1.185 | 1.217 | 1.18 |
| branches_100 | 602 | 0.451 | 0.465 | 0.75 |
| parallel_iteration_1000 | 2003 | 2.968 | 3.082 | 1.48 |
| loop10_x_iteration10 | 223 | 0.303 | 0.316 | 1.36 |

## Concurrent runs (multi-thread runtime, mock model, 10 streamed chunks)

| Model delay | Concurrent runs | Runs / s | Ideal runs / s |
|---:|---:|---:|---:|
| 0 ms | 1 | 53,015 | - |
| 0 ms | 10 | 98,930 | - |
| 0 ms | 100 | 95,211 | - |
| 0 ms | 1000 | 98,257 | - |
| 50 ms | 1 | 18 | 20 |
| 50 ms | 10 | 176 | 200 |
| 50 ms | 100 | 1,772 | 2,000 |
| 50 ms | 1000 | 14,999 | 20,000 |
| 50 ms | 10000 | 83,379 | 200,000 |

## Streaming

Time to first answer chunk through engine + ResponseAssembler: p50 11.7 µs (p95 12.2 µs); raw mock stream first delta: 2.2 µs; overhead ≈ 9.5 µs.

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

Variable pool clone (100 namespaces x 10 string variables): 8 bytes allocated, 3.9 ns; first write after a clone copies one namespace (1504 bytes).

## Startup

- Parse + build, 3-node YAML: p50 38 µs
- Parse + build, 1,002-node JSON: p50 5.20 ms
- Node registry construction: 3 µs
