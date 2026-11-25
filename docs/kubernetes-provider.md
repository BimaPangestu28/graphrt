# Kubernetes provider (gateway)

The `graphrt-provider-k8s` crate is the Kubernetes backend of the graphrt
gateway: it scales the agents of a deployment between zero and N replicas,
picks which replica goes away on scale-in, and reports which replicas are
ready. It works with any conformant cluster: self-hosted, kind, k3d/k3s, GKE,
EKS and AKS. Only cluster authentication differs between them.

> Status: preview. `graphrt gateway` loads this provider with
> `--features gateway-k8s` (`kind: k8s` under `providers:`, see
> [docs/gateway.md](gateway.md#providers)), and `graphrt deploy --target k8s`
> applies versions with its deploy driver.

It implements the `Provider` and `DeployDriver` traits of
`graphrt-provider-api` and talks to the API server through the shared
`ProviderHttp` layer: hand-rolled REST over the reqwest + rustls stack graphrt
already links, no `kube`, no `k8s-openapi`, no HTTP/2.

## Cargo features and size

| Feature | What | Binary size (macOS arm64, `min-size`) |
| --- | --- | --- |
| (always) | run-time provider, in-cluster service account auth, readiness watch | ≈ 130 KiB standalone; **86.8 KiB** in the gateway build (`gw-k8s`, budget 150 KiB) |
| `kubeconfig` (default) | kubeconfig files: token, token file, client certificate, exec plugins | ≈ +63 KiB: `gw-k8s-kubeconfig` totals **149.6 KiB** in the gateway build |
| `deploy` (default) | deploy driver: server-side apply of Deployment, Service, Secret | ≈ +49 KiB |

Sizes are measured against a binary that already links the shared provider
HTTP layer (reqwest, rustls, tokio, serde_json); the gateway figures are
Mach-O section totals against `graphrt-cli --features gateway`. Kubeconfig
and `ExecCredential` documents are read by walking a `serde_json::Value`
rather than through per-struct deserializers, which keeps `kubeconfig` small. A gateway running inside the
cluster can use `default-features = false`.

## How it maps onto Kubernetes

| Gateway operation | Kubernetes API |
| --- | --- |
| `wake(deployment, n)` | `GET/PATCH apps/v1 deployments/{name}/scale` (compare-and-swap on `resourceVersion`), then wait on `discovery.k8s.io/v1 endpointslices` (list + watch) until `n` endpoints are ready |
| `release(replica)` | annotate the pod `controller.kubernetes.io/pod-deletion-cost: "-2147483648"`, then scale the Deployment down by one: the ReplicaSet controller deletes the cheapest pod first, so the idle replica the gateway chose goes, not a busy one |
| `list(deployment)` | `GET pods?labelSelector=graphrt.dev/deployment=…,graphrt.dev/version=…` (reconciliation after a gateway restart) |
| readiness watch | `watch endpointslices?labelSelector=kubernetes.io/service-name=…`, re-listing on `410 Gone` |

Every deployment version is its own Deployment and Service named
`<deployment>-<version>` (sanitised to a DNS label, with a hash suffix when
it had to change). The deploy driver returns it as the version's `target`,
`<namespace>/<name>`; the provider refuses targets outside its configured
namespace before sending anything. Versions are immutable, so a paused run can still be
resumed on the version it started on while that version is kept.

Details that matter in production:

- **Singleflight.** Concurrent wakes of one version share one lock: 20
  simultaneous wakes from zero send one scale patch.
- **Idempotent release.** Releasing a pod that is gone, terminating or already
  marked as the victim does nothing. If the scale patch fails, the annotation
  is removed again.
- **Victim order.** The ReplicaSet controller removes pods that are not ready
  before it looks at deletion cost. The gateway only routes to ready pods, so
  the pod removed instead has no runs on it.
- **Errors are classified** for the gateway: at the wake deadline, an
  unschedulable pod or a ResourceQuota rejection (the Deployment's
  `ReplicaFailure` condition) is `Capacity` (fall back or stop), a bad image is
  `Fatal`. 429 is `RateLimited` (with `Retry-After`), 401/403 is `Auth` (a 403
  for an exceeded quota is `Capacity`), 404 `NotFound`, 5xx `Transient`. A
  409 on the scale subresource re-reads and retries the compare-and-swap.
- **Retries.** 429, 5xx and transport errors are retried up to four times with
  jittered backoff (100 ms doubling, capped at 2 s; a `Retry-After` is
  respected). A token bucket (20 requests/s, burst 40) protects the API
  server during wake storms.

## Deploying a version

`graphrt deploy` uses the deploy driver with deploy-time credentials. It
server-side applies (field manager `graphrt-deploy`):

- a **Deployment** with the agent container (`command`, port `http`,
  8080 unless `provider_settings.port` says otherwise), a 1 s
  `/healthz` readiness probe (keeps a cold wake within about 3 s),
  `terminationGracePeriodSeconds` (default 120), no service account token
  mounted (the agent never talks to the API server), the workflow fingerprint
  as an annotation, and `min_replicas` (normally 0) for a new version. Re-applying a version keeps its current replica count, so a
  deploy never fights the gateway's scaling.
- a ClusterIP **Service** selecting the version's pods. Its EndpointSlices are
  what the gateway watches.
- **Secret references only.** Each `SecretRef` becomes an
  `env[].valueFrom.secretKeyRef`; its `reference` is `<secret>/<key>`, or
  `<secret>` when the key is named like the env var. Values are written to a
  Kubernetes Secret at deploy time (`apply_secret`) and never pass through the
  gateway.

## RBAC

The gateway's service account needs only these permissions, in the agents'
namespace:

```yaml
apiVersion: rbac.authorization.k8s.io/v1
kind: Role
metadata: { name: graphrt-gateway, namespace: agents }
rules:
  - apiGroups: ["apps"]
    resources: ["deployments"]
    verbs: ["get"]          # status only, to diagnose a failed wake
  - apiGroups: ["apps"]
    resources: ["deployments/scale"]
    verbs: ["get", "patch"]
  - apiGroups: [""]
    resources: ["pods"]
    verbs: ["get", "list", "patch"]
  - apiGroups: ["discovery.k8s.io"]
    resources: ["endpointslices"]
    verbs: ["list", "watch"]
```

The deploy-time identity (CI, `graphrt deploy`) additionally needs `get` and
`patch` on `deployments`, `services` and `secrets`.

## Cluster authentication

Resolved in the order client-go uses:

1. **In-cluster**: `KUBERNETES_SERVICE_HOST` / `KUBERNETES_SERVICE_PORT`, the
   service account token and CA under
   `/var/run/secrets/kubernetes.io/serviceaccount`. The token file is re-read
   every minute because the kubelet rotates projected tokens in place.
2. **Kubeconfig**: `$KUBECONFIG` (every listed file, merged; the first
   definition wins) or `~/.kube/config`, the current context or a named one.
   Supported credentials:
   - `token` and `tokenFile`,
   - `client-certificate(-data)` + `client-key(-data)` (mTLS),
   - `exec` plugins (`client.authentication.k8s.io/v1` and `v1beta1`), such as
     `aws eks get-token`, `gke-gcloud-auth-plugin` and `kubelogin`. The plugin
     runs non-interactively with a 60 s timeout; its token is cached until 5
     minutes before `expirationTimestamp`.

   Rejected on purpose: `insecure-skip-tls-verify` (set the CA instead),
   `tls-server-name`, `proxy-url`, basic auth, and the removed `auth-provider`
   plugins (switch to their exec plugin).

Tokens are cached by the shared `TokenCache` until 5 minutes before they
expire (covers ±5 minutes of clock skew) and refreshed once on a 401. They are never logged: credentials
are held in a type whose `Debug` output is redacted, and redirects are not
followed.

### GKE, EKS and AKS from outside the cluster

Exec plugins work today. Built-in cluster tokens without a plugin, using the
gateway's own cloud identity, plug into the same hook: any
`graphrt_provider_api::http::TokenSource`, set with
`ClusterConfig::with_token_source` next to the cluster's endpoint and CA:

| Cluster | Token | Built on |
| --- | --- | --- |
| EKS | `k8s-aws-v1.` + base64url of a presigned STS `GetCallerIdentity` URL (SigV4, `x-k8s-aws-id` header) | `auth-aws` |
| GKE | Google OAuth access token (metadata server or service account JWT) | `auth-gcp` |
| AKS | Entra ID token for the AKS server application (`6dae42f8-4368-4678-94ff-3960e28e3630/.default`) | `auth-azure` |

These are the `gw-eks`, `gw-gke` and `gw-aks` features. They land once the
shared cloud auth modules are merged.

## Optional: KEDA instead of the gateway's scaler

If a cluster already runs [KEDA](https://keda.sh) with its HTTP add-on,
scaling can be left to KEDA. The gateway still holds every run's stream,
store and secrets, but `wake` and `release` become no-ops. Point an
`HTTPScaledObject` at the version's Service with `minReplicaCount: 0`:

```yaml
apiVersion: http.keda.sh/v1alpha1
kind: HTTPScaledObject
metadata: { name: support-bot-v3, namespace: agents }
spec:
  hosts: ["support-bot-v3.agents.svc"]
  scaleTargetRef: { name: support-bot-v3, kind: Deployment, apiVersion: apps/v1, service: support-bot-v3, port: 8080 }
  replicas: { min: 0, max: 10 }
  scaledownPeriod: 300
```

Trade-offs: KEDA's interceptor adds a proxy hop to every request, and KEDA
picks scale-in victims without knowing which replicas hold running streams.
Pausing for a drain then depends on the grace period alone, with no
`pod-deletion-cost` steering. This mode is documented only: the gateway's
built-in scaler is the supported path.

## Conformance

`cargo test -p graphrt-provider-k8s` runs the shared C1–C13 conformance
suite of `graphrt-provider-api` plus Kubernetes-specific checks (scale-in
victim order, readiness watches, capacity and quota errors); see
[CONTRIBUTING.md](../CONTRIBUTING.md#provider-conformance-tests).

Against a real cluster (opt-in; kind uses a client certificate and an inline
CA from the kubeconfig):

```sh
kind create cluster --name graphrt-k8s
kind load docker-image alpine:3.22 --name graphrt-k8s
GRAPHRT_K8S_E2E_CONTEXT=kind-graphrt-k8s \
  cargo test -p graphrt-provider-k8s --test real_cluster -- --ignored --nocapture
```

Measured on kind v1.36.1 (client-certificate kubeconfig): wake 0 → 1 with
n = 50, p50 340 ms, p95 812 ms, max 1.17 s (target ≤ 3 s); scale-in removed
exactly the released pod. Managed clusters (GKE, EKS, AKS) have not been
measured yet.
