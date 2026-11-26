# graphrt vs Rivet: results

Measured 2026-11-26 on one machine, 3 runs per configuration, systems taking
turns. Method and fairness rules: [README.md](README.md). Raw data:
[`results/2026-11-26-bimapangestu/`](results/2026-11-26-bimapangestu/) (one JSON
per scenario, system and run with every sample; `summary.md` has every metric
with its min–max across runs).

Every number below is the **median across 3 runs**; brackets give the range
across runs where it matters. Percentiles inside a run are nearest rank over
1,000 (cold) or 2,000 (warm) samples.

## Setup

| | |
| --- | --- |
| Machine | 13th Gen Intel Core i7-13700, 24 threads, 61.6 GiB RAM, ext4 on NVMe (LVM), Linux 7.0.0-44 |
| graphrt | 0.6.2 release binary, gateway build (`graphrt-gateway-x86_64-unknown-linux-musl`, checksum verified) |
| Rivet | `rivetdev/engine:3.1.1` (`sha256:55cdfe9a…25d4`) in Docker 29.9.3, `--network host`; `rivetkit` 3.1.1 on Node.js 24.15.0; `ai` 7.0.182, `@ai-sdk/openai` 4.0.124 |
| Model | local mock, OpenAI Chat Completions, 20 tokens, first token with the headers, 0 ms apart (50 ms in *concurrent*) |
| Client | one Node.js 24 process, undici, kept-alive connections, same code for both |
| Machine load | shared with other work; 1-minute load average 4.3–6.0 at the start of every run (24 threads), recorded per run |

## Summary

| Scenario | Metric | graphrt | Rivet (HTTP) | Rivet (WebSocket client) |
| --- | --- | ---: | ---: | ---: |
| 1. Cold start (sleeping agent → first token), 1,000 samples × 3 | p50 | **7.8 ms** | 21.5 ms | 19.6 ms |
| | p95 | **12.3 ms** | 28.8 ms | 26.4 ms |
| | p99 | 47.7 ms (26–48) | 37.1 ms (29–123) | **32.1 ms** (31–51) |
| | stream end p50 / p99 | **17.2** / 53.4 ms | 26.7 / 45.6 ms | 24.4 / **39.5** ms |
| 1b. First use (new agent → first token) | p50 / p99 | **7.8** / 46.3 ms | 31.3 / 66.3 ms | 27.7 / **45.2** ms |
| 2. Warm request → first token, 2,000 samples × 3 | p50 | **1.00 ms** | 5.86 ms | 4.40 ms |
| | p99 | 24.6 ms | 13.7 ms | **7.8 ms** |
| | overhead over the mock p50 | **0.95 ms** | 5.77 ms | 4.33 ms |
| | stream end p50 | 25.6 ms | 10.6 ms | **8.5 ms** |
| 3. Memory, 10,000 sleeping agents | total RSS / N | **14.2 KiB** | 406 KiB | – |
| | (RSS − idle baseline) / N | **12.7 KiB** | 388 KiB | – |
| | total RSS, all processes | **139 MiB** | 3,965 MiB | – |
| | disk / agent | **7.9 KB** | 77.6 KB | – |
| 4. 1,000 concurrent streams, 1 process vs 1 node | peak RSS | **322 MiB** | 1,979 MiB | – |
| | CPU time / cores busy | **2.9 s / 1.6** | 10.9 s / 3.0 | – |
| | failed / stalled (gap > 1 s) | **0 / 0** | 0 (0–27) / 0 | – |
| | first token p99 | **319 ms** | 2,448 ms | – |
| | token gap p99 (mock 50 ms) | **273 ms** | 517 ms | – |
| 4b. 1,000 concurrent streams, 4 vs 4 processes | peak RSS | **377 MiB** | 2,693 MiB | – |
| | CPU time / cores busy | **3.0 s / 2.0** | 16.5 s / 6.1 | – |
| | failed / stalled | **0 / 0** | 0 (0–3) / 0 | – |
| | first token p99 | **261 ms** | 1,680 ms | – |
| | token gap p99 | **132 ms** | 190 ms | – |
| 5. Recovery after `kill -9` of every runtime process | restart → first good turn | **22 ms** | 46.4 s | – |
| | earlier turns kept | yes (3/3) | yes (3/3) | – |

