### graphrt budgets (aarch64-linux (host), min-size)

| Build | Features | Binary (MiB) | Budget | Idle RSS (MiB) | Budget | exec→/healthz p50 / p95 (ms) |
| --- | --- | ---: | ---: | ---: | ---: | ---: |
| agent-minimal | `serve` | 3.609 | 3.85 | 3.11 | 5.0 | – |
| agent-slim | `serve,llm,plugins` | 6.029 | 6.35 | 4.82 | 6.0 | 1.89 / 6.11 |
| agent-slim-gateway | `serve,llm,plugins,trust-gateway` | 6.099 | +72.2 KiB over agent-slim (budget +77) | 4.87 | – | – |
| gateway-core | `gateway` | 6.842 | 6.95 | 6.66 | 20.0 | – |
| gateway-all | `gateway-all` | 7.631 | 7.75 | – | – | – |

All budgets met.

### graphrt gateway targets (aarch64, 4 CPUs, Linux 7.0.0-15-generic)

| Scenario | Metric | Value | Target | | Details |
| --- | --- | ---: | ---: | --- | --- |
| ttfb-uds | overhead p50 | 0.2412 ms | ≤ 0.3 ms | ok | gateway p50/p95 0.463/0.564 ms, direct 0.222/0.286 ms, n=1000; median of 3 rounds (0.2406/0.2915, 0.2412/0.2776, 0.2455/0.2696) |
| ttfb-uds | overhead p95 | 0.2776 ms | ≤ 1 ms | ok | gateway p50/p95 0.463/0.564 ms, direct 0.222/0.286 ms, n=1000; median of 3 rounds (0.2406/0.2915, 0.2412/0.2776, 0.2455/0.2696) |
| ttfb-tcp | overhead p95 (budget + 1 RTT) | 0.2859 ms | ≤ 1.0149 ms | ok | gateway p50/p95 0.479/0.656 ms, direct 0.250/0.370 ms, n=1000; median of 3 rounds (0.2639, 0.2859, 0.2914) |
| sse | gateway peak RSS | 44.53 MiB | ≤ 64 MiB | ok | 1000 streams x 20 chunks, RSS idle 7.27 MiB, max chunk gap 513.1 ms; median of 3 rounds (44.36/0/0, 44.53/0/0, 44.79/0/0) |
| sse | failed streams | 0  | ≤ 0  | ok | 1000 streams x 20 chunks, RSS idle 7.27 MiB, max chunk gap 513.1 ms; median of 3 rounds (44.36/0/0, 44.53/0/0, 44.79/0/0) |
| sse | stalled streams | 0  | ≤ 0  | ok | 1000 streams x 20 chunks, RSS idle 7.27 MiB, max chunk gap 513.1 ms; median of 3 rounds (44.36/0/0, 44.53/0/0, 44.79/0/0) |
| wake | p95 to first event | 5.2691 ms | ≤ 15 ms | ok | first event p50/p95 4.28/5.27 ms, run finished p50/p95 4.42/5.44 ms, n=100; median of 3 rounds (5.0895, 5.2691, 5.4215) |
| wake-node | p95 to plugin result | 70.5162 ms | ≤ 60 ms | **FAIL** | first event p50/p95 7.36/8.91 ms, run finished p50/p95 64.75/70.52 ms, n=100; the plugin alone (spawn -> initialize, no graphrt) p50/p95 59.1/61.6 ms; median of 3 rounds (70.0704, 70.5162, 70.7567) |
| wake-wasm | p95 to plugin result (reference, not gated) | 13.0594 ms | ≤ 60 ms | ok | first event p50/p95 10.68/11.94 ms, run finished p50/p95 12.27/13.06 ms, n=100; median of 3 rounds (12.9862, 13.0594, 13.265) |
| plugin-pool | p95 run after a warm hint | 10.628 ms | ≤ 15 ms | ok | RSS agent + plugins, pool 0/1/2: 73.32/74.39/140.13 MiB, agent alone after the idle stop 5.89 MiB; run p50/p95 warm 1.07/3.72 ms, after the idle stop cold 94.87/109.45 ms vs. 300 ms after a warm hint 6.62/10.63 ms, n=20 |
| plugin-pool | p95 run on a warm pool | 3.7195 ms | ≤ 15 ms | ok | RSS agent + plugins, pool 0/1/2: 73.32/74.39/140.13 MiB, agent alone after the idle stop 5.89 MiB; run p50/p95 warm 1.07/3.72 ms, after the idle stop cold 94.87/109.45 ms vs. 300 ms after a warm hint 6.62/10.63 ms, n=20 |
| webhook | ack p95 | 0.544 ms | ≤ 10 ms | ok | ack p50 0.308 ms, p99 0.763 ms, n=1000; median of 3 rounds (0.5123, 0.544, 0.5584) |
| cron | lag p95 (fire -> run finished) | 448 ms | ≤ 1000 ms | ok | 1000 simultaneous fires; fire -> record created p50/p95 62/86 ms |

1 target(s) missed.

### JWT verification (RS256, cached JWKS key)

| Metric | Value | Target | |
| --- | --- | ---: | --- |
| mean per verification | {"jwt_verify_rs256_mean_us": 16.92, "p50_us": 16.44, "p95_us": 18.91, "iterations": 5000} | ≤ 50.0 µs | ok |

### PID 1 drain

test-pid1-container: ok: docker stop drained graphrt (PID 1) in 1577 ms, exit 0; run run-18eadda987aa8ba5-1 paused (drained)

**Budgets missed (see FAIL rows).**
