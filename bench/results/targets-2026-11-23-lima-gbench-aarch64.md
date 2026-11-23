### graphrt budgets (aarch64-linux (host), min-size)

| Build | Features | Binary (MiB) | Budget | Idle RSS (MiB) | Budget | exec→/healthz p50 / p95 (ms) |
| --- | --- | ---: | ---: | ---: | ---: | ---: |
| agent-minimal | `serve` | 3.763 | 3.85 | 3.1 | 5.0 | – |
| agent-slim | `serve,llm,plugins` | 6.212 | 6.35 | 4.82 | 6.0 | 1.26 / 1.71 |
| agent-slim-gateway | `serve,llm,plugins,trust-gateway` | 6.283 | +73.3 KiB over agent-slim (budget +77) | 4.86 | – | – |
| gateway | `gateway` | 7.659 | 8.0 | 7.01 | 20.0 | – |
| gateway-all | `gateway-all` | 8.449 | 9.0 | – | – | – |

All budgets met.

### graphrt gateway targets (aarch64, 4 CPUs, Linux 7.0.0-15-generic)

| Scenario | Metric | Value | Target | | Details |
| --- | --- | ---: | ---: | --- | --- |
| ttfb-uds | overhead p50 | 0.2516 ms | ≤ 0.3 ms | ok | gateway p50/p95 0.475/0.526 ms, direct 0.224/0.281 ms, n=1000; median of 3 rounds (0.2514/0.2673, 0.2516/0.2448, 0.2518/0.2511) |
| ttfb-uds | overhead p95 | 0.2448 ms | ≤ 1 ms | ok | gateway p50/p95 0.475/0.526 ms, direct 0.224/0.281 ms, n=1000; median of 3 rounds (0.2514/0.2673, 0.2516/0.2448, 0.2518/0.2511) |
| ttfb-tcp | overhead p95 (budget + 1 RTT) | 0.2393 ms | ≤ 1.0153 ms | ok | gateway p50/p95 0.469/0.509 ms, direct 0.227/0.270 ms, n=1000; median of 3 rounds (0.236, 0.2393, 0.2446) |
| sse | gateway peak RSS | 75.45 MiB | ≤ 80 MiB | ok | 1000 streams x 20 chunks, RSS idle 8.16 MiB, max chunk gap 533.9 ms |
| sse | gateway peak RSS (audit target, not gated) | 75.45 MiB | ≤ 64 MiB | not met | 1000 streams x 20 chunks, RSS idle 8.16 MiB, max chunk gap 533.9 ms |
| sse | failed streams | 0  | ≤ 0  | ok | 1000 streams x 20 chunks, RSS idle 8.16 MiB, max chunk gap 533.9 ms |
| sse | stalled streams | 0  | ≤ 0  | ok | 1000 streams x 20 chunks, RSS idle 8.16 MiB, max chunk gap 533.9 ms |
| wake | p95 to first event | 7.126 ms | ≤ 15 ms | ok | first event p50/p95 6.37/7.13 ms, run finished p50/p95 6.50/7.28 ms, n=100; median of 3 rounds (7.0725, 7.126, 7.2973) |
| wake-node | p95 to plugin result | 66.8418 ms | ≤ 60 ms | **FAIL** | first event p50/p95 6.53/10.06 ms, run finished p50/p95 61.21/66.84 ms, n=100; the plugin alone (spawn -> initialize, no graphrt) p50/p95 57.2/58.9 ms; median of 3 rounds (62.6791, 66.8418, 71.1216) |
| webhook | ack p95 | 0.5168 ms | ≤ 10 ms | ok | ack p50 0.306 ms, p99 0.742 ms, n=1000; median of 3 rounds (0.5113, 0.5168, 0.5428) |
| cron | lag p95 (fire -> run finished) | 418 ms | ≤ 1000 ms | ok | 1000 simultaneous fires; fire -> record created p50/p95 27/48 ms |

1 target(s) missed.

### JWT verification (RS256, cached JWKS key)

| Metric | Value | Target | |
| --- | --- | ---: | --- |
| mean per verification | {"jwt_verify_rs256_mean_us": 17.30, "p50_us": 16.46, "p95_us": 20.79, "iterations": 5000} | ≤ 50.0 µs | ok |

### PID 1 drain

test-pid1-container: ok: docker stop drained graphrt (PID 1) in 1547 ms, exit 0; run run-18ea8bb2cff1b778-1 paused (drained)

**Budgets missed (see FAIL rows).**
