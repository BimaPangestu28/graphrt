### graphrt budgets (aarch64-linux (host), min-size)

| Build | Features | Binary (MiB) | Budget | Idle RSS (MiB) | Budget | exec→/healthz p50 / p95 (ms) |
| --- | --- | ---: | ---: | ---: | ---: | ---: |
| agent-minimal | `serve` | 3.763 | 3.85 | 3.1 | 5.0 | – |
| agent-slim | `serve,llm,plugins` | 6.212 | 6.35 | 4.82 | 6.0 | 1.8 / 4.28 |
| agent-slim-gateway | `serve,llm,plugins,trust-gateway` | 6.283 | +73.3 KiB over agent-slim (budget +77) | 4.87 | – | – |
| gateway | `gateway` | 7.772 | 8.0 | 7.07 | 20.0 | – |
| gateway-all | `gateway-all` | 8.564 | 9.0 | – | – | – |

All budgets met.

### graphrt gateway targets (aarch64, 4 CPUs, Linux 7.0.0-15-generic)

| Scenario | Metric | Value | Target | | Details |
| --- | --- | ---: | ---: | --- | --- |
| ttfb-uds | overhead p50 | 0.2347 ms | ≤ 0.3 ms | ok | gateway p50/p95 0.441/0.484 ms, direct 0.206/0.247 ms, n=1000; median of 3 rounds (0.2328/0.2261, 0.2347/0.2372, 0.2361/0.2396) |
| ttfb-uds | overhead p95 | 0.2372 ms | ≤ 1 ms | ok | gateway p50/p95 0.441/0.484 ms, direct 0.206/0.247 ms, n=1000; median of 3 rounds (0.2328/0.2261, 0.2347/0.2372, 0.2361/0.2396) |
| ttfb-tcp | overhead p95 (budget + 1 RTT) | 0.2289 ms | ≤ 1.0123 ms | ok | gateway p50/p95 0.449/0.500 ms, direct 0.221/0.271 ms, n=1000; median of 3 rounds (0.2262, 0.2289, 0.2321) |
| sse | gateway peak RSS | 77.38 MiB | ≤ 80 MiB | ok | 1000 streams x 20 chunks, RSS idle 8.28 MiB, max chunk gap 544.4 ms |
| sse | gateway peak RSS (audit target, not gated) | 77.38 MiB | ≤ 64 MiB | not met | 1000 streams x 20 chunks, RSS idle 8.28 MiB, max chunk gap 544.4 ms |
| sse | failed streams | 0  | ≤ 0  | ok | 1000 streams x 20 chunks, RSS idle 8.28 MiB, max chunk gap 544.4 ms |
| sse | stalled streams | 0  | ≤ 0  | ok | 1000 streams x 20 chunks, RSS idle 8.28 MiB, max chunk gap 544.4 ms |
| wake | p95 to first event | 6.5821 ms | ≤ 15 ms | ok | first event p50/p95 5.69/6.58 ms, run finished p50/p95 5.79/6.71 ms, n=100; median of 3 rounds (6.3875, 6.5821, 6.6109) |
| wake-node | p95 to plugin result | 56.8785 ms | ≤ 60 ms | ok | first event p50/p95 5.87/7.70 ms, run finished p50/p95 55.46/56.88 ms, n=100; the plugin alone (spawn -> initialize, no graphrt) p50/p95 50.4/50.8 ms; median of 3 rounds (56.5255, 56.8785, 59.0376) |
| webhook | ack p95 | 0.5229 ms | ≤ 10 ms | ok | ack p50 0.349 ms, p99 0.682 ms, n=1000; median of 3 rounds (0.5185, 0.5229, 0.5296) |
| cron | lag p95 (fire -> run finished) | 426 ms | ≤ 1000 ms | ok | 1000 simultaneous fires; fire -> record created p50/p95 47/70 ms |

All targets met.

### JWT verification (RS256, cached JWKS key)

| Metric | Value | Target | |
| --- | --- | ---: | --- |
| mean per verification | {"jwt_verify_rs256_mean_us": 16.66, "p50_us": 16.13, "p95_us": 17.25, "iterations": 5000} | ≤ 50.0 µs | ok |

### PID 1 drain

test-pid1-container: ok: docker stop drained graphrt (PID 1) in 1549 ms, exit 0; run run-18ea8dd3a640824d-1 paused (drained)

**All budgets met.**
