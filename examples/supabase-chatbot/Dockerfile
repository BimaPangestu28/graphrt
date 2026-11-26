# syntax=docker/dockerfile:1.7
#
# The Supabase chatbot backend: `graphrt serve` + the supabase plugin, nothing else.
# Build from the repository root:
#
#   docker build -f examples/supabase-chatbot/Dockerfile -t supabase-chatbot .
#
# Run (every secret comes from the environment; none is baked into the image):
#
#   docker run --rm -p 8080:8080 --env-file examples/supabase-chatbot/.env supabase-chatbot
#
# Stages:
#   builder  static musl graphrt with only the features this service needs
#            (llm, serve, jwt, plugins): no JS engine, MCP, RAG or WASM runtime
#   plugin   the supabase plugin at a pinned graphrt-plugins commit (plain .mjs
#            files, no npm dependencies)
#   runtime  distroless Node.js (the plugin is a Node process): no shell, no
#            package manager, runs as `nonroot`
#
# Build args:
#   PLUGINS_REV  graphrt-plugins commit to take the supabase plugin from
#   PROFILE      cargo profile (default: min-size)

ARG RUST_VERSION=1
ARG ALPINE_VERSION=3.22

FROM rust:${RUST_VERSION}-alpine${ALPINE_VERSION} AS builder
RUN apk add --no-cache musl-dev gcc make
WORKDIR /src
COPY . .
ARG PROFILE=min-size
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build -p graphrt-cli --profile "$PROFILE" --locked --no-default-features \
        --features llm,serve,jwt,plugins \
    && install -D -m 0755 "target/$PROFILE/graphrt" /out/graphrt

FROM alpine:${ALPINE_VERSION} AS plugin
RUN apk add --no-cache git
# graphrt-plugins main with the `user_rpc` mode (PR #60). Pin a reviewed commit.
ARG PLUGINS_REV=01cc745c905d10d33003d875d5d90b584a2f0045
RUN git init -q /src \
    && git -C /src fetch -q --depth 1 https://github.com/Graphrt/graphrt-plugins "$PLUGINS_REV" \
    && git -C /src checkout -q FETCH_HEAD \
    && mkdir -p /out/plugins \
    && cp -r /src/supabase /out/plugins/supabase \
    && rm -rf /out/plugins/supabase/test

FROM gcr.io/distroless/nodejs22-debian12:nonroot
COPY --from=builder /out/graphrt /usr/local/bin/graphrt
COPY --from=plugin /out/plugins /app/plugins
COPY examples/supabase-chatbot/agent.yaml /app/agent.yaml
# The plugin manifest runs `node`; distroless keeps it in /nodejs/bin.
ENV PATH=/nodejs/bin:/usr/local/bin:/usr/bin:/bin \
    SUPABASE_TOOL_MODE=user_rpc \
    SUPABASE_TOOL_RPC_ALLOWLIST=get_my_profile,list_diary_entries,list_important_dates,list_chat_messages,start_chat_session,add_chat_message \
    SUPABASE_TOOL_TIMEOUT_MS=8000 \
    SUPABASE_TOOL_MAX_ROWS=50 \
    GRAPHRT_JWT_AUDIENCE=authenticated
USER nonroot:nonroot
WORKDIR /app
EXPOSE 8080
# No HEALTHCHECK (no shell): probe GET /healthz over HTTP.
ENTRYPOINT ["graphrt"]
# Project-specific values (SUPABASE_URL, GRAPHRT_JWT_JWKS_URL, GRAPHRT_JWT_ISSUER,
# keys) come from the environment; see README.md.
CMD ["serve", "/app/agent.yaml", "--addr", "0.0.0.0:8080", \
     "--plugins", "/app/plugins", "--no-default-plugins", "--trust-plugin", "supabase", \
     "--input-header", "user_jwt=X-User-Authorization", "--verify-jwt", "user_jwt", \
     "--user-rate-limit", "20", "--max-active-runs", "32", "--max-body-bytes", "65536", \
     "--max-runs", "200"]
