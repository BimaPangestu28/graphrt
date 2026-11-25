# Provider conformance tests

For contributors. Every cloud provider crate runs the shared C1–C13 suite of
`graphrt-provider-api` (`run_conformance`) plus its own provider-specific
checks. These tests run per PR against local fake servers that model each
cloud API; they prove the provider's logic (retries, auth refresh, version
pools, secrets handling) but not the behaviour of the real platform. What
still needs a real account is listed on each provider's page under
`docs/`.

The gateway half (drain and re-dispatch, long stream, request cap, 7-day
version pool, idle release) runs every provider in
`crates/graphrt-gateway/tests/provider_harness.rs`.

## Fly Machines (`graphrt-provider-fly`)

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
| C13 size | measured separately, see `docs/gateway-fly.md#size` |

Fly-specific tests cover:

- suspended before stopped;
- the suspend → stop fallback, and remembering it;
- `SIGTERM` + `300s` on stop;
- capacity fallback and pool growth;
- version isolation, and waking an N−1 pool;
- refusing apps that are not configured;
- deploy idempotency, secrets sent only to `/secrets`, `fly:` references
  checked, and retire.

Direct deploys (`graphrt deploy --target fly --direct`) run against the fake
Machines API in `crates/graphrt-provider-fly/src/fake.rs`:
`cargo test -p graphrt-provider-fly --test fly_direct` and
`cargo test -p graphrt-cli --test deploy_direct`.

## Azure Container Apps (`graphrt-provider-azure`)

`cargo test -p graphrt-provider-azure --features deploy-aca` runs two suites.

The shared harness of `graphrt-provider-api` (`tests/harness.rs`) drives the
provider against its `FakeServer` (managed identity endpoint, ARM, ingress)
with its own fault injection:

```text
C1   pass  20 concurrent wakes answered (platform scaler)
C2   pass  p95 0.0 ms (target 10000 ms, n=50)
C3   skip  needs the gateway harness (graphrt-gateway)
C4   pass  release is a no-op for Url addressing
C5   skip  Url addressing: the platform replaces instances
C6   skip  needs the gateway harness (graphrt-gateway)
C7   skip  needs the gateway harness (graphrt-gateway)
C8   pass  401 → one refresh, no credentials in logs
C9   pass  429/5xx retried with backoff, 400 not retried
C10  pass  17 log lines / requests scanned
C11  skip  needs the gateway harness (graphrt-gateway)
C12  pass  release → 0 live replicas
C13  pass  gw-aca: Δ 48 KiB, budget 80 KiB   (with GRAPHRT_SIZE_DELTA_GW_ACA from measure-size.sh; skipped without it)
```

The Azure suite (`tests/conformance.rs`, `tests/idle_timeout.rs`) adds a
fuller fake of Entra, managed identity, IMDS, ARM (with long-running
operations), Key Vault and a scale-from-zero ingress (`tests/common/mod.rs`),
and covers the provider half of the cases the shared harness leaves to the gateway:

| ID | Test | Against the fakes |
| --- | --- | --- |
| C1 | 20 concurrent wakes from 0 | 1 token, 1 ARM read, 1 warm-up, 1 replica |
| C2 | wake latency, n = 50 | p95 25.6 ms with a 20 ms fake cold start (provider overhead ≈ 5 ms); target 10 s |
| C3 | grace | `terminationGracePeriodSeconds` 120 rendered and reported; > 600 rejected |
| C4 | scale-in | `release` makes no Azure call; no stream cut |
| C5 | replica killed outside the gateway | `list` drops it; the next wake brings one back |
| C6 | long stream, idle-timeout ingress | see `docs/deploy-azure.md#ingress-timeouts-and-long-streams` |
| C7 | request cap | `Standard` → preempt at 180 s; `Premium`/verified → none |
| C8 | auth | refresh 5 min before `exp`; ±5 min skew; 401 → one refresh, a second 401 is an `Auth` error; all five credential kinds; no secret or token in `Debug`, errors or request lines |
| C9 | API errors | 429/503/500 retried with backoff; persistent 429 → `RateLimited` after 4 attempts; quota codes → `Capacity` |
| C10 | secrets | only Key Vault references reach ARM; clear secret env values rejected without echoing them |
| C11 | old-version pool | version N-1 wakes on day 6; retired on day 8 → `NotFound` (410) |
| C12 | idle and leaks | `minReplicas 0`; stray revisions deactivated by reconciliation |
| C13 | binary size | `crates/graphrt-provider-azure/measure-size.sh`, see `docs/deploy-azure.md#binary-size` |

## Google Cloud Run (`graphrt-gcp`)

`cargo test -p graphrt-gcp --features deploy-cloud-run` runs the shared
`graphrt-provider-api` harness (`tests/cloud_run_harness.rs`) and the Cloud
Run-specific C1–C12 checks (`tests/cloud_run_conformance.rs`; C8 also runs
for `auth-gcp` alone) against fake Google APIs (metadata server, token
endpoint that verifies the RS256 signature, Admin API v2, Secret Manager) and a
fake Cloud Run instance. C13 is the size table in `docs/gateway-cloud-run.md` (the harness checks the
measured `gw-cloudrun` delta against its budget).

