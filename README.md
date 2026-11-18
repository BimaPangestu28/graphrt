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
| Container | ~24 MB distroless image — see [docs/deploy.md](docs/deploy.md) |
| Phone | Embed `graphrt-ffi` in an Android or iOS app — see [docs/mobile.md](docs/mobile.md) |
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

- **Small.** Core-only binary ≈ 3 MB, everything included ≈ 12 MB. No Python, no Node, no database.
- **Fast.** One task per run polls node futures concurrently; nothing is spawned per node. Variable pools are reference-counted, so loops and parallel iterations don't deep-copy state.
- **Portable.** The core is executor-agnostic (bring your own clock) and compiles to `wasm32-unknown-unknown`.
- **Agent-native.** First-class `agent` node with a tool-calling loop, parallel tool calls, MCP servers, human approval of tool calls, retries with backoff, per-node timeouts and step/time budgets.
- **Durable.** Pause anywhere (human input, tool approval, API call), get a JSON snapshot, resume later — even inside loops and iterations.
- **Safe by default.** Secrets never leave the process in logs, events or snapshots. HTTP egress blocks private networks (SSRF). Templates run with fuel limits. Code runs in an interpreter with memory/time limits and no I/O.

## Install

```sh
# Linux / macOS: prebuilt static binary (verifies SHA256SUMS)
curl -fsSL https://raw.githubusercontent.com/BimaPangestu28/graphrt/main/install.sh | sh

# with Rust
cargo install graphrt-cli            # installs the `graphrt` binary

# container
docker run --rm -p 8080:8080 -v "$PWD/flows:/workflows" ghcr.io/bimapangestu28/graphrt
```

Windows and other archives are on the [releases page](https://github.com/BimaPangestu28/graphrt/releases).
Libraries are on crates.io: `graphrt-core` (runtime) plus integration crates such as
`graphrt-llm`, `graphrt-http`, `graphrt-rag`.

## Quick start

```sh
cargo install --path crates/graphrt-cli        # installs `graphrt`

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

See [docs/cli.md](docs/cli.md) for every command and flag.

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
| `agent` | Tool-calling loop with approvals and parallel tool calls | llm |
| `question-classifier` | Route by LLM classification | llm |
| `parameter-extractor` | Extract typed parameters from text | llm |
| `http-request` | HTTP with SSRF-safe egress, auth, JSON/form/multipart bodies | http |
| `code` | JavaScript in QuickJS with memory/time limits; Python via a sandbox service or a local subprocess (trusted code) | code |

Every kind publishes a catalog descriptor with a config JSON Schema
(`graphrt nodes`, `GET /v1/nodes`), so visual builders can render palettes and
forms and validate documents (`POST /v1/validate`) before deploying them.

Model providers: any OpenAI-compatible API (OpenAI, Azure OpenAI, Ollama, vLLM,
Groq, OpenRouter, DeepSeek…) and Anthropic, configured from the environment or a
providers file (`--providers`); `--model ollama/qwen2.5:14b` points every model
node at another model. Tools: native Rust closures and
MCP servers (stdio and streamable HTTP).

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
graphrt nodes                                # plugin kinds show up next to built-ins
```

Plugin nodes stream, branch, pause for approval, report errors with retry hints,
and expose tools to `agent` nodes. WebAssembly plugins run sandboxed in-process
(fuel and memory limits, no system access) — drop a `.wasm` file into a plugin
directory. See [docs/plugins.md](docs/plugins.md),
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
| Secrets | `secret` values are masked in events, logs, `Debug` and templates; `env.*` is never written to snapshots. Plaintext is only exposed to nodes that must transmit it (HTTP auth, code inputs). |
| Network egress | `http-request` resolves DNS itself, rejects loopback/private/link-local/CGNAT/metadata addresses (including IPv4-mapped IPv6), pins the checked address and re-checks every redirect. `--allow-private-network` opts out. |
| Templates | `{{#…#}}` placeholders are parsed once; resolved values are never re-parsed (no prompt/template injection). Jinja runs without filesystem access, with a fuel budget and output cap. |
| Code | QuickJS with memory, stack and time limits, no filesystem, network, timers or modules. |
| MCP | stdio servers start with an empty environment plus an explicit allow-list; every call has a timeout; tools can be allow-listed. |
| Plugins | Process plugins start lazily with an empty environment plus the variables their manifest lists, optional `checksums` verified before every start, per-request timeouts and message size caps; secrets are masked unless the manifest asks for them. WebAssembly plugins run in an interpreter with fuel and memory limits and no system imports. A plugin can never replace a built-in or another plugin's node kind. |
| Server | Optional bearer auth (constant-time comparison), request body limit, env values cannot be set by requests. |
| Limits | Step budget, run duration, per-node timeouts, container depth, iteration/loop caps. |

## Binary size

Measured with `cargo build -p graphrt-cli --profile min-size` (opt-level `z`, LTO, stripped, unwinding kept so a panicking node fails only that node) on x86_64 Linux:

| Features | Size |
| --- | --- |
| core only (`--no-default-features`) | 2.9 MiB |
| `llm`, `http`, `code`, `python`, `mcp`, `serve` | 8.2 MiB |
| + `plugins`, `wasm` | 9.7 MiB |
| default (+ `doc`, `rag`) | 12.2 MiB |

Every feature is optional: build only what a deployment needs.

## Architecture

```
graphrt-core   graph model, variable pool, templates, conditions, engine, events,
               snapshots, built-in control nodes, tool contract (wasm-friendly)
graphrt-llm    chat model abstraction, OpenAI-compatible + Anthropic, llm/agent nodes
graphrt-http   http-request node and egress policy
graphrt-code   code node (QuickJS) and remote sandbox hook
graphrt-mcp    MCP tool provider
graphrt-cli    `graphrt` binary: run, validate, inspect, serve
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
