# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses
[Semantic Versioning](https://semver.org/spec/v2.0.0.html). Until 1.0, minor
releases may contain breaking changes; they are called out under **Changed**.

## [Unreleased]

## [0.2.1] - 2026-11-19

Security patch release for 0.2.0: sandbox limits that could be switched off,
secret plaintext in error messages and code outputs, more SSRF address forms,
unbounded concurrent runs and DNS rebinding in `graphrt serve`, predictable
temp directories, and a SQLite CVE. No breaking API changes; upgrading is
recommended for every 0.2.0 user.

Upgrade notes:
- **MSRV:** `graphrt-rag` now needs **Rust 1.89** (was 1.85), because the
  patched SQLite is only in `rusqlite` 0.41, which declares Rust 1.89. No
  `rusqlite` 0.40.x or `libsqlite3-sys` 0.38.x release carries the fix (the
  advisory lists `libsqlite3-sys >= 0.39.0` as patched, and that needs Rust
  1.88). `graphrt-ffi` enables `graphrt-rag` (with `sqlite`) through its
  default `rag` feature, so a default `graphrt-ffi` build needs Rust 1.89 too.
  `graphrt-cli` already needed 1.89; the other crates keep their MSRV.
- `graphrt serve` refuses new and resumed runs with `429 Too Many Requests`
  while 256 runs execute (`--max-active-runs`, `0` = unlimited).
- `graphrt serve` without an API key on a loopback address answers `403` to
  requests whose `Host` is not a loopback name. If you put a reverse proxy in
  front of such a server, set an API key or have the proxy forward a
  loopback `Host`.
- Plugin manifests asking for `limits.max_message_bytes` above 256 MiB are
  rejected.

### Security
- `code` node (JavaScript): a workflow could switch off the sandbox's memory
  and stack limits by asking for `limits.memory_bytes: 0` or
  `limits.stack_bytes: 0` (QuickJS reads zero as "unlimited"), and unbounded
  recursion then overflowed the native stack and aborted the whole process
  (for example `graphrt serve`). An operator stack ceiling above 16 MiB had the
  same effect, and one above the executor's thread stack (2 MiB on tokio's
  blocking pool) crashed the process too. Limits now have floors (1 MiB heap,
  64 KiB stack) and a 16 MiB stack ceiling, and JavaScript runs on a dedicated
  thread sized from the stack limit, so deep recursion is always a catchable
  `RangeError`. The `graphrt-code` `tokio` feature no longer changes behaviour
  and is kept for compatibility.
- `code` node (JavaScript): error messages thrown by scripts are truncated to
  8 KiB, so a script cannot push megabytes into events and run records; an
  operator time limit too large for the clock no longer panics; a long chain of
  promise jobs is bounded by the time limit between jobs as well.
- Python subprocess runner: the working directory is created fresh and
  owner-only (`0700`) instead of adopting an existing directory at the
  predictable temp path.
- Engine: node error messages are scrubbed of every secret the run has seen
  (`type: secret` env variables, secret-typed inputs, re-supplied resume
  secrets, secret outputs) before they reach `node_failed` / `node_retry` /
  `run_failed` events, `error_message` outputs of handled errors, the run store
  and OTel spans. Previously an error that echoed a key (a provider body such
  as "Incorrect API key provided: sk-...", an HTTP error snippet from an echo
  endpoint, an exception in code) was recorded verbatim. New public helpers in
  `graphrt_core::redact` (`redact_secrets`, `SecretSet`, `MIN_SCRUB_LEN`).
- LLM providers: error bodies of failed provider requests are scrubbed of the
  credentials the request carried (`Authorization`, `x-api-key`,
  `x-goog-api-key`, `api-key` headers, `key` query parameters) before they
  become error messages. OpenAI-compatible gateways and self-hosted servers
  often echo the full key.