Bold marks the better value. "Rivet (WebSocket client)" is the README's
client (`connect()` + `sendMessage`, tokens as broadcast events); "Rivet
(HTTP)" is the same actor's `onRequest` SSE route driven by the exact HTTP
client graphrt is driven by.

## Where graphrt wins

- **Typical latency, cold and warm.** A sleeping graphrt agent answers its
  first token in 7.8 ms (p50) against 19.6–21.5 ms for a sleeping Rivet actor,
  although graphrt starts a whole process (`graphrt serve`, ~16 MiB) and Rivet
  only loads an actor into a node that is already running. Warm, graphrt adds
  0.95 ms over the mock at p50 against 4.3–5.8 ms for Rivet.
- **Memory.** 10,000 idle agents with their history cost graphrt 139 MiB in
  total (one gateway process; agent processes are gone) against 3.9 GiB for
  Rivet (engine ≈2.7 GiB + node ≈1.1 GiB).
  Rivet's idle baseline alone (engine + empty node, 176 MiB) is larger than
  graphrt with 10,000 agents.
- **Load.** 1,000 concurrent streams: 6–7× less peak RSS, 3.7–5.4× less CPU
  time, no failed stream, and a 6–8× lower first-token p99. With one Rivet node
  the Node.js event loop is the bottleneck: tokens arrive in clumps (gap p50
  38 ms on a 50 ms stream, p99 517 ms) and one run of three refused 27 streams
  with `503 ups_overloaded` / `actor_stopped_while_waiting`; four nodes fix the
  clumping but one run still had 3 failures (`internal_error`, an aborted
  model call).
- **Recovery.** After `kill -9` of everything, graphrt serves the next turn
  22 ms after the restart command (the gateway is up in ~13 ms and starts the
  agent on demand). Rivet's processes are up after 1.4 s, but the first request
  to an existing actor waits ~45 s, consistently in all 15 rounds, before it
  succeeds: the engine apparently waits for the old node's lease to expire
  before rescheduling the actor. No data was lost on either side.

## Where graphrt loses

- **Stream completion through the gateway.** The first token is fast, but the
  end of the stream is late: warm stream end p50 25.6 ms against 8.5–10.6 ms
  for Rivet, although the mock sends all 20 tokens at once. The same turn
  against `graphrt serve` directly (no gateway, same client and mock) ends at
  p50 0.61 ms, p99 0.96 ms
  ([`side-serve-direct.json`](results/2026-11-26-bimapangestu/side-serve-direct.json),
  `harness/serve-direct.mjs`), so the gateway adds ~25 ms (up to ~40 ms) to
  the tail of every stream. The 40 ms ceiling and the fact that `graphrt serve`
  sets `TCP_NODELAY` on its listener (`crates/graphrt-cli/src/listen.rs`) while
  the gateway's public listener (`crates/graphrt-gateway/src/app.rs`, plain
  `axum::serve`) does not point to Nagle + delayed ACK; not yet confirmed with
  a patched build.
- **Tail latency.** The same effect shows up in the first token from time to
  time: warm p99 24.6 ms against 7.8–13.7 ms for Rivet, cold p99 47.7 ms
  against 32.1–37.1 ms. graphrt wins p50 and p95 everywhere, Rivet wins p99 in
  cold and warm.
- **A silent failure.** In one warm run (1 of 6,000 sequential turns) the
  gateway answered `200` with an empty SSE body: no event, no error. Rivet had
  no such response in this scenario.
- **Pool/agent limit mismatch fails silently.** The first concurrency run used
  a pool `max_active_runs` of 1,000 for one replica, above the agent's own
  `serve --max-active-runs` default of 256: 743 of 1,000 streams came back as
  `200` with an empty body instead of an error or a queued run. The published
  numbers use `--max-active-runs` equal to the pool limit (fixed in
  `harness/systems.mjs`; the first run was repeated), but a user who sets only
  the pool limit gets the same silent loss.
- **No state of its own.** graphrt keeps no conversation state: the client
  sends the history with every turn, and what the gateway stores is a log of
  runs, not something the agent reads back. A Rivet actor owns its SQLite
  database, reads its history itself and keeps any state across sleeps. The
  memory comparison counts what each system holds, which for graphrt excludes
  the history the client carries.
