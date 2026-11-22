# graphrt gateway

`graphrt gateway` is an always-on front for agents that scale to zero. It is
the only writer of the run store; agents (`graphrt serve --trust-gateway`) keep
nothing and can be stopped at any time. Every run (sync, streaming, async,
webhook, cron) is a stream the gateway holds, so a run survives the agent being
drained, preempted or killed, and survives a restart of the gateway itself.

> Status: preview (phase F1). Build with `--features gateway`. The agent side
> of the protocol (`serve --trust-gateway`, F0b) and the cloud providers (F2)
> ship separately; the local-process provider is built in.

```text
clients / webhooks / cron ──▶ graphrt gateway (SQLite store, auth, limits, wake/scale)
                                   │  HTTP/1.1, Unix socket or private network, HMAC per deployment
                                   ▼
                             agents: graphrt serve --trust-gateway  ×0..N
```

## Quick start

```sh
cargo install graphrt-cli --features gateway
graphrt gateway --dev examples/hello.yaml
curl -s localhost:8700/v1/deployments/dev/runs -d '{"inputs":{"name":"Ada"}}'
```

`--dev` keeps its store in `.graphrt/gateway-dev.db`, generates keys, accepts
requests without credentials and starts agents of the same binary on demand
(they stop after 2 idle minutes). Never use it on a network.

## Configuration

`graphrt gateway --config gateway.yaml` (or `$GRAPHRT_GATEWAY_CONFIG`). The
file holds no secret: only names of environment variables and SHA-256 digests
of API keys. `graphrt gateway check gateway.yaml` validates it.

```yaml
listen: 127.0.0.1:8700
store: /var/lib/graphrt/gateway.db      # SQLite, WAL; put it on a persistent volume
backup: { dir: /backup/graphrt, interval_secs: 300, keep: 12 }
admin_token_env: GRAPHRT_GATEWAY_ADMIN_TOKEN   # for graphrt deploy
per_ip: { per_second: 20, burst: 40 }
socket_dir: /run/graphrt                 # Unix sockets of local agents
store_ttl_days: 7                        # ≥ 3: webhook senders retry for days
deployments:
  - name: shop
    keys_env: SHOP_GATEWAY_KEYS          # kid:hex[,kid:hex]; the agents get the same value
    api_keys: [3f2a…]                    # graphrt gateway key new
    jwt: { issuer: https://auth.example, audience: shop, jwks_url: https://auth.example/.well-known/jwks.json }
    rate_limit: { per_second: 2, burst: 10 }          # per JWT subject / per API-key client
    input_headers: [{ header: authorization, input: user_jwt, strip_bearer: true }]
    pool: { max_active_runs: 8, min_replicas: 0, idle_timeout_secs: 300, queue_timeout_secs: 30 }
    quotas: { max_replicas: 4, max_queued: 256, max_wakes_per_hour: 600 }
    callback_secret_env: SHOP_CALLBACK_SECRET
    hooks:
      - { id: github, scheme: github, secret_env: GH_WEBHOOK_SECRET, idempotency_header: x-github-delivery, callback_url: https://example.com/done }
    crons:
      - { id: nightly, schedule: "0 3 * * *", timezone: Europe/Berlin, catchup: latest, jitter_secs: 120, prewarm_secs: 30, inputs: { report: daily } }
```

Keys: `graphrt gateway key new` prints a client API key and its digest;
`graphrt gateway key hmac` prints an agent key (`k1:<64 hex>`). Two agent keys
can be active at once for rotation (the first signs).

## API

| Method | Path | |
| --- | --- | --- |
| POST | `/v1/deployments/{d}/runs` | body as `graphrt serve` (`inputs`, `sys`, `conversation`, `metadata`, `session_id`, `tags`); `"stream": true` → SSE, `"async": true` → `202 {run_id}`, else the final record. `Idempotency-Key` makes retries return the same run |
| GET | `/v1/deployments/{d}/runs/{id}` | run record |
| GET | `/v1/deployments/{d}/runs/{id}/events` | SSE replay and live tail; `Last-Event-ID` or `?from=N` |
| POST | `/v1/deployments/{d}/runs/{id}/resume` | `{inputs, secrets, stream}` for a paused (human-input) run |
| POST | `/v1/deployments/{d}/runs/{id}/abort` | |
| POST | `/v1/deployments/{d}/hooks/{hook}` | webhook ingress; duplicates answer `{"duplicate": true}` |
| GET | `/metrics`, `/healthz` | Prometheus text; liveness |

