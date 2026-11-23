# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses
[Semantic Versioning](https://semver.org/spec/v2.0.0.html). Until 1.0, minor
releases may contain breaking changes; they are called out under **Changed**.

## [Unreleased]

### Added
- `bench/targets.sh` checks every measurable target of the gateway plan on
  Linux against `bench/budgets.toml` and exits 1 on a miss: sizes and idle RSS
  of the agent and gateway builds (`budget-gate.py` now runs `graphrt gateway`
  too), gateway TTFB overhead over a Unix socket and loopback TCP, 1,000
  concurrent SSE streams, cold wake with and without a Node.js plugin, webhook
  ack, cron lag (`bench/gateway/gwbench.py`), an RS256 JWT microbench and the
  SIGTERM drain of graphrt as PID 1 of a real container
  (`scripts/test-pid1-container.sh`). Manual CI workflow **Bench targets**.
- `graphrt serve --trust-gateway` starts process and HTTP plugins with the
  agent instead of at their first node (`PluginHost::prewarm`): a cold wake
  with a Node.js plugin no longer waits for the host after the dispatch.

### Fixed
- `release.yml` no longer tries to build the gateway archive for Windows: the
  gateway is Unix-only and does not compile there, so a tag build would have
  failed. Windows gets the agent archive only, as in the 0.5.0 release.
- `graphrt gateway`: warm first-byte overhead down from ~1.5 ms to ~0.25 ms
  (p50, Linux aarch64). Each client stream's first frame waited up to 1 ms
  for the heartbeat timer; a fresh run's stream read the store before its
  first frame; `start_run` made two store round trips it does not need
  (a duplicate read for a random id, a separate dispatch claim for an
  interactive run); SQLite statements were parsed again on every call.
- `graphrt gateway` started one blocking thread per store operation waiting
  for the SQLite connection (176 threads, 180 MiB RSS with 200 streams);
  operations now queue for it. With mimalloc built without transparent huge
  pages, a 32-frame broadcast per live run (a lagging stream catches up from
  the ring) and a smaller supervisor future, the gateway idles at 7 MiB
  instead of 20 MiB and holds 1,000 streams in ~76 MiB.
- `graphrt gateway`: a version's `min_replicas` prewarm that raced its first
  cold request was dropped as "in flight" and the pool stayed at one replica;
  background prewarms (`min_replicas`, cron, predictive) now wait for the
  in-flight wake and top up.

## [0.5.0] - 2026-11-23

Scale to zero, everywhere. `graphrt gateway` (preview) is an always-on front of
scale-to-zero agents and the only writer of the run store: it holds every run's
stream, wakes agents on demand and re-dispatches runs after drains, lost
replicas and its own restarts, with on-prem active/passive HA, warm-up
(`POST /v1/deployments/{d}/warm`, predictive prewarm) and cost guards.
Providers for local processes, Kubernetes, Cloud Run, ECS Fargate, Lambda, Fly
and Azure Container Apps, and `graphrt deploy --target
local|k8s|cloud-run|ecs|lambda|fly|aca` ships immutable versions. The cloud
providers are tested against API fakes (conformance suite); real-cloud
verification is pending. Releases now ship a second build per platform,
`graphrt-gateway-<target>`, and a `:gateway` image; the agent build stays as
slim as before. The gateway core is 7.91 MiB (x86_64 musl, `min-size`), with
every provider 8.89 MiB. `graphrt serve --trust-gateway` is the agent side of the
gateway protocol, plus graceful drain, `/readyz` and Unix sockets. Contains
breaking changes (see **Changed (Breaking)**).

### Changed (Breaking)
- `gateway.yaml` values are typed as YAML resolves them: a string setting given
  an unquoted number (`name: 2024`) is refused with its path and line
  (`deployments[0].name (line 3, column 11): invalid type: integer `2024`,
  expected a string`); quote it. Errors in the file now name the setting's path
  and line.
- `graphrt serve`: a bad query parameter is answered
  `400 Failed to deserialize query string: <field>: <reason>`; request bodies
  keep axum's statuses and texts (415, 400, 422 with the field path).
- `graphrt serve --verify-jwt` refuses to start without an API key on every
  address, loopback included. A reverse proxy on the same host forwards the
  internet to a loopback port (and one that rewrites `Host` also passes the
  DNS-rebinding check), which made run lists, events and resume of every user
  public. **Migration:** set `GRAPHRT_API_KEY` (or `--api-key-env VAR`) next to
  `--verify-jwt`; user tokens keep working without it on the client side.
- `graphrt serve --verify-jwt`: a run started with a user token streams only
  `response` (answer) events, `run_started` and the terminal event, and its
  blocking result only `run_id`, `status`, `outputs`, `answer`, `usage`,
  `reasons`, `error_type`, `reason` and `elapsed_ms`. Node events (inputs,
  outputs, tool results, retrieved documents, rendered prompts when capture
  is on), `run_failed`'s error text and the pause snapshot are no longer sent
  to end users. API-key callers are unchanged; resume and run reads still take
  only the API key. **Migration:** clients that read `node_chunk` should read
  `response` events instead; to keep the old behaviour for trusted clients,
  pass `--user-events all`.
- OpenTelemetry presets (`--otel-preset`, `GRAPHRT_OTEL_PRESET`) refuse to
  start when the preset's credential header (`Authorization`, `x-api-key`)
  would go to a host other than the preset's, because an explicit
  `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` or a pushed
  `OTEL_EXPORTER_OTLP_METRICS_ENDPOINT` points elsewhere, or over plain
  `http://` to a non-loopback host. `--langfuse-scores` refuses plain `http://`
  to a non-loopback host too. **Migration:** point the preset at its backend
  with its base URL variable (`LANGFUSE_BASE_URL`, `PHOENIX_COLLECTOR_ENDPOINT`,
  ...), use `https://` for a self-hosted backend on another machine, or set
  the credentials yourself in `OTEL_EXPORTER_OTLP_HEADERS` when exporting
  through a collector.
- `POST /v1/runs/{id}/feedback` with a user token (`--verify-jwt`) accepts
  only runs started with a token of the same user. Run records gain
  `user_verified` (`true` when `user_id` is a verified `sub`, kept on resume,
  `false` for records written before this release). A run started with the
  API key and a body `user_id` is no longer rateable by that user's token: the
  body value is an unverified label, so a key holder could make any run
  rateable by anyone. **Migration:** let end users start the runs they rate
  with their own token; API-key callers can still post feedback for any run,
  with `user` in the body.

### Changed
- `graphrt-gateway`, `graphrt-provider-api`, `graphrt-provider-k8s`,
  `graphrt-provider-aws`, `graphrt-provider-azure`, `graphrt-provider-fly` and
  `graphrt-gcp` are published on crates.io (they were internal), so
  `cargo install graphrt-cli --features gateway-all` works from crates.io.
  Their Rust APIs are young and may change between minor releases.
- Smaller binaries from less generic code, no feature removed. YAML is parsed
  by one function (`graphrt_core::yaml::to_value`) and every document then
  deserialized from a `serde_json::Value`; node configs and HTTP request bodies
  no longer go through `serde_path_to_error` (the offending path is recovered
  from the error, only when there is one: `graphrt_core::value_path`); the
  stores' blocking jobs and the background tasks share one tokio task type
  (`graphrt_core::task`); sorts with unique keys are unstable sorts. Gateway
  core (`min-size`): x86_64 musl 9,042,760 → 8,291,784 B (8.62 → 7.91 MiB,
  −733 KiB, now within the plan's 8 MiB), aarch64 musl 7,668,672 →
  7,112,896 B (−543 KiB), macOS arm64 6,758,768 → 6,259,824 B (−487 KiB);
  `gateway-all` x86_64 10,077,000 → 9,324,616 B (8.89 MiB, within 9 MiB),
  aarch64 −545 KiB, macOS −471 KiB. `bench/budgets.toml` holds x86_64 to
  8 / 9 MiB.
- Smaller binaries wherever SQLite or C code is linked: builds from this
  repository (CI, release binaries, the container image) compile the bundled
  SQLite without full-text search, R*Tree, STAT4, dbstat, soundex, column
  metadata and extension loading, which graphrt never used, plus SQLite's
  recommended `OMIT_DEPRECATED` / `OMIT_SHARED_CACHE`, and the C dependencies
  of the static musl builds without unwind tables (`.cargo/config.toml`).
  `base64` and `sha2` are no longer linked in two versions. Gateway core
  (`min-size`): x86_64 musl 9,503,416 → 9,040,840 B (−451.7 KiB), aarch64
  musl 8,124,200 → 7,667,392 B (−446.1 KiB), macOS arm64 7,061,136 →
  6,758,752 B (−295.3 KiB); `gateway-all` −461.1 / −449.2 / −311.5 KiB.
  `bench/budgets.toml` now gates `gateway-core` and `gateway-all` per
  architecture. `cargo install graphrt-cli` from crates.io still builds SQLite
  with libsqlite3-sys's defaults.
- The `trust-gateway` feature is about 40 % smaller (slim agent, `min-size`:
  +73 KiB instead of +126 KiB on Linux musl aarch64, +89 instead of +150 KiB
  on x86_64), with a budget in the CI gate (`agent-slim-gateway`, measured +
  about 5 %; `budget-gate.py` gained `delta_of` / `delta_kib`). Agents no
  longer report orphans (`--gateway-url` is gone): the gateway pulls them with
  `GET /internal/v1/orphans` after a lost stream as after a restart, so an agent
  never calls the gateway. One handler routes `/internal/v1` and boxes each endpoint's
  future, background tasks share one boxed task type, the gateway stream uses
  the public stream's type (its `hello` comes from the guard), request bodies
  are read from a JSON tree with the snapshot decoder of the run store, and
  responses are written as text. Behavior changes on the agent side of the
  (unreleased) gateway protocol: a start or reattach for an execution that
  already finished is `409 finished` instead of a replay of its stored events
  (its outcome reached the gateway on a stream or is held as an orphan), and
  the agent no longer rewrites an unreclaimed run as `failed{owner_lost}` in
  its own store (the gateway owns that decision). Refusals carry short, static
  messages.
- **Breaking (`graphrt-core` store API, 0.5.0).** `RunRecord` is
  `#[non_exhaustive]` (build it with `RunRecord::running` or the new
  `RunRecord::queued`) and gains `deployment`, `deployment_version`, `epoch`
  and `owner`; `RunStatus` gains `Queued`. `RunStore::append_events` takes the
  index of the batch's first event (`append_events(run_id, first_index,
  &events) -> event_count`) and is idempotent per index range: a retried batch
  is stored once, a gap or an altered retry is a `Conflict`. New
  `RunStore::put_if` (a write fenced by the record's epoch, `StoreError::
  Precondition` otherwise) and `RunStore::claim_dispatch` (queued/paused →
  running at `epoch + 1`); `claim_resume` increments the epoch too.
  `event_count` is now the index of the next event and keeps counting past
  the store's per-run cap. `graphrt serve` writes final states with `put_if`,
  and an event batch whose write failed goes out again at the same index with
  the next one instead of being lost. See
  [docs/gateway-protocol.md](docs/gateway-protocol.md#run-store-contract).
- Every SSE event of `graphrt serve` has an `id:`: the event's index in the
  run's log (continuing across resumes), `<index>.<k>` for `response` text;
  `lagged` events have none.
- `graphrt serve` writes a run's events to the store every 32 events **or**
  250 ms after the first unwritten one (before: only every 32 events), so
  replays trail live runs by at most a quarter second. Store writes failing
  with an I/O error are retried five times with backoff (about 0.8 s); a
  paused run whose snapshot cannot be stored gets `error_type:
  store_unavailable` instead of only a log line.
- A streaming caller or `/events` listener that falls behind a fast run is
  refilled from the run's in-memory feed instead of silently skipping events
  (streaming starts skipped them before); only events beyond the feed's first
  10,000 are reported with a `lagged` event.
- The example Fly config sets `kill_signal` and `kill_timeout`.
- The `eval` and `online-eval` cargo features of `graphrt-cli` are no longer
  on by default (`online-eval` needs `eval`), so `cargo install graphrt-cli`
  and `docker build .` are smaller. Enable them with
  `--features eval,online-eval` (Docker: `--build-arg
  EXTRA_FEATURES=eval,online-eval`). The release binaries, Homebrew formula
  and GHCR image are built with both, as in 0.4.0. `jwt`, `scores` and
  `langfuse` stay on by default.

### Added
- Release assets: every platform gets a second archive,
  `graphrt-gateway-<target>` (features `gateway-all,eval,online-eval`: the same
  CLI plus `graphrt gateway` and `graphrt deploy` with every provider; it links
  mimalloc, the agent archive `graphrt-<target>` does not), with its checksum in
  `SHA256SUMS` and the same smoke test plus `gateway --help` / `deploy --help`.
  The container image gains `ghcr.io/bimapangestu28/graphrt:<version>-gateway`
  (also `<major.minor>-gateway` and `gateway`). `install.sh --gateway`
  installs the gateway build. `release.yml` builds both.
- `graphrt gateway` warm-up and cost guards (plan F4), all off or bounded by
  default ([docs](docs/gateway.md#warm-up-and-cost-guards)):
  `POST /v1/deployments/{d}/warm` prewarms the current version ahead of
  traffic; `warm.predictive` learns per-deployment arrival patterns (288
  five-minute slots, weekdays and weekends apart, 2.3 KiB) and prewarms
  `lead_secs` before expected traffic (hit rate 81–84 % in the seeded
  simulation, target ≥ 70 %); `warm.max_warm_replica_secs_per_day` caps the
  idle replica-seconds kept warm per UTC day (prewarms refused, idle replicas
  released at once; runs still wake on demand); `wake_storm.max_parallel_wakes`
  (default 16) bounds provider wakes in flight gateway-wide and
  `prewarm_jitter_ms` (default 1000) spreads prewarms. Cron prewarms are
  jittered and no longer delay the fire. New metrics:
  `graphrt_gateway_prewarm_{started,hits,misses}_total{source}`,
  `…_warm_replica_seconds`, `…_warm_budget_replica_seconds`,
  `…_wakes_in_flight`, `…_wakes_gated_total`. Binary (`min-size`,
  `--features gateway`): +60.8 KiB x86_64 musl (9,441,208 → 9,503,416 B),
  +48.2 KiB aarch64 musl (8,074,536 → 8,123,944 B), +48.6 KiB macOS arm64
  (7,011,360 → 7,061,136 B).
- `graphrt gateway` preempts runs shortly before a platform's request cap
  (Cloud Run 60 min, Container Apps standard ingress 240 s) and re-dispatches
  them, so long runs continue across the cap: the gateway sends
  `preempt_after_ms` and `serve --trust-gateway` preempts the execution itself.
  Versions past the 7-day pool are retired on the platform too (Cloud Run
  traffic tags, Container Apps revisions, Fly Machine pools), with retry.
- The `kubeconfig` feature of `graphrt-provider-k8s` reads kubeconfig files
  without per-struct deserializers, so `gw-k8s-kubeconfig` is smaller.
- `graphrt gateway` runs deployments on Kubernetes, Cloud Run, ECS Fargate,
  Lambda, Fly and Azure Container Apps, besides local processes: cargo
  features `gateway-k8s` (`gateway-k8s-in-cluster` without kubeconfig),
  `gateway-cloud-run`, `gateway-ecs`, `gateway-lambda`, `gateway-fly`,
  `gateway-aca` and `gateway-all` of `graphrt-cli` (`gw-*` / `deploy-*` of
  `graphrt-gateway`).
  Providers are named under `providers:` in `gateway.yaml` and picked per
  deployment (`provider:`); endpoints and credentials come only from that
  operator configuration. Cloud Run requests carry the invoker ID token
  (`X-Serverless-Authorization`); Lambda runs go through
  `InvokeWithResponseStream`. `graphrt deploy --target
  k8s|cloud-run|ecs|lambda|fly|aca` applies a version with the provider's deploy
  driver and registers it with the gateway. See
  [docs/gateway.md](docs/gateway.md#providers).
- `Idempotency-Key` for side effects, since execution is at-least-once per
  node (retries, and the re-run of an in-flight node when a preempted run
  resumes). `http-request` sends `Idempotency-Key: <run_id>:<exec_id>` on
  POST, PUT, PATCH and DELETE: the same for every attempt and re-execution of
  a node execution, different for any other; a workflow-set header wins and
  `idempotency_key: false` turns it off. Plugins receive the key as
  `idempotency_key` in `node.run` (process and WebAssembly), exposed by the
  Rust, Python, TypeScript and WebAssembly guest SDKs.
  `NodeContext::idempotency_key()` in `graphrt-core` builds it. See
  [docs/workflow-format.md](docs/workflow-format.md#http-request).
- `graphrt serve --deployment NAME` (or `$GRAPHRT_DEPLOYMENT`), now a flag of
  `serve` itself (it was part of `--trust-gateway`): webhook idempotency run
  ids include the deployment, derived exactly as `graphrt gateway` derives them
  (`hook-` + first 32 hex digits of `sha256("hook|<deployment>|<hook>|<key>")`),
  so servers of different deployments can share a store and a delivery maps to
  one run whichever of the two receives it. Without it, ids are derived as
  before. See [docs/serving.md](docs/serving.md#idempotency).
- `graphrt-provider-fly` (feature `provider-fly`): the Fly
  Machines provider of the upcoming `graphrt gateway`. A deployment version is
  a pool of pre-created Machines, woken with `start` (suspended snapshots
  first), released with `suspend` falling back to `stop`, addressed over 6PN.
  Its `deploy` driver (`graphrt deploy --target fly`) creates the app, writes
  Fly app secrets from `env:`/`fly:` references and creates the stopped pool
  with `kill_timeout` = `grace_secs` (≤ 300 s). Hand-rolled Machines REST
  client, no Fly SDK; passes the provider conformance suite against a fake
  Machines API. Not wired into a binary yet. See
  [docs/gateway-fly.md](docs/gateway-fly.md).
- `graphrt-provider-api` crate: the `Provider` / `DeployDriver`
  traits, capabilities and error kinds of the upcoming `graphrt gateway`, the
  shared provider HTTP layer (token bucket per provider API, jittered retry,
  token cache with one refresh on 401) and the C1–C13 provider conformance
  harness with a scripted fake HTTP server; `Provider::invoke` lets a
  platform without an HTTP address (Lambda) carry the gateway's requests, and
  `max_grace_secs = 0` marks a platform where no preemption is possible;
  `ProviderErrorKind` is `#[non_exhaustive]` and has a `PlatformTimeout` kind
  for a run cut at such a platform's time cap (Lambda), recorded as
  `failed{platform_timeout}` and never retried. No user-visible change yet.
- `graphrt-gcp` crate (gateway plan F2, GCP family), each part behind its own
  cargo feature and without a Google SDK. `auth-gcp`: access and ID tokens from
  the metadata server or a service-account key (RS256 JWT signed with `ring`,
  exchanged at the OAuth 2.0 token endpoint), cached until `exp - 5 min`, one
  shared fetch per token, one refresh after a 401, ±5 min clock skew corrected
  from the token endpoint's `Date`, never logged. `provider-cloud-run`: the
  Cloud Run provider (`Addressing::Url`, platform-scaled, min instances 0,
  startup CPU boost, request-based billing, 10 s grace → preempt, 60-min cap →
  planned preempt); `deploy-cloud-run`: its deploy driver over the Cloud Run
  Admin API v2 (one traffic tag per version so old versions stay wakeable
  until retired) and Secret Manager references. Conformance C1–C12 against fake Google APIs; see
  [docs/gateway-cloud-run.md](docs/gateway-cloud-run.md) for IAM roles and what
  still needs a real project.
- `graphrt-provider-k8s` crate: the Kubernetes provider of the
  upcoming gateway, for self-hosted clusters, kind/k3s, GKE, EKS and AKS.
  Scales a version's Deployment 0↔N through the `scale` subresource
  (compare-and-swap, singleflight per version), picks the released replica
  as the scale-in victim with `pod-deletion-cost`, tracks readiness from
  EndpointSlice watches, and server-side applies Deployment + Service with
  Secret references only. Cluster auth: in-cluster service account, kubeconfig
  token / token file / client certificate / exec plugins, and a `TokenSource`
  hook for EKS, GKE and AKS tokens. Minimal REST on the shared provider HTTP
  layer, no `kube` / `k8s-openapi`. Passes the shared C1–C13 harness against an
  emulated API server and an opt-in kind test. See
  [docs/kubernetes-provider.md](docs/kubernetes-provider.md).
- `GET /v1/runs/{id}/events` honours `Last-Event-ID` (`N` resumes after event
  `N`, `N.k` at event `N`), so reconnecting clients get the rest of the run
  without gaps or duplicates.
- `graphrt serve --trust-gateway --deployment NAME`: the agent side of the
  gateway protocol (`Graphrt-Protocol: 1`). Signed (`Graphrt-Sig`,
  HMAC-SHA256 over method, path, deployment, run id, epoch, a per-request
  nonce and the body hash,
  key ids from `$GRAPHRT_GATEWAY_KEYS` for rotation, ±30 s window, replays
  refused) `/internal/v1` routes: `hello`, `runs` (start, or resume with the
  snapshot in the body; same epoch attaches, never re-executes),
  `runs/{id}/stream` (reattach to a running execution from any
  `Last-Event-ID`, response text included), `runs/{id}/abort`, `drain`, and `orphans`. A gateway-started run
  whose stream breaks is preempted after 3 s without a reattach and held for
  `--reclaim-grace-secs` (60) as an orphan that the gateway pulls
  (`GET /internal/v1/orphans`), instead of being aborted. Cargo
  feature `trust-gateway` (default; no new dependency). See [docs/gateway-protocol.md](docs/gateway-protocol.md).
- `graphrt serve --listen unix:/path` (alias of `--addr`): listen on a Unix
  socket created mode `0600`.
- `CoordinationStore` in `graphrt-core` (versioned objects written with
  `create_if_absent` / `compare_and_swap`), implemented by the memory and
  directory stores, and the abstract store conformance suite
  `graphrt_core::store_conformance` (feature `store-conformance`), including
  a property test of retried, overlapping and lost-reply appends.
- Graceful shutdown for `graphrt serve`: SIGTERM and SIGINT start a drain that
  refuses new runs (`503` + `Retry-After`, `GET /readyz` turns `503
  draining`), pauses every active run (async, streaming and blocking),
  preempts the ones still busy shortly before `--drain-timeout` (default
  `4s`), stores them and exits `0`. Drained runs end `paused` with reason kind
  `drained`, never `failed`; async and webhook runs are resumed by the next
  server on the same store (`--no-resume-drained` to opt out). Before, a
  SIGTERM was ignored when graphrt ran as PID 1 of a container (the image has
  no init) and every active run was lost to the SIGKILL. See
  [docs/serving.md](docs/serving.md#shutdown-and-restarts) and the grace
  periods per platform in [docs/deploy.md](docs/deploy.md#graceful-shutdown).
- `RunHandle::preempt` / `preempt_with` and `RunHandle::pause_with` in
  `graphrt-core`: a preempt cancels in-flight nodes and pauses at once; a
  cancelled node is saved as pending with its `exec_id` and runs again on
  resume (at-least-once per node), and container bodies resume at the item
  that was running.
- `GET /readyz` readiness endpoint.
- CI gate `scripts/budget-gate.py` with budgets in `bench/budgets.toml`: binary
  size and idle RSS of the `serve` and `serve,llm,plugins` builds,
  exec→`/healthz` p95, and banned crates (cloud SDKs, Kubernetes clients, `h2`),
  run as the `budgets` job for Linux musl x86_64 and aarch64. Baseline in
  `bench/baseline/`.
- `graphrt-provider-aws` crate: AWS targets of the upcoming
  gateway without the AWS SDK. `auth-aws` is a hand-rolled SigV4 signer
  (header and presigned), the credential chain env → container endpoint (ECS
  task role, EKS Pod Identity) → STS web identity (IRSA) → IMDSv2 with caching,
  refresh-once on 401/403 and clock-skew correction, and presigned EKS tokens.
  `provider-ecs` runs one ECS Fargate task per replica (`RunTask` / `StopTask`
  / `DescribeTasks` / `ListTasks`, `stopTimeout` ≤ 120 s, for async and cron
  runs). `provider-lambda` runs short synchronous runs (≤ 14 min) through
  `InvokeWithResponseStream` and Lambda Web Adapter, detects buffered replies
  and ends over-long runs with an explicit `platform_timeout`. App Runner is
  not supported (closed to new customers). The `deploy-ecs` / `deploy-lambda`
  features add the `DeployDriver`s of `graphrt deploy`: an ECS task definition
  revision per version (secrets as Secrets Manager / SSM `valueFrom`
  references, `stopTimeout` ≤ 120 s), and a Lambda container-image function
  created or updated, published, aliased by version, with Lambda Web Adapter
  streaming settings and an IAM Function URL in `RESPONSE_STREAM` mode. See
  [docs/gateway-aws.md](docs/gateway-aws.md). No user-visible change yet.
- `graphrt gateway` (preview, opt-in `--features gateway`, crate
  `graphrt-gateway`): an always-on front of scale-to-zero agents and the only
  writer of the run store. SQLite (WAL) store with in-process `VACUUM INTO`
  backups and import of `graphrt serve --store` directories; per-deployment
  API keys and JWT `iss`/`aud` with a JWKS cache; per-IP and per-user GCRA
  limits; activator (hold + singleflight wake + bounded, age-limited queue),
  least-loaded scaler 0→N, idle release and reconciliation; quotas
  (`max_replicas`, `max_queued`, `max_wakes_per_hour`); webhooks whose run
  record is the idempotency marker, with a completion-callback outbox; cron
  with time zones, an explicit DST policy, catch-up, deterministic jitter and
  prewarm; immutable versions with a 7-day pool for paused runs; re-dispatch
  of drained, preempted and orphaned runs, and reclaim after a gateway
  restart; `/metrics`; `graphrt gateway --dev`, `key`, `import`, `check`;
  local-process provider; `graphrt deploy --target local`. The gateway build
  uses mimalloc, agents do not. See [docs/gateway.md](docs/gateway.md).
- Gateway active/passive for on-prem (`ha:` section, store tier T1-AP): two
  `graphrt gateway` processes share the SQLite store and a lease on one volume
  (one host, or DRBD / a SAN LUN). A passive opens nothing and does not listen;
  it takes over when the active exits (`flock` released), releases on SIGTERM,
  or (`takeover: auto`) stops heartbeating for `stale_after_ms`, killing a
  frozen holder on the same host. Every take raises a fencing token that each
  store write checks inside its SQLite transaction, so a superseded active can
  never commit again (I8); a self-fence stops an unconfirmed active even
  earlier. Takeover verifies the store (`quick_check`) and restores the newest
  backup when it is missing or corrupt, then reclaims runs as after a restart;
  cron and webhook run ids keep fires and deliveries single across failovers.
  `graphrt gateway --ha-node`, `graphrt gateway ha status|promote`. Docs with
  systemd units and unsupported volumes (NFS, SMB, RWX) in
  [docs/gateway-ha.md](docs/gateway-ha.md).
- `graphrt-provider-api` crate: the `Provider` / `DeployDriver`
  traits, capabilities and error kinds of the upcoming `graphrt gateway`, the
  shared provider HTTP layer (token bucket per provider API, jittered retry,
  token cache with one refresh on 401) and the C1–C13 provider conformance
  harness with a scripted fake HTTP server; `Provider::invoke` lets a
  platform without an HTTP address (Lambda) carry the gateway's requests, and
  `max_grace_secs = 0` marks a platform where no preemption is possible.
  No user-visible change yet.
- `graphrt-provider-azure` crate: the Azure provider family of the
  upcoming gateway, without an Azure SDK. `auth-azure`: Entra ID tokens from
  managed identity (`IDENTITY_ENDPOINT`, IMDS), client secret, PS256
  certificate assertion and workload identity federation, cached until
  `exp - 5 min`. `provider-aca`: Azure Container Apps provider
  (scale-to-zero HTTP rule, one label URL per deployment version, single-flight
  warm-up). `deploy-aca`: deploy driver over ARM REST with revision labels,
  7-day retention of old versions and Key Vault secret references. Passes the
  shared conformance harness against fake servers; +48.8 KiB (`auth-azure`)
  and +48.8 KiB (`provider-aca`) on macOS arm64. See
  [docs/deploy-azure.md](docs/deploy-azure.md), including what still needs a
  real subscription and the RBAC roles.
- Live provider test matrix: `crates/graphrt-llm/tests/live_providers.rs` sends
  tiny streamed chat and tool-call round-trip requests to OpenAI, Anthropic,
  Gemini and Bedrock and a typed question to TypeSafe (opt-in with
  `GRAPHRT_LIVE_PROVIDERS=1`, each provider skipped without its credentials),
  and the `Live providers` workflow runs it nightly and on demand behind
  repository secrets. See [docs/providers.md](docs/providers.md#live-provider-tests).
- CI workflow `mobile.yml` builds `graphrt-ffi` for `aarch64-linux-android` and
  `x86_64-linux-android` (cargo-ndk) and for `aarch64-apple-ios`,
  `aarch64-apple-ios-sim` and `x86_64-apple-ios`, builds both sample apps, and
  runs the hello workflow in an Android emulator and an iOS simulator through
  the new `examples/mobile/*/scripts/smoke-test.sh`. docs/mobile.md gains a
  checklist for testing on physical devices.
- Release automation per tag: `.github/workflows/cross-platform.yml` runs the
  workspace tests on Windows and macOS for pull requests and `main`, and on
  Linux, macOS and Windows before a tag is built; nothing is published unless
  they pass. Release binaries are smoke-tested on their own OS
  (`scripts/smoke-release-binary.sh`; x86_64 macOS is cross-built only), and
  the Windows `.exe` links the C runtime statically (no `VCRUNTIME140.dll`).

### Documentation
- `docs/gateway.md` has a "Measured so far" table: agent exec → `/healthz`,
  gateway restart, gateway start, a first run that wakes a local agent from
  zero (p50 17.4 ms with `curl`), HA takeover, Kubernetes (kind) wake p95
  812 ms and the gateway binary sizes (core 6.69 MiB, `gateway-all`
  7.40 MiB, macOS arm64). It states that Cloud Run, Container Apps, ECS,
  Lambda and Fly are tested against API fakes only. The quick start installs
  from git until the gateway is released. The kind
  measurement is in `docs/kubernetes-provider.md`, the README row names
  `graphrt deploy --target`, and the docs index groups the gateway guides.

### Fixed
- Gateway HA: a passive now takes over from an active frozen (`SIGSTOP`, VM
  pause, CPU starvation) *inside* a heartbeat. The heartbeat holds
  `graphrt-gateway.lease.lock` for its write + fsync + rename (4 ms p50, 8 ms
  max on macOS APFS, about 2 % of a 200 ms beat, longer under load), and a
  process frozen there kept the lock, so every passive poll found it busy and
  waited until the process resumed: no steal, no kill (the cause of a
  `gateway_failover` "no steal" failure under CPU load). A lock still busy
  after `stale_after_ms` now means its holder is frozen, and the passive
  applies the usual rules without it (the lease file is replaced atomically;
  the store fence still orders the tokens): it kills the frozen holder on
  the same host and takes over after `stale_after_ms` (1.56–1.89 s with
  1500 ms, real processes, 13 runs at load average 59–126). `ha promote` goes through such a lock as
  well. The failover simulation now freezes processes inside the lock (with
  the heartbeat's write landing late on resume) and checks that a frozen
  active on one host loses the lease before it resumes: 147 violations in
  1,000 seeds without the fix, 0 in 10,000 with it. `gateway_failover`
  freezes its active with the lease lock held, so the case runs every time.
- `graphrt gateway`: a `socket_dir` too long for Unix socket paths (104 bytes
  on macOS, 108 on Linux, e.g. a deep `$TMPDIR`) made every local agent exit
  with status 2 (`agent exited during start-up`). The gateway now puts the
  agents' sockets and pid files in a short private directory,
  `/tmp/graphrt-gw-<hash of socket_dir>` (0700, owner checked, the same after
  a restart so orphans are adopted), and logs it. Tests with a long base
  directory in `graphrt-gateway/tests/local_provider.rs` and
  `graphrt-cli/tests/gateway_dev.rs`.
- The `gateway` feature includes `trust-gateway`. A
  `--no-default-features --features gateway` build answered every run of
  `graphrt gateway --dev` (and of `kind: local` deployments) with
  `agent exited during start-up (exit status: 2)`: its local agents are
  `graphrt serve --trust-gateway` of the same binary, which that build could
  not run. Costs 48.8 KiB on the gateway core build (macOS arm64, `min-size`:
  6,961,376 → 7,011,360 B) and 81.1 KiB on `gateway-all`. New test
  `crates/graphrt-cli/tests/gateway_dev.rs`, run by the new CI job
  `gateway without default features` (and `scripts/ci-local.sh` job
  `minimal-gateway`).
- `graphrt gateway`: a prewarm (cron `prewarm_secs`, `min_replicas` of a new
  version) went around the pool's singleflight and the wake quota, so a prewarm
  and a cold request at the same moment could start two replicas, and prewarms
  did not count against `max_wakes_per_hour`. Prewarms now join the in-flight
  wake and are counted.
- `graphrt gateway` no longer loses a run without a checkpoint when the gateway
  restarts. Each accepted run request is written to the store in the
  transaction that creates the run (no extra write), without the start inputs
  the version declares `type: secret` and without the inputs mapped from
  request headers (`user_jwt`); only their names are kept (`needs_secrets`).
  After a restart such a run is re-dispatched from the stored request with the
  same run id; only one that needs a secret ends `failed{owner_lost}` with
  `credentials_required: it needs the secret inputs …`, so the client submits
  it again. `graphrt deploy` and `graphrt gateway --dev` register the
  workflow's secret inputs with the version (`secret_inputs` on
  `POST /admin/v1/deployments/{name}/versions`); a version registered without
  them has every input treated as secret, so nothing undeclared is persisted.
  In the gateway crash simulation (40 seeds on a one-URL platform) the 18 runs
  that used to end `owner_lost` now finish; the 14 that carry a secret end
  `credentials_required`.
- `graphrt gateway` no longer loses a run whose executor disappears without an
  outcome. On a one-URL platform (Cloud Run, ACA, Lambda), where no orphan can
  be pulled from an instance, or when no orphan comes within the reclaim grace
  (the instance is gone), the run is re-dispatched at the next epoch from its
  latest stored snapshot, else from its request with the same run id (nodes may
  run again: at-least-once, keyed by `Idempotency-Key`). It ends
  `failed{owner_lost}` only when that is impossible, and says why: the
  request's credentials expired, the executor was lost 4 times in a row
  (`SupervisorSettings::max_lost_executions`), or it needs a secret input after
  a gateway restart (see below). A re-dispatch that reaches
  the instance still holding the lost execution retries its `409
  epoch_conflict` until that instance preempted it. New metric
  `graphrt_gateway_lost_executions_total`.
- `graphrt serve` drain: runs could be left `running` at the drain deadline (and
  marked failed by the next start) when the store was slow. The preemption
  reserve assumed 10 ms per active run, but a `--store` directory serialises
  writes behind one lock and syncs several files per write, so on a busy disk
  the final writes of many runs stopped at once overran it (seen as 8 of 10
  runs paused under load). The reserve now uses the store's measured recent
  write latency (three writes per run, at least 10 ms) and is recomputed while
  the drain waits.
- Telemetry: `DO_NOT_TRACK=` (empty) and values such as `yes`/`no` now turn
  telemetry off, as documented ("anything but `0`/`false`"); only `0` and
  `false` leave it alone. A build with a telemetry endpoint that the user did
  not opt into no longer creates the config directory and `telemetry.lock` on
  every `run` / `serve`; only the `telemetry` command, or telemetry being on,
  writes there.
- `--langfuse-scores` posted to `.../api/public/otel/v1/traces/api/public/scores`
  when `LANGFUSE_BASE_URL` already named the OTLP traces endpoint (which the
  preset accepts). Preset base URLs now drop the traces path.
- `graphrt serve`: resuming a paused run whose workflow is no longer served
  answered `404` but left the run `running` until the next restart, so it
  could not be resumed or deleted. It stays `paused` now.
- Windows: plugin manifests no longer accept rooted paths such as `/etc/x` or
  `\x` for `module`, checksums or the entrypoint (they are not "absolute" on
  Windows, yet escaped the plugin directory when joined).
- Windows: `file:///C:/dir/index.json` URLs work for `GRAPHRT_PLUGIN_INDEX` and
  catalog tarballs.
- Windows: a killed or wedged process plugin takes its child processes with it
  (`taskkill /T`), and plugin and MCP stdio processes get `TEMP`, `TMP` and
  `PATHEXT` besides `SYSTEMROOT`, `USERPROFILE` and `APPDATA`.
- Windows: the Python code node keeps the host `PATH` and `SYSTEMROOT`, without
  which CPython cannot start (the Unix default `PATH` does not exist there).

### Security
- Plugin manifests: a process plugin's `cwd` must be a relative path inside
  the plugin directory, and a relative `command` must not contain `..`, so a
  manifest cannot point the process at files outside its directory. Bare
  names on `PATH` (`python3`) and absolute system paths stay allowed.
  **Migration:** move a `cwd` such as `../shared` inside the plugin, or start
  the process through a script in the plugin directory, as the
  `rust-text-stats` example now does (`run.sh`).
- Bundles: extraction refuses Windows device names (`CON`, `PRN`, `AUX`,
  `NUL`, `COM1`-`COM9`, `LPT1`-`LPT9`, with any extension) in any path
  component, and paths that differ only in case, which overwrite each other
  on case-insensitive file systems so a verified file could be swapped.
- URLs are shown without `user:password@` userinfo in startup lines, errors
  and `Debug` output: `OtelConfig`, the preset's resolved settings, the
  Langfuse uploader, JWKS, webhook callback and telemetry endpoints.
- Run data written to disk is owner-only on Unix (`0600` files, `0700`
  directories): the `serve --store` directory (records, events, snapshots,
  `scores.json`, `.lock`), replay cassettes, and `graphrt eval -o` / `--junit`
  reports. They hold prompts, answers and user data and were created with the
  umask, so usually world-readable. Existing files keep their mode; run
  `chmod -R go= <store-dir>` once to tighten an existing store. The four
  "fresh owner-only temp directory" implementations (bundle configs, the
  Python runner, catalog and git plugin installs) now share
  `graphrt_core::private_fs`. (Low; 0.4.0 is affected.)
- `graphrt serve`: human feedback could fill a run's 256-score cap and make
  every later online-evaluator score of that run fail to store, so an end user
  (`--verify-jwt`) could keep a safety judge from ever scoring their own runs.
  The cap now counts each source (`human`, `online`) separately. (Low; 0.4.0
  is affected.)
- `graphrt serve`: `POST /v1/runs/{id}/feedback` parsed the JSON body (and
  answered `415`/`400`) before checking the API key or user token, so
  unauthenticated callers could make the server parse up to
  `--max-body-bytes` of JSON per request. It now authenticates first, like
  every other route. (Low; 0.4.0 is affected.)

- With OpenTelemetry content capture on, the agent's `tool_result` progress
  (and so `gen_ai.tool.call.result`, `-o events` output and stored run events)
  carried the tool's raw result text, so a tool echoing its `tool_context`
  (for example an upstream error repeating the end user's JWT) leaked the
  plaintext. It now carries the same masked text the model sees, masked before
  it is cut to `max_tool_output_chars` so no prefix of the secret survives.
  Affects 0.4.0 with capture on.

- `graphrt plugin install <name>`: errors that quote a catalog tarball's
  contents (a refused entry path, a manifest field) no longer print its
  control characters, so a published plugin cannot send terminal escape
  sequences (OSC 52 clipboard writes, screen clears). Catalog text also has
  Unicode bidirectional formatting characters (U+202E and friends) replaced,
  and plugin manifests refuse control characters in node kinds and tool
  names, which `plugin install`, `list` and `info` print. Affects 0.4.0.
- `graphrt plugin install <name>` extracts into, and `plugin install
  <git-url>` clones into, an owner-only (`0700`) scratch directory. Both used
  the umask, so under a permissive one (`000`) in a
  shared `/tmp` another local user could replace a file between the checksum
  check and the copy into the plugin directory. Affects 0.4.0.
- `install.sh` refuses a `--version` / `GRAPHRT_VERSION` with characters other
  than those of a release tag: a `/` could point the archive and its
  `SHA256SUMS` at another repository's release
  (`v1/../../../../other/repo/releases/download/v1`). `GRAPHRT_DOWNLOAD_BASE`
  must be `https://` or `file://`; with wget, an `http://` mirror was fetched
  without TLS. Affects the 0.4.0 installer.
- `packaging/homebrew/render-formula.sh` accepts only a semver version and
  64-hex digests, which it pastes into Ruby that `brew` evaluates, and no
  longer prints a formula with an empty `sha256` when an archive is missing
  from `SHA256SUMS`.
- CI: `ci.yml` and `live-providers.yml` pin every third-party action by
  commit SHA (a moved `@v4` / `@stable` tag could have run new code next to
  the live provider API keys), `ci.yml` runs with a read-only token and
  without persisted checkout credentials, and the PostgREST binary the
  chatbot e2e job installs as root is checked against its sha256.
- Release workflow: a manual run's `tag` input must name an existing `v*`
  tag, and every job checks out `refs/tags/<tag>`, so a branch with the same
  name can no longer be built and published as that release (the input was
  used unchecked as the ref and the release tag). `cargo-zigbuild`, which runs
  in the jobs that produce the release binaries, is pinned to 0.24.1, and no
  release job keeps checkout credentials.

- Dependencies: `wasmi` 2.0.1 (yanked) is updated to 2.0.2. The remaining
  `cargo deny` ignores (unmaintained, no vulnerability) carry a reason and a
  review date.
- A workflow could grow a variable exponentially: an `assigner` in a loop that
  appends or extends a variable with itself doubled it every round (memory
  with `extend`; CPU through shared children with `append`, where type
  inference walked the whole value). Assigner inputs read from a variable are
  now limited to 1,000,000 values and 128 levels of nesting, and type
  inference no longer descends into nested arrays.
- `template-transform` checked its 1 MiB output limit only after rendering,
  so a short loop over a large string could render gigabytes first. The
  render now writes into a capped buffer and stops at the limit.
- PDF extraction: the decompression budget (`max_decompressed_bytes`) could be
  bypassed, letting a small uploaded PDF make the parser allocate gigabytes.
  Only `FlateDecode` streams were measured, by a text scan that missed a
  stream whose dictionary held a nested `DecodeParms` dictionary after
  `/Filter`, or had whitespace before `stream`, or had a bad zlib header
  (the parser then retries as raw deflate). `LZWDecode`, filter chains,
  PNG-predictor row buffers (sized from `/Columns`) and encrypted documents
  were not measured at all. Streams are now decoded the way the parser
  decodes them, with every filter it supports, both before loading and on
  the parsed document before text is extracted.
- `{{#…#}}` variable templates parsed in quadratic time: every `{{#` searched
  the rest of the text for `#}}`, so a 900 KB string of `{{#` took minutes
  (in a workflow document, or at run time in conditions and list filters).
  The search now stops at the longest possible placeholder.


## [0.4.0] - 2026-11-20

Evaluation. `graphrt eval` runs datasets against a workflow with deterministic,
trajectory, retrieval and judge assertions (a judge is an ordinary graphrt
workflow), trials with pass@k / pass^k, baselines, judge calibration and CI
gates; the `replay` provider makes it deterministic. `graphrt serve` scores
sampled production runs online, takes human feedback and exports scores as
OpenTelemetry `gen_ai.evaluation.result` events. Spans now follow the
OpenTelemetry GenAI semantic conventions, with presets for Langfuse, LangSmith,
Braintrust, Opik, Phoenix and MLflow. Also end-user JWT verification and
per-user rate limits for `serve`. Contains breaking changes (see
**Changed (Breaking)**).

### Added
- Online evaluation: `graphrt serve --evaluators <file>` (cargo feature
  `online-eval`, on by default, not in the IoT build) scores a deterministic
  sample of finished runs with the assertions of `graphrt eval` that need no
  expected answer (`judge` workflows, `regex`, `contains*`, `starts-with`,
  `is-json`, `json-schema`, `latency`, `tokens`, `trajectory:*`), validated at
  startup. Evaluators filter on workflow, run status, tags and metadata;
  sampling hashes the run id with the evaluator name. Runs are queued after
  their response to a bounded pool (`concurrency`, `queue`); a full queue
  drops and counts (`graphrt.eval.dropped`). Without `--evaluators` nothing
  runs and runs pay only an empty `Option` check (serve throughput within
  noise). Scores, online evaluation, presets and the uploader add about
  98 KiB to the default min-size binary and nothing to the IoT build. `graphrt_eval::online` holds
  the config, sampling and evaluation; `examples/evals/online` replays the
  reference judges offline. See docs/evals.md#online-evaluation.
- Run scores (cargo feature `scores`): `GET /v1/runs/{id}/scores`, and
  `POST /v1/runs/{id}/feedback` for human feedback `{name, value, comment?,
  user?}` (number, boolean or text values). Scores are stored next to the run
  (`runs/<id>/scores.json`, or in memory, bounded by `--max-runs` and the TTL)
  with deterministic ids, so a repeated score replaces the earlier one. With
  `--verify-jwt`, an end user's token may give feedback on the user's own runs
  only, as that user.
- `evaluate {name}` spans: every score (online, human, and each assertion of
  `graphrt eval`) becomes a span in the evaluated run's trace, under its root
  span, with a `gen_ai.evaluation.result` event (`gen_ai.evaluation.name`,
  `.score.value`, `.score.label`, `.explanation`), plus a `graphrt.eval.score`
  histogram by evaluator. Names live in `graphrt_otel::semconv`.
- Vendor presets: `--otel-preset <name>` (or `GRAPHRT_OTEL_PRESET`) for
  `langfuse`, `langsmith`, `braintrust`, `opik`, `phoenix` and `mlflow` fills
  the traces endpoint, auth and routing headers (Langfuse Basic auth and
  `x-langfuse-ingestion-version: 4`, `x-bt-parent`, `x-mlflow-experiment-id`...)
  and resource attributes (`opik.project_name`, `openinference.project.name`)
  from each vendor's own environment variables, and turns the metrics push off.
  Small static tables in `graphrt_otel::preset`.
- `graphrt serve --langfuse-scores` (cargo feature `langfuse`) uploads online
  and human scores to Langfuse's `/api/public/scores` with the run's trace and
  root observation ids and the score's deterministic id, through the egress
  policy, with retries.
- `graphrt eval --otel` exports one trace per test run with
  `langfuse.experiment.*` attributes (a Langfuse experiment over OTLP; with
  the Braintrust preset and `BRAINTRUST_PARENT=experiment_id:<id>`, a
  Braintrust experiment) and an `evaluate` span per assertion. It used to
  ignore `--otel`.
- `graphrt-otel`: `EventTracer::with_span_attributes`, `with_root_attributes`
  and `with_root_span_id`; `TraceId::from_hex` / `SpanId::from_hex`;
  `OtelConfig::export_metrics` and `preset`. `graphrt-eval`:
  `EngineTarget::engine` and `env`.
- `graphrt serve --verify-jwt INPUT` (cargo feature `jwt`, on by default)
  verifies the token of a header-mapped input as a JWT: `ES256` / `RS256` keys
  from a JWKS (`--jwt-jwks-url`, refreshed in the background and on unknown
  `kid`) and/or an `HS256` secret (`--jwt-secret-env`), with `exp`, `nbf`,
  `--jwt-issuer` and `--jwt-audience` checks and a required `sub`. A valid
  token may start runs (`POST /v1/workflows/{name}/runs` only, of workflows
  that declare the input; others are `403`) without the API key and sets
  `sys.user_id` and the run's `user_id` (record and spans, replacing the
  body's `user_id`); an invalid one is `401`. `--user-rate-limit N`
  allows N runs per minute per `sub` (`429` with `Retry-After`). The server
  refuses `--verify-jwt` on a non-loopback address without an API key. No new
  crates; about 33 KiB of binary.
- `examples/supabase-chatbot`: a multi-user chatbot backend for apps signed in
  with Supabase Auth: agent with the supabase plugin in `user_rpc` mode and
  `tool_context`, memory from `sys.history`, a sample schema with RLS and an
  isolation test, a distroless container and Fly.io / Cloud Run / VPS notes.
- `graphrt serve --input-header INPUT=HEADER` maps a request header (leading
  `Bearer ` stripped) to a secret start input of new, streamed and resumed
  runs, so an app can send a user's JWT next to the serve API key. The value is
  masked everywhere and never stored; resumes must send the header again. A
  missing header for a required input is `401`, the same input in the body
  `400`, and a non-`secret` declaration fails at startup.
- `agent_thought` progress events carry `streamed` (whether the turn's text
  was streamed).
- docs/serving.md documents the SSE format, the `response` event and how
  retry / `on_error` policies hold back live answer text.
- `agent` node `tool_context`: per-run values (e.g. the end user's JWT) sent
  to one provider's tools beside the arguments, as a new `context` field of
  the plugin protocol's `tool.call` (`{"secrets": {name: string}}`). Only
  plugins with `permissions.secrets` that the operator trusts receive it;
  other providers fail the node with `tool_context_denied`. The values are
  never shown to the model or recorded, and are masked in tool results,
  logged arguments and node errors. Hosts announce the `tool_context` feature
  in `initialize`.
- Plugin SDKs pass the context as the tool handler's second argument
  (TypeScript `(args, context)`, Python `(args, context)` for two-parameter
  handlers, Rust `ToolCall::context`); existing handlers keep working.
- `NodeContext::observe_secret` lets a node register plaintext it handles as a
  run secret, so later node error messages are scrubbed.
- OpenTelemetry: agent tool calls are `execute_tool {tool}` child spans
  (`gen_ai.tool.name`, `gen_ai.tool.call.id`, error status for failed or denied
  calls); `knowledge-retrieval` nodes are `retrieval` spans with
  `gen_ai.retrieval.top_k`; `gen_ai.provider.name`, `gen_ai.agent.name` and
  `gen_ai.response.finish_reasons`.
- OpenTelemetry content capture follows `OTEL_INSTRUMENTATION_GENAI_CAPTURE_CONTENT`
  (`NO_CONTENT` default, `SPAN_ONLY`, `EVENT_ONLY`, `SPAN_AND_EVENT`). With it on,
  the run span carries the run input and output (`graphrt.run.input` /
  `output` and `langfuse.observation.input` / `output`), model spans
  `gen_ai.system_instructions` and `gen_ai.input.messages` /
  `gen_ai.output.messages` as structured values, tool spans their arguments and
  result, retrieval spans the query and document ids. Values stay masked and
  capped at 4 KB.
- `graphrt_core::node::CaptureContent` service: when registered (the CLI does so
  while content capture is on), `llm` nodes report their rendered messages as
  `model_request` progress, agents add result text to `tool_result` progress and
  `knowledge-retrieval` adds its `query` to the node metadata. Nothing extra is
  built without it.
- `POST /v1/workflows/{name}/runs` accepts `metadata`, `session_id`, `user_id`
  and `tags`; they are stored on the run record and stamped on every span
  (`session.id`, `user.id`, `gen_ai.conversation.id`, `langfuse.session.id`,
  `langfuse.user.id`, `langfuse.trace.tags`, `langfuse.trace.metadata.*`).
  `graphrt run` gets `--session-id`, `--user-id`, `--tag` and `--metadata`.
- `RunRecord` gains `trace_id` and `span_id` (root span of the latest traced
  execution), `metadata`, `session_id`, `user_id`, `tags`, and `inputs`, kept
  (secrets masked) only with the new `graphrt serve --store-inputs`.
- `graphrt-otel`: `semconv` module with every attribute name, `CaptureMode`,
  `RunContext`, `EventTracer::{capture, with_context, content_limit,
  root_span_id}`, `Span::is_run_root`, `AttrValue::Structured`.
- docs/observability.md: setups for Langfuse v4, Phoenix, Opik, MLflow,
  LangSmith and Braintrust.
- `graphrt eval <file.eval.yaml>`: evaluate a workflow against test cases and
  gate CI on the pass rate. Eval files use Promptfoo's `tests` / `assert` /
  `defaultTest` shape plus JSONL datasets; assertions cover outputs (`equals`,
  `contains` and its variants, `regex`, `starts-with`, `is-json`,
  `json-schema`, `transform` paths), `latency` and `tokens` budgets, agent
  trajectories (`trajectory:tool-used`, `tool-sequence`, `tool-args-match`,
  `step-count`, from the run's tool-call events) and retrieval quality
  (`retrieval:recall-at-k`, `precision-at-k`, `mrr`), each negatable with
  `not-` and weighted (`weight: 0` reports only). The workflow is compiled
  once and tests run in-process and concurrently (`-j`), with `--filter`,
  `--trials` (alias `--repeat`), per-test `--timeout-ms`, `--output` JSON
  results (report `schema_version: 2`) and `--junit` XML. A failed gate exits
  with the new exit code 5 (`--fail-exit-code`). Feature `eval`, on by
  default; not in the small-device build. See [docs/evals.md](docs/evals.md).
- LLM judges as graphrt workflows: `{type: judge, workflow: judges/x.yaml}`
  runs the judge on the test's `output`, `vars`, `expected`, retrieved
  `contexts` and tool `trajectory` (plus `inputs`), and reads `verdict:
  pass|fail|unknown` or a 0..1 `score` with `reasoning`. `unknown: fail`
  (default) or `exclude` (reported, not scored or gated). Each judge is
  compiled once and runs inside its test under `-j`; its workflow fingerprint,
  verdict, reasoning and tokens are recorded with the result
  (`summary.judge_usage` totals them). `--judge-model PROVIDER/NAME` overrides
  the judges' models; `--model` never does.
- `examples/evals/judges`: reference judges for faithfulness, answer
  relevance, goal success and refusal (binary, single criterion, with an
  `unknown` option) and a System One refusal judge; `examples/evals/judged`
  runs them offline on a RAG workflow from replay cassettes.
- `--trials N`: per test case unbiased pass@k (`1 - C(n-c,k)/C(n,k)`) and
  pass^k (`C(c,k)/C(n,k)`) in `cases` and `summary.trials`;
  `gate: {pass_hat_k, k}` gates on the mean pass^k.
- `--baseline results.json`: compare with an earlier `--output` file; metric
  means and per-test pass rates regress only beyond `gate.baseline`
  tolerances (`tolerance` 0.05, `test_tolerance` 0, per-metric `metrics`),
  improvements and changed judge fingerprints are reported, and any regression
  fails the gate.
- Judge calibration: a suite with `calibration:` evaluates a judge workflow
  against human labels in `expected` (new `verdict` assertion) and reports
  agreement, TPR, TNR, Cohen's kappa and the confusion matrix, with
  `min_kappa` / `min_tpr` / `min_tnr` gates (`examples/evals/calibration`).
- `gate.failures` in the JSON results and `gate: FAILED (...)` on the
  terminal name every unmet gate condition.
- `graphrt-eval` crate: the same harness as a library, for `cargo test`, with
  an `Assertion` trait and registry for custom assertion types, a
  `JudgeLoader` for `judge` assertions, and the `stats` (pass@k, pass^k,
  agreement), `baseline` and `calibration` modules.
- `kind: replay` providers: serve model responses from recorded cassettes
  (keyed by a hash of the normalized request, streamed delta by delta), record
  them from an `upstream` provider (`mode: record`), or both (`mode: auto`,
  a response cache). `GRAPHRT_REPLAY_MODE` overrides the mode. Works with
  `eval`, `run` and `serve` (`--providers`). Feature `replay` of graphrt-llm
  and graphrt-cli.
- `schema/eval-v1.schema.json`: JSON Schema of eval files.
- `examples/evals`: offline suites for the hello and branching workflows, an
  agent judged on its tool trajectory from committed cassettes, and
  retrieval metrics over a committed knowledge store.

### Changed (Breaking)
- OpenTelemetry span names: the run span is `invoke_workflow {workflow}`
  (was `workflow {workflow}`) with `gen_ai.operation.name=invoke_workflow`, and
  is kind `INTERNAL` unless it continues a caller's `traceparent` (then
  `SERVER`, as before); model nodes are `chat {model}` (was the node title,
  now in `graphrt.node.title`); agents are `invoke_agent {title}` with
  `gen_ai.operation.name=invoke_agent` (was `chat`).
- `system-one` spans report `gen_ai.operation.name=chat` (was the non-standard
  `classify`, now `graphrt.operation.kind=classify`).
- `gen_ai.system` is deprecated in favor of `gen_ai.provider.name`; both are
  emitted for this release, `gen_ai.system` will be removed in the next.
- `tool_call` / `tool_result` progress no longer appear as span events on the
  agent span (they are `execute_tool` spans); `model_request` progress never
  becomes a span event.
- `graphrt-otel` API: `OtelConfig::capture_content: bool` is replaced by
  `OtelConfig::capture: CaptureMode`; `AttrValue` is `#[non_exhaustive]`;
  `RunRecord` and the CLI's `ServiceOptions` have new public fields (construct
  them with `RunRecord::running` / `..Default::default()`).

### Changed
- `agent` streams the text of every turn token by token, including turns that
  may still call tools, so the final answer of a tool-using agent no longer
  arrives as one chunk after the model finished. Tool-call arguments are never
  streamed as text. Text a turn streams before its tool calls (a preamble such
  as "Let me check.") is now part of the `text` output, separated from later
  text by a blank line, so the streamed text always equals `text`.
  `stream_tool_turns: false` restores the previous behaviour.
- **Breaking (Rust API):** `ToolRuntime::call` takes a third argument,
  `&ToolCallContext` (pass `ToolCallContext::empty()` when there is nothing to
  send), and `ToolRuntime::accepts_secrets` (default `false`) was added.
  `graphrt_plugin_sdk::ToolCall` has a new `context` field.

### Fixed
- Plaintext of secret-typed `conversation` variables is now scrubbed from node
  error messages, like other secrets.
- The OTLP exporter no longer drops a run's root span when its queue is full;
  several backends hide or discard every span of a trace without its root.
- `graphrt serve` keeps keep-alive connections usable after responses sent
  before the request body was read (`401`/`403` auth, `429` rate limits, `415`,
  `413`): the unread body is drained (up to `--max-body-bytes`, at most 2 s),
  or the response says `Connection: close`. Pooling clients such as Dart's
  `http.Client` no longer fail their next request with "Broken pipe".
- `examples/supabase-chatbot/tests`: end-to-end security test (real
  `graphrt serve`, supabase plugin in `user_rpc` mode, PostgREST and Postgres,
  a scripted hostile model) proving user A cannot reach user B's rows and that
  prompt injection cannot call functions outside the allowlist or reveal the
  user's JWT; `tests/run.sh` runs it with the smoke test. New guide
  [docs/chatbot-supabase.md](docs/chatbot-supabase.md) and a docs index.

## [0.3.2] - 2026-11-20

Telemetry fix and the project's collector endpoint in release builds.

### Fixed
- Telemetry: the daily report of a short command such as `graphrt run` was
  lost. The background thread claimed the day and removed its counts, then the
  process exited before the request finished, so nothing arrived and nothing
  was retried (only long-running `serve` delivered reports). A finishing
  process now waits for that once-a-day report, at most the 5-second send
  timeout. Telemetry is opt-in and no released binary had an endpoint, so no
  user data was affected.

### Changed
- Release binaries, the Homebrew formula and the container image report to
  the project's collector (`graphrt-telemetry.bimapangestu280.workers.dev`)
  once a user opts in with `graphrt telemetry enable`; still off by default.

## [0.3.1] - 2026-11-20

Loading large workflows is fast again. 0.3.0's strict validation built every
node and checked every node config twice, and re-parsed the document for source
positions even when it was clean: `graphrt run` / `validate` on a 1,000-node
workflow took ~54 ms instead of 0.2.0's ~14 ms. No behaviour changes; errors
are reported exactly as in 0.3.0.

### Performance
- Validation compiles a clean document from the nodes it already built and
  checked, instead of building and checking them again in `Graph::build`.
- Source positions are computed only when there is something to report.
- `Graph::fingerprint` is computed on first use and cached: only snapshots, run
  records and traces need it. Values are unchanged.
- `graphrt run` on a 1,000-node chain (macOS arm64, `min-size` build): 0.3.0
  55 ms, 0.3.1 16 ms, 0.2.0 15 ms.

### Changed
- Every crate now declares Rust **1.89** as its MSRV (was 1.85 for most). The
  declaration was wrong, not the code: `graphrt-core`'s default `yaml` feature
  has needed 1.89 since 0.2.0 through `serde-saphyr` 2, and the CLI, FFI and
  RAG crates already declared it. The MSRV CI job (and `scripts/ci-local.sh`)
  now checks the whole workspace on 1.89.

### Added
- The iOS sample app is verified on a physical iPhone (iOS 18.3); its README
  has the device build, install and launch steps.
- Plugin SDKs 0.2.0 on PyPI and npm (`graphrt-plugin`): tool handlers can
  return images (`tool_images`), as shipped in graphrt 0.3.0.

## [0.3.0] - 2026-11-19

Workflow format v1 frozen with a JSON Schema and located, collected validation
errors; webhook triggers; opt-in anonymous telemetry; plugins installable by
name from the plugin catalog; a security review with fixes (most also shipped
as 0.2.1); Android and iOS sample apps; a comparison against LangGraph; and
another round of engine, LLM-path and server speedups. Contains breaking
changes to the workflow format and the Rust API (see **Changed (Breaking)**).

### Added
- `graphrt serve --webhooks <file>`: webhook triggers. `POST /v1/hooks/{hook}`
  starts an async run of the hook's workflow from any JSON body and answers
  `202` with the run id. Bodies map to inputs by JSON Pointer and header
  (`inputs`, `headers`) or pass whole (`body_input`, default `body`).
  Optional HMAC-SHA256 signature checks in the GitHub, Stripe, Slack, generic
  or graphrt formats, compared in constant time, with a replay window for the
  timestamped ones; secrets from the environment, `--env-file` or a file.
  An idempotency key header (default `Idempotency-Key`) maps redeliveries to
  the first run, durably with `--store`. Deliveries share `--max-body-bytes`.
  `GET /v1/hooks` lists the hooks. See docs/serving.md.
- Webhook completion callbacks (`callback`): a signed
  (`X-Graphrt-Signature: t=..,v1=..`) JSON result POSTed when the run
  succeeds, fails, aborts or pauses, through the HTTP egress policy (private
  networks refused unless `--allow-private-network`, no redirects), retried
  with exponential backoff on network errors, `408`, `429` and `5xx`.
- Opt-in anonymous usage telemetry for the `graphrt` binary (`telemetry`
  feature, in the default set). Off by default; on only after
  `graphrt telemetry enable` or `GRAPHRT_TELEMETRY=1`, and always off with
  `DO_NOT_TRACK`, in CI, or without an endpoint (`GRAPHRT_TELEMETRY_URL` at
  build or run time). At most one report a day, sent from a background
  thread with a 5 s timeout: a random install id, version, OS/arch,
  features, build (`cli`/`docker`) and per-day counts of runs and `serve`
  requests, aggregated in `telemetry.json` in the user config directory.
  `graphrt telemetry status` prints exactly what would be sent. Not in the
  embedded or FFI libraries. See docs/telemetry.md.
- `tools/telemetry-collector/`: a Cloudflare Worker + D1 collector for those
  reports (strict payload validation, no IP addresses stored, schema, wrangler
  config template, weekly-active-installs and daily-runs queries). Not
  deployed.
- Workflow format v1 is frozen, with a JSON Schema (draft 2020-12):
  `schema/workflow-v1.schema.json`, one `oneOf` branch per node kind keyed on
  `type`, built from the node catalog. `graphrt schema` prints the schema for
  the node kinds in the running build, including plugins; the examples carry a
  `# yaml-language-server: $schema=...` header. Editor setup (VS Code YAML,
  JetBrains) and the stability promise are in `docs/workflow-format.md`.
- Validation reports every problem at once: `graphrt validate` and `run` show
  rustc-style errors with `file:line:column`, a source snippet, the document
  path (`graph.nodes[1].template`) and "did you mean" suggestions for field
  names, node types, node ids in edges, parents and selectors, and edge
  handles. `graphrt validate --format json` prints a machine-readable report.
  Library: `graphrt_core::validate::validate_text`, `Diagnostic`,
  `source_map::SourceMap`, `doc_schema::workflow_schema`,
  `Registry::descriptor_ref`, `Node::branch_handles`, `ConfigError::field`.
- `x-` keys (document, `graph`, nodes, edges) are free for tools and ignored.
- Tools can return images that the `agent` node shows to the model, enabling
  vision loops such as computer use (screenshot, then click by coordinates).
  `ToolOutput` gains `images` (`ToolImage::Base64 {media_type, data_base64}` or
  `ToolImage::Url`), checked against `check_images` (4 per result, 5 MiB each,
  png/jpeg/webp/gif, https URLs). Anthropic, Gemini 3+ and Bedrock Claude/Nova
  receive them inside the tool result; OpenAI-compatible servers and other
  models get them in a user message right after the tool results. New agent
  option `keep_tool_images` (default 3, `null` for all) keeps only the latest N image-bearing results.
  Plugin protocol: `tool.call` results accept `images`, and `initialize`
  announces `features: ["tool_images"]`; the Python, TypeScript and Rust SDKs
  send images only to hosts that announce it. MCP image blocks are forwarded
  too. See `docs/tool-images.md`.
- `graphrt plugin install <name>[@version]` installs a plugin from the
  graphrt-plugins catalog (`index.json` of its `catalog` release, or
  `--index <url|path>` / `$GRAPHRT_PLUGIN_INDEX`): the latest version or the
  exact one, downloaded over https only with a timeout and a size cap, checked
  against the catalog's sha256, extracted with absolute paths, `..`, symlinks,
  hard links and device files refused, then installed like a local directory
  (manifest validation and checksums included). The plugin's `status` and
  `tos_risk` are printed; anything but status `stable`/`beta` with ToS risk
  `none`/`low`/`medium` needs `--yes`, including unlabelled entries and
  unrecognised labels. A directory with the same name in the current directory still wins.
- `graphrt plugin search [query]`: catalog plugins with their latest version,
  status and a one-line description.
- `docs/plugin-manifest.md`: a versioned specification of the plugin manifest
  (every field with its type, default and constraints, the validation rules
  and a compatibility policy for `protocol_version`), and a JSON Schema for
  manifests, `docs/schemas/plugin-manifest.schema.json`, tested against the
  example, fixture and scaffolded manifests and against the parser itself.

### Changed (Breaking)
- **Unknown fields are errors in v1 documents**: at the document, `graph`,
  node, edge, `retry`, `on_error` and variable-declaration level, and in node
  configs whose catalog schema is closed (`additionalProperties: false`; all
  built-in kinds, as `catalog::schema::object` now produces). Plugin configs
  with open schemas are unaffected; `schema::open_object` builds one.
  Required config fields from the catalog schema are enforced (for example a
  `template-transform` without `template`). The import layout (top-level
  `workflow`/`app`, `data`-wrapped nodes) stays lenient.
  *Migration:* run `graphrt validate` and fix what it reports; move tool
  metadata to `x-` keys.
- **`version`** must be `"1"` (or `1`) or absent. *Migration:* set
  `version: "1"`.
- **Edge handles are checked**: an edge must use a handle the source node can
  select (`source` for non-branch nodes; case ids and `false` for `if-else`;
  class ids; action ids; a plugin's fixed handles). Non-branch nodes used to
  follow edges with any handle. `fail-branch` edges on nodes without that
  strategy are a warning (never taken). *Migration:* use `source` or the
  node's handle.
- **Reserved names**: top-level `imports`, `inputs`, `outputs`, `triggers`,
  `metadata`, `requires`, `subgraphs`; node-level `disabled`, `cache`,
  `metadata` (plugin configs cannot use them either).
- **`human-input`**: the request expiry is `expire_after_ms` (pause data still
  reports `timeout_ms`). A config `timeout_ms` was always taken as the node's
  per-attempt timeout and never reached the node. *Migration:* rename it.
- **Loop variables** are `{name, type, input_type, value}`; `label`,
  `var_type` and `value_type` (which meant the input source, unlike every
  other `value_type`) still work as deprecated aliases.
- Deprecated, still accepted with a warning: `environment_variables`,
  `conversation_variables`, declaration `value_type`, node `parentId` /
  `container_id` / `iteration_id` / `loop_id`, `error_strategy` /
  `default_value`, `retry_config`, edge `sourceHandle` / `source_handle`.
- API: `NodeDoc` has a `strict` field; `ConfigError` has a `field` field;
  `DocumentError::Field`; `GraphError::{MissingNode, MissingParent,
  UnknownReference}` carry a `suggestion`, new `GraphError::{InvalidField,
  UnknownHandle}`. `graphrt validate` takes `--format`; `CliError` has
  `preformatted`.
- **Breaking** (`graphrt-core`): `Event::NodeChunk::field` is an `Arc<str>`
  instead of a `String`, so a streamed chunk no longer allocates its field
  name. Serialized events are unchanged. New `NodeContext::chunk_shared(&field,
  text)` streams under a name the node keeps in an `Arc<str>`;
  `NodeContext::chunk(&str, text)` keeps its signature (and allocates the name
  per chunk). Migrating: compare with `field.as_ref() == "text"`, use
  `field.to_string()` where a `String` is needed, and `.into()` when building
  the event.
- **Breaking** (`graphrt-core`): `ResponseChunk::node_id` is an `Arc<str>`
  instead of a `String` (shared with the graph instead of copied per chunk).
  Migrating as for `NodeChunk::field`. New `ResponseAssembler::push_into(&event,
  &mut out)` appends to a caller-owned buffer; `push` is unchanged.
- **Breaking** (`graphrt-llm`): `collect`'s callback receives each `ChatDelta`
  by value (`FnMut(ChatDelta)`, was `FnMut(&ChatDelta)`), after the delta was
  added to the response, so streamed text can be moved into a chunk instead of
  copied. Closures that ignore the delta (`|_| {}`) compile unchanged; others
  match on the owned delta instead of a reference.
- **Breaking** (`graphrt-llm`): `nodes::common::PromptTemplate::messages`
  returns `&[PromptMessage]` instead of `Vec<&PromptMessage>`. New helpers
  `nodes::common::finish_reason_value` and `StreamField::shared`.
- Node outputs and pool namespaces use the new `graphrt_core::Variables` map
  (keys are `graphrt_core::VarName`) instead of `IndexMap<String, Value>` /
  `IndexMap<Arc<str>, Value>`. It is a reference-counted, insertion-ordered
  flat map: cloning is a reference-count bump, so a node's outputs become its
  pool namespace and its event payload without being copied. Affected:
  - `NodeOutput::outputs` is `Variables`; `NodeOutput::with` takes
    `impl Into<VarName>` (string literals, `String`, `&String`, `Arc<str>`).
  - `Event::NodeSucceeded::outputs` and `Event::NodeFailed::outputs` are
    `Variables` (same JSON).
  - `VariablePool::namespace()` returns `Option<&Variables>` and
    `VariablePool::set_outputs` takes `&Variables`.

  Migration: most code compiles unchanged, since `Variables` has the familiar
  `get`, `insert`, `remove`, `iter`, `keys`, `values`, `len`, `contains_key`,
  `extend` and `collect()` API. Iteration yields `(&str, &Value)` instead of
  `(&String, &Value)`; use `.to_owned()` where a `String` is needed. Pass a
  non-`'static` `&str` name as `name.to_owned()`. `outputs.insert("x".into(), v)`
  becomes `outputs.insert("x", v)`. Convert with `Variables::from(index_map)`,
  `variables.to_index_map()` or `IndexMap::from(variables)`. Indexing a missing
  name (`outputs["x"]`) yields `Value::Null` instead of panicking. `remove`
  keeps the order of the remaining entries (like `IndexMap::shift_remove`).

### Fixed
- `http-request` `timeout: {connect, read, write}` in a document was silently
  dropped (the loader treated `timeout` as a node policy key), so documents'
  HTTP timeouts never applied.
- The generated Kotlin bindings (`bindings/kotlin`) did not compile: every
  `GraphrtException` variant declared a `message` field that clashes with
  `Throwable.message`. Kotlin now names the field `detail`
  (`crates/graphrt-ffi/uniffi.toml`); `Throwable.message` still carries the
  text, and Swift and Python are unchanged. The `bindgen-cli` feature now
  enables UniFFI's `cargo-metadata` so the generator reads that file.

### Security
- `code` node (JavaScript): a workflow could switch off the sandbox's memory
  and stack limits by asking for `limits.memory_bytes: 0` or
  `limits.stack_bytes: 0` (QuickJS reads zero as "unlimited"), and unbounded
  recursion then overflowed the native stack and aborted the whole process
  (for example `graphrt serve`). An operator stack ceiling above 16 MiB had the
  same effect, and one above the executor's thread stack (2 MiB on tokio's
  blocking pool) crashed the process too. Limits now have floors (1 MiB heap,
  64 KiB stack) and a 16 MiB stack ceiling, and JavaScript runs on a dedicated
  thread sized from the stack limit, so deep recursion is always a catchable
  `RangeError`. Also fixed in 0.2.1.
- `code` node (JavaScript): error messages thrown by scripts are truncated to
  8 KiB, so a script cannot push megabytes into events and run records; an
  operator time limit too large for the clock no longer panics; a long chain of
  promise jobs is bounded by the time limit between jobs as well.
- Python subprocess runner: the working directory is created fresh and
  owner-only (`0700`) instead of adopting an existing directory at the
  predictable temp path.
- Engine: node error messages are scrubbed of every secret the run has seen
  (`type: secret` env variables, secret-typed inputs, re-supplied resume
  secrets, secret outputs) before they reach `node_failed` / `node_retry` /
  `run_failed` events, `error_message` outputs of handled errors, the run store
  and OTel spans. Previously an error that echoed a key (a provider body such
  as "Incorrect API key provided: sk-...", an HTTP error snippet from an echo
  endpoint, an exception in code) was recorded verbatim. New public helpers in
  `graphrt_core::redact`. Also fixed in 0.2.1.
- LLM providers: error bodies of failed provider requests are scrubbed of the
  credentials the request carried (`Authorization`, `x-api-key`,
  `x-goog-api-key`, `api-key` headers, `key` query parameters) before they
  become error messages. OpenAI-compatible gateways and self-hosted servers
  often echo the full key. Also fixed in 0.2.1.
- `code` node: returned strings containing a secret input (`key`, or
  `"Bearer " + key`) are re-tagged as secrets, so they are masked in
  `node_succeeded` events, SSE, run records and snapshots while staying usable
  downstream; captured Python stdout is scrubbed. Previously they were stored
  in plaintext. Also fixed in 0.2.1.
- HTTP egress policy: also blocks the IPv4-translated form `::ffff:0:a.b.c.d`
  (judged by the embedded IPv4 address), the local-use NAT64 prefix
  `64:ff9b:1::/48`, Teredo `2001::/32` and the Azure WireServer address
  `168.63.129.16`. Also fixed in 0.2.1.
- OTel HTTP exporter: redirects are no longer followed, so vendor key headers
  cannot follow a redirect to another host.
- Bundles: `graphrt run/serve --bundle` extracted configs to a fixed,
  predictable directory in the shared temp dir and reused it if it existed, so
  another local user could pre-create it and plant symlinks or swap
  `providers.json` (e.g. pointing `base_url` at their server). Each invocation
  now extracts into a new owner-only (`0700`) directory. Signatures are checked
  with strict Ed25519 verification. Also fixed in 0.2.1.
- Plugins: `limits.max_message_bytes` is capped at 256 MiB; WebAssembly
  plugins get at most 4 tables of 100,000 elements (was 8 x 1,000,000), which
  lived outside the `memory_bytes` budget.
- `graphrt serve`: new `--max-active-runs` (default 256, `0` = unlimited)
  caps runs executing at once; further starts and resumes get `429`.
  Previously `"async": true` requests could start unlimited parallel runs
  (`--max-runs` only bounds kept records). Also fixed in 0.2.1.
- `graphrt serve` without an API key on a loopback address now accepts only
  loopback `Host` names (`localhost`, `127.0.0.1`, `[::1]`), blocking DNS
  rebinding from web pages; a non-loopback address without a key prints an
  explicit warning. Run store I/O errors no longer return server file paths to
  clients, and `top_k` on `/v1/knowledge/retrieve` is capped at 100. Also in
  0.2.0.
- `Debug` output of `OtelConfig` (OTLP headers) and the MCP `StdioConfig`
  (`env`) / `HttpConfig` (headers, bearer token) shows names only, never values.
- Docs: new [docs/security.md](docs/security.md) (sandbox limits, egress
  policy, masking guarantees and their limits, deployment knobs); SECURITY.md
  now has a threat model, explicit scope and supported versions (0.2.x, 0.3.x).
- Dependencies: `rusqlite` 0.40 -> 0.41 (bundled SQLite 3.54), fixing
  RUSTSEC-2026-0459 / CVE-2026-119375 (uninitialized page-cache memory read
  through a crafted database and `-wal` pair) in `graphrt-rag`'s `SqliteStore`.
  `graphrt-rag` now needs Rust 1.89. `paste` (unmaintained, build-time only, via
  candle) is acknowledged in `deny.toml`. Also fixed in 0.2.1.

### Performance
- Engine, second round (shared outputs map, batched commits, one clock read
  per node, faster ids, direct template variables). Benchmark harness, Apple
  M3 Pro, against the first round below (p50, median of 21 alternating runs on
  a shared machine): 1,000-node chain 0.63 -> 0.40 µs per node, 1,000-way
  fan-out 0.99 -> 0.52 µs, 100-way fan-out 0.79 -> 0.46 µs, template chain
  1.35 -> 1.00 µs, branches 0.60 -> 0.44 µs, parallel iteration 1.25 -> 1.03
  µs, loop x iteration 1.20 -> 1.00 µs; allocations per node 9.1 -> 4.1 (noop
  chain), 10.9 -> 5.4 (fan-out), 16.7 -> 11.0 (template chain). Concurrent
  instant LLM runs (median of 15): 239k -> 282k runs/s at 1,000 in flight,
  206k -> 230k at 100; runs bound by a 50 ms model are unchanged.
- Engine: a node's outputs become its pool namespace and its `NodeSucceeded`
  payload without being copied (see **Changed (Breaking)**), string-literal
  output names no longer allocate, and namespace lookups scan a few entries
  instead of hashing with SipHash.
- Engine: nodes that have already finished are committed together before new
  nodes start. In a wide fan-out every commit used to copy the namespace table
  and a shard because a freshly started sibling held a snapshot of it; a batch
  of completions now shares one copy.
- Engine: the clock is read about once per node instead of twice. The time a
  finished node already read is reused for the next node's start and for any
  nodes skipped in the same step, and a batch of nodes started together shares
  one reading.
- Engine: execution ids are written as characters into a reused per-thread
  buffer, which drops the UTF-8 validation pass each id needed (same format).
- `template-transform`: string, number, boolean and null variables are handed
  to the template directly (a string shares its allocation) instead of
  through serde; other values still go through serde, which masks secrets.
- Engine: fewer allocations and less work per node execution. Node task
  futures are stored unboxed and are less than half their former size (584 ->
  264 bytes); a node that runs alone (chains, fan-ins) is polled without a
  `FuturesUnordered` task; the clock is read once per node start instead of
  twice; pool namespace lookups use a keyed FxHash-style hasher instead of
  SipHash; output names are shared across commits through a per-node cache
  (since replaced by `Variables`, whose literal names never allocate);
  the root pool's shards are sized from the graph. `template-transform`
  resolves its variables when the template reads them instead of building and
  converting a map per render. Benchmark harness, Apple M3 Pro, same machine
  before and after (p50, median of 11 runs): 1,000-node chain 0.74 -> 0.56 µs
  per node, 1,000-way fan-out 0.93 -> 0.86 µs, template node 1.38 -> 1.06 µs;
  allocations per node 12.2 -> 9.1 (noop chain), 13.1 -> 10.9 (fan-out),
  23.8 -> 16.7 (template chain). Concurrent instant LLM runs are unchanged
  (~285k runs/s at 11 workers): their cost is outside the per-node path.
- `bench/run.sh` records allocations per run and per node (`graphrt-bench
  allocs`).
- LLM path: fewer allocations and copies per model call and per streamed
  chunk. Chunks share their field name and move the provider's text instead
  of copying it; the `llm` node renders its prompt into one pre-sized message
  list, builds `usage` directly, moves the answer out of the response and
  builds its metadata once per node. Instant mock model, 10 chunks, Apple M3
  Pro (11 cores), against 7444368: 133 -> 103 allocations per run;
  -13% instructions per run (sequential and with 1,000 runs in flight);
  sequential latency 6.5 -> 5.8 µs. Throughput of runs whose events are
  discarded (`run_to_end`) is unchanged at 11 workers (~265k runs/s at 100 in
  flight, ~290k at 1,000): with the macOS system allocator it is bound by the
  allocator and by the efficiency cores, not by this work.
- `ResponseAssembler`: no allocation per chunk on the streaming path (stream
  buffers looked up by `&str`, template parts borrowed, only referenced
  outputs copied), and `push_into` lets consumers reuse one chunk buffer.
  Instant LLM run consumed through the assembler: 253 -> 161 allocations, -25%
  instructions, time to first answer chunk 11.1 -> 9.2 µs, concurrent runs
  +24% (161k -> 200k runs/s at 100 in flight, 171k -> 214k at 1,000).
- `graphrt serve`: live events are shared as `Arc<Event>` between the run
  feed, the broadcast channel and SSE listeners instead of deep-copied, and a
  streaming run's 4,096-slot channel holds 8-byte slots instead of ~256-byte
  events (it was over 1 MiB per streaming request). LLM workflow against a
  local instant OpenAI-compatible endpoint, 50 connections, macOS: synchronous
  32.7k -> 35.8k req/s, streaming 16.3k -> 21.8k req/s, server RSS after 40k
  requests 120 -> 39 MiB. Static musl build in a 16 MiB, 1-CPU container
  with 8 streaming clients and the default `--max-runs`: the server was
  OOM-killed before (15.1 MiB peak even with `--max-runs 100`); it now peaks at
  13.8 MiB (7.0 MiB with `--max-runs 100`), streaming 2.2k -> 3.6k req/s.

## [0.2.1] - 2026-11-19

Security patch release for 0.2.0: sandbox limits that could be switched off,
secret plaintext in error messages and code outputs, more SSRF address forms,
unbounded concurrent runs and DNS rebinding in `graphrt serve`, predictable
temp directories, and a SQLite CVE. No breaking API changes; upgrading is
recommended for every 0.2.0 user.

Upgrade notes:
- **MSRV:** `graphrt-rag` now needs **Rust 1.89** (was 1.85), because the
  patched SQLite is only in `rusqlite` 0.41, which declares Rust 1.89. No
  `rusqlite` 0.40.x or `libsqlite3-sys` 0.38.x release carries the fix (the
  advisory lists `libsqlite3-sys >= 0.39.0` as patched, and that needs Rust
  1.88). `graphrt-ffi` enables `graphrt-rag` (with `sqlite`) through its
  default `rag` feature, so a default `graphrt-ffi` build needs Rust 1.89 too.
  `graphrt-cli` already needed 1.89; the other crates keep their MSRV.
- `graphrt serve` refuses new and resumed runs with `429 Too Many Requests`
  while 256 runs execute (`--max-active-runs`, `0` = unlimited).
- `graphrt serve` without an API key on a loopback address answers `403` to
  requests whose `Host` is not a loopback name. If you put a reverse proxy in
  front of such a server, set an API key or have the proxy forward a
  loopback `Host`.
- Plugin manifests asking for `limits.max_message_bytes` above 256 MiB are
  rejected.

### Security
- `code` node (JavaScript): a workflow could switch off the sandbox's memory
  and stack limits by asking for `limits.memory_bytes: 0` or
  `limits.stack_bytes: 0` (QuickJS reads zero as "unlimited"), and unbounded
  recursion then overflowed the native stack and aborted the whole process
  (for example `graphrt serve`). An operator stack ceiling above 16 MiB had the
  same effect, and one above the executor's thread stack (2 MiB on tokio's
  blocking pool) crashed the process too. Limits now have floors (1 MiB heap,
  64 KiB stack) and a 16 MiB stack ceiling, and JavaScript runs on a dedicated
  thread sized from the stack limit, so deep recursion is always a catchable
  `RangeError`. The `graphrt-code` `tokio` feature no longer changes behaviour
  and is kept for compatibility.
- `code` node (JavaScript): error messages thrown by scripts are truncated to
  8 KiB, so a script cannot push megabytes into events and run records; an
  operator time limit too large for the clock no longer panics; a long chain of
  promise jobs is bounded by the time limit between jobs as well.
- Python subprocess runner: the working directory is created fresh and
  owner-only (`0700`) instead of adopting an existing directory at the
  predictable temp path.
- Engine: node error messages are scrubbed of every secret the run has seen
  (`type: secret` env variables, secret-typed inputs, re-supplied resume
  secrets, secret outputs) before they reach `node_failed` / `node_retry` /
  `run_failed` events, `error_message` outputs of handled errors, the run store
  and OTel spans. Previously an error that echoed a key (a provider body such
  as "Incorrect API key provided: sk-...", an HTTP error snippet from an echo
  endpoint, an exception in code) was recorded verbatim. New public helpers in
  `graphrt_core::redact` (`redact_secrets`, `SecretSet`, `MIN_SCRUB_LEN`).
- LLM providers: error bodies of failed provider requests are scrubbed of the
  credentials the request carried (`Authorization`, `x-api-key`,
  `x-goog-api-key`, `api-key` headers, `key` query parameters) before they
  become error messages. OpenAI-compatible gateways and self-hosted servers
  often echo the full key.
- `code` node: returned strings containing a secret input (`key`, or
  `"Bearer " + key`) are re-tagged as secrets, so they are masked in
  `node_succeeded` events, SSE, run records and snapshots while staying usable
  downstream; captured Python stdout is scrubbed. Previously they were stored
  in plaintext.
- HTTP egress policy: also blocks the IPv4-translated form `::ffff:0:a.b.c.d`
  (judged by the embedded IPv4 address), the local-use NAT64 prefix
  `64:ff9b:1::/48`, Teredo `2001::/32` and the Azure WireServer address
  `168.63.129.16`.
- OTel HTTP exporter: redirects are no longer followed, so vendor key headers
  cannot follow a redirect to another host.
- Bundles: `graphrt run/serve --bundle` extracted configs to a fixed,
  predictable directory in the shared temp dir and reused it if it existed, so
  another local user could pre-create it and plant symlinks or swap
  `providers.json` (e.g. pointing `base_url` at their server). Each invocation
  now extracts into a new owner-only (`0700`) directory. Signatures are checked
  with strict Ed25519 verification.
- Plugins: `limits.max_message_bytes` is capped at 256 MiB; WebAssembly
  plugins get at most 4 tables of 100,000 elements (was 8 x 1,000,000), which
  lived outside the `memory_bytes` budget.
- `graphrt serve`: new `--max-active-runs` (default 256, `0` = unlimited)
  caps runs executing at once; further starts and resumes get `429`.
  Previously `"async": true` requests could start unlimited parallel runs
  (`--max-runs` only bounds kept records). `AppState::with_max_active_runs`
  sets it for embedders (default unlimited there).
- `graphrt serve` without an API key on a loopback address now accepts only
  loopback `Host` names (`localhost`, `127.0.0.1`, `[::1]`), blocking DNS
  rebinding from web pages; a non-loopback address without a key prints an
  explicit warning. Run store I/O errors no longer return server file paths to
  clients, and `top_k` on `/v1/knowledge/retrieve` is capped at 100.
- `Debug` output of `OtelConfig` (OTLP headers) and the MCP `StdioConfig`
  (`env`) / `HttpConfig` (headers, bearer token) shows names only, never values.
- Dependencies: `rusqlite` 0.40 -> 0.41 (bundled SQLite 3.54.1), fixing
  RUSTSEC-2026-0459 / CVE-2026-119375 (uninitialized page-cache memory read
  through a crafted database and `-wal` pair) in `graphrt-rag`'s `SqliteStore`.
  This raises `graphrt-rag`'s MSRV to Rust 1.89 (see the upgrade notes).
  `paste` (unmaintained, build-time only, via candle) is acknowledged in
  `deny.toml`.
- Docs: new [docs/security.md](docs/security.md) (sandbox limits, egress
  policy, masking guarantees and their limits, deployment knobs); SECURITY.md
  now has a threat model, explicit scope and supported versions.

## [0.2.0] - 2026-11-19

Knowledge bases you can manage from an app, in-process embeddings, small-device
serving, and a much faster engine and server. Contains breaking API changes
(see **Changed**).

### Fixed
- `graphrt serve` without `--store` kept every run (record and events) in memory
  forever: under a 32 MiB limit the server was OOM-killed after ~1,800 runs. The
  in-memory store now keeps at most `--max-runs` runs (default 1000), dropping
  the oldest finished runs first and always keeping running and paused ones.

### Added
- `graphrt serve --max-runs <n>`; see docs/serving.md for small-device numbers.
- Knowledge base API in `graphrt serve` (with `--knowledge`): list datasets and
  documents (paged), add or replace documents from text or uploaded files
  (`--max-upload-bytes`, default 16 MiB), inspect a document's chunks, delete
  documents, and test retrieval, all on the knowledge base the workflows use.
  Ingestion runs one request at a time to keep small devices responsive.
- Documents can be disabled and re-enabled (`PATCH .../documents/{document}`,
  `KnowledgeBase::set_document_enabled`): a disabled document keeps its chunks
  and stays disabled across re-ingestion, but no search returns it (memory,
  SQLite and Qdrant stores; reserved chunk metadata key `_disabled`).
- Parent-child chunking per dataset (`chunking: { max_chars, parent_max_chars }`
  in the knowledge config, `KnowledgeBase::set_chunking`): small children are
  embedded and searched, and retrieval returns each matching parent once,
  rebuilt from its children (nothing extra stored). `ChunkerConfig` gains
  `parent_max_chars` and `TextChunk` gains `parent` (**breaking** for struct
  literals); children carry `parent_index` metadata.
- Knowledge base datasets can be created and deleted at runtime:
  `GET /v1/knowledge/resources`, `POST /v1/knowledge/datasets` and
  `DELETE /v1/knowledge/datasets/{dataset}`, backed by
  `KnowledgeBase::{create_dataset, remove_dataset, dataset_details,
  resource_names, runtime_datasets}`. New datasets name a store, embedder and
  reranker declared in the knowledge config; they are saved next to it
  (`kb.datasets.json`) and recreated on start by `serve`, `run` and `kb`.
- `graphrt-rag`: `VectorStore::list_documents` (memory and SQLite stores),
  `DocumentSummary`, `KnowledgeBase::{list_documents, document_chunks,
  delete_document, is_managed}`, `RagError::Unsupported`; `IngestReport` is
  serializable.
- `graphrt-rag`: `LocalEmbedder` (feature `local-embed`), sentence embeddings
  computed in-process with candle on the CPU: no embedding server, API key or
  network once the model is cached. BERT-family models from Hugging Face
  (`sentence-transformers/all-MiniLM-L6-v2` by default; `intfloat/multilingual-e5-small`
  for Indonesian and other languages) or a local model directory. Configured as
  `embedders: { local: { type: local, model: ... } }`. On by default in
  `graphrt-cli` (feature `local-embed`, about +2.1 MiB).
- `graphrt-ffi`: the SQLite knowledge store is available on mobile, so bundles
  using `stores: { ...: { type: sqlite } }` load on Android and iOS
  (built and linked for Android arm64; iOS not built yet).
- `graphrt plugin install <git-url> --subdir <path>` installs a plugin from a
  subdirectory of a git repository, so monorepos with one plugin per directory
  (such as `graphrt-plugins`) work. Absolute paths, `..` and symlinks are
  rejected; only that directory is copied. Library: `graphrt-plugin` adds
  `install_from_git_subdir` (`install_from_git` is unchanged).
- `graphrt-core`: `Registry::register_lazily_described`, which builds a node
  kind's catalog descriptor only when it is first requested.
- `graphrt-cli`: optional `mimalloc` feature to replace the global allocator.

### Changed
- **Breaking** (`graphrt-rag`): clones of a `KnowledgeBase` share their
  datasets (so datasets created through one clone are visible to all);
  `KnowledgeBase::datasets` returns `Vec<DatasetInfo>` and `chunking` returns
  an owned `ChunkerConfig`.
- **Breaking** (`graphrt-core`): the `node_id` and `exec_id` fields of every
  `Event` variant are `Arc<str>` instead of `String`, so the engine shares the
  ids it already holds instead of allocating two strings per event (every
  streamed chunk included). Serialized events are unchanged. Migrating:
  compare with `node_id.as_ref() == "llm"` instead of `node_id == "llm"`, use
  `node_id.to_string()` where a `String` is needed, and `.into()` when building
  an event from a `String`.
- **Breaking** (`graphrt-core`): `GraphNode::{kind, title}` and the `kind` and
  `title` of `Event::NodeStarted` are `Arc<str>` (shared, not copied per
  execution). Serialized events are unchanged.
- **Breaking** (`graphrt-core`): run stores keep events as serialized JSON:
  `RunStore::append_events` / `events` take and return `Vec<StoredEvent>`
  (`Box<serde_json::value::RawValue>`), built with `event_to_stored`;
  `stored_event_type` reads an event's type. The stored text is identical to
  what `event_to_json` produced, so `GET /v1/runs/{id}/events` is unchanged.
- **Breaking** (`graphrt-http`): `HttpClient::client` and
  `HttpClient::manual_redirect_client` return `Result<&reqwest::Client, ClientError>`;
  the four underlying clients are built on first use. `ClientError` is `Clone`.

### Performance
- Startup no longer loads the system trust store for HTTP clients a run never
  uses: LLM providers (`graphrt-llm`) and `graphrt-http` build their clients on
  the first request. `graphrt run hello.yaml` used to read every CA certificate
  six times (361 file opens, now 1).
- Built-in node kinds build their catalog descriptors lazily: node registry
  construction went from ~530 µs to ~7 µs.
- `template-transform` compiles its template once instead of on every run:
  ~20% less engine time per template node.
- Net effect, `graphrt run hello.yaml` (static musl binary, Linux arm64):
  10.1 ms → 6.5 ms wall clock, 3.1 ms → 0.6 ms user CPU.
- Fewer allocations per run and per node execution: a variable pool allocates
  its namespace shards on first write (an empty pool was 33 allocations, one per
  execution frame), namespace keys are reused instead of re-allocated on every
  write, node commits without variable writes skip the undo log, execution ids
  are written without `format!`, and `Arc<str>` ids are copied without going
  through `Display`. On the benchmark harness (Apple M-series, same machine
  before and after): 1,000 concurrent instant runs 63k → 82k runs/s (+31%),
  per-node scheduling 1.55 → 1.27 µs (noop chain) and 2.31 → 1.57 µs (1,000-way
  fan-out), time to first streamed chunk overhead 14.5 → 11.6 µs.
- Shared `Arc<str>` event ids (see **Changed**) took the same benchmark to
  94k runs/s (+49% over 0.1.0 in total), 1.06 µs per noop node and a 10.2 µs
  streaming overhead. The response assembler and the OpenTelemetry tracer key
  their maps by the shared ids too.
- Cheaper per-frame bookkeeping: index-keyed maps use a multiplicative hasher
  and are sized up front, and the scope's id set is built only when a node
  writes variables. Against 0.1.0 on the same machine: 0.87 µs per node on a
  1,000-node chain (was 1.55), 1.18 µs on a 1,000-way fan-out (2.31), 98k
  instant runs/s (63k); raw data in `bench/results/2026-11-1{8,9}-*`.
- `graphrt serve`: a run's 4,096-slot broadcast channel is created only when
  something subscribes to its events (it took ~30% of server CPU), and
  accepted connections set `TCP_NODELAY` (streamed responses no longer wait
  ~40 ms for a delayed ACK on Linux). Static musl, Linux arm64, 1 CPU:
  synchronous runs 2,875 -> 20,613 req/s, streaming 382 -> 3,392 req/s, async
  4,232 -> 20,499 req/s; macOS native synchronous 26k -> 46k req/s.
- Engine: fewer allocations per node execution (ids built on the stack,
  shared `Arc<str>` kind/title, outputs no longer copied into the pool), run
  events queued in a reusable buffer with an atomic waker instead of an mpsc
  channel, and stored events kept as JSON text (297 ns and 2.6 allocations per
  event, was 485 ns and 14.3). Per-node scheduling another 7-15% lower
  (0.73 µs on a 1,000-node chain).
- `bench/run.sh`: the harness's global allocation counter is enabled only in
  the sections that read it; it had capped the concurrency section at ~60-80k
  runs/s. With it gated the same machine measures ~300k runs/s for the llm
  graph and ~390k for a no-LLM graph at 11 workers (a measurement fix, not a
  runtime speed-up).

### Changed
- `graphrt_core::tool::ToolOutput` has a new `images` field and derives
  `Default`: struct literals must add `images: Vec::new()` or
  `..ToolOutput::default()` (`ToolOutput::text` / `ToolOutput::error` are
  unchanged). `graphrt_plugin_sdk::ToolResult` likewise gains `images`.
- Plugin SDKs (Python, TypeScript) answer `message_too_large` instead of
  writing a reply line above the host's `max_message_bytes`, which used to
  break the connection.

## [0.1.0] - 2026-11-18

First public release: one small runtime that runs agentic AI workflows on a
laptop, a server, a container, the edge or a phone.

### Added

**Runtime (`graphrt-core`)**
- Executor-agnostic engine: runs are a `Stream` of events driven by polling, no
  thread pool. Concurrent branches, OR-joins with skip propagation, branch handles.
- Per-node retries with exponential backoff and provider `retry-after`, per-attempt
  timeouts and deadlines, step / time / nesting limits, real cancellation on abort.
- Error strategies: `fail`, `fail-branch`, `default-value`.
- Iteration (parallel or sequential, three error modes) and loop containers run
  as child frames; pause and resume work inside them.
- Pause / resume with JSON snapshots (human input, tool approval, external pause),
  graph fingerprint checks, secrets never persisted.
- Reference-counted values and copy-on-write variable pools; typed values with a
  dedicated secret type masked in every serialization path.
- Workflow documents in YAML or JSON (YAML 1.2 booleans), validated at build time:
  ids, scopes, cycles, entry nodes and every variable reference.
- Built-in nodes: start, end, answer, if-else (full operator set), template-transform
  (sandboxed Jinja with fuel), variable assigner / aggregator, list operator,
  iteration, loop, human-input, tool.
- Ordered response assembler for streaming answers across branches and retries.
- Node catalog: descriptors with JSON Schema config, outputs and handles.
- File resolver contract (data URLs, local directories, policy-checked HTTP).
- Compiles to `wasm32-unknown-unknown`.

**Models and agents (`graphrt-llm`)**
- Providers: OpenAI-compatible (OpenAI, Azure, Groq, DeepSeek, OpenRouter, Mistral,
  Ollama, vLLM...), Anthropic, Google Gemini, AWS Bedrock (SigV4 or API key), and
  TypeSafe System One (Jev) for calibrated classification.
- Nodes: llm (streaming, memory, vision, structured output), question-classifier
  (LLM or Jev engine with confidence thresholds), parameter-extractor, agent
  (tool-calling loop, parallel tools, per-tool human approval), system-one.
- Credential sources for apps: per-request credentials, caching, token broker,
  401/403 refresh-and-retry; providers file with model aliases and defaults.

**Integrations**
- `graphrt-http`: http-request node with an SSRF-safe egress policy (private
  ranges, DNS rebinding, per-hop redirect checks, credential stripping).
- `graphrt-code`: JavaScript code node in QuickJS with memory / time / output
  limits; Python via a remote sandbox or an explicit local subprocess runner.
- `graphrt-mcp`: Model Context Protocol tools over stdio (clean environment) and HTTP.
- `graphrt-doc`: document-extractor for PDF, DOCX, PPTX, XLSX/ODS, EPUB, HTML,
  CSV, e-mail and subtitles, hardened against hostile files.
- `graphrt-rag`: knowledge-retrieval node, embedders, in-memory (optionally
  JSON-file backed), SQLite and Qdrant stores, hybrid vector + BM25 search,
  reranking, chunking, ingestion. The SQLite store keeps a knowledge base in
  one local file with incremental writes, shared safely between processes.

**Extensibility**
- `graphrt-plugin`: plug-and-play plugins over JSON-RPC (process or HTTP), with
  host-granted trust, clean environments, checksums and crash recovery.
- SDKs for plugins in Rust (`graphrt-plugin-sdk`), Python and TypeScript.
- `graphrt-wasm`: sandboxed WebAssembly plugins (wasmi, fuel and memory limits).

**Distribution**
- `graphrt` CLI: `run`, `validate`, `inspect`, `nodes`, `serve` (HTTP + SSE),
  `kb ingest|query`, `plugin list|install|remove|info|new`.
- Distroless container image, release binaries, `install.sh`.
- `graphrt-ffi`: UniFFI bindings for Kotlin (Android) and Swift (iOS), with
  host credential providers and `load_bundle`.
- `graphrt-bundle`: portable `.grt` bundles (workflow, WebAssembly plugins,
  configs) with per-file SHA-256 and optional Ed25519 signatures;
  `graphrt bundle build|inspect|verify|sign|keygen`, `graphrt run app.grt`.
  Building refuses a SQLite store whose write-ahead log still holds commits.

**Operations**
- `graphrt-otel`: OpenTelemetry traces and metrics over OTLP/HTTP (`--otel`,
  standard `OTEL_*` variables, W3C `traceparent` continuation).
- Durable runs for `graphrt serve` (`--store`): resume by run id, async runs,
  event replay, restart-safe paused runs.
- Reproducible benchmarks in `bench/` (docs/benchmarks.md).

### Security
- Three rounds of security review across the engine, nodes, providers,
  credentials, plugins, WebAssembly runtime, document extraction and retrieval,
  with regression tests for every finding.

[Unreleased]: https://github.com/BimaPangestu28/graphrt/compare/v0.5.0...HEAD
[0.5.0]: https://github.com/BimaPangestu28/graphrt/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/BimaPangestu28/graphrt/compare/v0.3.2...v0.4.0
[0.3.2]: https://github.com/BimaPangestu28/graphrt/compare/v0.3.1...v0.3.2
[0.3.1]: https://github.com/BimaPangestu28/graphrt/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/BimaPangestu28/graphrt/compare/v0.2.1...v0.3.0
[0.2.1]: https://github.com/BimaPangestu28/graphrt/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/BimaPangestu28/graphrt/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/BimaPangestu28/graphrt/releases/tag/v0.1.0
