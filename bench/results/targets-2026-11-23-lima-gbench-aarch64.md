### graphrt budgets (aarch64-linux (host), min-size)

| Build | Features | Binary (MiB) | Budget | Idle RSS (MiB) | Budget | exec→/healthz p50 / p95 (ms) |
| --- | --- | ---: | ---: | ---: | ---: | ---: |
| agent-minimal | `serve` | 3.763 | 3.85 | 3.1 | 5.0 | – |
| agent-slim | `serve,llm,plugins` | 6.209 | 6.35 | 4.81 | 6.0 | 1.95 / 3.6 |
| agent-slim-gateway | `serve,llm,plugins,trust-gateway` | 6.28 | +73.3 KiB over agent-slim (budget +77) | 4.87 | – | – |
| gateway-core | `gateway` | 7.337 | 7.55 | 6.79 | 20.0 | – |
| gateway-all | `gateway-all` | 8.125 | 8.35 | – | – | – |

All budgets met.

### graphrt gateway targets (aarch64, 4 CPUs, Linux 7.0.0-15-generic)

| Scenario | Metric | Value | Target | | Details |
| --- | --- | ---: | ---: | --- | --- |
| ttfb-uds | overhead p50 | 0.2434 ms | ≤ 0.3 ms | ok | gateway p50/p95 0.440/0.476 ms, direct 0.197/0.235 ms, n=1000; median of 3 rounds (0.2424/0.2411, 0.2434/0.2414, 0.2438/0.2446) |
| ttfb-uds | overhead p95 | 0.2414 ms | ≤ 1 ms | ok | gateway p50/p95 0.440/0.476 ms, direct 0.197/0.235 ms, n=1000; median of 3 rounds (0.2424/0.2411, 0.2434/0.2414, 0.2438/0.2446) |
| ttfb-tcp | overhead p95 (budget + 1 RTT) | 0.2264 ms | ≤ 1.0151 ms | ok | gateway p50/p95 0.440/0.475 ms, direct 0.210/0.249 ms, n=1000; median of 3 rounds (0.2193, 0.2264, 0.2342) |
| sse | gateway peak RSS | 77.8 MiB | ≤ 80 MiB | ok | 1000 streams x 20 chunks, RSS idle 7.97 MiB, max chunk gap 529.7 ms |
| sse | gateway peak RSS (audit target, not gated) | 77.8 MiB | ≤ 64 MiB | not met | 1000 streams x 20 chunks, RSS idle 7.97 MiB, max chunk gap 529.7 ms |
| sse | failed streams | 0  | ≤ 0  | ok | 1000 streams x 20 chunks, RSS idle 7.97 MiB, max chunk gap 529.7 ms |
| sse | stalled streams | 0  | ≤ 0  | ok | 1000 streams x 20 chunks, RSS idle 7.97 MiB, max chunk gap 529.7 ms |
| wake | p95 to first event | 6.7782 ms | ≤ 15 ms | ok | first event p50/p95 5.72/6.78 ms, run finished p50/p95 5.84/6.96 ms, n=100; median of 3 rounds (6.4554, 6.7782, 8.1671) |
| wake-node | p95 to plugin result | 57.2701 ms | ≤ 60 ms | ok | first event p50/p95 5.88/8.59 ms, run finished p50/p95 55.26/57.27 ms, n=100; the plugin alone (spawn -> initialize, no graphrt) p50/p95 50.0/50.6 ms; median of 3 rounds (57.092, 57.2701, 57.9516) |
| webhook | ack p95 | 0.5198 ms | ≤ 10 ms | ok | ack p50 0.346 ms, p99 0.748 ms, n=1000; median of 3 rounds (0.5129, 0.5198, 0.5392) |
| cron | lag p95 (fire -> run finished) | 433 ms | ≤ 1000 ms | ok | 1000 simultaneous fires; fire -> record created p50/p95 66/91 ms |

All targets met.

### JWT verification (RS256, cached JWKS key)

| Metric | Value | Target | |
| --- | --- | ---: | --- |
| mean per verification | {"jwt_verify_rs256_mean_us": 16.67, "p50_us": 16.13, "p95_us": 17.13, "iterations": 5000} | ≤ 50.0 µs | ok |

### PID 1 drain

test-pid1-container: ok: docker stop drained graphrt (PID 1) in 1586 ms, exit 0; run run-18ea92f4d52c369e-1 paused (drained)

**All budgets met.**
