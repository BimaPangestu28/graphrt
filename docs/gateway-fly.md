# Fly Machines provider (`provider-fly`)

> Status: preview. `graphrt gateway` uses this provider with
> `--features gateway-fly` (`kind: fly` under `providers:`) and
> `graphrt deploy --target fly` deploys versions; both pass the conformance
> suite against a fake Machines API. See [docs/gateway.md](gateway.md#providers).
> This page describes how the Fly target behaves and how to set it up.

For a single agent served straight by the Fly proxy, without a gateway, see
[direct deploys](deploy-direct.md) (`graphrt deploy --target fly --direct`).

The `graphrt gateway` stays on all the time and wakes agents
(`graphrt serve --trust-gateway`) only when there is work. On Fly, each
deployment version is a **pool of Fly Machines** in one app:

| | |
| --- | --- |
| Crate / feature | `graphrt-provider-fly`, cargo feature `provider-fly` (`deploy` sub-feature for `graphrt deploy`) |
| Addressing | per replica, over Fly's private 6PN network: `http://[fdaa:…]:8080` |
| Scaler | the gateway (0→N, least loaded) |
| Wake | `start` of a pooled Machine: resumes a **suspended** snapshot first (target p95 ≤ 800 ms), then a stopped one (cold, target p95 ≤ 2.5 s) |
| Release | `suspend`; if Fly refuses (guest > 2 GB, swap, unsupported) it falls back to `stop` (cold) and remembers that for the Machine |
| Request / SSE limit | none |
| Grace | `stop_config.timeout` = Fly's `kill_timeout`, set by `graphrt deploy` (default and maximum 300 s) |
| API calls | Machines REST API v1: list, start, suspend, stop, wait, create, destroy, secrets. No Fly SDK |
| Credentials | gateway: one deploy token per app (start/suspend/stop/list); deploy: a token that may create apps and set secrets |

## How it works

```text
graphrt deploy --target fly
  ├─ app exists?  no → POST /v1/apps (needs `org`)
  ├─ secrets      → POST /v1/apps/{app}/secrets   (values from the deploying shell)
  └─ pool         → POST /v1/apps/{app}/machines × pool_size, skip_launch (stopped)
                     metadata graphrt_pool=1, graphrt_deployment, graphrt_version

gateway wake(d, n)                      gateway release(r)
  GET  machines?metadata.…  (1 call)      GET  machines/{id}
  POST machines/{id}/start  (per miss)    POST machines/{id}/suspend  ─refused→ POST …/stop
  GET  machines/{id}/wait?state=started
```

- **Singleflight:** concurrent wakes of one version share one pass, so 20
  simultaneous first requests start one Machine (conformance C1).
- A warm wake (enough Machines already started) costs one `GET`, with no
  mutation. A cold wake costs three calls per Machine.
- **Capacity:** when a host is full (`409 insufficient … available`, or
  `insufficient_capacity`), the provider tries the next pool Machine. When
  the pool is used up, it creates a clone of a pool member (same config, a
  new deterministic name). Fly's placer then picks another host, up to
  `max_machines_per_version`. After that, the error is `Capacity`.
- **Versions** are separate pools, selected by metadata. A suspended pool of
  version N−1 can still be woken while the gateway keeps it (7 days,
  decision 6). `FlyDeployDriver::retire_version` destroys it afterwards.
- **Rate limit:** one token bucket per gateway (20 calls/s, burst 40). Fly
  limits mutations to about 1/s per Machine and action. Retries of 429, 5xx,
  408 and 412 use jittered exponential backoff. On a 401, the token is read
  again once (rotation) and the call is retried.

## Setting it up

1. **Gateway inside the same Fly organization.** 6PN addresses are reachable
   only from the same org (or over a WireGuard peer). Run the gateway as its
   own Fly app, and point it at the internal API:
   `api_base_url = "http://_api.internal:4280"`. Outside Fly, use
   `https://api.machines.dev` plus a WireGuard tunnel for 6PN.
2. **Agents listen on IPv6.** 6PN is IPv6, so the agent's command must bind
   `[::]`, for example `serve /app/workflows --addr [::]:8080 --trust-gateway`.
   The pool has no public `services`: only the gateway can reach it. The
   gateway signs each call (HMAC per deployment). 6PN is shared by the whole
   organization, so it is not a trust boundary by itself.
3. **Tokens.**
   - Gateway (run-time): `fly tokens create deploy -a <app>`, one per app, in
     `FLY_API_TOKEN` or a mounted file. The gateway re-reads it after a 401.
   - Deploy (CLI/CI): a token that can create the app and set secrets.
4. **Secrets** are Fly app secrets. Fly injects them as environment variables
   when a Machine boots. The deployment spec carries references only:
   - `env:LOCAL_VAR`: `graphrt deploy` reads `LOCAL_VAR` from the deploying
     shell and writes it as the Fly secret named like the variable in the
     agent.
   - `fly:NAME`: the secret must already exist (`fly secrets set NAME=…`).
     Fly injects secrets under their own name, so `NAME` must equal the env
     name.

   The gateway never sees a value. The Machine config contains none either.

### Provider settings of a deployment spec

| Key | Default | Meaning |
| --- | --- | --- |
| `app` | deployment name | Fly app holding the pool |
| `org` | – | org slug; needed only to create a missing app |
| `region` | Fly default | region of the pool Machines |
| `network` | org default | custom 6PN network for a new app |
| `cpu_kind` | `shared` | `shared` or `performance` |
| `pool_size` | `max_replicas` | stopped Machines created per version (1–100) |

Resources: `cpu_millis` is rounded up to whole CPUs. `memory_mib` is rounded
up to 256 MB steps (default 256 MB). Keep guests at or below **2 GB** so
suspend works; above that every release is a cold `stop`.

## Drain and `kill_timeout`

- `graphrt deploy` sets `stop_config = {signal: SIGTERM, timeout: "<grace_secs>s"}`.
  `grace_secs` defaults to 300, which is also Fly's maximum.
- Give the agent's drain deadline at most `grace − 2 s` (for example
  `--drain-timeout 298s` with the default). Runs that do not finish in time
  are preempted and continue on another replica (decision 1, at-least-once
  per node).