- **Gateway memory grows with stored runs.** 12.7 KiB of gateway RSS per stored
  session after the idle period (10,000 one-turn sessions: 15 → 139 MiB) that
  is not released while the runs are in the store.

## Observations about Rivet worth knowing

- With the guide's engine config, the 4,001st awake actor on a node fails
  (`500 actor stopping`): `pegboard.sqlite_sessions` defaults to 4,000 per node
  connection. The memory scenario (10,000 actors awake for 30 s) therefore uses
  `rivet/rivet-10k.json`, which sets it to 65,536 (the value of Rivet's own
  benchmark); every other scenario uses the guide's config unchanged.
- Most of Rivet's idle memory is the engine (≈2.7 GiB for 10,000 sleeping
  actors, 50 s after they slept), likely caches whose bounds depend on the
  engine's storage settings; the node keeps ≈1.1 GiB after its actors sleep. Rivet's published 72 KB per actor is for a running actor with the Rust
  RivetKit SDK; the TypeScript README agent here costs ≈707 KiB per awake
  actor (engine + node, over the baseline).

## How this relates to the published numbers

| Published | Here |
| --- | --- |
| graphrt: cold wake to the first event 5.3 ms p95 (local process) | cold wake to the first **model token** 12.3 ms p95 (the first engine event comes earlier; this path also runs the LLM node and the client parses SSE) |
| graphrt: 1,000 SSE streams 44.5 MiB (gateway RSS only) | 322–377 MiB peak for gateway **and** agents together |
| Rivet: 4.99 ms p99 cold start (ping actor, 3-zone Azure cluster) | 32–37 ms p99 wake to the first token of an LLM agent with SQLite history (single instance, one machine, TypeScript SDK) |
| Rivet: 72 KB per actor (running, Rust SDK) | 707 KiB per awake actor, 388 KiB per sleeping actor (TypeScript SDK, engine + node) |

None of the published numbers is wrong; they measure different things. This
report measures the same thing on both sides.

## Limitations

- **One machine, shared.** Other workloads ran on the host during the
  measurements (load average 4.3–6.0 of 24 threads at every start). The
  systems took turns and ran under similar load, but absolute tails (p99) are
  noisier than on an idle host; cold p99 varied from 26 to 48 ms (graphrt) and
  29 to 123 ms (Rivet HTTP) across runs.
- **Single-instance Rivet.** A production Rivet cluster (three engine
  instances, a bucket, writes acknowledged by two zones) adds replication to
  every durable write, so Rivet's latencies here are a lower bound. graphrt's
  gateway runs as one process with local SQLite in production too.
- **Different idle mechanisms.** graphrt's "sleeping agent" is a stopped
  process behind a running gateway; Rivet's is an actor on a running node. In
  the memory scenario, graphrt's unit is a session in one deployment (its
  agent process exits when idle), Rivet's an actor.
- **Different state models** (above): the history a graphrt client sends is
  not in graphrt's RSS.
- **TypeScript Rivet SDK, Node.js client.** The Rust RivetKit SDK would likely
  use far less memory per actor. At 1,000 streams the single Node.js client
  adds its own latency (the burst straight to the mock has a first-token p99
  of 72–76 ms).
- **Mock model.** Real providers add tens to hundreds of milliseconds before
  the first token; the differences here are the runtimes' own, not what an end
  user would see in total.
- **Rivet's kill -9 recovery** was measured with the engine restarted on the
  same volumes; a multi-instance cluster may move actors faster when only a
  node dies.

## Conclusion

For an LLM agent that is mostly asleep, graphrt is cheaper to keep and faster
to wake at the median: about 28× less memory per sleeping agent, 2.5–2.8×
faster to the first token from sleep at p50, and 6–7× less memory and
3.7–5.4× less CPU for 1,000 concurrent streams, with recovery in milliseconds instead
of ~45 s. Rivet wins the tails (p99 of cold and warm first token) and the end
of every stream, both because of what looks like one missing socket option in
the graphrt gateway, and it offers what graphrt does not: durable per-agent
state that the agent itself owns. The two graphrt silent failures (an empty
`200` once in 6,000 turns, and empty `200`s when the pool limit exceeds the
agent's) are bugs to fix before these numbers are used anywhere.
