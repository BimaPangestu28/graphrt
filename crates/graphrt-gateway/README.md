# graphrt-gateway

The always-on front of scale-to-zero graphrt agents (`graphrt gateway`,
feature `gateway` of `graphrt-cli`). Its Rust API is mainly for the CLI and may
change between minor releases.

User documentation: [docs/gateway.md](https://github.com/Graphrt/graphrt/blob/main/docs/gateway.md).

| Module | |
| --- | --- |
| `store` | T1 `SqliteStore` (`RunStore` + `CoordinationStore` + callback outbox), `backup`, `import` |
| `supervisor` | run lifecycle: CAS dispatch, stream pump with 32-event / 250 ms batches, pause / resume, reattach, orphans, re-dispatch, reclaim after restart |
| `pool` | activator (hold + singleflight + bounded queue), least-loaded scaler, idle release, reconciliation, prewarm (hit / miss counters) |
| `warm` | predictive arrival model, keep-warm budget, wake-storm gate |
| `server` | public API, orphan endpoint, admin API |
| `auth`, `limiter`, `quota` | API keys, JWT + JWKS, GCRA, wake quota |
| `webhook`, `outbox`, `cron`, `tz`, `scheduler` | ingress, callbacks, schedules (TZif + POSIX rules, no tz database embedded) |
| `signature`, `agent_client`, `sse` | protocol 1 (`/internal/v1`, `Graphrt-Sig`), SSE parsing |
| `stream_conn` | lean HTTP/1.1 client of the run streams (Unix socket, `http://`): one 2 KiB adaptive read buffer per stream, keep-alive reuse |
| `local`, `deploy` | local-process provider (`gw-local`), `graphrt deploy --target local` |

Tests: `tests/gateway_e2e.rs` (client → gateway → fake protocol-1 agents),
`tests/client_streams.rs` (slow client, disconnect mid-stream, 1,000 concurrent streams),
`tests/simulation.rs` (seeded simulation + invariant checker I1–I7;
`GRAPHRT_SIM_SEEDS=10000` for the nightly size), `tests/warm_simulation.rs`
(virtual-time simulation of the wake-storm gate, prewarm jitter and the
predictive prewarm's hit rate).
