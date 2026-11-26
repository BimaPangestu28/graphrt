# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses
[Semantic Versioning](https://semver.org/spec/v2.0.0.html). Until 1.0, minor
releases may contain breaking changes; they are called out under **Changed**.

## [Unreleased]

### Added

- graphrt-mcp: dynamic access tokens for Streamable HTTP servers.
  `HttpConfig::token_source` takes a `TokenProvider` (set in code, never
  parsed from a config file) that is asked for the token before every request
  and, after an HTTP 401, once for a new one before the request is retried
  once. Hosts use it for OAuth sign-in with refresh tokens; the crate itself
  stays free of OAuth dependencies. A static `bearer_token` and a token source
  cannot be combined. The token client never follows redirects.
- graphrt-plugin: `runtime: mcp` for MCP server presets. A plugin without code
  declares a remote MCP server (`mcp.url`, `transport: streamable-http`,
  `auth: none | bearer | oauth`, per-tool `approval` defaults with `prefix*`
  patterns, and a `guide` file for agent builders); hosts with an MCP client
  register it when the plugin is installed. `nodes` and `tools` must be empty,
  and the plugin host loads such a plugin without registering anything.
- graphrt-plugin: `templates` (any runtime): starter prompts a host can offer
  while the plugin is installed, behind the `mcp-presets` feature of
  graphrt-plugin (default on, and in the CLI's default features).

## [0.1.0] - 2026-11-18

First public release: one small runtime that runs agentic AI workflows on a
laptop, a server, a container, the edge or a phone.

### Added

**Runtime (`graphrt-core`)**
- Executor-agnostic engine: runs are a `Stream` of events driven by polling, no
  thread pool. Concurrent branches, OR-joins with skip propagation, branch handles.
- Per-node retries with exponential backoff and provider `retry-after`, per-attempt
  timeouts and deadlines, step / time / nesting limits, real cancellation on abort.
- Error strategies: `fail`, `fail-branch`, `default-value`.
- Iteration (parallel or sequential, three error modes) and loop containers run
  as child frames; pause and resume work inside them.
- Pause / resume with JSON snapshots (human input, tool approval, external pause),
  graph fingerprint checks, secrets never persisted.
- Reference-counted values and copy-on-write variable pools; typed values with a
  dedicated secret type masked in every serialization path.
- Workflow documents in YAML or JSON (YAML 1.2 booleans), validated at build time:
  ids, scopes, cycles, entry nodes and every variable reference.
- Built-in nodes: start, end, answer, if-else (full operator set), template-transform
  (sandboxed Jinja with fuel), variable assigner / aggregator, list operator,
  iteration, loop, human-input, tool.
- Ordered response assembler for streaming answers across branches and retries.
- Node catalog: descriptors with JSON Schema config, outputs and handles.
- File resolver contract (data URLs, local directories, policy-checked HTTP).
- Compiles to `wasm32-unknown-unknown`.

**Models and agents (`graphrt-llm`)**
- Providers: OpenAI-compatible (OpenAI, Azure, Groq, DeepSeek, OpenRouter, Mistral,
  Ollama, vLLM...), Anthropic, Google Gemini, AWS Bedrock (SigV4 or API key), and
  TypeSafe System One (Jev) for calibrated classification.
- Nodes: llm (streaming, memory, vision, structured output), question-classifier
  (LLM or Jev engine with confidence thresholds), parameter-extractor, agent
  (tool-calling loop, parallel tools, per-tool human approval), system-one.
- Credential sources for apps: per-request credentials, caching, token broker,
  401/403 refresh-and-retry; providers file with model aliases and defaults.

**Integrations**
- `graphrt-http`: http-request node with an SSRF-safe egress policy (private
  ranges, DNS rebinding, per-hop redirect checks, credential stripping).
- `graphrt-code`: JavaScript code node in QuickJS with memory / time / output
  limits; Python via a remote sandbox or an explicit local subprocess runner.
- `graphrt-mcp`: Model Context Protocol tools over stdio (clean environment) and HTTP.
- `graphrt-doc`: document-extractor for PDF, DOCX, PPTX, XLSX/ODS, EPUB, HTML,
  CSV, e-mail and subtitles, hardened against hostile files.
- `graphrt-rag`: knowledge-retrieval node, embedders, in-memory (optionally
  JSON-file backed), SQLite and Qdrant stores, hybrid vector + BM25 search,
  reranking, chunking, ingestion. The SQLite store keeps a knowledge base in
  one local file with incremental writes, shared safely between processes.

**Extensibility**
- `graphrt-plugin`: plug-and-play plugins over JSON-RPC (process or HTTP), with
  host-granted trust, clean environments, checksums and crash recovery.
- SDKs for plugins in Rust (`graphrt-plugin-sdk`), Python and TypeScript.
- `graphrt-wasm`: sandboxed WebAssembly plugins (wasmi, fuel and memory limits).

**Distribution**
- `graphrt` CLI: `run`, `validate`, `inspect`, `nodes`, `serve` (HTTP + SSE),
  `kb ingest|query`, `plugin list|install|remove|info|new`.
- Distroless container image, release binaries, `install.sh`.
- `graphrt-ffi`: UniFFI bindings for Kotlin (Android) and Swift (iOS), with
  host credential providers and `load_bundle`.
- `graphrt-bundle`: portable `.grt` bundles (workflow, WebAssembly plugins,
  configs) with per-file SHA-256 and optional Ed25519 signatures;
  `graphrt bundle build|inspect|verify|sign|keygen`, `graphrt run app.grt`.
  Building refuses a SQLite store whose write-ahead log still holds commits.

**Operations**
- `graphrt-otel`: OpenTelemetry traces and metrics over OTLP/HTTP (`--otel`,
  standard `OTEL_*` variables, W3C `traceparent` continuation).
- Durable runs for `graphrt serve` (`--store`): resume by run id, async runs,
  event replay, restart-safe paused runs.
- Reproducible benchmarks in `bench/` (docs/benchmarks.md).

### Security
- Three rounds of security review across the engine, nodes, providers,
  credentials, plugins, WebAssembly runtime, document extraction and retrieval,
  with regression tests for every finding.

[Unreleased]: https://github.com/BimaPangestu28/graphrt/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/BimaPangestu28/graphrt/releases/tag/v0.1.0
