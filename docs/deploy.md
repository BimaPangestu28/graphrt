# Deploying graphrt

graphrt ships as one static binary. The same binary runs workflows from the
command line (`graphrt run`) or serves them over HTTP with Server-Sent Events
(`graphrt serve`). There is no database, interpreter or sidecar to deploy.

## Container image

The repository `Dockerfile` builds a fully static (musl) binary and copies it
into `gcr.io/distroless/static-debian12:nonroot`: CA certificates, a `nonroot`
user, no shell and no package manager.

```sh
docker build -t graphrt .
docker run --rm -p 8080:8080 \
  -v "$PWD/examples:/workflows:ro" \
  -e GRAPHRT_API_KEY=change-me \
  -e OPENAI_API_KEY \
  graphrt
```

The default command is `serve /workflows --addr 0.0.0.0:8080`. Every `*.yaml` /
`*.json` workflow in `/workflows` is compiled once at startup and exposed at
`POST /v1/workflows/<name>/runs`.

The image is the binary plus the distroless base (about 2 MB). Static musl
`min-size` binaries of agent builds measure 7.4 MiB on x86_64 and 6.2 MiB on
aarch64 for `FEATURES=serve,llm,plugins`, 4.4 / 3.7 MiB for `FEATURES=serve`
([bench/baseline/linux-musl.md](../bench/baseline/linux-musl.md)); the default
feature set is larger (see the README's Binary size). CI keeps these within the budgets of
[bench/budgets.toml](../bench/budgets.toml).

| Build argument | Default | Meaning |
| --- | --- | --- |
| `FEATURES` | *(empty: CLI defaults)* | Cargo features of `graphrt-cli`, e.g. `FEATURES="llm,http,serve"` for an image without the JS engine or MCP. |
| `EXTRA_FEATURES` | *(empty)* | Features added to the CLI defaults when `FEATURES` is empty, e.g. `EXTRA_FEATURES=eval,online-eval` for `graphrt eval` and `serve --evaluators` (the published GHCR image is built so). |
| `PROFILE` | `min-size` | Cargo profile. `release` is faster to run, `min-size` is smaller. |
| `RUST_VERSION`, `ALPINE_VERSION` | `1`, `3.22` | Builder image versions. |

Other ways to use the image:

```sh
# One-off run
docker run --rm -v "$PWD/examples:/workflows:ro" graphrt run /workflows/hello.yaml --input name=Ada
# Validate every workflow in CI
docker run --rm -v "$PWD/flows:/workflows:ro" graphrt validate /workflows/main.yaml
```

### Health checks

Distroless images have no shell, so the Dockerfile has no `HEALTHCHECK`. Use
HTTP probes (no authentication): `GET /healthz` returns `ok` while the process
runs; `GET /readyz` returns `ready`, or `503 draining` once a shutdown began.

```yaml
# Kubernetes
livenessProbe:
  httpGet: { path: /healthz, port: 8080 }
readinessProbe:
  httpGet: { path: /readyz, port: 8080 }
```

To scale agents between zero and N behind the graphrt gateway on Kubernetes,
see the [Kubernetes provider](kubernetes-provider.md).

## Graceful shutdown

On SIGTERM or SIGINT, `graphrt serve` stops accepting runs (`503` +
`Retry-After`), pauses the active ones, preempts what is still running shortly
before `--drain-timeout`, stores every run's state and exits `0` (details in
[serving.md](serving.md#shutdown-and-restarts)). No run is lost as `failed`:
each ends `paused` (reason `drained`) or finished, and resumes after the restart
or on another replica that shares the store. It needs no init process: graphrt
handles the signals itself, also as PID 1.

The platform kills the process when its grace period ends, so set
`--drain-timeout` a little below it. The default (`4s`) fits the shortest common
grace period, Fly's 5 s.

| Platform | Stop signal | Grace period (default / maximum) | Recommended |
| --- | --- | --- | --- |
| Kubernetes (GKE, EKS, AKS) | SIGTERM | `terminationGracePeriodSeconds`: 30 s / unbounded | `terminationGracePeriodSeconds: 30`, `--drain-timeout 25s` |
| Cloud Run | SIGTERM | 10 s, fixed | `--drain-timeout 8s` |
| Fly.io Machines | SIGINT (`kill_signal`) | `kill_timeout`: 5 s / 300 s | `kill_timeout = 30`, `--drain-timeout 25s` (or keep 5 s with the default `4s`) |
| Azure Container Apps | SIGTERM | `terminationGracePeriodSeconds`: 30 s / 600 s | `terminationGracePeriodSeconds: 60`, `--drain-timeout 55s` |
| Amazon ECS (Fargate, EC2) | SIGTERM | `stopTimeout`: 30 s / 120 s | `stopTimeout: 60`, `--drain-timeout 55s` |
| Docker / Compose | SIGTERM | `docker stop -t`, `stop_grace_period`: 10 s | `stop_grace_period: 30s`, `--drain-timeout 25s` |
| AWS Lambda | none without an extension | 0 ms; 500 ms (internal extension); 2 s (external extensions) | not suited to `graphrt serve`: short synchronous runs only |

Notes:

- A longer grace period means fewer preempted nodes: nodes that finish within
  the drain are not repeated. A preempted node (an LLM call, an HTTP request)
  runs again when the run resumes; see
  [at-least-once nodes](serving.md#shutdown-and-restarts).
- Kubernetes removes a terminating pod from its Service endpoints while the
  SIGTERM is delivered, so a few requests may still arrive; they get `503` with
  `Retry-After`. A `preStop: { sleep: { seconds: 3 } }` hook (Kubernetes 1.30+;
  distroless has no shell for `exec: sleep`) delays the SIGTERM until the
  endpoints are updated; it counts against the grace period.
- Fly sends SIGINT by default: graphrt treats it like SIGTERM. The example
  [fly.toml](../examples/supabase-chatbot/deploy/fly.toml) sets `kill_timeout`.
- Cloud Run gives 10 s whatever the configuration, so long nodes are always
  preempted there. With an in-memory store a drained run lives only until the
  instance exits: use a shared `--store` (or keep the snapshot client-side) for
  runs that must survive scale-in.
- Async and webhook runs resume by themselves when a server starts on the same
  store; streaming and blocking callers get `run_paused` and resume over the API.

### Multi-arch images

The builder compiles natively inside each platform's container, so buildx with
QEMU produces both architectures from the same Dockerfile:

```sh
docker buildx create --use
docker buildx build --platform linux/amd64,linux/arm64 -t ghcr.io/you/graphrt:0.1 --push .
```

Emulated arm64 builds are slow; on CI prefer native arm64 runners and
`docker buildx imagetools create` to merge the per-arch images.

### Network note

If `apk add` fails during the build with "temporary error", the Docker bridge
network on the build host has no DNS. Build with `docker build --network host .`.

## Configuration

| What | How |
| --- | --- |
| Model providers | `OPENAI_API_KEY`, `OPENAI_BASE_URL`, `ANTHROPIC_API_KEY`, `OLLAMA_HOST`, `GRAPHRT_OPENAI_COMPAT_<NAME>_BASE_URL` / `_API_KEY` |
| Workflow `env.*` variables | Environment variables with the same names (only names a workflow declares are exposed) |
| API authentication | `GRAPHRT_API_KEY` (bearer token); without it the API is unauthenticated |
| End-user JWTs | `--verify-jwt` with `GRAPHRT_JWT_JWKS_URL`, `GRAPHRT_JWT_ISSUER`, `GRAPHRT_JWT_AUDIENCE` ([serving.md](serving.md#verifying-end-user-jwts)) |
| MCP servers | `graphrt serve ... --mcp /config/mcp.json` |
| Private network egress | `--allow-private-network` (off by default; SSRF protection) |

Mount secrets as environment variables or files; snapshots returned by paused
runs never contain secret values.

A complete service image, `graphrt serve` with only the `llm`, `serve`, `jwt`
and `plugins` features plus a Node.js plugin on distroless Node, with Fly.io,
Cloud Run and VPS notes, is in
[examples/supabase-chatbot](../examples/supabase-chatbot/README.md#container).

## Bare binary

```sh
cargo build -p graphrt-cli --profile min-size
./target/min-size/graphrt serve ./workflows --addr 0.0.0.0:8080
```

For a static Linux binary outside Docker, build for
`x86_64-unknown-linux-musl` (needs `musl-tools` for the QuickJS C code).

## Embedding

To run workflows inside an application instead of a separate service, use the
Rust crates directly ([embedding.md](embedding.md)) or the mobile/desktop
bindings ([mobile.md](mobile.md)).
