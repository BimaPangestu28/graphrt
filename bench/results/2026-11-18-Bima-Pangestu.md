# graphrt benchmarks — 2026-11-18-Bima-Pangestu

arm, 11 logical CPUs, None GiB RAM, Darwin 25.6.0, rustc 1.95.0 (59807616e 2026-04-14), commit f9f9623

## Engine overhead (sequential runs, current-thread runtime)

| Graph | Nodes | p50 ms | p95 ms | µs / node (p50) |
|---|---:|---:|---:|---:|
| chain_noop_10 | 12 | 0.021 | 0.024 | 1.75 |
| chain_template_10 | 12 | 0.030 | 0.032 | 2.47 |
| chain_noop_100 | 102 | 0.151 | 0.163 | 1.48 |
| chain_template_100 | 102 | 0.247 | 0.275 | 2.42 |
| chain_noop_1000 | 1002 | 1.556 | 1.793 | 1.55 |
| chain_template_1000 | 1002 | 2.662 | 2.891 | 2.66 |
| fan_out_in_100 | 103 | 0.170 | 0.186 | 1.65 |
| fan_out_in_1000 | 1003 | 2.316 | 5.704 | 2.31 |
| branches_100 | 602 | 0.748 | 0.879 | 1.24 |
| parallel_iteration_1000 | 2003 | 4.159 | 4.335 | 2.08 |
| loop10_x_iteration10 | 223 | 0.446 | 0.552 | 2.00 |

## Concurrent runs (multi-thread runtime, mock model, 10 streamed chunks)

| Model delay | Concurrent runs | Runs / s | Ideal runs / s |
|---:|---:|---:|---:|
| 0 ms | 1 | 42,903 | - |
| 0 ms | 10 | 53,659 | - |
| 0 ms | 100 | 64,091 | - |
| 0 ms | 1000 | 63,232 | - |
| 50 ms | 1 | 18 | 20 |
| 50 ms | 10 | 181 | 200 |
| 50 ms | 100 | 1,788 | 2,000 |
| 50 ms | 1000 | 15,137 | 20,000 |
| 50 ms | 10000 | 63,546 | 200,000 |

## Streaming

Time to first answer chunk through engine + ResponseAssembler: p50 16.7 µs (p95 20.2 µs); raw mock stream first delta: 2.2 µs; overhead ≈ 14.5 µs.

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

Variable pool clone (100 namespaces x 10 string variables): 8 bytes allocated, 4.8 ns; first write after a clone copies one namespace (1528 bytes).

## Startup

- Parse + build, 3-node YAML: p50 40 µs
- Parse + build, 1,002-node JSON: p50 6.07 ms
- Node registry construction: 531 µs