- `code` node: returned strings containing a secret input (`key`, or
  `"Bearer " + key`) are re-tagged as secrets, so they are masked in
  `node_succeeded` events, SSE, run records and snapshots while staying usable
  downstream; captured Python stdout is scrubbed. Previously they were stored
  in plaintext.
- HTTP egress policy: also blocks the IPv4-translated form `::ffff:0:a.b.c.d`
  (judged by the embedded IPv4 address), the local-use NAT64 prefix
  `64:ff9b:1::/48`, Teredo `2001::/32` and the Azure WireServer address
  `168.63.129.16`.
- OTel HTTP exporter: redirects are no longer followed, so vendor key headers
  cannot follow a redirect to another host.
- Bundles: `graphrt run/serve --bundle` extracted configs to a fixed,
  predictable directory in the shared temp dir and reused it if it existed, so
  another local user could pre-create it and plant symlinks or swap
  `providers.json` (e.g. pointing `base_url` at their server). Each invocation
  now extracts into a new owner-only (`0700`) directory. Signatures are checked
  with strict Ed25519 verification.
- Plugins: `limits.max_message_bytes` is capped at 256 MiB; WebAssembly
  plugins get at most 4 tables of 100,000 elements (was 8 x 1,000,000), which
  lived outside the `memory_bytes` budget.
- `graphrt serve`: new `--max-active-runs` (default 256, `0` = unlimited)
  caps runs executing at once; further starts and resumes get `429`.
  Previously `"async": true` requests could start unlimited parallel runs
  (`--max-runs` only bounds kept records). `AppState::with_max_active_runs`
  sets it for embedders (default unlimited there).
- `graphrt serve` without an API key on a loopback address now accepts only
  loopback `Host` names (`localhost`, `127.0.0.1`, `[::1]`), blocking DNS
  rebinding from web pages; a non-loopback address without a key prints an
  explicit warning. Run store I/O errors no longer return server file paths to
  clients, and `top_k` on `/v1/knowledge/retrieve` is capped at 100.
- `Debug` output of `OtelConfig` (OTLP headers) and the MCP `StdioConfig`
  (`env`) / `HttpConfig` (headers, bearer token) shows names only, never values.
- Dependencies: `rusqlite` 0.40 -> 0.41 (bundled SQLite 3.54.1), fixing
  RUSTSEC-2026-0459 / CVE-2026-119375 (uninitialized page-cache memory read
  through a crafted database and `-wal` pair) in `graphrt-rag`'s `SqliteStore`.
  This raises `graphrt-rag`'s MSRV to Rust 1.89 (see the upgrade notes).
  `paste` (unmaintained, build-time only, via candle) is acknowledged in
  `deny.toml`.
- Docs: new [docs/security.md](docs/security.md) (sandbox limits, egress
  policy, masking guarantees and their limits, deployment knobs); SECURITY.md
  now has a threat model, explicit scope and supported versions.

## [0.2.0] - 2026-11-19

Knowledge bases you can manage from an app, in-process embeddings, small-device
serving, and a much faster engine and server. Contains breaking API changes
(see **Changed**).

### Fixed
- `graphrt serve` without `--store` kept every run (record and events) in memory
  forever: under a 32 MiB limit the server was OOM-killed after ~1,800 runs. The
  in-memory store now keeps at most `--max-runs` runs (default 1000), dropping
  the oldest finished runs first and always keeping running and paused ones.

### Added
- `graphrt serve --max-runs <n>`; see docs/serving.md for small-device numbers.
- Knowledge base API in `graphrt serve` (with `--knowledge`): list datasets and
  documents (paged), add or replace documents from text or uploaded files
  (`--max-upload-bytes`, default 16 MiB), inspect a document's chunks, delete
  documents, and test retrieval, all on the knowledge base the workflows use.
  Ingestion runs one request at a time to keep small devices responsive.
- Documents can be disabled and re-enabled (`PATCH .../documents/{document}`,
  `KnowledgeBase::set_document_enabled`): a disabled document keeps its chunks
  and stays disabled across re-ingestion, but no search returns it (memory,
  SQLite and Qdrant stores; reserved chunk metadata key `_disabled`).
