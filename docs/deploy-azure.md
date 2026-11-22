# Azure: Container Apps provider and Entra ID auth

The `graphrt-provider-azure` crate is the Azure family of the graphrt gateway
providers (see the gateway plan: one always-on gateway, stateless agents that
scale 0→N). `graphrt gateway` uses it with `--features gateway-aca`
(`kind: aca` under `providers:`), and `graphrt deploy --target aca` deploys
versions; see [docs/gateway.md](gateway.md#providers). It has three cargo features:

| Feature | What it adds | Budget (plan §9) |
| --- | --- | --- |
| `auth-azure` | Entra ID tokens: managed identity, client secret, client certificate (PS256 assertion), workload identity federation; per-scope cache | 60 KiB |
| `provider-aca` | Run-time Azure Container Apps provider (`azure-container-apps`) for the gateway: label URL per version, warm-up, replica listing over ARM REST. Implies `auth-azure` | 80 KiB |
| `deploy-aca` | Deploy-time half for `graphrt deploy`: `DeployDriver` (createOrUpdate, revision labels, retirement of old versions) and Key Vault references / set-secret. Implies `provider-aca` | CLI only, no gateway budget |

No Azure SDK: about ten REST calls over the `reqwest`/`rustls`/`ring` the
workspace already links. Measured sizes are at the end of this page.

> Status: the provider implements the `Provider` / `DeployDriver` traits of
> `graphrt-provider-api` and passes the shared conformance harness
> (`run_conformance`) plus Azure-specific C1–C13 tests against fake Azure
> servers. The gateway and `graphrt deploy` wiring land separately; until then
> the crate is a library only. Items that only real Azure can prove are listed under
> [What still needs real Azure](#what-still-needs-real-azure).

## How a deployment maps to Container Apps

| graphrt | Container Apps |
| --- | --- |
| deployment `bot`, version `vN` (or `N`) | container app `bot`, revision `bot--vN` (`revisionSuffix: vN`; a version must fit a revision label: ≤ 30 of `[a-z0-9-]`, a leading digit gets a `v`) |
| the version's address | traffic label `vN` → `https://bot---vN.<env domain>` (internal ingress) |
| current version | label `vN` at 100 % of the app's traffic |
| version N-1 for paused runs (7 days) | label `vN-1` at 0 %, `minReplicas 0`: woken only by its label URL |
| scale 0→N | HTTP scale rule, `concurrentRequests = max_active_runs`, `minReplicas 0` |
| drain grace | `terminationGracePeriodSeconds` (default 120, max 600) |
| secrets | `configuration.secrets[].keyVaultUrl` + `identity`, env `secretRef` |

The app uses `activeRevisionsMode: Multiple`, internal ingress only
(`external: false`: only the gateway reaches it) and `transport: http`
(HTTP/1.1, which is what `graphrt serve` speaks).

### Provider behaviour

The provider reports `Addressing::Url` and `Scaler::Platform`:

- `wake` resolves the version's label URL from ARM once (cached), then sends
  one warm-up request to the agent's `/healthz`, which makes the platform start
  a replica. Concurrent wakes for the same URL share that one request (C1: 20
  wakes → 1 ARM read, 1 warm-up, 1 replica). A version without a label answers
  `ProviderError::NotFound`; the gateway turns that into
  `410 {error_type: version_retired}`.
- `release` is a no-op: the platform scales in after the HTTP rule's cooldown,
  and no stream is cut by the gateway.
- `list` returns the replicas of active revisions, for reconciliation and metrics.

Throttling and failures: every ARM call goes through a token bucket (10/s,
burst 40 by default), retries 429/5xx/timeouts with full-jitter backoff that
honours `Retry-After`, refreshes the token once on 401, and maps failures to
`Capacity` (quota codes), `RateLimited`, `Auth`, `NotFound`, `Transient` or
`Fatal`. Long-running operations are polled through `Azure-AsyncOperation`
(or `Location`); poll URLs outside the configured ARM endpoint are refused, so
a token is never sent to another host.

### Old versions (7-day pool)

A deploy of version N keeps every labelled older version at weight 0.
`retire_old_versions` (feature `deploy-aca`: on `AcaDeployDriver`, run after
each deploy, or on the provider if the gateway is built with `deploy-aca` and
has the optional write role) removes the label of
version N-1 and deactivates it once N is older than the retention window
(default 7 days), and deactivates stray unlabelled revisions left by a failed
deploy. The gateway enforces `410 version_retired` from its own version records,
so a read-only gateway is still correct; retirement only stops paying for
revisions nobody can reach.

## Ingress timeouts and long streams

Microsoft documents the standard ingress limit only as "Request time out is
240 seconds", without saying whether it counts idle time or the whole request.
Premium ingress has an explicit idle setting
(`ingressConfiguration.requestIdleTimeout`, 4–30 minutes, on the managed
environment). The provider therefore offers three modes (`IngressMode`), and
the gateway plans heartbeats and preempts from the resulting capabilities:

| Mode | `idle_timeout_secs` | `max_request_secs` | Gateway behaviour |
| --- | --- | --- | --- |
| `Standard` (default) | 240 | 240 | 15 s heartbeats, and a planned preempt + re-dispatch at 180 s (cap − 60 s). Each node of a run must finish within 180 s |
| `StandardIdleVerified` | 240 | none | Heartbeats only. Use after the real-Azure C6 test passes in your region |
| `Premium { request_idle_timeout_minutes }` | 4–30 min | none | Heartbeats only |

What is proven: `tests/idle_timeout.rs` puts a proxy with a 300 ms idle
timeout between a client and an SSE agent that sends events 400 ms apart.
With heartbeats every 100 ms (the same "two heartbeats per idle window" bound
as 15 s against 240 s) the stream outlives the timeout fourfold with every
event delivered once and in order; without heartbeats it is cut. Through the
proxy, 60 events of 1 KiB arrive with no buffering and no added p95 latency
(1.1 ms vs 1.6 ms direct). Whether Azure's 240 s is idle-only is proven by the
ignored test `c06_real_azure_stream_outlives_240s` against a deployed app.

## Authentication (`auth-azure`)

`AzureCredential::from_env` picks the first credential the environment
describes, in the order the Azure SDKs use:

| Credential | Environment | Request |
| --- | --- | --- |
| Workload identity federation (AKS, GitHub OIDC) | `AZURE_FEDERATED_TOKEN_FILE`, `AZURE_TENANT_ID`, `AZURE_CLIENT_ID`, optional `AZURE_AUTHORITY_HOST` | client-credentials with the token file as `client_assertion` (re-read on every exchange, so rotation is picked up) |
| Client secret | `AZURE_TENANT_ID`, `AZURE_CLIENT_ID`, `AZURE_CLIENT_SECRET` | client-credentials |
| Client certificate | `AZURE_TENANT_ID`, `AZURE_CLIENT_ID`, `AZURE_CLIENT_CERTIFICATE_PATH` (PEM with the certificate and an unencrypted RSA key, PKCS#8 or PKCS#1) | client-credentials with a PS256 assertion carrying `x5t#S256` |
| Managed identity (Container Apps, App Service) | `IDENTITY_ENDPOINT`, `IDENTITY_HEADER`, optional `AZURE_CLIENT_ID` | `GET $IDENTITY_ENDPOINT?api-version=2019-08-01&resource=…` with `X-IDENTITY-HEADER` |
| Managed identity (VM, VMSS, AKS node) | none | IMDS `GET http://169.254.169.254/metadata/identity/oauth2/token`, `Metadata: true`; 404/410/429/5xx retried while IMDS starts |

The authority host (default `https://login.microsoftonline.com`), the IMDS
address, the ARM endpoint and the allowed Key Vault host suffixes come from the
gateway operator's configuration, never from a deployment spec.

Tokens are cached per scope until `exp − 5 min` and refreshed by one request
even when many callers wait. Expiry is taken from `expires_in` (relative) when
present, so a skewed local clock cannot stretch a token's life; with an
absolute `expires_on` only (managed identity), a token the API already rejects
is dropped on its first 401 and fetched again once. Tokens, client secrets and
the identity header are `SecretString`s: `Debug`/`Display` print `[redacted]`,
error messages carry only Entra's error code and the first line of its
description, and no token is ever put in a URL.

## Key Vault secret references

A deployment spec carries only references:

```json
"secrets": [{ "name": "openai-key",
              "keyVaultUrl": "https://kv1.vault.azure.net/secrets/openai-key",
              "identity": "/subscriptions/…/userAssignedIdentities/bot" }],
"env": [{ "name": "OPENAI_API_KEY", "secretRef": "openai-key" }]
```

The spec is rejected when an env var whose name looks like a secret (`*KEY*`,
`*TOKEN*`, `*SECRET*`, `*PASSWORD*`) has a clear value, when a `secretRef`
names an unknown secret, or when a vault URL is not `https://<vault>.vault.azure.net/secrets/<name>[/<version>]`.
A versionless URL follows the latest version (Container Apps re-reads it
within 30 minutes). `KeyVaultClient::set_secret` lets `graphrt deploy` write a
value once at deploy time; the gateway never sees secret values.

Prefer a **user-assigned identity** for the app: a system-assigned identity
does not exist before the first deploy, so its `Key Vault Secrets User`
assignment cannot be granted before the first revision tries to resolve the
reference.

## RBAC roles

| Who | Scope | Role |
| --- | --- | --- |
| Gateway (run time), read-only | resource group of the apps | Custom role with `Microsoft.App/containerApps/read`, `Microsoft.App/containerApps/revisions/read`, `Microsoft.App/containerApps/revisions/replicas/read` |
| Gateway, if it also retires old versions | same | add `Microsoft.App/containerApps/write`, `Microsoft.App/containerApps/revisions/deactivate/action` |
| `graphrt deploy` / CI (deploy time, OIDC federation, no static secret) | resource group | `Container Apps Contributor` |
| | user-assigned identity | `Managed Identity Operator` (to attach it to the app) |
| | Key Vault (only when the CLI writes secrets) | `Key Vault Secrets Officer` |
| The app's identity | Key Vault | `Key Vault Secrets User` |
| | container registry | `AcrPull` |

Do not give the gateway `Container Apps Contributor` or `Container Apps
Operator`: their `Microsoft.App/containerApps/*/action` wildcard includes
`listSecrets/action`. The custom role's action names above follow the
`Microsoft.App` resource provider's naming; confirm them with
`az provider operation show --namespace Microsoft.App` before creating the role.

## Conformance (C1–C13)

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
| C6 | long stream, idle-timeout ingress | see [Ingress timeouts](#ingress-timeouts-and-long-streams) |
| C7 | request cap | `Standard` → preempt at 180 s; `Premium`/verified → none |
| C8 | auth | refresh 5 min before `exp`; ±5 min skew; 401 → one refresh, a second 401 is an `Auth` error; all five credential kinds; no secret or token in `Debug`, errors or request lines |
| C9 | API errors | 429/503/500 retried with backoff; persistent 429 → `RateLimited` after 4 attempts; quota codes → `Capacity` |
| C10 | secrets | only Key Vault references reach ARM; clear secret env values rejected without echoing them |
| C11 | old-version pool | version N-1 wakes on day 6; retired on day 8 → `NotFound` (410) |
| C12 | idle and leaks | `minReplicas 0`; stray revisions deactivated by reconciliation |
| C13 | binary size | `crates/graphrt-provider-azure/measure-size.sh`, below |

## What still needs real Azure

These need a subscription and are not covered by the fakes:

1. **C6 for real**: whether standard ingress's 240 s is idle-only
   (`c06_real_azure_stream_outlives_240s` with `GRAPHRT_AZURE_C6_URL`), and that
   premium ingress `requestIdleTimeout` behaves as documented.
2. **C2 wake p95** with n ≥ 50 per region and image size (target 10 s;
   community reports 2–15 s), and that a weight-0 labelled revision scales from
   zero on a request to its label URL.
3. **ARM contract at api-version `2025-01-01`**: revisions list,
   `revisions/{name}/deactivate`, `revisions/{name}/replicas`,
   `Azure-AsyncOperation` on PUT and PATCH; a PATCH with only
   `ingress.traffic` merging without creating a revision; `latestRevision: true`
   alongside labelled entries in one PUT; the label FQDN form
   `<app>---<label>.<domain>` for internal ingress.
4. **C3/C12 timings**: SIGTERM → `terminationGracePeriodSeconds` honoured;
   scale-in to 0 within the HTTP rule's cooldown (300 s) + 60 s.
5. **Auth**: PS256 certificate assertions accepted by Entra; workload identity
   on AKS and from GitHub OIDC; the Container Apps identity endpoint with a
   user-assigned `client_id`.
6. **Key Vault**: reference resolution with a user-assigned identity on the
   first revision; `Key Vault Secrets User` is enough.
7. **Error codes** that should map to `Capacity` (regional quota, environment
   limits) and ARM throttling headroom for the default 10 req/s.
8. **RBAC**: the custom read-only role's action names, and that it is enough
   for `wake`/`list`.

## Binary size

`crates/graphrt-provider-azure/measure-size.sh` builds a probe binary with the
`min-size` profile on top of what every gateway build links (tokio, the shared
`ProviderHttp` over reqwest + rustls/ring, serde_json) and reports the delta.
macOS arm64, 2026-11-22 (Mach-O segments are 16 KiB aligned, so deltas move in
16 KiB steps):

| Build | Size | Δ vs baseline | Δ of the feature itself | Budget (plan §9) |
| --- | --- | --- | --- | --- |
| baseline | 1,786,144 B | – | – | – |
| `auth-azure` | 1,836,096 B | 48.8 KiB | 48.8 KiB | 60 KiB |
| `provider-aca` (gateway) | 1,886,080 B | 97.6 KiB | 48.8 KiB | 80 KiB |
| `deploy-aca` (`graphrt deploy`) | 1,969,136 B | 178.7 KiB | 81.1 KiB | none (CLI only) |

No `azure_*` crate and no new dependency: `base64`, `ring`, `reqwest`,
`rustls`, `serde_json` and `tokio` are already in the workspace's lock file.