| ID | Fake-server test | Covers |
| --- | --- | --- |
| C1 | 20 concurrent wakes from zero, 300 ms cold start | one `/healthz`, one `services.get`, one ID token |
| C2 | 50 wakes | wake overhead p95 (fake, warm instance) |
| C3 | capabilities + rendered spec | grace 10 s → preempt; no grace field sent |
| C4 | `release` during an open stream | no API call, stream completes |
| C5 | service deleted out of band | `list` empty, `NotFound` |
| C6 | 200 events of 1 KB, 10 ms apart, through the invoke auth path | delivery lag p95 ≤ 50 ms, no buffering |
| C7 | timeout 3600 / 900 | `max_request_secs`, rendered `timeout`, preempt at cap − 60 |
| C8 | token caching, refresh before exp, ±5 min skew, 401/403 refresh once, log scan | auth rules in `docs/gateway-cloud-run.md` |
| C9 | 503 ×2, 429 + `Retry-After`, invoke 429, Admin API bucket | jittered backoff, `Capacity` not retried, quota |
| C10 | Secret Manager writes + deploy | only references reach the Admin API; plaintext refs refused |
| C11 | deploy v1, v2; wake both; retire v1 | old tag wakeable, then `NotFound` |
| C12 | rendered scaling + drift + fresh provider | min 0, `cpuIdle`, drift warnings, nothing to clean after `kill -9` |

## AWS ECS Fargate and Lambda (`graphrt-provider-aws`)

Every case runs per PR against fake AWS endpoints (ECS JSON 1.1, Lambda event
stream, STS, IMDSv2, container credentials) that verify each SigV4 signature
and the ±5 min clock window: `cargo test -p graphrt-provider-aws --features provider-ecs,provider-lambda`.

| Case | ECS Fargate | Lambda |
| --- | --- | --- |
| C1 20 concurrent wakes | pass (exactly the requested tasks) | pass (no-op) |
| C2 wake latency n=50 | pass on the fake; **real p95 needs Fargate** | pass (no-op); real cold start needs AWS |
| C3 grace / SIGTERM | `StopTask` + `stopTimeout ≤ 120` checked; migration needs the gateway | n/a (no grace, no preempt) |
| C4 scale-in | pass (only the chosen task stops) | pass (no-op) |
| C5 external kill | pass (one `list` sees it; replaced on next wake) | n/a (`Url`) |
| C6 long stream, no buffering | n/a (gateway talks to the task IP) | pass on the fake (per-event overhead, buffering detection); **real 14-min stream needs AWS** |
| C7 request cap | n/a (no cap) | pass (`platform_timeout` from Lambda and from the gateway cap) |
| C8 auth | pass (refresh before exp, 403 → refresh once, skew ±5 min, no secrets in errors/`Debug`) | pass |
| C9 API errors | pass (throttle/5xx backoff, `RateLimited`, `Capacity`, no doubled tasks) | pass (throttle retried, 5xx not) |
| C10 secrets | pass (no credential in `Debug`/errors) | pass |
| C11 old-version pool | pass (version N−1 starts its own revision); day-6/day-8 needs the gateway | qualifier per version |
| C12 orphans | pass (tasks of a killed gateway listed and stopped) | n/a |
| C13 size | see `docs/gateway-aws.md#binary-size-c13` | same |

The deploy drivers are tested end to end on the fakes
(`cargo test -p graphrt-provider-aws --features deploy`): ECS registers a
revision with secrets as `valueFrom` and `stopTimeout`, and `EcsProvider`
starts exactly that ARN; Lambda creates, waits out `InProgress`, updates,
publishes, aliases, adds the streaming IAM Function URL, and `LambdaProvider`
invokes `function:alias`; re-apply is a no-op, a reused version name with new
code is refused, a create race falls back to update, secrets are refused
unless `resolve-to-env`, and invalid specs make no AWS call.

## Kubernetes (`graphrt-provider-k8s`)

`cargo test -p graphrt-provider-k8s` runs:

- the shared suite of `graphrt-provider-api` (`run_conformance`, C1–C13)
  against an emulated cluster behind the harness's scripted fake server, with
  its fault injection (401, 429, 5xx, 400) and request scan;
- deeper Kubernetes checks against a streaming fake API server that models
  scale CAS, the ReplicaSet victim order, EndpointSlice watches with
  `410 Gone`, server-side apply, token checks, capacity (unschedulable) and
  ResourceQuota (`ReplicaFailure`).

| Check | What is verified |
| --- | --- |
| C1 | 20 concurrent wakes from zero: one scale patch, exactly 1 replica |
| C2 | 50 wakes from zero: the provider's own latency on top of pod readiness |
| C4 | scale-in removes only the released replica; idempotent; 409 conflicts retried |
| C5 | a pod killed outside the gateway is seen by the readiness watch, also across a `410 Gone` relist |
| C8 | a rejected token is refreshed exactly once; revoked credentials give `Auth`; no token in errors, `Debug` or requests |
| C9 | 503/429/500 retried with backoff, 400/403 not retried; unschedulable and exceeded quota are `Capacity` |
| C10 | the applied Deployment carries Secret references only; re-deploy keeps replicas |
| C12 | `list` reports every replica for reconciliation; releasing from the list reaches zero |

C3, C6, C7 and C11 exercise the gateway's run supervisor (drain, long streams,
request caps, the version pool) and are reported as skipped until the
gateway's harness exists. C13 is the binary-size gate.
