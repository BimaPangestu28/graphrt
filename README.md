# graphrt

[![CI](https://github.com/BimaPangestu28/graphrt/actions/workflows/ci.yml/badge.svg)](https://github.com/BimaPangestu28/graphrt/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/graphrt-core.svg)](https://crates.io/crates/graphrt-core)
[![docs.rs](https://img.shields.io/docsrs/graphrt-core)](https://docs.rs/graphrt-core)
[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)

**Run your agent everywhere.**

No server to rent. The same agent workflow runs on your laptop, in a container,
at the edge, or on your own phone.

graphrt is a tiny runtime for agentic AI workflows: LLM calls, tool-using
agents, MCP tools, knowledge retrieval, document extraction, HTTP requests,
sandboxed code, branches, loops and human approvals, described in one plain
YAML/JSON document. That document runs as a single small binary, as a streaming
HTTP service, inside a mobile app (Android / iOS via Kotlin and Swift bindings),
or embedded in your own Rust program.

| Where | How |
| --- | --- |
| Laptop / server | `graphrt run flow.yaml` or `graphrt serve flows/` (single static binary) |
| Scale to zero | `graphrt gateway` (preview, `--features gateway`): one always-on front that holds every run's stream and wakes agents on demand on local processes, Kubernetes, Cloud Run, ECS Fargate, Lambda, Fly or Azure Container Apps (`gateway-k8s`, `gateway-cloud-run`, `gateway-ecs`, `gateway-lambda`, `gateway-fly`, `gateway-aca`, `gateway-all`); runs survive drains, lost replicas and gateway restarts; on-prem active/passive with a fenced lease ([docs/gateway-ha.md](docs/gateway-ha.md)); `graphrt deploy --target local\|k8s\|cloud-run\|aca\|ecs\|lambda\|fly` ships immutable versions. Measured locally: agent exec → `/healthz` ~6 ms, gateway restart 350 ms, HA takeover 73 ms, Kubernetes (kind) wake p95 812 ms; the cloud targets are tested against API fakes, real-cloud verification pending ([measurements](docs/gateway.md#measured-so-far)) — see [docs/gateway.md](docs/gateway.md) |
| Container | ~17 MB distroless image (linux/amd64, linux/arm64) — see [docs/deploy.md](docs/deploy.md) |
| Small devices / IoT | A 6 MiB build serves workflows in a 16 MiB memory limit on a tenth of a core — see [docs/serving.md](docs/serving.md#small-devices) |
| Phone | Embed `graphrt-ffi` in an Android or iOS app — see [docs/mobile.md](docs/mobile.md) and the [sample apps](examples/mobile/), smoke-tested in an Android emulator and an iOS simulator by the [mobile CI workflow](.github/workflows/mobile.yml); or chat with a served agent from Flutter / React Native — see [docs/chatbot-supabase.md](docs/chatbot-supabase.md) |
| Browser / edge | The core compiles to WebAssembly |
| Your Rust app | `graphrt-core` as a library — see [docs/embedding.md](docs/embedding.md) |

Models come from API providers: any OpenAI-compatible endpoint (OpenAI, Azure,
Groq, DeepSeek, OpenRouter, Mistral, Ollama, vLLM...), Anthropic, Google Gemini,
AWS Bedrock, and TypeSafe's Jev decision model for fast, calibrated
classification. Apps on phones never need to ship a provider key: use the
user's own key from the keychain, a thin proxy, or short-lived tokens from your
backend — see [docs/credentials.md](docs/credentials.md) and
[docs/providers.md](docs/providers.md).

```text
start ──▶ if-else ──▶ agent (MCP tools) ──▶ answer
              └─────▶ http-request ──▶ code ──▶ end
```

## Why graphrt

- **Small.** Core-only binary ≈ 3 MB, everything included ≈ 12–14 MB, a trimmed server build ≈ 6 MB that runs in 16 MiB of RAM. No Python, no Node, no database.
- **Fast.** About 0.4 µs of scheduling per node, ~6 µs from a model's first token to your stream, a few milliseconds for `graphrt run` from process start to exit, and well over 10k requests/s per core from `graphrt serve` (see [Performance](#performance)). One task per run polls node futures concurrently; nothing is spawned per node, and variable pools are reference-counted, so loops and parallel iterations don't deep-copy state.
- **Portable.** The core is executor-agnostic (bring your own clock) and compiles to `wasm32-unknown-unknown`.
- **Agent-native.** First-class `agent` node with a tool-calling loop that streams its answer token by token, parallel tool calls, MCP servers, human approval of tool calls, retries with backoff, per-node timeouts and step/time budgets.
- **Durable.** Pause anywhere (human input, tool approval, API call), get a JSON snapshot, resume later — even inside loops and iterations.
- **Safe by default.** Secrets never leave the process in logs, events or snapshots. HTTP egress blocks private networks (SSRF). Templates run with fuel limits. Code runs in an interpreter with memory/time limits and no I/O.

## Install

```sh
# Linux / macOS: prebuilt static binary (verifies SHA256SUMS)
curl -fsSL https://raw.githubusercontent.com/BimaPangestu28/graphrt/main/install.sh | sh

# Homebrew (macOS, Linux)
brew install bimapangestu28/graphrt/graphrt

# with Rust (1.89+)
cargo install graphrt-cli            # installs the `graphrt` binary
cargo install graphrt-cli --features eval,online-eval   # + `graphrt eval` and `serve --evaluators`

# container (linux/amd64, linux/arm64)
docker run --rm -p 8080:8080 -v "$PWD/flows:/workflows" ghcr.io/bimapangestu28/graphrt
```

Linux (x86_64, aarch64; static musl), macOS (x86_64, aarch64) and Windows (x86_64,
`.zip`, no Visual C++ runtime needed) archives with `SHA256SUMS` are on the
[releases page](https://github.com/BimaPangestu28/graphrt/releases). The release
workflow builds them on each OS after the test suite passed on Linux, macOS and Windows.
Libraries are on crates.io: `graphrt-core` (runtime) plus integration crates such as
`graphrt-llm`, `graphrt-http`, `graphrt-rag`.

## Quick start

```sh
cargo install graphrt-cli                      # or, from a checkout: cargo install --path crates/graphrt-cli

graphrt run examples/hello.yaml --input name=Ada
# {"greeting": "Hello, Ada."}

graphrt run examples/branching.yaml --input priority:=9 --input subject="DB down"
# URGENT: paging on-call for 'DB down' (priority 9)

OPENAI_API_KEY=sk-... graphrt run examples/chatbot.yaml --sys query="What is a DAG?"
# (tokens stream to your terminal)
```

Human in the loop:

```sh
graphrt run examples/approval.yaml --input amount:=1200 --input reason="New laptop"
# exit code 3: paused, snapshot written to examples/approval.snapshot.json
graphrt run examples/approval.yaml --resume examples/approval.snapshot.json \
  --resume-input 'review={"action":"approve","values":{"note":"ok"}}'
```

Serve workflows over HTTP with Server-Sent Events:

```sh
GRAPHRT_API_KEY=change-me graphrt serve examples/ --addr 0.0.0.0:8080
curl -N -H 'Authorization: Bearer change-me' \
  -d '{"inputs":{"name":"Ada"},"stream":true}' \
  http://localhost:8080/v1/workflows/hello/runs
```

Each run event is one SSE message; `response` events carry the ordered answer
text token by token, from `llm` and `agent` nodes alike (see
[docs/serving.md](docs/serving.md#streaming-sse)). Every event has an `id:` (its
index in the run's log), so a client that reconnects to
`/v1/runs/{id}/events` with `Last-Event-ID` gets exactly the rest of the run.

On SIGTERM or SIGINT the server drains: new runs get `503`, active runs pause
and are stored (nodes still running near `--drain-timeout` are cancelled and
run again on resume), then it exits, also as PID 1 of a container. Async runs
resume when the next server starts on the same store; see the grace periods
per platform in [docs/deploy.md](docs/deploy.md#graceful-shutdown).

`graphrt serve --trust-gateway --deployment shop --listen unix:/run/shop.sock`
runs the server as an agent behind a graphrt gateway: a signed (HMAC-SHA256)
`/internal/v1` API for starting, resuming, reattaching to and draining runs,
where a lost gateway connection pauses the run for reclaim instead of losing
it. See [docs/gateway-protocol.md](docs/gateway-protocol.md).

Per-user credentials (a Supabase or OAuth JWT from a mobile app) can travel in
their own header and reach the workflow as a masked secret input, while
`Authorization` stays the server's API key:

```sh
graphrt serve flows/ --input-header user_jwt=X-User-Authorization
curl -H "X-User-Authorization: Bearer $USER_JWT" -d '{"stream":true}' \
  http://localhost:8080/v1/workflows/chat/runs
```

With `--verify-jwt user_jwt` the server verifies that token itself (JWKS or
HS256, `exp`, `iss`, `aud`), so the app needs no API key, and
`--user-rate-limit` caps runs per user. A full Supabase chatbot backend (schema
with RLS, agent, container, deploy notes, end-to-end security tests) is in
[examples/supabase-chatbot](examples/supabase-chatbot/), with Flutter and React
Native clients in [graphrt-examples](https://github.com/BimaPangestu28/graphrt-examples);
the guide is [docs/chatbot-supabase.md](docs/chatbot-supabase.md).

See [docs/serving.md](docs/serving.md#per-user-credentials-from-headers) and
[docs/cli.md](docs/cli.md) for every command and flag, and the
[docs index](docs/README.md) for everything else.

## A workflow

```yaml
version: "1"
name: support-triage
env:
  - { name: CRM_TOKEN, type: secret }
graph:
  nodes:
    - id: start
      type: start
      variables: [{ variable: ticket, type: paragraph, required: true }]

    - id: classify
      type: question-classifier
      model: { provider: openai, name: gpt-4o-mini }
      query_variable_selector: [start, ticket]
      classes: [{ id: billing, name: Billing }, { id: bug, name: Bug report }]

    - id: lookup
      type: http-request
      method: get
      url: "https://crm.example.com/api/tickets?q={{#start.ticket#}}"
      authorization: { type: api-key, config: { type: bearer, api_key: "{{#env.CRM_TOKEN#}}" } }
      retry: { max_retries: 3, interval_ms: 500, backoff: 2.0 }
      timeout_ms: 10000
      on_error: { strategy: fail-branch }

    - id: reply
      type: answer
      answer: "Category: {{#classify.class_name#}}"
  edges:
    - { source: start, target: classify }
    - { source: classify, target: lookup, handle: billing }
    - { source: classify, target: reply, handle: bug }
    - { source: lookup, target: reply }
```

Values are referenced as `{{#node.output.path#}}`. The full format is in
[docs/workflow-format.md](docs/workflow-format.md).

## Nodes

| Kind | What it does | Crate |
| --- | --- | --- |
| `start` / `end` / `answer` | Typed inputs; final outputs; streamed answer text | core |
| `if-else` | Multi-case conditions (20+ operators, file attributes) | core |
| `template-transform` | Jinja2 templates (sandboxed, fuel-limited) | core |
| `variable-aggregator` | First non-null of several branches | core |
| `assigner` | Write conversation / loop variables (set, append, `+=`…) | core |
| `list-operator` | Filter, sort, slice and pick list items | core |
| `iteration` | Map a sub-graph over a list, optionally in parallel | core |
| `loop` | Repeat a sub-graph with loop variables and break conditions | core |
| `human-input` | Pause for a form / decision, resume with the answer | core |
| `tool` | Call a native or MCP tool | core |
| `llm` | Chat completion with streaming, memory, vision, structured output | llm |
| `agent` | Tool-calling loop with approvals and parallel tool calls; streams every turn per token; per-run tool context (e.g. the user's JWT, never shown to the model) | llm |
| `question-classifier` | Route by LLM classification | llm |
| `parameter-extractor` | Extract typed parameters from text | llm |
| `http-request` | HTTP with SSRF-safe egress, auth, JSON/form/multipart bodies | http |
| `code` | JavaScript in QuickJS with memory/time limits; Python via a sandbox service or a local subprocess (trusted code) | code |
| `document-extractor` | Text from PDF, Office, spreadsheets, HTML, EPUB, CSV, e-mail and subtitles | doc |
| `knowledge-retrieval` | Search one or more knowledge bases (vector, keyword or hybrid, rerank, metadata filters) | rag |

Every kind publishes a catalog descriptor with a config JSON Schema
(`graphrt nodes`, `GET /v1/nodes`), so visual builders can render palettes and
forms and validate documents (`POST /v1/validate`) before deploying them.

Model providers: any OpenAI-compatible API (OpenAI, Azure OpenAI, Ollama, vLLM,
Groq, OpenRouter, DeepSeek…) and Anthropic, configured from the environment or a
providers file (`--providers`); `--model ollama/qwen2.5:14b` points every model
node at another model. Tools: native Rust closures and
MCP servers (stdio and streamable HTTP). Opt-in live tests send tiny chat,
streaming and tool-calling requests to OpenAI, Anthropic, Gemini, Bedrock and
TypeSafe, nightly in CI behind repository secrets
([docs/providers.md](docs/providers.md#live-provider-tests)).

## Knowledge bases

`knowledge-retrieval` searches datasets described in a knowledge config
(`--knowledge kb.yaml`), and `graphrt kb ingest|query` fills and tests them from
the command line.

- **Stores**: in memory (optionally a JSON file), a single **SQLite** file
  (incremental writes, shared by several processes), or Qdrant.
- **Embeddings**: computed **in-process** on the CPU (BERT-family models such as
  `all-MiniLM-L6-v2` or the multilingual `multilingual-e5-small`, downloaded
  once, then offline — no server or API key), Ollama, or any OpenAI-compatible
  API.
- **Retrieval**: vector, BM25 keyword or weighted hybrid search, rerankers
  (Cohere, Jina, Voyage), score thresholds, metadata filters, and
  **parent-child chunking** (search small chunks, answer with their section).
- **Management API** for knowledge base UIs: `graphrt serve --knowledge`
  exposes `/v1/knowledge` to create and delete datasets, list documents, upload files (PDF,
  Office, Markdown...), inspect chunks, enable or disable and delete documents,
  and test retrieval — on the same knowledge base the workflows use. See
  [docs/serving.md](docs/serving.md#knowledge-base-api).

```yaml
embedders: { local: { type: local } }                  # in-process, offline after first use
stores:    { local: { type: sqlite, path: kb.db } }
datasets:
  - { id: handbook, store: local, embedder: local,
      chunking: { max_chars: 400, parent_max_chars: 1600 } }
```

In-process embeddings, the SQLite store, parent-child chunking and the
management API (including creating and deleting datasets) are available since
0.2.0.

## Evaluate

`graphrt eval` checks a workflow against test cases and gates CI on the pass
rate (exit code 5 when the gate fails). Eval files use Promptfoo's
`tests` / `assert` shape; assertions cover answers, JSON outputs, latency and
token budgets, the tools an agent called (and in what order, with which
arguments), retrieval recall / precision / MRR, and LLM judges that are
themselves graphrt workflows (versioned by fingerprint, calibrated against
human labels with TPR / TNR / Cohen's kappa). `--trials` reports pass@k and
pass^k, and `--baseline` fails CI on regressions beyond a tolerance. Model
responses can be replayed from recorded cassettes (`kind: replay` providers),
so agent and judge suites run offline, free and deterministically.

```yaml
# support.eval.yaml
workflow: support-agent.yaml
providers: providers.replay.yaml        # recorded model responses
tests:
  - vars: { question: "Please refund order 456." }
    assert:
      - { type: icontains, value: refunded }
      - { type: trajectory:tool-sequence, value: [search_orders, refund_order] }
      - { type: judge, workflow: judges/goal-success.yaml, inputs: { goal: "Order 456 is refunded." } }
      - { type: tokens, threshold: 4000, weight: 0 }   # reported, never fails the test
gate: { pass_rate: 0.95, pass_hat_k: 0.8 }
```

```sh
graphrt eval support.eval.yaml -j 8 --trials 3 --baseline main.json --output results.json --junit junit.xml
```

The same harness is a library ([`graphrt-eval`](crates/graphrt-eval)) for
`cargo test`. See [docs/evals.md](docs/evals.md) and the offline
[examples](examples/evals).

In production, `graphrt serve --evaluators evaluators.yaml` scores a sample of
finished runs with the same judges and checks in a bounded background pool,
`POST /v1/runs/{id}/feedback` records human feedback, and every score is a
`gen_ai.evaluation.result` span event in the run's trace. `--otel-preset
langfuse` (or `langsmith`, `braintrust`, `opik`, `phoenix`, `mlflow`) sends the
traces to a vendor configured from its own environment variables, and
`--langfuse-scores` uploads the scores.

## Plugins: write a node in 20 lines

Add node kinds (and agent tools) in Python, TypeScript/JavaScript, Rust or
WebAssembly, without recompiling graphrt. A plugin is a directory with a
manifest; graphrt starts it on first use and talks JSON-RPC over stdio.

```python
# plugin.py — scaffold with: graphrt plugin new acme --lang python
from graphrt_plugin import Plugin, PluginError

plugin = Plugin("acme")

@plugin.node("acme.shout")
async def shout(ctx, req):
    text = req.config.get("text")          # `{{#start.msg#}}` already resolved
    if not isinstance(text, str):
        raise PluginError("`text` must be a string", kind="invalid_input")
    await ctx.chunk("text", text.upper())  # streams like LLM tokens
    return {"outputs": {"text": text.upper()}}

if __name__ == "__main__":
    plugin.run()
```

```sh
graphrt plugin new acme --lang python        # or typescript, rust
graphrt run acme/example.yaml --plugins acme --input who=World
graphrt plugin install ./acme                # or a git URL; lands in ~/.graphrt/plugins
graphrt plugin install open-meteo            # or a name from the plugin catalog (`graphrt plugin search`)
graphrt nodes                                # plugin kinds show up next to built-ins
```

Plugin nodes stream, branch, pause for approval, report errors with retry hints,
and expose tools to `agent` nodes. WebAssembly plugins run sandboxed in-process
(fuel and memory limits, no system access) — drop a `.wasm` file into a plugin
directory. See [docs/plugins.md](docs/plugins.md), the
[manifest specification](docs/plugin-manifest.md),
[docs/wasm-plugins.md](docs/wasm-plugins.md) and
[`plugins/examples`](plugins/examples).

## Embed it

```rust
use std::sync::Arc;
use futures::StreamExt;
use graphrt_core::{Engine, Event, Graph, Registry, RunInput};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut registry = Registry::with_builtins();
    graphrt_llm::register(&mut registry);

    let graph = Arc::new(Graph::from_text(include_str!("workflow.yaml"), &registry)?);
    let services = graphrt_core::Services::new().with(graphrt_llm::ModelRegistry::from_env()?);
    let engine = Engine::new(graph).with_services(services);

    let mut run = engine.run(RunInput::new().input("name", "Ada"));
    while let Some(event) = run.next().await {
        match event {
            Event::NodeChunk { chunk, .. } => print!("{chunk}"),
            Event::RunSucceeded { outputs, .. } => println!("\n{outputs:?}"),
            Event::RunPaused { snapshot, .. } => { /* persist snapshot.to_json()? and resume later */ }
            _ => {}
        }
    }
    Ok(())
}
```

Custom nodes, services, model providers and native tools are covered in
[docs/embedding.md](docs/embedding.md).

## Security model

| Concern | Default |
| --- | --- |
| Secrets | `secret` values are masked in events, logs, `Debug` and templates; `env.*` is never written to snapshots. Plaintext is only exposed to nodes that must transmit it (HTTP auth, code inputs). Node error messages are scrubbed of every secret the run knows (and provider errors of the request's credentials); strings a code node returns that contain a secret become secrets again. |
| Network egress | Every workflow-controlled URL (`http-request`, file downloads) resolves DNS itself, rejects loopback/private/link-local/unique-local/CGNAT/cloud-metadata addresses (in any IP notation, including IPv4-embedding IPv6 forms), connects only to the checked addresses and re-checks every redirect. `--allow-private-network` opts out. |
| Templates | `{{#…#}}` placeholders are parsed once; resolved values are never re-parsed (no prompt/template injection). Jinja runs without filesystem access, with a fuel budget and output cap. |
| Code | QuickJS on its own stack-sized thread with memory, stack and time limits that workflows can only lower (and never switch off), no filesystem, network, environment, timers or modules; a fresh runtime per execution. |
| MCP | stdio servers start with an empty environment plus an explicit allow-list; every call has a timeout; tools can be allow-listed. |
| Plugins | Process plugins start lazily with an empty environment plus the variables their manifest lists, optional `checksums` verified before every start, per-request timeouts and message size caps; secrets are masked unless the manifest asks for them. WebAssembly plugins run in an interpreter with fuel and memory limits and no system imports. A plugin can never replace a built-in or another plugin's node kind. |
| Server | Optional bearer auth (constant-time comparison); without a key, a loopback server only accepts loopback `Host` names (DNS rebinding). Body and upload limits, a cap on concurrently executing runs (`--max-active-runs`), no CORS, env values cannot be set by requests. Per-user credentials mapped from headers (`--input-header`) are secrets, never stored or logged in plaintext; `--verify-jwt` verifies them as JWTs (a valid token may only start runs and rate its own runs) and `--user-rate-limit` limits each user. Webhooks verify HMAC-SHA256 signatures in constant time with a replay window; completion callbacks are signed and go through the egress policy. |
| Limits | Step budget, run duration, per-node timeouts, container depth, iteration/loop caps. |

The threat model, sandbox limits, egress rules and deployment knobs are in
[docs/security.md](docs/security.md); how to report a vulnerability is in
[SECURITY.md](SECURITY.md).

### Privacy: usage telemetry

The `graphrt` binary has **opt-in** anonymous usage telemetry, **off by
default**. Nothing is sent unless you run `graphrt telemetry enable` (or set
`GRAPHRT_TELEMETRY=1`), and never when `DO_NOT_TRACK` is set, in CI, or when
the build has no telemetry endpoint. `graphrt telemetry status` prints exactly
what would be sent; `graphrt telemetry disable` turns it off and deletes the
local id and counts.

When on, at most one report a day contains only: a random install id, the
graphrt version, OS and CPU architecture, the compiled features, the build
(`cli` or `docker`), and per-day counts of runs and `serve` requests. It never
contains workflows, inputs, outputs, file paths, host names or IP addresses.
The embedded and FFI libraries contain no telemetry, and builds without the
`telemetry` feature (such as the small-device server build) contain none
either. Details: [docs/telemetry.md](docs/telemetry.md).

## Binary size

Measured with `cargo build -p graphrt-cli --profile min-size` (opt-level `z`, LTO, stripped, unwinding kept so a panicking node fails only that node) on x86_64 Linux:

| Features | Size |
| --- | --- |
| core only (`--no-default-features`) | 2.9 MiB |
| `llm`, `http`, `code`, `python`, `mcp`, `serve` | 8.2 MiB |
| + `plugins`, `wasm` | 9.7 MiB |
| default (+ `doc`, `rag`) | 12.2 MiB |

In-process embedding models (`local-embed`, on by default since 0.2.0) add
about 2.1 MiB (measured on macOS arm64: 10.3 MiB → 12.4 MiB);
models themselves are downloaded on first use, not bundled. A server build for
small devices, `--no-default-features --features llm,http,wasm,serve`, is
6.3 MiB as a static aarch64 musl binary (6.2 MiB for 32-bit ARMv7).

End-user JWT verification for `serve` (`jwt`, on by default) adds about
33 KiB: it reuses the `ring` and `reqwest` crates already linked for TLS.

Evaluation (`eval` for `graphrt eval`, `online-eval` for `serve --evaluators`)
is opt-in: `cargo install graphrt-cli --features eval,online-eval`. The release
binaries, Homebrew formula and GHCR image are built with both.

Every feature is optional: build only what a deployment needs.

## Performance

Runtime overhead only: every model call goes to an in-process mock, so these
numbers exclude providers and the network. Apple M-series laptop (11 cores),
`bench/run.sh`; raw data in [bench/results](bench/results) (`2026-11-19-Bima-Pangestu`
for 0.2.0, `2026-11-19-Bima-Pangestu-0.3.0` for 0.3.0). The server and CLI rows
compare the two release binaries side by side in one session (static musl,
Linux arm64 container, 1 CPU, median of 3 rounds); absolute numbers move with
machine load, the ratio is what to read.
[docs/benchmarks.md](docs/benchmarks.md) has the methodology and Linux x86_64 numbers.

| What | 0.2.0 | 0.3.0 |
| --- | --- | --- |
| Scheduling per node, 1,000-node chain | 0.80 µs | **0.35 µs** |
| Scheduling per node, 1,000-way fan-out / fan-in | 1.04 µs | **0.47 µs** |
| Template node, per node (100-node chain) | 1.39 µs | **0.80 µs** |
| Allocations per node, noop chain | 12.2 | **4.1** |
| First streamed chunk, engine overhead | 9.7 µs | **5.8 µs** |
| 1,000 concurrent instant runs | 205k runs/s | **289k runs/s** |
| `graphrt run hello.yaml`, start to exit | 7.5 ms | **7.3 ms** |
| `graphrt serve`, synchronous runs, 1 CPU | 13,837 req/s | **15,540 req/s** |
| `graphrt serve`, streaming (SSE) runs, 1 CPU | 2,996 req/s | **7,639 req/s** |

On small devices (Linux arm64, cgroup limits), the 6.3 MiB server build serves
a 3-node workflow at ~600 requests/s on a quarter of a core with ~3.4 MiB of
steady memory, and stays up under a 16 MiB limit at a tenth of a core
([details](docs/serving.md#small-devices)).

Against LangGraph 1.2 (CPython 3.11, async, tracing off) on the same laptop,
both calling one local fake OpenAI server: a 3-node LLM workflow cold-starts in
5 ms vs 728 ms with 13 vs 101 MiB peak RSS, orchestration costs ~0.4 µs vs
~100–200 µs per node, and with 50 ms model latency 1,000 concurrent runs reach
~17k vs ~210 runs/s. With one run in flight against a slow model the stacks are
within ~20% of each other, since model latency dominates
([methodology and caveats](docs/benchmarks.md#graphrt-vs-python-agent-stacks)).

## Architecture

```
graphrt-core    graph model, variable pool, templates, conditions, engine, events,
                snapshots, built-in control nodes, tool contract (wasm-friendly)
graphrt-llm     chat model abstraction, OpenAI-compatible, Anthropic, Gemini, Bedrock,
                llm / agent / classifier / extractor nodes
graphrt-http    http-request node and egress policy
graphrt-code    code node (QuickJS) and remote sandbox hook
graphrt-mcp     MCP tool provider
graphrt-doc     document-extractor node (PDF, Office, HTML, EPUB, e-mail...)
graphrt-rag     knowledge-retrieval node, stores, embedders, ingestion
graphrt-plugin  process / HTTP plugin host; graphrt-wasm: sandboxed WebAssembly plugins
graphrt-bundle  portable .grt bundles; graphrt-otel: OpenTelemetry export
graphrt-eval    eval harness: test suites, assertions, trajectories, retrieval metrics, CI gate
graphrt-ffi     Kotlin / Swift bindings for Android and iOS
graphrt-gcp     gateway on Google Cloud: GCP auth, Cloud Run provider + deploy driver
graphrt-cli     `graphrt` binary: run, validate, inspect, serve, eval, kb, plugin, bundle
```

Read [docs/architecture.md](docs/architecture.md) for the scheduling rules,
failure policies, containers and pause/resume design.

## Roadmap

See [ROADMAP.md](ROADMAP.md) and the [CHANGELOG](CHANGELOG.md).

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) and the [Code of Conduct](CODE_OF_CONDUCT.md).
Issues and pull requests are welcome. Report vulnerabilities privately as described
in [SECURITY.md](SECURITY.md).

## License

Apache-2.0. See [LICENSE](LICENSE).