- Parent-child chunking per dataset (`chunking: { max_chars, parent_max_chars }`
  in the knowledge config, `KnowledgeBase::set_chunking`): small children are
  embedded and searched, and retrieval returns each matching parent once,
  rebuilt from its children (nothing extra stored). `ChunkerConfig` gains
  `parent_max_chars` and `TextChunk` gains `parent` (**breaking** for struct
  literals); children carry `parent_index` metadata.
- Knowledge base datasets can be created and deleted at runtime:
  `GET /v1/knowledge/resources`, `POST /v1/knowledge/datasets` and
  `DELETE /v1/knowledge/datasets/{dataset}`, backed by
  `KnowledgeBase::{create_dataset, remove_dataset, dataset_details,
  resource_names, runtime_datasets}`. New datasets name a store, embedder and
  reranker declared in the knowledge config; they are saved next to it
  (`kb.datasets.json`) and recreated on start by `serve`, `run` and `kb`.
- `graphrt-rag`: `VectorStore::list_documents` (memory and SQLite stores),
  `DocumentSummary`, `KnowledgeBase::{list_documents, document_chunks,
  delete_document, is_managed}`, `RagError::Unsupported`; `IngestReport` is
  serializable.
- `graphrt-rag`: `LocalEmbedder` (feature `local-embed`), sentence embeddings
  computed in-process with candle on the CPU: no embedding server, API key or
  network once the model is cached. BERT-family models from Hugging Face
  (`sentence-transformers/all-MiniLM-L6-v2` by default; `intfloat/multilingual-e5-small`
  for Indonesian and other languages) or a local model directory. Configured as
  `embedders: { local: { type: local, model: ... } }`. On by default in
  `graphrt-cli` (feature `local-embed`, about +2.1 MiB).
- `graphrt-ffi`: the SQLite knowledge store is available on mobile, so bundles
  using `stores: { ...: { type: sqlite } }` load on Android and iOS
  (built and linked for Android arm64; iOS not built yet).
- `graphrt plugin install <git-url> --subdir <path>` installs a plugin from a
  subdirectory of a git repository, so monorepos with one plugin per directory
  (such as `graphrt-plugins`) work. Absolute paths, `..` and symlinks are
  rejected; only that directory is copied. Library: `graphrt-plugin` adds
  `install_from_git_subdir` (`install_from_git` is unchanged).
- `graphrt-core`: `Registry::register_lazily_described`, which builds a node
  kind's catalog descriptor only when it is first requested.
- `graphrt-cli`: optional `mimalloc` feature to replace the global allocator.

### Changed
- **Breaking** (`graphrt-rag`): clones of a `KnowledgeBase` share their
  datasets (so datasets created through one clone are visible to all);
  `KnowledgeBase::datasets` returns `Vec<DatasetInfo>` and `chunking` returns
  an owned `ChunkerConfig`.
- **Breaking** (`graphrt-core`): the `node_id` and `exec_id` fields of every
  `Event` variant are `Arc<str>` instead of `String`, so the engine shares the
  ids it already holds instead of allocating two strings per event (every
  streamed chunk included). Serialized events are unchanged. Migrating:
  compare with `node_id.as_ref() == "llm"` instead of `node_id == "llm"`, use
  `node_id.to_string()` where a `String` is needed, and `.into()` when building
  an event from a `String`.
- **Breaking** (`graphrt-core`): `GraphNode::{kind, title}` and the `kind` and
  `title` of `Event::NodeStarted` are `Arc<str>` (shared, not copied per
  execution). Serialized events are unchanged.
- **Breaking** (`graphrt-core`): run stores keep events as serialized JSON:
  `RunStore::append_events` / `events` take and return `Vec<StoredEvent>`
  (`Box<serde_json::value::RawValue>`), built with `event_to_stored`;
  `stored_event_type` reads an event's type. The stored text is identical to
  what `event_to_json` produced, so `GET /v1/runs/{id}/events` is unchanged.
