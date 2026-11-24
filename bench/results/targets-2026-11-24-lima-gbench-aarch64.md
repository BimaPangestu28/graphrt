### graphrt budgets (aarch64-linux (host), min-size)

| Build | Features | Binary (MiB) | Budget | Idle RSS (MiB) | Budget | exec→/healthz p50 / p95 (ms) |
| --- | --- | ---: | ---: | ---: | ---: | ---: |
| agent-minimal | `serve` | 3.609 | 3.85 | 3.11 | 5.0 | – |
| agent-slim | `serve,llm,plugins` | 6.028 | 6.35 | 4.81 | 6.0 | 1.67 / 4.31 |
| agent-slim-gateway | `serve,llm,plugins,trust-gateway` | 6.099 | +72.1 KiB over agent-slim (budget +77) | 4.86 | – | – |
| gateway-core | `gateway` | 6.802 | 6.95 | 6.6 | 20.0 | – |
| gateway-all | `gateway-all` | 7.587 | 7.75 | – | – | – |

All budgets met.

### graphrt gateway targets (aarch64, 4 CPUs, Linux 7.0.0-15-generic)

| Scenario | Metric | Value | Target | | Details |
| --- | --- | ---: | ---: | --- | --- |
| ttfb-uds | overhead p50 | 0.2354 ms | ≤ 0.3 ms | ok | gateway p50/p95 0.433/0.487 ms, direct 0.198/0.239 ms, n=1000; median of 3 rounds (0.2348/0.2447, 0.2354/0.2475, 0.2375/0.2354) |
| ttfb-uds | overhead p95 | 0.2475 ms | ≤ 1 ms | ok | gateway p50/p95 0.433/0.487 ms, direct 0.198/0.239 ms, n=1000; median of 3 rounds (0.2348/0.2447, 0.2354/0.2475, 0.2375/0.2354) |
| ttfb-tcp | overhead p95 (budget + 1 RTT) | 0.2469 ms | ≤ 1.0138 ms | ok | gateway p50/p95 0.431/0.509 ms, direct 0.214/0.262 ms, n=1000; median of 3 rounds (0.2231, 0.2469, 0.5264) |
| sse | gateway peak RSS | 74.2 MiB | ≤ 80 MiB | ok | 1000 streams x 20 chunks, RSS idle 7.71 MiB, max chunk gap 524.8 ms; median of 3 rounds (73.65/73.65/0/0, 74.2/74.2/0/0, 76.44/76.44/0/0) |
| sse | gateway peak RSS (audit target, not gated) | 74.2 MiB | ≤ 64 MiB | not met | 1000 streams x 20 chunks, RSS idle 7.71 MiB, max chunk gap 524.8 ms; median of 3 rounds (73.65/73.65/0/0, 74.2/74.2/0/0, 76.44/76.44/0/0) |
| sse | failed streams | 0  | ≤ 0  | ok | 1000 streams x 20 chunks, RSS idle 7.71 MiB, max chunk gap 524.8 ms; median of 3 rounds (73.65/73.65/0/0, 74.2/74.2/0/0, 76.44/76.44/0/0) |
| sse | stalled streams | 0  | ≤ 0  | ok | 1000 streams x 20 chunks, RSS idle 7.71 MiB, max chunk gap 524.8 ms; median of 3 rounds (73.65/73.65/0/0, 74.2/74.2/0/0, 76.44/76.44/0/0) |
| wake | p95 to first event | 6.6639 ms | ≤ 15 ms | ok | first event p50/p95 5.59/6.66 ms, run finished p50/p95 5.72/6.92 ms, n=100; median of 3 rounds (6.5246, 6.6639, 6.8021) |
| wake-node | p95 to plugin result | 59.6784 ms | ≤ 60 ms | ok | first event p50/p95 5.95/9.41 ms, run finished p50/p95 56.36/59.68 ms, n=100; the plugin alone (spawn -> initialize, no graphrt) p50/p95 50.7/51.6 ms; median of 3 rounds (57.8163, 59.6784, 74.0049) |
| wake-wasm | p95 to plugin result (reference, not gated) | 15.0902 ms | ≤ 60 ms | ok | first event p50/p95 11.79/13.92 ms, run finished p50/p95 13.40/15.09 ms, n=100; median of 3 rounds (14.8823, 15.0902, 15.7018) |
| plugin-pool | p95 run after a warm hint | 10.4622 ms | ≤ 15 ms | ok | RSS agent + plugins, pool 0/1/2: 73.52/73.48/141.0 MiB, agent alone after the idle stop 5.89 MiB; run p50/p95 warm 1.71/3.94 ms, after the idle stop cold 87.33/96.17 ms vs. 300 ms after a warm hint 6.34/10.46 ms, n=20 |
| plugin-pool | p95 run on a warm pool | 3.9411 ms | ≤ 15 ms | ok | RSS agent + plugins, pool 0/1/2: 73.52/73.48/141.0 MiB, agent alone after the idle stop 5.89 MiB; run p50/p95 warm 1.71/3.94 ms, after the idle stop cold 87.33/96.17 ms vs. 300 ms after a warm hint 6.34/10.46 ms, n=20 |
| webhook | ack p95 | 0.5867 ms | ≤ 10 ms | ok | ack p50 0.361 ms, p99 0.888 ms, n=1000; median of 3 rounds (0.5752, 0.5867, 0.6405) |
| cron | lag p95 (fire -> run finished) | 420 ms | ≤ 1000 ms | ok | 1000 simultaneous fires; fire -> record created p50/p95 65/90 ms |

All targets met.

### JWT verification (RS256, cached JWKS key)

| Metric | Value | Target | |
| --- | --- | ---: | --- |
| mean per verification | {"jwt_verify_rs256_mean_us": 16.56, "p50_us": 16.15, "p95_us": 17.03, "iterations": 5000} | ≤ 50.0 µs | ok |

### PID 1 drain

test-pid1-container: ok: docker stop drained graphrt (PID 1) in 1589 ms, exit 0; run run-18ead32f54cd3ca0-1 paused (drained)

**All budgets met.**
