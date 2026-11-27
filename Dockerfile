# syntax=docker/dockerfile:1.7
#
# graphrt container image.
#
#   docker build -t graphrt .
#   docker run --rm -p 8080:8080 -v "$PWD/examples:/workflows:ro" graphrt
#
# Build args:
#   FEATURES   cargo features of graphrt-cli (empty = the CLI's default set)
#   EXTRA_FEATURES  features added to the default set when FEATURES is empty,
#              e.g. `eval,online-eval,skills,learning,console` (the release image
#              does this), or `gateway-all,eval,online-eval,skills,learning,console` (the
#              `:<version>-gateway` release
#              image; run it with `gateway --config ...` instead of the default
#              `serve` command)
#   PROFILE    cargo profile (default: min-size)
#   GRAPHRT_TELEMETRY_URL  opt-in telemetry endpoint baked into the binary (default: none)
#
# The binary is linked statically against musl, so the runtime stage needs no
# libc. The runtime image is distroless/static: CA certificates, a `nonroot`
# user, no shell, no package manager.

ARG RUST_VERSION=1
ARG ALPINE_VERSION=3.22

FROM rust:${RUST_VERSION}-alpine${ALPINE_VERSION} AS builder
# C toolchain for QuickJS (code node); everything else is pure Rust + rustls.
RUN apk add --no-cache musl-dev gcc make
WORKDIR /src
COPY . .
ARG FEATURES=""
ARG EXTRA_FEATURES=""
ARG PROFILE=min-size
# Opt-in usage telemetry (docs/telemetry.md): reports from this image say
# `build: docker`; without GRAPHRT_TELEMETRY_URL the binary has no endpoint and
# telemetry stays inert. Build args are visible to `option_env!` during the build.
ARG GRAPHRT_BUILD=docker
ARG GRAPHRT_TELEMETRY_URL=""
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    set -eux; \
    if [ -n "$FEATURES" ]; then \
        cargo build -p graphrt-cli --profile "$PROFILE" --locked --no-default-features --features "$FEATURES"; \
    else \
        cargo build -p graphrt-cli --profile "$PROFILE" --locked --features "$EXTRA_FEATURES"; \
    fi; \
    install -D -m 0755 "target/$PROFILE/graphrt" /out/graphrt

FROM gcr.io/distroless/static-debian12:nonroot
COPY --from=builder /out/graphrt /usr/local/bin/graphrt
USER nonroot:nonroot
WORKDIR /workflows
EXPOSE 8080
# distroless has no shell, so there is no HEALTHCHECK instruction; point your
# orchestrator's HTTP probe at GET /healthz.
ENTRYPOINT ["graphrt"]
CMD ["serve", "/workflows", "--addr", "0.0.0.0:8080"]
