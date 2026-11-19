# Embedding graphrt

The `graphrt` binary is a thin layer over the libraries. Embedding them gives
you full control over services, persistence and transport.

```toml
[dependencies]
graphrt-core = "0.3"
graphrt-llm = "0.3"      # optional: llm / agent / classifier / extractor nodes
graphrt-http = "0.3"     # optional: http-request node
graphrt-code = "0.3"     # optional: code node
graphrt-mcp = "0.3"      # optional: MCP tools
futures = "0.3"
tokio = { version = "1", features = ["rt", "macros"] }
```

## Running a workflow

```rust
use std::sync::Arc;
use futures::StreamExt;
use graphrt_core::{Engine, Event, Graph, Registry, RunInput, Services};

async fn run(yaml: &str) -> Result<(), Box<dyn std::error::Error>> {
    // 1. Node kinds: builtins plus whatever crates you enable.
    let mut registry = Registry::with_builtins();
    graphrt_llm::register(&mut registry);
    graphrt_http::register(&mut registry);

    // 2. Compile once; share the Arc<Graph> across any number of runs.
    let graph = Arc::new(Graph::from_text(yaml, &registry)?);

    // 3. Host services that nodes look up at run time.
    let services = Services::new()
        .with(graphrt_llm::ModelRegistry::from_env()?)
        .with(graphrt_http::HttpClient::new(Default::default())?);

    let engine = Engine::new(graph).with_services(services);

    // 4. Polling the stream drives the run; nothing is spawned.
    let mut run = engine.run(RunInput::new().input("question", "hi").sys("user_id", "u-1"));
    let handle = run.handle(); // abort / pause / update_variable from elsewhere
    while let Some(event) = run.next().await {
        if let Event::RunSucceeded { outputs, .. } = &event {
            println!("{outputs:?}");
        }
    }
    let _ = handle;
    Ok(())
}
```

`Engine::run_to_end(input)` is a convenience that drains the stream and returns
the terminal event.

### Clocks and executors

The engine never depends on a specific async runtime. Timers (retry delays,
timeouts, run duration) come from the `Clock` trait. With the default `tokio`
feature `TokioClock` is used; on other executors implement `Clock` and pass it
with `Engine::with_clock`. `NoTimerClock` runs everything without timers
(useful in tests and in wasm hosts without a timer).

### Limits

```rust
use graphrt_core::engine::Limits;
let engine = engine.with_limits(Limits {
    max_steps: 500,
    max_duration_ms: Some(60_000),
    default_node_timeout_ms: Some(30_000),
    ..Limits::default()
});
```

## Ordered answer streaming

Engine events carry raw token chunks (`Event::NodeChunk`) as soon as they are
produced, even from branches that are not yet known to reach an answer. Use
`ResponseAssembler` to turn events into the user-facing answer text in
template order:

```rust
use graphrt_core::response::ResponseAssembler;

let mut assembler = ResponseAssembler::new(engine.graph());
while let Some(event) = run.next().await {
    for chunk in assembler.push(&event) {
        print!("{}", chunk.text);
    }
}
```

## Pause and resume

```rust
use graphrt_core::{Event, Snapshot};

if let Event::RunPaused { reasons, snapshot } = event {
    store.save(&snapshot.run_id, &snapshot.to_json()?)?;   // your storage
}

// Later, possibly in another process (same workflow document):
let snapshot = Snapshot::from_json(&store.load(run_id)?)?;
let mut inputs = indexmap::IndexMap::new();
inputs.insert("review".to_owned(), graphrt_core::Value::from_json(serde_json::json!({
    "action": "approve", "values": {"note": "ok"}
})));
let run = engine.run(RunInput::new().resume(snapshot, inputs));
```

Snapshots never contain `env.*` values; supply them again in `RunInput::env`.
The engine rejects a snapshot whose graph fingerprint does not match.

## Custom nodes