- `restart.policy = "no"`: a Machine that dies on its own stays stopped. The
  gateway sees it in `list` and re-dispatches its runs (C5).
- Fly's own `init` is PID 1 in a Machine, so SIGTERM reaches `graphrt`
  directly. A plain `fly.toml` deployment without the gateway still uses
  `kill_timeout = 5 s` unless you set it. Raise it there too.
- The gateway suspends a replica only after it has drained (`active_runs = 0`).
  A suspended process is frozen with its memory. On resume, its clock jumps
  forward and connections it held are gone. That is why suspend is used only
  on idle replicas.

## Conformance

Results of `cargo test -p graphrt-provider-fly` against the fake Machines API:

| Case | Result |
| --- | --- |
| C1 20 concurrent wakes → 1 Machine | pass |
| C2 wake latency | pass (provider overhead: 3 calls; real p95 to be measured on Fly) |
| C4 scale-in releases only the victim; releasing again is `Ok` | pass |
| C5 Machine stopped outside the gateway is detected | pass |
| C8 one 401 → token reload → success; persistent 401 → `Auth`, no token in URLs or errors | pass |
| C9 429/503 retried, 400 not retried, full host → `Capacity` | pass |
| C10 no secret or token in gateway requests | pass |
| C12 release → 0 live Machines | pass (idle/orphan half needs the gateway) |
| C3, C6, C7, C11 | gateway harness (`graphrt-gateway`) |
| C13 size | measured separately, see below |

Fly-specific tests cover:

- suspended before stopped;
- the suspend → stop fallback, and remembering it;
- `SIGTERM` + `300s` on stop;
- capacity fallback and pool growth;
- version isolation, and waking an N−1 pool;
- refusing apps that are not configured;
- deploy idempotency, secrets sent only to `/secrets`, `fly:` references
  checked, and retire.

### Still to verify on real Fly (nightly, needs an org token)

- C2 on real Machines: n ≥ 50 suspend→resume and stop→start wakes in one
  region, to lock the p95 numbers above.
- Suspend refusal: the real status and body for > 2 GB guests and swap
  (assumed: a 4xx classified `Fatal` → stop fallback).
- The `metadata.<key>=` list filter (the provider re-checks metadata itself
  anyway), and the exact `/wait` semantics after `suspend`.
- `POST /v1/apps/{app}/secrets` bulk update, plus secret injection into
  Machines created through the API with `skip_launch`.
- Rate limits during a wake storm (C1 with 20+ Machines), and a 412 when
  someone else holds a lease.
- C3/C6/C7 with the gateway: drain within `kill_timeout`, and long SSE over
  6PN without buffering.
- C12: orphan Machines left behind after a `kill -9` of the gateway are
  stopped within 2 min.
- The gateway running outside Fly over WireGuard.

## Size

Measured on macOS arm64 with the `min-size` profile. Each probe binary
exercises every trait method through `dyn Provider` / `dyn DeployDriver`
(`crates/graphrt-provider-fly/examples/size_*.rs`). The baseline is what the
gateway already links: tokio, reqwest + rustls, serde_json and the shared
`ProviderHttp`.

| Build | `.text` of the crate | Δ text+const+cstring | Δ file |
| --- | --- | --- | --- |
| run-time only (`--no-default-features`, gateway) | 40.0 KiB | **77.0 KiB** | 97.6 KiB |
| with `deploy` (CLI) | 72.6 KiB | 118.6 KiB | 146.3 KiB |

The §9 budget for this feature (`gw-fly`) is **80 KiB** of sections, and the gateway-only build fits it. The `deploy` driver belongs to the CLI build and is outside this budget. Mach-O pads segments to 16 KiB pages, which explains the gap between the section and file columns.

What kept it small:
- the Machine config read from the API stays opaque JSON, borrowed when a Machine is cloned;
- a wake tracks only `(id, 6PN IP)` of booting Machines;
- the start order is computed without a generic sort;
- the per-version locks are a short `Vec`, not a map;
- the metadata filter is a fixed query string;
- the 6PN address is checked by character set instead of linking an IP parser;
- the shared `ProviderHttp` does all retry and rate limiting.

The authoritative number is the Linux musl
gateway build, once the gateway exists.
