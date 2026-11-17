# Architecture

graphrt runs workflow graphs: LLM calls, tools, HTTP requests, code, branches,
loops and human approvals, wired together as a directed graph. It is designed
to be embedded (as a library), shipped (as a single small binary), and to run
anywhere a platform-built workflow needs to execute.

## Design goals

| Goal | How |
| --- | --- |
| Small | The core depends on `futures`, `serde`, `indexmap`, `thiserror` and optionally `minijinja` / `serde-saphyr`. HTTP, model providers, code sandboxes and MCP live in separate crates behind features. |
| Fast | No thread pool, no queue hopping: one task per run polls a `FuturesUnordered` of node futures. Values are reference-counted, so copying a variable pool for a loop round or iteration item is O(namespaces), not a deep copy. |
| Portable | The core never touches a specific async runtime. Timers come from a `Clock` trait (Tokio implementation included). The core compiles to `wasm32-unknown-unknown`. |
| Flexible | Every node kind, model provider and tool source is pluggable through `Registry` (node factories) and `Services` (typed host services). |
| Safe by default | Secrets are a distinct value type that is masked in every serialization path and never written to snapshots. HTTP egress blocks private networks unless allowed. Templates are sandboxed with fuel limits. Code runs in an interpreter with memory and time limits. |

## Crates

```
graphrt-core   graph model, variable pool, templates, conditions, engine,
               events, snapshots, built-in control nodes, tool contract
graphrt-llm    chat model abstraction, OpenAI-compatible + Anthropic clients,
               llm / question-classifier / parameter-extractor / agent nodes
graphrt-http   http-request node with an SSRF-safe egress policy
graphrt-code   code node (embedded JavaScript; remote sandbox hook for others)
graphrt-mcp    Model Context Protocol tool provider
graphrt-cli    `graphrt` binary: run, validate, resume, serve
```

## Lifecycle of a run

1. A `Workflow` document (JSON or YAML) is parsed and compiled into an immutable
   `Graph` by a `Registry`. Compilation validates ids, edges, container scopes,
   cycles and entry nodes, and builds every node once.
2. `Engine::run(RunInput)` returns a `RunStream`. Polling the stream drives the
   run; nothing is spawned.
3. The engine seeds the variable pool (`sys.*`, `env.*`, `conversation.*`, start
   inputs) and schedules the entry node.
4. Ready nodes run concurrently. Each receives an owned `NodeContext` with a
   cheap snapshot of the pool and returns a `NodeOutcome`. The engine commits
   outputs and variable writes, then resolves outgoing edges.
5. The stream ends with exactly one terminal event: `RunSucceeded`,
   `RunFailed`, `RunAborted` or `RunPaused` (which carries a snapshot).

## Scheduling rules

- Every edge is `unknown`, `taken` or `skipped`.
- A node becomes ready once all its incoming edges are resolved and at least one
  is taken. If all are skipped, the node is skipped and the skip propagates.
- Branch nodes (if-else, question-classifier, human-input) take only the edges
  whose handle matches their selection.
- Response nodes (answer, end) contribute to the run outputs.

## Failures

Each node has a policy:

- `retry`: max retries, interval and exponential backoff. Waiting happens inside
  the node's own future, so other branches keep running. Errors marked
  non-retryable (bad input, bad configuration) skip retries.
- `timeout_ms`: per-attempt timeout (when the clock supports timers).
- `on_error`: `fail` (default), `fail-branch` (route through `fail-branch`
  edges), or `default-value` (continue with configured outputs).

A node that succeeds after a retry is a success, not a partial failure.

## Containers

Iteration and loop nodes own a child scope. They execute it through
`NodeContext::subgraph`, which runs the scope as a child frame with the same
scheduler, event sink, limits and cancellation. Because nodes are futures,
nesting needs no extra worker pools and cannot deadlock. Parallel iteration is
bounded concurrency over child frames; results keep input order.

## Pause and resume

A run pauses when the host calls `RunHandle::pause` or a node returns
`NodeOutcome::Paused` (human input, tool approval). In-flight nodes are allowed
to finish, then the run emits `RunPaused` with a `Snapshot`:

- variable pool (without `env.*`),
- node and edge states,
- per-node resume state (containers store their child frame snapshots here).

Resume by passing the snapshot plus any host input (e.g. a submitted form) in
`RunInput::resume`. The engine checks the graph fingerprint before restoring.

## Cancellation

`RunHandle::abort` drops every in-flight node future immediately. There are no
detached threads that keep calling external services after an abort.

## Events

Events are plain serde data tagged by `type` and can be streamed over SSE as-is.
Raw LLM tokens are emitted as `node_chunk`. `ResponseAssembler` turns raw events
into the ordered answer text that end users see, buffering tokens from branches
that finish out of order.
