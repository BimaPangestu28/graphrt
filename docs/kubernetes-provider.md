# Kubernetes provider (gateway)

The `graphrt-provider-k8s` crate is the Kubernetes backend of the graphrt
gateway: it scales the agents of a deployment between zero and N replicas,
picks which replica goes away on scale-in, and reports which replicas are
ready. It works with any conformant cluster: self-hosted, kind, k3d/k3s, GKE,
EKS and AKS. Only cluster authentication differs between them.

> Status: the provider and its conformance suite are in the repository; the
> `graphrt gateway` binary that loads it (cargo feature `provider-k8s`) lands
> with the gateway MVP. Until then the crate is a library.

It is hand-rolled REST over the reqwest + rustls stack graphrt already links:
no `kube`, no `k8s-openapi`, no HTTP/2.

## How it maps onto Kubernetes

| Gateway operation | Kubernetes API |
| --- | --- |
| `wake(deployment, n)` | `GET/PATCH apps/v1 deployments/{name}/scale` (compare-and-swap on `resourceVersion`), then wait on `discovery.k8s.io/v1 endpointslices` (list + watch) until `n` endpoints are ready |
| `release(replica)` | annotate the pod `controller.kubernetes.io/pod-deletion-cost: "-2147483648"`, then scale the Deployment down by one: the ReplicaSet controller deletes the cheapest pod first, so the idle replica the gateway chose goes, not a busy one |
| `list(deployment)` | `GET pods?labelSelector=graphrt.dev/deployment=…,graphrt.dev/version=…` (reconciliation after a gateway restart) |
| readiness watch | `watch endpointslices?labelSelector=kubernetes.io/service-name=…`, re-listing on `410 Gone` |

Every deployment version is its own Deployment and Service named
`<deployment>-<version>` (sanitised to a DNS label, with a hash suffix when
it had to change). Versions are immutable, so a paused run can still be
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
- **Errors are classified** for the gateway: an unschedulable pod at the wake
  deadline is `Capacity` (fall back or stop), a bad image is `Fatal`, 429 is
  `RateLimited` (with `Retry-After`), 401/403 is `Auth`, 404 `NotFound`, 409
  and 5xx `Transient`.
- **Retries.** 429, 5xx and transport errors are retried up to four times with
  full-jitter backoff (100 ms doubling, capped at 2 s; a `Retry-After` is
  respected). A token bucket (20 requests/s, burst 40) protects the API
  server during wake storms.

## Deploying a version

`graphrt deploy` uses the deploy driver with deploy-time credentials. It
server-side applies (field manager `graphrt-deploy`):

- a **Deployment** with the agent container (`args`, port `http`), a 1 s
  `/healthz` readiness probe (keeps a cold wake within about 3 s),
  `terminationGracePeriodSeconds` (default 120), no service account token
  mounted (the agent never talks to the API server) and `replicas: 0` for a
  new version. Re-applying a version keeps its current replica count, so a
  deploy never fights the gateway's scaling.
- a ClusterIP **Service** selecting the version's pods. Its EndpointSlices are
  what the gateway watches.
- **Secret references only.** Secrets are referenced through `envFrom`
  (`secretRef`) or `env[].valueFrom.secretKeyRef`. Their values are written to
  a Kubernetes Secret at deploy time (`apply_secret`) and never pass through
  the gateway.

## RBAC

The gateway's service account needs only these permissions, in the agents'
namespace:

```yaml
apiVersion: rbac.authorization.k8s.io/v1
kind: Role
metadata: { name: graphrt-gateway, namespace: agents }
rules:
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

Tokens are cached until 5 minutes before they expire (covers ±5 minutes of
clock skew) and refreshed once on a 401. They are never logged: credentials
are held in a type whose `Debug` output is redacted, and redirects are not
followed.

### GKE, EKS and AKS from outside the cluster

Exec plugins work today. Built-in cluster tokens without a plugin, using the
gateway's own cloud identity, plug into the same `TokenSource` hook:

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

`cargo test -p graphrt-provider-k8s` runs the provider conformance checks
(plan §7) against an in-process fake API server. The fake models scale CAS,
the ReplicaSet victim order, EndpointSlice watches with `410 Gone`, server-side
apply, token checks, fault injection and capacity limits.

| Check | What is verified |
| --- | --- |
| C1 | 20 concurrent wakes from zero: one scale patch, exactly 1 replica |
| C2 | 50 wakes from zero: the provider's own latency on top of pod readiness |
| C4 | scale-in removes only the released replica; idempotent; 409 conflicts retried |
| C5 | a pod killed outside the gateway is seen by the readiness watch (≈1 ms on the fake), also across a `410 Gone` relist |
| C8 | a rejected token is refreshed exactly once; revoked credentials give `Auth`, no token in errors or `Debug` |
| C9 | 503/429/500 retried with backoff; persistent errors classified; unschedulable is `Capacity` |
| C10 | the applied Deployment carries Secret references only; re-deploy keeps replicas |
| C12 | `list` reports every replica for reconciliation; releasing from the list reaches zero |

C3, C6, C7 and C11 exercise the gateway's run supervisor (drain, long streams,
request caps, the version pool) and run in the gateway's harness. C13 is the
binary-size gate.

Against a real cluster (opt-in):

```sh
kind create cluster --name graphrt-k8s
kind load docker-image alpine:3.22 --name graphrt-k8s
GRAPHRT_K8S_E2E_CONTEXT=kind-graphrt-k8s \
  cargo test -p graphrt-provider-k8s --test real_cluster -- --ignored --nocapture
```
