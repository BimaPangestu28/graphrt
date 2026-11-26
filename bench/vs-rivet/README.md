# graphrt vs Rivet: same machine, same client, same model

A reproducible comparison of graphrt and [Rivet](https://github.com/rivet-dev/rivet)
(Apache-2.0) running the same chat agent, self-hosted on one machine, driven
by one load generator against one mock model. Results and conclusions are in
[REPORT.md](REPORT.md); this file describes the method.

Why: the numbers each project publishes do not compare. graphrt's cold wake is
measured to the first engine event of a local process, Rivet's cold start to
the full reply of a ping actor on a three-zone Azure cluster. Here both run the
same workload on the same hardware, with the same client and the same mock.

## Run it

```sh
bench/vs-rivet/run.sh              # 3 runs of every scenario, ~1.5–2 h
bench/vs-rivet/run.sh --quick      # fewer samples, one smoke pass
bench/vs-rivet/run.sh --only cold,warm --runs 5
```

Needs Linux x86_64, Docker usable without sudo, Node.js 24 on `PATH` (or
`NODE=/path/to/node`), cargo, curl and sqlite3. The script downloads the
graphrt release binary (checksum verified), pulls the Rivet engine image,
builds the mock, installs the npm lockfiles, runs every configuration, and
writes `results/<date>-<host>/`:

| File | Content |
| --- | --- |
| `<scenario>-<system>[-<variant>]-run<N>.json` | `meta` (load average, host), `summary`, `raw` (every sample) |
| `versions.json` | exact versions, image digest, machine |
| `summary.json`, `summary.md` | median across runs and min–max per metric (`harness/report.mjs`) |

## What runs

| | graphrt | Rivet |
| --- | --- | --- |
| Version | 0.6.2 release binary, gateway build (`graphrt-gateway-x86_64-unknown-linux-musl`, as published; mimalloc, `min-size` profile) | `rivetdev/engine:3.1.1` + `rivetkit@3.1.1` on Node.js 24 |
| Topology | `graphrt gateway` (SQLite store) starting local agents (`graphrt serve --trust-gateway`, same binary) on demand, over Unix sockets | one engine instance in Docker (config of the Docker Compose self-host guide: file-system object store, one partition) + RivetKit node process(es) connected to it |
| Agent | [`graphrt/agent.yaml`](graphrt/agent.yaml): `start → llm (memory window 10) → answer` | [`rivet/app/src/server.ts`](rivet/app/src/server.ts): the agent of the RivetKit README (SQLite history, `streamText`, tokens broadcast) plus an `onRequest` SSE route for the same turn |
| History | sent by the client with every turn (`sys.history`, last 10 turns); the gateway stores every run (request, events, answer) | the actor's SQLite database (last 20 messages read per turn) |
| "Asleep" | the agent's process is stopped (gateway idle release), the gateway stays up | the actor is asleep (`c.sleep()` or `sleepTimeout`); engine and node stay up |
| Client path | `POST /v1/deployments/{d}/runs` with `"stream": true` (SSE) | `POST /gateway/agent/request/chat?rvt-method=getOrCreate&rvt-key=…` (SSE from `onRequest`), and the README's WebSocket client (`connect()` + `sendMessage`, variant `ws`) |

Both call the same mock (`mock-openai/`, Rust): OpenAI Chat Completions,
`stream: true`, 20 deltas `tok0 ` … `tok19 `, the first delta in the same write
as the response headers (0 ms to first token), the rest 0 ms apart (50 ms in
the concurrency scenario). HTTP/1.1 keep-alive. No API key, no network.

The load generator ([`harness/`](harness/)) is one Node.js process with one
HTTP client (undici, kept-alive connections) for both systems. "First token"
is when the bytes `tok0 ` arrive, which both runtimes forward unchanged, so
the marker is the same regardless of each runtime's event format. RSS and CPU
time are read from `/proc` (`VmRSS`, `utime + stime`) for every process of a
runtime, including the engine container's processes, in a worker thread so
sampling never blocks the client.

## Scenarios

Every scenario starts each system fresh (new store, new container and
volumes, new node processes) and stops it afterwards. Percentiles are nearest
rank, no trimming, no retries (the method of Rivet's published benchmark).

1. **cold**: an agent that was used once and then put to sleep receives its
   next message; time to the first token at the client. 1,000 agents per run
   (plus 20 warm-up), in batches of 100: give each a turn, put the batch to
   sleep, wait at least 10 s (Rivet's benchmark rule), then wake each agent one
   at a time. graphrt: one deployment per agent with `idle_timeout_secs: 0`, so
   the gateway's 5 s idle release stops its process; the harness waits until no
   agent process is left. Rivet: one actor per agent, slept with `c.sleep()` as
   Rivet's own wake benchmark does; every wake sample is checked to have hit a
   freshly started actor (`X-Bench-Cold` header). The first turn of each agent
   is reported as **create** (graphrt: first process start; Rivet: actor
   creation).
2. **warm**: the agent is awake (graphrt: a warm replica, `min_replicas: 1`;
   Rivet: actors used within their 30 s `sleepTimeout`). 50 sessions round
   robin, one request at a time, 200 warm-up + 2,000 measured. The same client
   also measures the mock directly; **overhead** is the difference.
3. **memory**: 10,000 agents each get one turn (concurrency 32), then
   everything goes idle with production idle settings (Rivet `sleepTimeout`
   30 s; graphrt `idle_timeout_secs: 30`), plus 15 s. RSS summed over every
   runtime process (graphrt: gateway and agents; Rivet: engine container and
   node), before, right after the turns ("awake") and after the idle period
   ("asleep"); reported as total / N and (total − baseline) / N. Disk: the
   gateway's SQLite files; the engine's two volumes.
4. **concurrent**: 1,000 sessions, each with one earlier turn and awake, start
   a turn at the same instant; the mock streams 20 tokens 50 ms apart (~1 s).
   Before the burst the client opens one kept-alive connection per stream
   (each system's health route), so the burst measures streaming rather than
   1,000 simultaneous TCP handshakes from one process.
   Peak RSS and CPU time of the runtime during the burst, failed and stalled
   streams (a gap of more than 1 s between two tokens), first-token and
   inter-token latency. Two shapes: one graphrt agent process vs one Rivet node
   (`x1`), and four vs four (`x4`, graphrt's published SSE configuration of
   4 replicas × 250 runs; Rivet's checklist asks for at least three nodes per
   pool). The same burst straight to the mock gives the client's own floor.
5. **recovery**: `kill -9` of every runtime process (graphrt: gateway and
   agents; Rivet: `docker kill -s KILL` of the engine and `SIGKILL` of the
   node), restart, then time from the restart command to the first successful
   streamed turn of an existing agent (retried every 10 ms), and check that its
   earlier turns survived (graphrt: runs in the gateway store; Rivet: rows in
   the actor's SQLite). 5 rounds per run.

## Fairness rules and where the setups differ

- **Same everything else.** One machine, one client process, one mock; the
  systems never run at the same time; odd runs start with graphrt, even runs
  with Rivet; 3 runs, the report gives the median across runs. Before each
  measurement the harness waits until the 1-minute load average is at most 6
  (`BENCH_MAX_LOAD`, on 24 threads), because the machine is shared with other
  work; the load at the start and end of every run is in the JSON and in
  `summary.md`.
- **Production builds.** graphrt: the released binary as users download it.
  Rivet: the released image and npm package; the app is compiled with `tsc`
  and run with `node` (no `tsx`/esbuild loader, unlike Rivet's own benchmark),
  `NODE_ENV=production`, the node flags of Rivet's benchmark service
  (`--max-old-space-size=16384`), and `--ulimit nofile=1048576` for the engine.
- **Docker networking.** The engine runs with `--network host` instead of the
  guide's `-p 6420:6420`, so no docker-proxy hop sits on Rivet's measured path
  (graphrt runs natively). Container overhead is otherwise only namespaces and
  cgroups.
- **Single engine instance.** Rivet's production checklist asks for three or
  more engine instances, a bucket and one disk each; the single-instance
  Docker setup keeps one copy of the data on local volumes. That is the
  cheapest Rivet can be (no replication round trip), so latency here is a
  lower bound for a production Rivet cluster; graphrt's gateway is a single
  process with a local SQLite store in production too (HA is a separate mode).
- **Default engine tuning.** The engine runs the guide's config, not the
  overrides of Rivet's Azure benchmark (8 GiB log-shard memory, 6 GiB block
  cache, larger queues), which are sized for 64 GiB storage hosts and would
  only raise memory. One exception: the memory scenario keeps 10,000 actors
  awake at once, and the default `pegboard.sqlite_sessions` (4,000 per node
  connection) refuses the 4,001st (`500 actor stopping`), so that scenario
  uses [`rivet/rivet-10k.json`](rivet/rivet-10k.json): the guide's config plus
  `sqlite_sessions: 65536`, the value of Rivet's own benchmark.
- **graphrt agent limit.** Each agent runs with `serve --max-active-runs`
  equal to the pool's `max_active_runs`; the agent's default (256) below the
  pool limit makes the extra runs fail (see REPORT.md).
- **What "asleep" means differs.** A sleeping graphrt agent is a stopped
  process; waking it starts a process. A sleeping Rivet actor lives on a node
  process that keeps running; waking it loads its state into that process.
  The scenario measures each system's own idle mechanism, the thing a user
  pays for, not the same mechanism.
- **Where history lives differs.** graphrt keeps no conversation state between
  runs: the client sends the last turns with each request, and the gateway
  stores each run. Rivet stores the history in the actor and reads it on every
  turn. Both send the model the same prompt (system prompt, last 20 messages,
  the new message); the bytes on the wire to the runtime differ.
- **SDK language.** Rivet's 72 KB per actor figure was measured with the Rust
  RivetKit SDK; the agent here is the TypeScript one from the README, which is
  what an agent built from Rivet's examples runs.
- **Model API.** The README agent calls `openai("gpt-5")` (Responses API);
  here it calls `openai.chat("gpt-4o-mini")` (Chat Completions), the API the
  mock and graphrt's `openai` provider speak.
- **One client process.** At 1,000 concurrent streams the Node.js client
  itself adds latency; the direct-to-mock burst shows how much, and it is the
  same for both systems.

## Layout

| Path | |
| --- | --- |
| `run.sh` | the one command |
| `mock-openai/` | the mock model (standalone Rust crate) |
| `graphrt/agent.yaml` | the graphrt agent |
| `rivet/app/` | the RivetKit app (`npm run build`); `rivet/rivet.json` the engine config, `rivet/rivet-10k.json` the memory scenario's |
| `harness/bench.mjs` | one scenario × one system × one run |
| `harness/systems.mjs` | starting, addressing and stopping each system and the mock |
| `harness/client.mjs` | the shared streaming HTTP client and percentiles |
| `harness/procs.mjs` | RSS and CPU time from `/proc` |
| `harness/report.mjs` | medians across runs |
| `harness/serve-direct.mjs` | side measurement: the warm turn against `graphrt serve` without the gateway |
| `harness/cold-phases.mjs` | side measurement: graphrt's cold start split into phases |
| `results/` | raw results of the published runs |