- **Breaking** (`graphrt-http`): `HttpClient::client` and
  `HttpClient::manual_redirect_client` return `Result<&reqwest::Client, ClientError>`;
  the four underlying clients are built on first use. `ClientError` is `Clone`.

### Performance
- Startup no longer loads the system trust store for HTTP clients a run never
  uses: LLM providers (`graphrt-llm`) and `graphrt-http` build their clients on
  the first request. `graphrt run hello.yaml` used to read every CA certificate
  six times (361 file opens, now 1).
- Built-in node kinds build their catalog descriptors lazily: node registry
  construction went from ~530 µs to ~7 µs.
- `template-transform` compiles its template once instead of on every run:
  ~20% less engine time per template node.
- Net effect, `graphrt run hello.yaml` (static musl binary, Linux arm64):
  10.1 ms → 6.5 ms wall clock, 3.1 ms → 0.6 ms user CPU.
- Fewer allocations per run and per node execution: a variable pool allocates
  its namespace shards on first write (an empty pool was 33 allocations, one per
  execution frame), namespace keys are reused instead of re-allocated on every
  write, node commits without variable writes skip the undo log, execution ids
  are written without `format!`, and `Arc<str>` ids are copied without going
  through `Display`. On the benchmark harness (Apple M-series, same machine
  before and after): 1,000 concurrent instant runs 63k → 82k runs/s (+31%),
  per-node scheduling 1.55 → 1.27 µs (noop chain) and 2.31 → 1.57 µs (1,000-way
  fan-out), time to first streamed chunk overhead 14.5 → 11.6 µs.
- Shared `Arc<str>` event ids (see **Changed**) took the same benchmark to
  94k runs/s (+49% over 0.1.0 in total), 1.06 µs per noop node and a 10.2 µs
  streaming overhead. The response assembler and the OpenTelemetry tracer key
  their maps by the shared ids too.
- Cheaper per-frame bookkeeping: index-keyed maps use a multiplicative hasher
  and are sized up front, and the scope's id set is built only when a node
  writes variables. Against 0.1.0 on the same machine: 0.87 µs per node on a
  1,000-node chain (was 1.55), 1.18 µs on a 1,000-way fan-out (2.31), 98k
  instant runs/s (63k); raw data in `bench/results/2026-11-1{8,9}-*`.
- `graphrt serve`: a run's 4,096-slot broadcast channel is created only when
  something subscribes to its events (it took ~30% of server CPU), and
  accepted connections set `TCP_NODELAY` (streamed responses no longer wait
  ~40 ms for a delayed ACK on Linux). Static musl, Linux arm64, 1 CPU:
  synchronous runs 2,875 -> 20,613 req/s, streaming 382 -> 3,392 req/s, async
  4,232 -> 20,499 req/s; macOS native synchronous 26k -> 46k req/s.
- Engine: fewer allocations per node execution (ids built on the stack,
  shared `Arc<str>` kind/title, outputs no longer copied into the pool), run
  events queued in a reusable buffer with an atomic waker instead of an mpsc
  channel, and stored events kept as JSON text (297 ns and 2.6 allocations per
  event, was 485 ns and 14.3). Per-node scheduling another 7-15% lower
  (0.73 µs on a 1,000-node chain).
- `bench/run.sh`: the harness's global allocation counter is enabled only in
  the sections that read it; it had capped the concurrency section at ~60-80k
  runs/s. With it gated the same machine measures ~300k runs/s for the llm
  graph and ~390k for a no-LLM graph at 11 workers (a measurement fix, not a
  runtime speed-up).

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

[Unreleased]: https://github.com/BimaPangestu28/graphrt/compare/v0.2.1...HEAD
[0.2.1]: https://github.com/BimaPangestu28/graphrt/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/BimaPangestu28/graphrt/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/BimaPangestu28/graphrt/releases/tag/v0.1.0
