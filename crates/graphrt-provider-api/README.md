# graphrt-provider-api

The contract between the graphrt gateway and the platforms it wakes agents on.
Internal crate (`publish = false`) until the provider wave (F2) has settled.

## What is in it

- `Provider` (run time: `wake` / `release` / `list`) and `DeployDriver`
  (deploy time: `apply`), object safe so the gateway holds `Arc<dyn Provider>`.
- Types: `Capabilities`, `Addressing` (`Replica` | `Url`), `Scaler`,
  `ReleaseMode` (`Suspend` | `Stop` | `ScaleIn`), `DeploymentRef`,
  `ReplicaRef`, `Endpoint`, `ReplicaState`, `DeploymentSpec`, `SecretRef`.
- `ProviderError` with kinds `Capacity`, `RateLimited`, `Auth`, `NotFound`,
  `Transient`, `Fatal`; `DeployError`.
- `http`: `TokenBucket` per provider API, `RetryPolicy` (equal-jitter
  exponential backoff, honours `Retry-After`), `TokenCache` (refresh at
  `exp - 5 min`, forced refresh on 401), `SecretString` (redacted `Debug`).
  Feature `http` adds `ProviderHttp`, the reqwest client every provider uses.
- Feature `conformance` (dev-dependency only): the C1–C13 harness
  (`run_conformance`), a scripted `FakeServer` with fault injection (401, 429,
  5xx, capacity) and request recording, and `MemoryProvider`, the in-memory
  reference provider.

## Writing a provider

1. New crate or module behind its own cargo feature (`gw-<name>`), no cloud SDK.
2. Implement `Provider` with `ProviderHttp` (pass a `StatusClassifier` that maps
   the platform's capacity errors to `Capacity`).
3. In tests, emulate the few API endpoints you call with a `FakeServer`
   handler, implement `ConformanceTarget`, and assert
   `run_conformance(&target, &config).await.assert_no_failures()`.
   `tests/conformance.rs` is the template. Gateway-in-the-loop cases (C3, C6,
   C7, C11, the orphan half of C12) come from `graphrt-gateway`'s
   `GatewayHarness`; without it they are reported as skipped.

Contract details (idempotent absolute `wake`, `release` of a gone replica is
`Ok`, `Url` addressing makes `release` a no-op) are on the `Provider` trait docs.
