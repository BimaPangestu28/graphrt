# Gateway protocol (`Graphrt-Protocol: 1`)

graphrt 0.5 prepares `graphrt serve` to run as an **agent** behind a graphrt
**gateway**: one always-on process that holds the run store, authenticates
clients, wakes and scales agents (0 to N replicas on Kubernetes, Cloud Run, Fly
and others), and holds the stream of every run. The agents keep only an
in-memory store and execute what the gateway sends them.

This page is the contract between the two: the run store model, the signed
`/internal/v1` API of an agent, and the SSE `id:` scheme that lets every client
reconnect without gaps. The gateway itself ships separately; an agent works
without one too, and the public `/v1` API is unchanged.

- [Run store contract](#run-store-contract)
- [Starting an agent](#starting-an-agent)
- [Signatures](#signatures)
- [Endpoints](#endpoints)
- [Streams and event ids](#streams-and-event-ids)
- [Lost connections: reclaim and orphans](#lost-connections-reclaim-and-orphans)

## Run store contract

The `RunStore` trait of `graphrt-core` (0.5.0, a breaking change) is built so a
single writer can retry any write:

| Item | Meaning |
| --- | --- |
| `RunRecord.deployment`, `deployment_version` | The deployment namespace (`d/<deployment>/…` keys in a gateway store) and its immutable version |
| `RunRecord.epoch` | 0 when created; every claim (dispatch, resume) increments it |
| `RunRecord.owner` | Replica executing the current epoch |
| `RunStatus::Queued` | Accepted by a gateway, not dispatched yet; never purged by TTL cleanup nor failed by restart recovery |
| `append_events(run_id, first_index, events)` | Appends at an index. A batch already stored (a retry after a lost reply) is accepted once; a gap or an altered retry is a `Conflict` |
| `put_if(record, expected_epoch)` | Replaces the record only while it is at that epoch (`Precondition` otherwise): a stale executor cannot overwrite a newer one |
| `claim_resume` / `claim_dispatch` | Atomic `paused → running` / `queued\|paused → running` claims at `epoch + 1` |
| `RunRecord.event_count` | Index of the next event. Keeps counting past the store's per-run cap, so indices (and SSE ids) never shift |

`RunRecord` is `#[non_exhaustive]`: build one with `RunRecord::running` or
`RunRecord::queued`.

`CoordinationStore` is a second trait: small versioned objects (idempotency
markers, leases, cron fires) written only with `create_if_absent` and
`compare_and_swap`, which map onto a SQLite transaction, a directory lock, or a
conditional PUT on S3 (`If-None-Match` / `If-Match`) and GCS
(`x-goog-if-generation-match`). The memory and directory stores implement both
traits.

Every backend runs the same abstract suite, `graphrt_core::store_conformance`
(feature `store-conformance`): create/claim exclusivity under concurrency,
fenced writes, read-after-write, prefix listing, and a property test of
retried, overlapping and lost-reply appends (every event exactly once, in
order). A new backend calls:

```rust
graphrt_core::store_conformance::run_store_suite(&store).await;
graphrt_core::store_conformance::append_retry_property(&store, 200).await;
graphrt_core::store_conformance::coordination_suite(&store).await;
```

## Starting an agent

```sh
export GRAPHRT_GATEWAY_KEYS="k2026a:$(openssl rand -hex 32)"
graphrt serve ./workflows --trust-gateway --deployment shop \
  --listen unix:/run/graphrt/shop-1.sock
```

The agent side is the cargo feature `trust-gateway` of `graphrt-cli`, on by
default (release binaries and the container image have it) and part of the
`gateway` feature, whose local agents are this same binary. It adds no crate;
on a slim agent build (`--no-default-features --features
serve,llm,plugins,trust-gateway`, `--profile min-size`) it adds about 73 KiB on
Linux musl aarch64 and 89 KiB on x86_64. The CI budget gate checks that cost
(`agent-slim-gateway` in `bench/budgets.toml`: +77 / +94 KiB).

| Flag | Effect |
| --- | --- |
| `--trust-gateway` | Serve the signed `/internal/v1` API |
| `--deployment NAME` (or `$GRAPHRT_DEPLOYMENT`) | The only deployment this agent accepts requests for; also namespaces webhook run ids (see [serving.md](serving.md#idempotency)). A `serve` flag of its own, usable without `--trust-gateway` |
| `--gateway-keys-env VAR` | Variable holding the keys (default `GRAPHRT_GATEWAY_KEYS`): `kid:hexsecret`, comma-separated, 32 bytes or more each. The first signs, all verify, so two can be active during a rotation |
| `--reclaim-grace-secs N` | How long a lost run's outcome waits to be reclaimed (default 60) |
| `--listen unix:/path` | Listen on a Unix socket (mode `0600`) instead of TCP; `--listen` is an alias of `--addr` |

`--trust-gateway` and `--verify-jwt` are exclusive: behind a gateway, the
gateway verifies end-user tokens and passes `user_id` in the run request.

## Signatures

Every `/internal/v1` request, and every report an agent sends to its gateway,
carries:

```text
Graphrt-Protocol: 1
Graphrt-Deployment: shop
Graphrt-Run-Id: run-7f3a         (requests about a run)
Graphrt-Epoch: 3                 (requests about a run)
Graphrt-Sig: v1;kid=k2026a;ts=1795400000;n=4f1c9a2e7b3d;mac=<64 hex digits>
```

```text
mac = HMAC-SHA256(key[kid], "v1|<kid>|<deployment>|<METHOD>|<path?query>|<ts>|<nonce>|<run_id>|<epoch>|<hex sha256(body)>")
```

The signature binds the deployment, the method, the path with its query, the
run execution (`run_id`, `epoch`) and the body. The nonce (8–64 chars of
`[A-Za-z0-9_-]`, unique per request) keeps two identical requests sent in the
same second (two `hello` polls, a retried abort) from looking like a replay. The agent accepts it within
±30 s of its clock and only once: an accepted signature is remembered for as
long as its timestamp stays inside the window, so a replay is refused even with
an identical body. Refusals are `401 bad_signature` (with the reason: unknown
key id, clock skew, mismatch, already used), `403 wrong_deployment` and
`400 unsupported_protocol`; every internal response carries `Graphrt-Protocol: 1`.

HMAC is computed with the `sha2` crate `serve` already links, on keyed states
prepared once per key; verifying a request with a 1 KiB body takes well under
5 µs.

The network between gateway and agents is not trusted: Fly's private network
spans the organization and a Kubernetes pod network is flat without a
NetworkPolicy. Add NetworkPolicy, an internal ingress or Flycast as
defence in depth, and never let clients set `Graphrt-*` headers through the
gateway (it strips them).

## Endpoints

| Method | Path | Purpose |
| --- | --- | --- |
| GET | `/internal/v1/hello` | `{protocol, version, deployment, workflows: [{name, fingerprint}], max_active_runs, active_runs, warm_plugins, draining}` |
| POST | `/internal/v1/runs` | Start or resume run `Graphrt-Run-Id` at `Graphrt-Epoch`; the response is the run's SSE stream |
| GET | `/internal/v1/runs/{id}/stream?from=N` | Reattach to a running execution after a lost connection (or `Last-Event-ID`); `409 finished` once it ended |
| POST | `/internal/v1/runs/{id}/abort` | Abort the execution at `Graphrt-Epoch` |
| POST | `/internal/v1/drain` | `{deadline_ms}`: the shutdown drain of `serve` (no new runs, active runs pause, preempted near the deadline); answers `202` at once |
| GET | `/internal/v1/orphans` | Lost executions held for reclaim |
| DELETE | `/internal/v1/orphans/{id}` | The gateway took that orphan over (at `Graphrt-Epoch`) |

`POST /internal/v1/runs` takes the body of a public run start (`inputs`, `sys`,
`conversation`, `metadata`, `session_id`, `user_id`, `tags`) plus:

| Field | Meaning |
| --- | --- |
| `workflow` | Served workflow name |
| `fingerprint` | Expected graph fingerprint; `409 fingerprint_mismatch` when this agent serves another version |
| `deployment_version` | Copied to the run record |
| `event_index` | Global index of this execution's first event (the gateway's event count), so ids continue the run's log |
| `resume` | `{snapshot, inputs, secrets}`: resume a paused run from the snapshot in the body; the agent never reads a store |
| `preempt_after_ms` | Optional: the agent preempts this execution after that many milliseconds (shortly before the platform's request cap); the stream ends with `run_paused` (`kind: external`, `data: "request_cap"`) and the gateway re-dispatches the run |

Epochs make every start idempotent:

- the same `run_id` and `epoch` again attaches to the running execution; once
  it finished, the request is `409 finished` (it is never executed twice). Its
  outcome reached the gateway on a stream, or is held as an orphan when every
  stream broke before the terminal event;
- an older epoch than one already seen is `409 stale_epoch`;
- a run executing at another epoch is `409 epoch_conflict`;
- while draining, starts get `503 draining`.

## Streams and event ids

Every SSE stream of `graphrt serve` now carries ids, public ones included:

- an engine event has `id: <index>`, its index in the run's event log (the
  `?from=` index of `GET /v1/runs/{id}/events`); ids keep counting across the
  executions of a run (pause, resume);
- a `response` event (assembled answer text) has `id: <index>.<k>`, where
  `<index>` is the engine event it was assembled from and `k` counts from 0;
  response events go out just before that engine event;
- synthetic events (`hello`, `lagged`) have no id.

A client that reconnects to `GET /v1/runs/{id}/events` with `Last-Event-ID: N`
gets the log from `N + 1`; with `N.k` it gets it from `N`. Browsers'
`EventSource` sends the header by itself. The gateway's reattach stream also
honours `N.k` exactly: it rebuilds the response text from the execution's first
event, so the text after the cursor is what an uninterrupted stream would have
sent.

An internal stream starts with `event: hello` and `{run_id, epoch, protocol}`,
and keeps idle connections open with an SSE comment every 15 s (the idle
timeouts of Azure Container Apps, ALB and Cloud Run ingress).

## Lost connections: reclaim and orphans

A public streaming request aborts its run when the client goes away. A run the
gateway started is not lost with its stream, which breaks when the gateway
restarts:

1. the agent waits 3 s for a reattach (`GET /internal/v1/runs/{id}/stream`);
2. it then preempts the run (in-flight nodes are cancelled and run again on
   resume: at-least-once per node) and holds the outcome, a `run_paused` event
   with the snapshot, as an orphan for the reclaim grace (60 s);
3. the gateway pulls it: after a lost stream (and after its own restart) it
   polls `GET /internal/v1/orphans` on the deployment's replicas, backing off
   from 100 ms to 2 s, and releases a taken orphan with `DELETE`. The agent
   never calls the gateway, so it needs no gateway address, no HTTP client and
   no outbound network access;
4. an orphan nobody took over is dropped at the end of the grace. What happens
   to the run is the gateway's decision: it owns the run's record, and the
   agent's store is only a cache of its executions. Without an orphan (a
   one-URL platform, or an instance that is gone) the gateway re-dispatches the
   run at the next epoch from its latest stored snapshot, else from its
   request (after a gateway restart, the copy stored without its secrets), and
   ends it `failed{owner_lost}` only when that is impossible, e.g.
   `credentials_required` for a run that needs a secret input
   (see [gateway.md](gateway.md)).

The orphan body is `{run_id, epoch, deployment, status, event_count, terminal,
answer, orphaned_ms}`; `terminal` is the last event as streamed (a
`run_paused` carries the snapshot), so the gateway can store the outcome and
resume the run elsewhere.
