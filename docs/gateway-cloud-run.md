# Gateway on Google Cloud Run

> Status: preview. The crate `graphrt-gcp` ships the GCP auth module and the
> Cloud Run provider and deploy driver; `graphrt gateway` uses them with
> `--features gateway-cloud-run` (`kind: cloud_run` under `providers:`) and
> `graphrt deploy --target cloud-run` deploys versions. See
> [docs/gateway.md](gateway.md#providers).

The gateway is the only always-on process. Agents (`graphrt serve
--trust-gateway`) run as a Cloud Run service with **min instances 0**: Cloud
Run starts an instance when the gateway sends a request and scales back to
zero when no request is open.

## Cargo features

| Feature | Crate | What it adds | Measured Δ (min-size, macOS arm64) | Budget (plan §9) |
| --- | --- | --- | --- | --- |
| `auth-gcp` | `graphrt-gcp` | Metadata-server tokens, service-account JWT (RS256 with `ring`) → OAuth 2.0 token endpoint, ID tokens | 48.8 KiB | 60 KiB |
| `provider-cloud-run` | `graphrt-gcp` | Run-time Cloud Run `Provider` (wake, list, invoker ID tokens). Implies `auth-gcp`. The gateway's `gw-cloudrun` | 48.8 KiB on top of `auth-gcp` | 80 KiB |
| `deploy-cloud-run` | `graphrt-gcp` | `DeployDriver` (Admin API v2 `services.patch`, operations, tag retirement) and Secret Manager writes. Implies `provider-cloud-run`. Deploy time only (`graphrt deploy`), never in the gateway build | 65.0 KiB on top of `provider-cloud-run` | – |

The deltas are measured with the probe binaries in `crates/graphrt-gcp/examples`
(`cargo build -p graphrt-gcp --profile min-size --features deploy-cloud-run --examples`):
`size_baseline` links what the gateway has anyway (tokio, reqwest + rustls,
serde_json, `ProviderHttp`), and each next probe adds one feature's calls.

No Google SDK: every call is hand-written REST over the `reqwest` + `rustls`
(ring) stack graphrt already links. `cargo tree` must never show
`google-cloud-*`, `gcp_auth`, `yup-oauth2` or `jsonwebtoken`.

## Credentials (`auth-gcp`)

`GcpAuth::from_env()` picks the source the way Google's client libraries do:

1. `GOOGLE_APPLICATION_CREDENTIALS` names a **service-account key file**. graphrt
   signs a JWT assertion (`iss` = the key's e-mail, `aud` = its `token_uri`,
   lifetime one hour) with RS256 and exchanges it at
   `https://oauth2.googleapis.com/token` for an access token (scope
   `cloud-platform`), or, with a `target_audience` claim, for an ID token.
2. Otherwise the **metadata server** (`GCE_METADATA_HOST`, default
   `metadata.google.internal`) of Compute Engine, Cloud Run or GKE (workload
   identity) hands out the attached service account's tokens:
   `…/service-accounts/default/token` and `…/identity?audience=…&format=full`.
   The response must carry `Metadata-Flavor: Google`; the metadata client never
   uses `HTTP(S)_PROXY`.

Other credential types (`authorized_user`, `external_account` / workload
identity federation) are refused with a clear error for now.

Caching rules (conformance C8):

- Every token is cached until **`exp − 5 min`**, measured on the local
  monotonic clock from when it was fetched (`expires_in`, or `exp − iat` of an
  ID token). A skewed wall clock cannot make a cached token look fresh.
- Concurrent callers share **one fetch** per token (the access token, and one
  ID token per audience).
- After a **401** (Admin API) or **401/403** (Cloud Run invoke) the token is
  refreshed **once**; a second rejection is an `Auth` error.
- A local clock **±5 min** off is corrected once: when the token endpoint
  rejects the assertion's `iat`/`exp`, graphrt reads the endpoint's `Date`
  header, remembers the offset and signs again.
- Tokens are `SecretString`s (`Debug` prints `***`). No error message carries
  a token, an assertion or key material.

## The provider

| Capability | Value | Why |
| --- | --- | --- |
| Addressing / scaler | `Url` / `Platform` | Cloud Run routes and scales; `maxInstanceRequestConcurrency = max_active_runs` |
| `max_request_secs` | 3600 (configurable lower) | Cloud Run request cap; the gateway preempts at cap − 60 s and re-dispatches |
| `max_grace_secs` | 10 | SIGTERM → SIGKILL after 10 s, not configurable: drain always **preempts** |
| `background_cpu` | `false` | request-based billing (`cpuIdle = true`) |
| `wake_p95_target_ms` | 2000 | startup CPU boost on |

- **wake** resolves the version's **tag URL** from `services.get` (cached 60 s;
  never from the agent), then sends one `GET /healthz` with
  `X-Serverless-Authorization: Bearer <ID token>`. That request is what makes
  Cloud Run start an instance from zero. Twenty concurrent wakes share one
  probe (C1). A 429 is `Capacity` (max instances reached or no regional
  capacity) and is not retried; 5xx and network errors are retried with
  jittered backoff.