Credentials: `X-Api-Key: <key>` or `Authorization: Bearer <key or JWT>`. A
verified JWT's `sub` becomes the run's `user_id`. Errors are JSON
`{error, error_type}`; `503` and `429` carry `Retry-After`; a resume of a
version retired more than 7 days ago is `410 version_retired`.

SSE ids: engine events are `id: N` (their index in the run's log, stable across
executions); response text chunks are `id: N.k`. A reconnect with
`Last-Event-ID` continues without gaps or duplicates.

## How runs survive

| Event | What the gateway does |
| --- | --- |
| Cold start | The request holds; one wake per pool (singleflight); waiters queue up to `max_queued` / `queue_timeout`, then `503 Retry-After` |
| Agent drains (SIGTERM, scale-down) | The agent pauses or preempts the run; the gateway re-dispatches it at once to another replica with the snapshot and the request's inputs, which it keeps **in memory only** |
| Stream breaks | Reattach within 3 s; otherwise the agent reports the run as an orphan (`POST /internal/v1/orphans/{id}`, HMAC) within 60 s and the gateway re-dispatches it; no report → `failed{owner_lost}` |
| Gateway restarts | Queued runs are re-dispatched, running ones are reclaimed from the agents' orphan reports. Request inputs (secrets, `user_jwt`) are not persisted, so a run that needs them stays resumable by the client with fresh credentials |
| Platform without grace (Lambda) | No preemption; a run past the platform limit fails with `platform_timeout` |

Nodes can therefore run more than once (at-least-once per node); `http-request`
and plugins receive `Idempotency-Key = run_id:epoch:node_exec`.

Webhook and cron runs have deterministic ids (`H(deployment, hook, key)`,
`H(deployment, schedule, fire_time)`); the run record is the idempotency
marker, so a duplicate delivery or a fire repeated after a crash never starts a
second run. Completion callbacks go through an outbox written in the same
transaction as the final status and are retried with backoff (signed
`X-Graphrt-Signature: t=…,v1=…`).

Cron: five fields or `@daily` etc., in any IANA zone (read from the system's
zoneinfo; install `tzdata` in containers) or a fixed offset. In a DST overlap a
fire happens once; a fire in a DST gap is skipped and logged, or with
`catchup: latest|all` runs at the end of the gap. After downtime, `catchup:
none` logs the missed fires, `latest` runs the last one, `all` runs up to
`max_catchup`. Jitter is deterministic per fire.

## Deploying versions

Versions are immutable. `graphrt deploy` registers a new one and makes it
current; the previous version keeps a pool with `min_replicas: 0` for 7 days so
its paused runs can still resume.

```sh
export GRAPHRT_GATEWAY_ADMIN_TOKEN=…
graphrt deploy --target local --deployment shop --version v7 agent.yaml -- --providers providers.yaml
```

## Store

T1 is SQLite in WAL mode (`synchronous=NORMAL`): nothing is lost on a process
crash; with `backup:` a consistent copy (`VACUUM INTO`) is written every
`interval_secs` (RPO on disk loss). Uploading backups to object storage comes
with the cloud auth modules. An existing `graphrt serve --store` directory can
be imported:

```sh
graphrt gateway import --from ./runs --store /var/lib/graphrt/gateway.db --deployment shop
```

Idle deployments cause no store writes and no provider calls besides a
reconciliation `list` at most once a minute.

On-prem high availability: two gateways on one store with an `ha:` section,
active/passive with a fenced lease (takeover in tens of milliseconds after a
crash, `stale_after_ms` after a freeze). See [gateway-ha.md](gateway-ha.md).

## Security notes

- Agent addresses come only from the provider (or a socket path the gateway
  chose), never from an agent's claim.
- Client `Graphrt-*` headers are never forwarded; input headers named
  `graphrt-*` are refused in the config.
- The JWKS URL must be `https://` (or loopback for development).
- Request inputs and tokens live only in gateway memory, never in the store,
  logs or metrics.

## Metrics

`graphrt_gateway_runs_started_total`, `…_runs_finished_total`,
`…_runs_paused_total`, `…_redispatches_total`, `…_auth_failures_total`,
`…_rate_limited_total`, `…_activation_failures_total`,
`…_webhooks_accepted_total`, `…_webhooks_duplicate_total`,
`…_cron_fires_total`, `…_cron_skipped_total`, `…_callbacks_delivered_total`,
`…_callbacks_failed_total`, `…_store_write_errors_total`,
`graphrt_gateway_client_streams`, `graphrt_gateway_live_runs`, and per
deployment version `graphrt_gateway_replicas`, `…_active_runs`, `…_queued`.