```rust
use std::sync::Arc;
use futures::future::BoxFuture;
use graphrt_core::node::{parse_config, NodeResult};
use graphrt_core::{Node, NodeContext, NodeError, NodeOutput, Registry, Selector, Value};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct UppercaseConfig {
    input: Selector,
}

#[derive(Debug)]
struct Uppercase(UppercaseConfig);

impl Node for Uppercase {
    fn run(&self, ctx: NodeContext) -> BoxFuture<'_, NodeResult> {
        Box::pin(async move {
            let text = ctx
                .get(&self.0.input)
                .and_then(|v| v.as_str().map(str::to_uppercase))
                .ok_or_else(|| NodeError::invalid_input("input must be a string"))?;
            Ok(NodeOutput::new().with("text", text).into())
        })
    }
}

let mut registry = Registry::with_builtins();
registry.register("uppercase", |doc| Ok(Arc::new(Uppercase(parse_config(doc)?))));
```

Rules for nodes:

- Read inputs from `ctx.pool` (an immutable snapshot). Return outputs and
  writes in `NodeOutput`; never mutate shared state.
- `NodeOutput::outputs` is a `graphrt_core::Variables` map. The engine stores
  it as the node's pool namespace and emits it in `NodeSucceeded` without
  copying, so build it once, with `.with("name", value)` or `outputs.insert`.
  String-literal names cost no allocation.
- Use `NodeError::fatal` for errors a retry cannot fix and `NodeError::new` for
  transient ones; the engine only retries retryable errors.
- Stream partial output with `ctx.chunk(field, text)`; report structured
  progress with `ctx.progress(name, data)`. A node that streams many chunks
  can keep the field name in an `Arc<str>` and call
  `ctx.chunk_shared(&field, text)` to avoid allocating it per chunk
  (`Event::NodeChunk::field` is an `Arc<str>`).
- Return `NodeOutcome::Paused(PauseRequest { .. })` to wait for the host; you
  receive your saved `state` and the host input in `ctx.resume`.
- Override `role()` for branch (`NodeRole::Branch`, set `NodeOutput::branch`),
  response or container nodes.

## Custom services

`Services` is a type map. Register anything `Send + Sync + 'static` and fetch it
in a node with `ctx.service::<T>()` or `ctx.require_service::<T>("name")`:

```rust
#[derive(Debug)]
struct Crm { base_url: String }

let services = Services::new().with(Crm { base_url: "https://crm.internal".into() });
// in a node: let crm = ctx.require_service::<Crm>("CRM client")?;
```

## Custom model providers

Implement `graphrt_llm::ChatModel` and register it under a provider name used in
workflow `model.provider` fields:

```rust
use std::sync::Arc;
use futures::future::BoxFuture;
use graphrt_llm::{ChatModel, ChatRequest, ChatStream, LlmError, ModelRegistry};

#[derive(Debug)]
struct MyModel;

impl ChatModel for MyModel {
    fn chat(&self, request: ChatRequest) -> BoxFuture<'_, Result<ChatStream, LlmError>> {
        Box::pin(async move {
            // Call your backend and stream `ChatDelta`s (Text, Reasoning, ToolCall, Usage, Finish).
            let _ = request;
            Err(LlmError::Config("not implemented".into()))
        })
    }
}

let models = ModelRegistry::new().with("my-provider", Arc::new(MyModel));
```

Any OpenAI-compatible endpoint works without code: see
`graphrt_llm::openai::OpenAiConfig` or the `GRAPHRT_OPENAI_COMPAT_<NAME>_*`
environment variables.

## Native tools

Expose Rust functions to `tool` and `agent` nodes:

```rust
use std::sync::Arc;
use graphrt_core::tool::{ToolOutput, ToolRegistry, ToolSpec};
use graphrt_core::tool_node::NativeTools;
use serde_json::json;

let tools = NativeTools::new().with(
    ToolSpec {
        name: "add".into(),
        description: "Add two numbers".into(),
        parameters: json!({"type": "object", "properties": {"a": {"type": "number"}, "b": {"type": "number"}}, "required": ["a", "b"]}),
    },
    |args| async move {
        let sum = args["a"].as_f64().unwrap_or(0.0) + args["b"].as_f64().unwrap_or(0.0);
        Ok(ToolOutput::text(sum.to_string()))
    },
);

let mut registry = ToolRegistry::new();
registry.add("math", Arc::new(tools));
let services = graphrt_core::Services::new().with(registry);
// workflow: tools: [math/add]   or   type: tool, provider_id: math, tool_name: add
```

MCP servers are added the same way: `graphrt_mcp::connect_all(&configs).await?`
returns a `ToolRegistry` with each server registered as `<name>` and `mcp:<name>`.