- **release** is a no-op: Cloud Run scales in by itself once no request is open.
- **list** reports the single pseudo-replica `url` of a version (empty when the
  service or the version's tag is gone).
- `invoke_authorization(deployment)` returns the header the gateway adds to
  every request it sends to the agent. Cloud Run validates the ID token and
  removes its signature before the container, so it never mixes with the
  agent's `Graphrt-Sig` HMAC.
- `service_status(deployment).drift_warnings(min)` reports settings changed by
  hand that cost money or slow wakes (`minInstanceCount`, `cpuIdle`,
  `startupCpuBoost`).

### Billing caveat: the gateway holds every stream

With request-based billing an instance only has CPU while a request is open.
graphrt never relies on CPU after a response: **the gateway holds the stream of
every run (sync, async, webhook, cron) for the run's whole life**, so Cloud Run
bills each run as one open request and keeps its CPU. Consequences:

- A run costs request time for its full duration, including model wait time.
  Many long, mostly-idle runs can be cheaper on instance-based billing; graphrt
  still deploys `cpuIdle = true` because idle deployments then cost nothing.
- Nothing may run in the agent after its stream closes (no background flush);
  the gateway writes the store.
- Cloud Run caps one request at 60 minutes. Longer runs are preempted at
  `request_timeout_secs − 60 s` (snapshot before the next node) and
  re-dispatched to a fresh request: one node may run twice per segment
  (decision 1, at-least-once per node with `Idempotency-Key`).
- Shutdown grace is 10 s, too short to let a model call finish, so drain on
  SIGTERM always preempts.

## Deploying (`CloudRunDeployDriver`)

`apply(spec)` renders one Cloud Run Admin API v2 `Service` and sends
`PATCH /v2/projects/{p}/locations/{r}/services/{id}?allowMissing=true`
(create-or-update in one call), then polls the long-running operation and
checks that the new revision is the latest ready one.

| Spec field | Cloud Run |
| --- | --- |
| `name` | service id `graphrt-<name>` (DNS label, ≤ 40 chars) |
| `version` | revision `<service>-<tag>` and traffic tag `<tag>` (`v7`, ≤ 20 chars) |
| `image`, `command` | `containers[0].image`, `command` = first word, `args` = the rest; port 8080 (`http1`) |
| `env` | plain `env` values |
| `secrets` | `valueSource.secretKeyRef {secret, version}`; only `projects/<p>/secrets/<s>[/versions/<v>]` references are accepted |
| `max_active_runs` | `maxInstanceRequestConcurrency` (1–1000) |
| `min_replicas` / `max_replicas` | `scaling.minInstanceCount` (default 0) / `maxInstanceCount` |
| `resources` | `limits.cpu` (`<1 vCPU` needs `max_active_runs = 1`), `limits.memory` |
| `grace_secs` | ignored: Cloud Run's grace is fixed at 10 s |
| always | `cpuIdle = true`, `startupCpuBoost = true`, `timeout = request_timeout_secs`, labels `managed-by=graphrt`, a custom audience `graphrt/<p>/<r>/<service>` |

`provider_settings` accepts exactly: `service_account` (runtime identity of the
agent), `ingress` (`all` default, `internal`, `internal-and-cloud-load-balancing`),
`startup_cpu_boost` (default `true`), `execution_environment` (`gen1`|`gen2`).
Unknown keys are an error. Endpoints are operator configuration
(`CloudRunConfig`), never spec settings.

Re-applying a version that is already the ready latest revision is a no-op.
Versions are immutable: change the version to change the spec.

### Old versions (decision 6)

A new version takes 100 % of new traffic under its tag; every older tag stays
on the service at 0 % and keeps its own URL (`https://v6---…run.app`), with
min instances 0. A run paused on version 6 is resumed on revision 6. After 7
days the gateway calls `retire_versions(deployment, ["v6"])`, which drops the
tag (`updateMask=traffic`); a later wake of v6 is `NotFound`, which the gateway
answers with `410 {error_type: version_retired}`. The version serving new
traffic cannot be retired.

### Secrets

`SecretManager::store_secrets(prefix, values)` (deploy time) creates each
secret with automatic replication when missing and adds the value as a new
version, returning pinned references (`…/versions/3`) for the spec. The value
goes only to Secret Manager; the Admin API, the spec and the gateway see the
reference (C10).

## IAM roles

| Who | Roles | For |
| --- | --- | --- |
| Gateway (run time) | `roles/run.viewer` on the services (or project) | `services.get`: tag URLs, readiness, drift |
| Gateway (run time) | `roles/run.invoker` on each service | ID-token invocations (`/healthz`, runs) |
| `graphrt deploy` / CI (deploy time) | `roles/run.developer` | `services.patch` (create/update), `operations.get` |
| `graphrt deploy` / CI | `roles/iam.serviceAccountUser` on the agent's runtime service account | deploying a revision that runs as it (`actAs`) |
| `graphrt deploy` / CI | `roles/secretmanager.admin` (or `roles/secretmanager.secretVersionAdder` on pre-created secrets) | `secrets.create`, `secrets.addVersion` |
| Agent runtime service account | `roles/secretmanager.secretAccessor` on its secrets | Cloud Run resolves `secretKeyRef` at instance start |
| Cloud Run service agent | `roles/artifactregistry.reader` (only for an image in another project) | image pull |
| Operator, once | `roles/run.admin` (or `run.services.setIamPolicy`) | granting the gateway `run.invoker` with `gcloud run services add-iam-policy-binding` |

The gateway never holds deploy-time roles and never sees a secret value.

## Conformance

`cargo test -p graphrt-gcp --features deploy-cloud-run` runs the shared
`graphrt-provider-api` harness (`tests/cloud_run_harness.rs`) and the Cloud
Run-specific C1–C12 checks (`tests/cloud_run_conformance.rs`; C8 also runs
for `auth-gcp` alone) against fake Google APIs (metadata server, token
endpoint that verifies the RS256 signature, Admin API v2, Secret Manager) and a
fake Cloud Run instance. C13 is the size table above (the harness checks the
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
| C8 | token caching, refresh before exp, ±5 min skew, 401/403 refresh once, log scan | auth rules above |
| C9 | 503 ×2, 429 + `Retry-After`, invoke 429, Admin API bucket | jittered backoff, `Capacity` not retried, quota |
| C10 | Secret Manager writes + deploy | only references reach the Admin API; plaintext refs refused |
| C11 | deploy v1, v2; wake both; retire v1 | old tag wakeable, then `NotFound` |
| C12 | rendered scaling + drift + fresh provider | min 0, `cpuIdle`, drift warnings, nothing to clean after `kill -9` |

### Real-GCP verification (nightly, needs a sandbox project)

The fakes follow Google's documented API shapes; these need a real project
before the provider is marked stable:

1. Cloud Run accepts ID tokens whose `aud` is the **custom audience**
   `graphrt/<p>/<r>/<service>` on the service URL **and on tag URLs**.
2. `X-Serverless-Authorization` is accepted and its signature stripped before
   the container; the gateway's `Graphrt-Sig` header passes through untouched.
3. `services.patch?allowMissing=true` creates a missing service; the
   long-running operation's `response` is the service; `validateOnly` for dry runs.
4. Revision names `<service>-<tag>` are accepted, and a re-apply of the same
   version is a no-op; Cloud Run's error when a version is re-applied with a
   different template.
5. Traffic: 0 % `REVISION` targets keep their tags and `trafficStatuses[].uri`;
   `updateMask=traffic` drops a tag without a new revision.
6. **C2** cold start p95 ≤ 2 s from zero with startup CPU boost (n ≥ 50, small
   image), and the warm-path overhead.
7. **C3** SIGTERM → SIGKILL in 10 s with preempt + `POST /internal/v1/orphans`
   inside the grace (needs F0a drain + F0b protocol).
8. **C6** SSE through the Cloud Run front end with chunks ≤ 1 KiB: no
   buffering, 15 s heartbeats keep the stream; **C7** behaviour at the 60-min cap
   and the planned preempt at 3540 s.
9. **C9** 429 when `maxInstanceCount` is saturated (mapped to `Capacity`);
   Admin API quota under a wake storm; 403 `RESOURCE_EXHAUSTED` handling.
10. Request-based billing: CPU throttled after the response; a held stream keeps
    the run's CPU (billing report of a test run).
11. **C8** the token endpoint's clock-skew error text and `Date` header match
    the correction heuristic; metadata `identity?format=full` on Cloud Run, GCE
    and GKE workload identity.
12. Secret Manager: `addVersion` names versions with the project **number**
    (graphrt rewrites to the project id); `secretKeyRef` with a pinned version
    resolves for the runtime service account.
13. **C12** time from last request to zero instances (platform-controlled,
    typically up to ~15 min), and that no graphrt-created resource outlives a
    gateway `kill -9` (none is created at run time).
14. Workload identity federation for CI (`external_account` credentials) is
    not implemented yet; the nightly job needs it or a short-lived key.
