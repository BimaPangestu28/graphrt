# graphrt-provider-fly

Fly Machines provider of the graphrt gateway. Its API follows
`graphrt-provider-api` and may change between minor releases.

## Overview

- `FlyProvider` implements `graphrt_provider_api::Provider` (run-time, gateway credentials):
  - each deployment version is a pool of pre-created Machines;
  - `wake` is singleflight: it resumes suspended Machines first, cold-starts stopped ones next, and when every host is full it clones a pool member;
  - `release` suspends, and falls back to `stop`;
  - `list` reports 6PN addresses.
- `FlyDeployDriver` (feature `deploy`, on by default) implements `DeployDriver` for `graphrt deploy --target fly`:
  - creates the app;
  - writes Fly app secrets from `env:` / `fly:` references;
  - creates the stopped pool with `stop_config` = SIGTERM + `grace_secs` (≤ 300 s).
- No Fly SDK: `MachinesClient` is a small REST client over the shared `ProviderHttp` layer. That layer brings the token bucket and jittered retry. The client adds the `FlyV1` auth scheme and reloads the token once on a 401.

## Usage

```rust
use graphrt_provider_fly::{FlyProvider, FlyProviderConfig, TokenOrigin, INTERNAL_API_BASE_URL};

let mut config = FlyProviderConfig::single_app("chat-agents", TokenOrigin::Env("FLY_API_TOKEN".into()));
config.api_base_url = INTERNAL_API_BASE_URL.to_owned(); // gateway running on Fly
let provider = FlyProvider::new(config)?;
```

For a gateway-only build, use `--no-default-features`. It leaves out the deploy driver.

## Tests

```sh
cargo test -p graphrt-provider-fly
```

This runs the C1–C13 conformance harness and the Fly-specific tests against a stateful fake Machines API (`tests/support`).

## Documentation

Behaviour, setup, drain guidance, size and the real-Fly verification list: [docs/gateway-fly.md](https://github.com/Graphrt/graphrt/blob/main/docs/gateway-fly.md).

## License

Apache-2.0
