### Install

```sh
# agent build (run, serve, eval)
curl -fsSL https://github.com/{{REPO}}/releases/download/{{TAG}}/install.sh | sh
# gateway build (the same CLI plus `graphrt gateway` and `graphrt deploy`, every provider)
curl -fsSL https://github.com/{{REPO}}/releases/download/{{TAG}}/install.sh | sh -s -- --gateway
brew install {{OWNER}}/graphrt/graphrt
cargo install graphrt-cli                          # add --features gateway-all for the gateway
docker pull ghcr.io/{{OWNER}}/graphrt:{{VERSION}}
```

- **Assets**: `graphrt-<target>` is the agent build. `graphrt-gateway-<target>` (features `gateway-all,eval,online-eval`, with mimalloc) is the gateway build for Linux (static musl, x86_64 and aarch64) and macOS (x86_64 and aarch64). The gateway is Unix only, so there is no Windows gateway archive.
- **Gateway providers**: Kubernetes is measured on a local kind cluster. Cloud Run, ECS Fargate, Lambda, Fly and Azure Container Apps are tested against API fakes (conformance suite). Real cloud verification is pending.
- **Windows (experimental)**: cross compiled (MSVC, static C runtime), not yet run on Windows.
