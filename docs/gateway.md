# graphrt gateway

`graphrt gateway` is an always-on front for agents that scale to zero. It is
the only writer of the run store; agents (`graphrt serve --trust-gateway`) keep
nothing and can be stopped at any time. Every run (sync, streaming, async,
webhook, cron) is a stream the gateway holds, so a run survives the agent being
drained, preempted or killed, and survives a restart of the gateway itself.

> Status: preview. Build with `--features gateway` (local-process agents) and
> add the providers you run on: `gateway-k8s`, `gateway-cloud-run`,
> `gateway-ecs`, `gateway-lambda`, `gateway-fly`, `gateway-aca`, or `gateway-all`
> (see [Providers](#providers)).

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
| Stream breaks | Reattach within 3 s; otherwise the agent preempts the run and holds it as an orphan, the gateway polls the replicas' `GET /internal/v1/orphans` (backoff to 2 s) for up to 60 s and re-dispatches it. On a one-URL platform (Cloud Run, ACA, Lambda: no instance to pull from), or when no orphan comes (the instance is gone), the gateway re-dispatches at the next epoch from the latest stored snapshot, else from the request with the same run id: nodes may run again (at-least-once, keyed by `Idempotency-Key`). `failed{owner_lost}` only when that is impossible, with the reason: the request's credentials expired, or the executor was lost 4 times in a row |
| Gateway restarts | Queued runs are re-dispatched, running ones are reclaimed from the orphans polled from the agents, else (one-URL platforms, no orphan) re-dispatched from their latest stored snapshot. The request is stored with the run (same transaction) without its secrets: start inputs the version declares `type: secret` and header-mapped inputs (`user_jwt`) are stripped and only their names kept, so a run with neither an orphan nor a snapshot is re-dispatched from the stored request with the same run id. A run that needs a stripped secret ends `failed{owner_lost}` with `credentials_required: it needs the secret inputs …`, and the client submits it again |
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

## Providers

Each deployment runs on one provider, chosen by the **operator** in
`gateway.yaml`. Endpoints, regions, namespaces, allowed apps and credentials
live only there; what `graphrt deploy` sends the gateway is a target *name*
the provider checks against this configuration, so a deployment can never
point the gateway at another host.

```yaml
providers:
  cluster:  { kind: k8s, namespace: agents }                 # in-cluster SA, else kubeconfig (kubeconfig:, context:)
  run:      { kind: cloud_run, project: acme, region: europe-west1 }   # metadata server or GOOGLE_APPLICATION_CREDENTIALS
  tasks:    { kind: ecs, region: eu-west-1, deployments: { shop: { cluster: agents, task_definition_family: shop, subnets: [subnet-1], security_groups: [sg-1] } } }
  fn:       { kind: lambda, region: eu-west-1, functions: { faq: graphrt-faq } }      # AWS credential chain of the gateway
  machines: { kind: fly, apps: { graphrt-chat: { token_env: FLY_API_TOKEN } } }
  apps:     { kind: aca, subscription_id: 00000000-…, resource_group: agents }  # managed / workload identity, or AZURE_CLIENT_*
deployments:
  - { name: shop, provider: tasks, keys_env: SHOP_KEYS, api_keys: [...] }
  - { name: chat, provider: machines, keys_env: CHAT_KEYS, api_keys: [...] }
```

| Kind | Cargo feature (`graphrt-cli`) | Addressing | Notes |
| --- | --- | --- | --- |
| `local` | `gateway` | per replica (Unix socket) | default when a deployment names no provider |
| `k8s` | `gateway-k8s` (`gateway-k8s-in-cluster`: in-cluster service account only, no kubeconfig, no deploy driver) | per pod | scale subresource + EndpointSlices; [Kubernetes guide](kubernetes-provider.md) |
| `cloud_run` | `gateway-cloud-run` | one URL | the gateway adds `X-Serverless-Authorization` (ID token) to every request; [Cloud Run guide](gateway-cloud-run.md) |
| `ecs` | `gateway-ecs` | per task | the gateway starts and stops Fargate tasks; [AWS guide](gateway-aws.md) |
| `lambda` | `gateway-lambda` | `lambda://` | requests go through `InvokeWithResponseStream`; sync runs only, a run past the function timeout fails with `platform_timeout` |
| `fly` | `gateway-fly` | per Machine (6PN) | pre-created pool, start / suspend; [Fly guide](gateway-fly.md) |
| `aca` | `gateway-aca` | one label URL per version | Azure Container Apps on the internal ingress ([Azure guide](deploy-azure.md)); `ingress: standard` (240 s), `standard_idle_verified` or `premium` (+ `request_idle_timeout_minutes`) |

A kind whose feature is not in the build fails at start-up with a message
naming the feature. Every provider runs the C1–C13 conformance suite and the
gateway cases (drain and re-dispatch, long stream, request cap, 7-day version
pool, idle release) against fake cloud APIs in
`crates/graphrt-gateway/tests/provider_harness.rs`.

Request caps: on platforms with a cap and a grace period (Cloud Run 60
minutes, Container Apps' standard ingress 240 s) the gateway asks the agent
to preempt each execution at `cap − min(60 s, cap/4)` (3540 s, 180 s); the run
pauses with a snapshot and is re-dispatched at once, so a run longer than the
cap continues instead of being cut. On Lambda (no grace) a run past the
function timeout fails with `platform_timeout`.

Retirement: when a version's 7-day pool expires the gateway closes it (new
resumes get `410 version_retired`) and frees what the platform keeps for it:
Cloud Run traffic tags (`deploy-cloud-run`), Container Apps revisions
(`deploy-aca`, needs a write role) and Fly Machine pools (`deploy-fly`); a
failed retirement is retried every hour. Kubernetes, ECS and Lambda keep
nothing running for an old version once its pool is closed.

## Deploying versions

Versions are immutable. `graphrt deploy` registers a new one and makes it
current; the previous version keeps a pool with `min_replicas: 0` for 7 days so
its paused runs can still resume.

```sh
export GRAPHRT_GATEWAY_ADMIN_TOKEN=…
graphrt deploy --target local --deployment shop --version v7 agent.yaml -- --providers providers.yaml
```

For the other targets `graphrt deploy` applies the version on the platform
with the deploy driver of the provider (with the **deploy-time** credentials
of the machine running the CLI, never the gateway's), then registers it with
the gateway under the target name the platform returned. The provider entry
comes from the same `gateway.yaml` (`--config`, `--provider` when several of
one kind exist):

```sh
graphrt deploy --target k8s       --config gateway.yaml --deployment shop --version v8 --image ghcr.io/acme/shop:8 agent.yaml
graphrt deploy --target cloud-run --config gateway.yaml --deployment shop --version v8 --image europe-docker.pkg.dev/acme/graphrt/shop:8 \
  --secret OPENAI_API_KEY=projects/acme/secrets/openai/versions/3 agent.yaml
graphrt deploy --target ecs       --config gateway.yaml --deployment shop --version v8 --image 1234.dkr.ecr.eu-west-1.amazonaws.com/shop:8 agent.yaml
graphrt deploy --target lambda    --config gateway.yaml --deployment faq  --version v8 --image 1234.dkr.ecr.eu-west-1.amazonaws.com/faq:8 agent.yaml
graphrt deploy --target fly       --config gateway.yaml --deployment chat --version v8 --image registry.fly.io/graphrt-chat:8 --set app=graphrt-chat agent.yaml
graphrt deploy --target aca       --config gateway.yaml --deployment shop --version v8 --image acme.azurecr.io/shop:8 agent.yaml
```

`--set KEY=VALUE` passes provider settings (see each guide), `--env` plain
environment, `--secret ENV=REFERENCE` secret-store references (never values).
`graphrt deploy` also registers the start inputs `agent.yaml` declares
`type: secret` with the version: the gateway strips them from the request it
stores for a restart. A version registered without that list (an older
`graphrt deploy`, or a workflow file it cannot read) has every input treated as
secret.

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
