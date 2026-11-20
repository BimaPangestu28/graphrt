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

Measured on x86_64 with the CLI's default features (`llm`, `http`, `code`,
`mcp`, `serve`): the image is **18.7 MB**.

| Build argument | Default | Meaning |
| --- | --- | --- |
| `FEATURES` | *(empty: CLI defaults)* | Cargo features of `graphrt-cli`, e.g. `FEATURES="llm,http,serve"` for an image without the JS engine or MCP. |
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

Distroless images have no shell, so the Dockerfile has no `HEALTHCHECK`. Use an
HTTP probe on `GET /healthz` (returns `ok`, no authentication):

```yaml
# Kubernetes
livenessProbe:
  httpGet: { path: /healthz, port: 8080 }
readinessProbe:
  httpGet: { path: /healthz, port: 8080 }
```

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
