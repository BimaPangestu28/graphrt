# syntax=docker/dockerfile:1.7
#
# Linux side of the graphrt vs Python comparison. Build context: the repo root.
#
#   docker build -f bench/compare/docker/Dockerfile --target measure -t graphrt-compare:measure .
#   docker run --rm --cpus 1 --memory 1g graphrt-compare:measure > linux.json
#
# Targets:
#   graphrt-image  distroless image with the default-feature graphrt binary (size reference;
#                  same recipe as the repo Dockerfile)
#   python-image   python:3.11-slim plus requirements.txt (size reference)
#   measure        python-image + GNU time + hyperfine + graphrt + fakeoai: runs
#                  `measure.py cold` (cold start and peak RSS) and prints the JSON

ARG RUST_VERSION=1
ARG ALPINE_VERSION=3.22

FROM rust:${RUST_VERSION}-alpine${ALPINE_VERSION} AS builder
RUN apk add --no-cache musl-dev gcc make
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,id=graphrt-compare-target,target=/src/target \
    set -eux; \
    cargo build -p graphrt-cli --profile min-size --locked; \
    install -D -m 0755 target/min-size/graphrt /out/graphrt; \
    CARGO_TARGET_DIR=/src/target/compare cargo build --release --locked \
        --manifest-path bench/compare/rust/Cargo.toml --bin fakeoai; \
    install -D -m 0755 target/compare/release/fakeoai /out/fakeoai

FROM gcr.io/distroless/static-debian12:nonroot AS graphrt-image
COPY --from=builder /out/graphrt /usr/local/bin/graphrt
ENTRYPOINT ["graphrt"]

FROM python:3.11-slim AS python-image
COPY bench/compare/requirements.txt /tmp/requirements.txt
RUN pip install --no-cache-dir --root-user-action=ignore -r /tmp/requirements.txt

FROM python-image AS measure
RUN apt-get update \
    && apt-get install -y --no-install-recommends time hyperfine \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /out/graphrt /out/fakeoai /usr/local/bin/
COPY bench/compare/measure.py /bench/measure.py
COPY bench/compare/python /bench/python
COPY bench/compare/workflows /bench/workflows
COPY bench/compare/docker/entrypoint.sh /bench/entrypoint.sh
ENTRYPOINT ["/bench/entrypoint.sh"]
