### graphrt budgets (aarch64-linux (host), min-size)

| Build | Features | Binary (MiB) | Budget | Idle RSS (MiB) | Budget | exec→/healthz p50 / p95 (ms) |
| --- | --- | ---: | ---: | ---: | ---: | ---: |
| agent-minimal | `serve` | 3.609 | 3.85 | 3.12 | 5.0 | – |
| agent-slim | `serve,llm,plugins` | 6.001 | 6.35 | 4.61 | 6.0 | 2.34 / 5.92 |
| agent-slim-gateway | `serve,llm,plugins,trust-gateway` | 6.072 | +73.6 KiB over agent-slim (budget +77) | 4.71 | – | – |
| gateway-core | `gateway` | 6.799 | 6.95 | 6.61 | 20.0 | – |
| gateway-all | `gateway-all` | 7.584 | 7.75 | – | – | – |

All budgets met.

### graphrt gateway targets (aarch64, 4 CPUs, Linux 7.0.0-15-generic)

| Scenario | Metric | Value | Target | | Details |
| --- | --- | ---: | ---: | --- | --- |
| ttfb-uds | overhead p50 | 0.2495 ms | ≤ 0.3 ms | ok | gateway p50/p95 0.465/0.604 ms, direct 0.215/0.296 ms, n=1000; median of 3 rounds (0.2486/0.2611, 0.2495/0.3078, 0.2511/0.4293) |
| ttfb-uds | overhead p95 | 0.3078 ms | ≤ 1 ms | ok | gateway p50/p95 0.465/0.604 ms, direct 0.215/0.296 ms, n=1000; median of 3 rounds (0.2486/0.2611, 0.2495/0.3078, 0.2511/0.4293) |
| ttfb-tcp | overhead p95 (budget + 1 RTT) | 0.3879 ms | ≤ 1.0166 ms | ok | gateway p50/p95 0.479/0.916 ms, direct 0.244/0.528 ms, n=1000; median of 3 rounds (0.238, 0.3879, 0.7386) |
| sse | gateway peak RSS | 76.17 MiB | ≤ 80 MiB | ok | 1000 streams x 20 chunks, RSS idle 7.59 MiB, max chunk gap 558.8 ms; median of 3 rounds (74.25/74.25/0/0, 76.17/76.17/0/0, 76.31/76.31/0/0) |
| sse | gateway peak RSS (audit target, not gated) | 76.17 MiB | ≤ 64 MiB | not met | 1000 streams x 20 chunks, RSS idle 7.59 MiB, max chunk gap 558.8 ms; median of 3 rounds (74.25/74.25/0/0, 76.17/76.17/0/0, 76.31/76.31/0/0) |
| sse | failed streams | 0  | ≤ 0  | ok | 1000 streams x 20 chunks, RSS idle 7.59 MiB, max chunk gap 558.8 ms; median of 3 rounds (74.25/74.25/0/0, 76.17/76.17/0/0, 76.31/76.31/0/0) |
| sse | stalled streams | 0  | ≤ 0  | ok | 1000 streams x 20 chunks, RSS idle 7.59 MiB, max chunk gap 558.8 ms; median of 3 rounds (74.25/74.25/0/0, 76.17/76.17/0/0, 76.31/76.31/0/0) |
| wake | p95 to first event | 6.5551 ms | ≤ 15 ms | ok | first event p50/p95 5.69/6.56 ms, run finished p50/p95 5.84/6.71 ms, n=100; median of 3 rounds (6.36, 6.5551, 8.26) |
| wake-node | p95 to plugin result | 67.9247 ms | ≤ 60 ms | **FAIL** | first event p50/p95 6.68/10.28 ms, run finished p50/p95 56.24/67.92 ms, n=100; the plugin alone (spawn -> initialize, no graphrt) p50/p95 51.9/67.5 ms; median of 3 rounds (65.8608, 67.9247, 78.0717) |
| webhook | ack p95 | 0.6725 ms | ≤ 10 ms | ok | ack p50 0.359 ms, p99 1.149 ms, n=1000; median of 3 rounds (0.6463, 0.6725, 1.4894) |
| cron | lag p95 (fire -> run finished) | 460 ms | ≤ 1000 ms | ok | 1000 simultaneous fires; fire -> record created p50/p95 56/82 ms |

1 target(s) missed.

### JWT verification (RS256, cached JWKS key)

| Metric | Value | Target | |
| --- | --- | ---: | --- |
| mean per verification | {"jwt_verify_rs256_mean_us": 16.62, "p50_us": 16.15, "p95_us": 18.16, "iterations": 5000} | ≤ 50.0 µs | ok |

### PID 1 drain

test-pid1-container: ok: docker stop drained graphrt (PID 1) in 1593 ms, exit 0; run run-18ea9f8fece088fb-1 paused (drained)

**Budgets missed (see FAIL rows).**
