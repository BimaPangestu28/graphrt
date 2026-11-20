# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses
[Semantic Versioning](https://semver.org/spec/v2.0.0.html). Until 1.0, minor
releases may contain breaking changes; they are called out under **Changed**.

## [Unreleased]

### Added
- `graphrt serve --input-header INPUT=HEADER` maps a request header (leading
  `Bearer ` stripped) to a secret start input of new, streamed and resumed
  runs, so an app can send a user's JWT next to the serve API key. The value is
  masked everywhere and never stored; resumes must send the header again. A
  missing header for a required input is `401`, the same input in the body
  `400`, and a non-`secret` declaration fails at startup.

### Fixed
- Plaintext of secret-typed `conversation` variables is now scrubbed from node
  error messages, like other secrets.

## [0.3.2] - 2026-11-20

Telemetry fix and the project's collector endpoint in release builds.

### Fixed
- Telemetry: the daily report of a short command such as `graphrt run` was
  lost. The background thread claimed the day and removed its counts, then the
  process exited before the request finished, so nothing arrived and nothing
  was retried (only long-running `serve` delivered reports). A finishing
  process now waits for that once-a-day report, at most the 5-second send
  timeout. Telemetry is opt-in and no released binary had an endpoint, so no
  user data was affected.

### Changed
- Release binaries, the Homebrew formula and the container image report to
  the project's collector (`graphrt-telemetry.bimapangestu280.workers.dev`)
  once a user opts in with `graphrt telemetry enable`; still off by default.

## [0.3.1] - 2026-11-20

Loading large workflows is fast again. 0.3.0's strict validation built every
node and checked every node config twice, and re-parsed the document for source
positions even when it was clean: `graphrt run` / `validate` on a 1,000-node
workflow took ~54 ms instead of 0.2.0's ~14 ms. No behaviour changes; errors
are reported exactly as in 0.3.0.

### Performance
- Validation compiles a clean document from the nodes it already built and
  checked, instead of building and checking them again in `Graph::build`.
- Source positions are computed only when there is something to report.
- `Graph::fingerprint` is computed on first use and cached: only snapshots, run
  records and traces need it. Values are unchanged.
- `graphrt run` on a 1,000-node chain (macOS arm64, `min-size` build): 0.3.0
  55 ms, 0.3.1 16 ms, 0.2.0 15 ms.

### Changed
- Every crate now declares Rust **1.89** as its MSRV (was 1.85 for most). The
  declaration was wrong, not the code: `graphrt-core`'s default `yaml` feature
  has needed 1.89 since 0.2.0 through `serde-saphyr` 2, and the CLI, FFI and
  RAG crates already declared it. The MSRV CI job (and `scripts/ci-local.sh`)
  now checks the whole workspace on 1.89.

### Added
- The iOS sample app is verified on a physical iPhone (iOS 18.3); its README
  has the device build, install and launch steps.
- Plugin SDKs 0.2.0 on PyPI and npm (`graphrt-plugin`): tool handlers can
  return images (`tool_images`), as shipped in graphrt 0.3.0.

## [0.3.0] - 2026-11-19

Workflow format v1 frozen with a JSON Schema and located, collected validation
errors; webhook triggers; opt-in anonymous telemetry; plugins installable by
name from the plugin catalog; a security review with fixes (most also shipped
as 0.2.1); Android and iOS sample apps; a comparison against LangGraph; and
another round of engine, LLM-path and server speedups. Contains breaking
changes to the workflow format and the Rust API (see **Changed (Breaking)**).

### Added
- `graphrt serve --webhooks <file>`: webhook triggers. `POST /v1/hooks/{hook}`
  starts an async run of the hook's workflow from any JSON body and answers
  `202` with the run id. Bodies map to inputs by JSON Pointer and header
  (`inputs`, `headers`) or pass whole (`body_input`, default `body`).
  Optional HMAC-SHA256 signature checks in the GitHub, Stripe, Slack, generic
  or graphrt formats, compared in constant time, with a replay window for the
  timestamped ones; secrets from the environment, `--env-file` or a file.
  An idempotency key header (default `Idempotency-Key`) maps redeliveries to
  the first run, durably with `--store`. Deliveries share `--max-body-bytes`.
  `GET /v1/hooks` lists the hooks. See docs/serving.md.
- Webhook completion callbacks (`callback`): a signed
  (`X-Graphrt-Signature: t=..,v1=..`) JSON result POSTed when the run
  succeeds, fails, aborts or pauses, through the HTTP egress policy (private
  networks refused unless `--allow-private-network`, no redirects), retried
  with exponential backoff on network errors, `408`, `429` and `5xx`.
- Opt-in anonymous usage telemetry for the `graphrt` binary (`telemetry`
  feature, in the default set). Off by default; on only after
  `graphrt telemetry enable` or `GRAPHRT_TELEMETRY=1`, and always off with
  `DO_NOT_TRACK`, in CI, or without an endpoint (`GRAPHRT_TELEMETRY_URL` at
  build or run time). At most one report a day, sent from a background
  thread with a 5 s timeout: a random install id, version, OS/arch,
  features, build (`cli`/`docker`) and per-day counts of runs and `serve`
  requests, aggregated in `telemetry.json` in the user config directory.
  `graphrt telemetry status` prints exactly what would be sent. Not in the
  embedded or FFI libraries. See docs/telemetry.md.
- `tools/telemetry-collector/`: a Cloudflare Worker + D1 collector for those
  reports (strict payload validation, no IP addresses stored, schema, wrangler
  config template, weekly-active-installs and daily-runs queries). Not
  deployed.
- Workflow format v1 is frozen, with a JSON Schema (draft 2020-12):
  `schema/workflow-v1.schema.json`, one `oneOf` branch per node kind keyed on
  `type`, built from the node catalog. `graphrt schema` prints the schema for
  the node kinds in the running build, including plugins; the examples carry a
  `# yaml-language-server: $schema=...` header. Editor setup (VS Code YAML,
  JetBrains) and the stability promise are in `docs/workflow-format.md`.
- Validation reports every problem at once: `graphrt validate` and `run` show
  rustc-style errors with `file:line:column`, a source snippet, the document
  path (`graph.nodes[1].template`) and "did you mean" suggestions for field
  names, node types, node ids in edges, parents and selectors, and edge
  handles. `graphrt validate --format json` prints a machine-readable report.
  Library: `graphrt_core::validate::validate_text`, `Diagnostic`,
  `source_map::SourceMap`, `doc_schema::workflow_schema`,
  `Registry::descriptor_ref`, `Node::branch_handles`, `ConfigError::field`.
- `x-` keys (document, `graph`, nodes, edges) are free for tools and ignored.
- Tools can return images that the `agent` node shows to the model, enabling
  vision loops such as computer use (screenshot, then click by coordinates).
  `ToolOutput` gains `images` (`ToolImage::Base64 {media_type, data_base64}` or
  `ToolImage::Url`), checked against `check_images` (4 per result, 5 MiB each,
  png/jpeg/webp/gif, https URLs). Anthropic, Gemini 3+ and Bedrock Claude/Nova
  receive them inside the tool result; OpenAI-compatible servers and other
  models get them in a user message right after the tool results. New agent
  option `keep_tool_images` (default 3, `null` for all) keeps only the latest N image-bearing results.
  Plugin protocol: `tool.call` results accept `images`, and `initialize`
  announces `features: ["tool_images"]`; the Python, TypeScript and Rust SDKs
  send images only to hosts that announce it. MCP image blocks are forwarded
  too. See `docs/tool-images.md`.
- `graphrt plugin install <name>[@version]` installs a plugin from the
  graphrt-plugins catalog (`index.json` of its `catalog` release, or
  `--index <url|path>` / `$GRAPHRT_PLUGIN_INDEX`): the latest version or the
  exact one, downloaded over https only with a timeout and a size cap, checked
  against the catalog's sha256, extracted with absolute paths, `..`, symlinks,
  hard links and device files refused, then installed like a local directory
  (manifest validation and checksums included). The plugin's `status` and
  `tos_risk` are printed; anything but status `stable`/`beta` with ToS risk
  `none`/`low`/`medium` needs `--yes`, including unlabelled entries and
  unrecognised labels. A directory with the same name in the current directory still wins.
- `graphrt plugin search [query]`: catalog plugins with their latest version,
  status and a one-line description.
- `docs/plugin-manifest.md`: a versioned specification of the plugin manifest
  (every field with its type, default and constraints, the validation rules
  and a compatibility policy for `protocol_version`), and a JSON Schema for
  manifests, `docs/schemas/plugin-manifest.schema.json`, tested against the
  example, fixture and scaffolded manifests and against the parser itself.

### Changed (Breaking)
- **Unknown fields are errors in v1 documents**: at the document, `graph`,
  node, edge, `retry`, `on_error` and variable-declaration level, and in node
  configs whose catalog schema is closed (`additionalProperties: false`; all
  built-in kinds, as `catalog::schema::object` now produces). Plugin configs
  with open schemas are unaffected; `schema::open_object` builds one.
  Required config fields from the catalog schema are enforced (for example a
  `template-transform` without `template`). The import layout (top-level
  `workflow`/`app`, `data`-wrapped nodes) stays lenient.
  *Migration:* run `graphrt validate` and fix what it reports; move tool
  metadata to `x-` keys.
- **`version`** must be `"1"` (or `1`) or absent. *Migration:* set
  `version: "1"`.
- **Edge handles are checked**: an edge must use a handle the source node can
  select (`source` for non-branch nodes; case ids and `false` for `if-else`;
  class ids; action ids; a plugin's fixed handles). Non-branch nodes used to
  follow edges with any handle. `fail-branch` edges on nodes without that
  strategy are a warning (never taken). *Migration:* use `source` or the
  node's handle.
- **Reserved names**: top-level `imports`, `inputs`, `outputs`, `triggers`,
  `metadata`, `requires`, `subgraphs`; node-level `disabled`, `cache`,
  `metadata` (plugin configs cannot use them either).
- **`human-input`**: the request expiry is `expire_after_ms` (pause data still
  reports `timeout_ms`). A config `timeout_ms` was always taken as the node's
  per-attempt timeout and never reached the node. *Migration:* rename it.
- **Loop variables** are `{name, type, input_type, value}`; `label`,
  `var_type` and `value_type` (which meant the input source, unlike every
  other `value_type`) still work as deprecated aliases.
- Deprecated, still accepted with a warning: `environment_variables`,
  `conversation_variables`, declaration `value_type`, node `parentId` /
  `container_id` / `iteration_id` / `loop_id`, `error_strategy` /
  `default_value`, `retry_config`, edge `sourceHandle` / `source_handle`.
- API: `NodeDoc` has a `strict` field; `ConfigError` has a `field` field;
  `DocumentError::Field`; `GraphError::{MissingNode, MissingParent,
  UnknownReference}` carry a `suggestion`, new `GraphError::{InvalidField,
  UnknownHandle}`. `graphrt validate` takes `--format`; `CliError` has
  `preformatted`.
- **Breaking** (`graphrt-core`): `Event::NodeChunk::field` is an `Arc<str>`
  instead of a `String`, so a streamed chunk no longer allocates its field
  name. Serialized events are unchanged. New `NodeContext::chunk_shared(&field,
  text)` streams under a name the node keeps in an `Arc<str>`;
  `NodeContext::chunk(&str, text)` keeps its signature (and allocates the name
  per chunk). Migrating: compare with `field.as_ref() == "text"`, use
  `field.to_string()` where a `String` is needed, and `.into()` when building
  the event.
- **Breaking** (`graphrt-core`): `ResponseChunk::node_id` is an `Arc<str>`
  instead of a `String` (shared with the graph instead of copied per chunk).
  Migrating as for `NodeChunk::field`. New `ResponseAssembler::push_into(&event,
  &mut out)` appends to a caller-owned buffer; `push` is unchanged.
- **Breaking** (`graphrt-llm`): `collect`'s callback receives each `ChatDelta`
  by value (`FnMut(ChatDelta)`, was `FnMut(&ChatDelta)`), after the delta was
  added to the response, so streamed text can be moved into a chunk instead of
  copied. Closures that ignore the delta (`|_| {}`) compile unchanged; others
  match on the owned delta instead of a reference.
- **Breaking** (`graphrt-llm`): `nodes::common::PromptTemplate::messages`
  returns `&[PromptMessage]` instead of `Vec<&PromptMessage>`. New helpers
  `nodes::common::finish_reason_value` and `StreamField::shared`.
- Node outputs and pool namespaces use the new `graphrt_core::Variables` map
  (keys are `graphrt_core::VarName`) instead of `IndexMap<String, Value>` /
  `IndexMap<Arc<str>, Value>`. It is a reference-counted, insertion-ordered
  flat map: cloning is a reference-count bump, so a node's outputs become its
  pool namespace and its event payload without being copied. Affected:
  - `NodeOutput::outputs` is `Variables`; `NodeOutput::with` takes
    `impl Into<VarName>` (string literals, `String`, `&String`, `Arc<str>`).
  - `Event::NodeSucceeded::outputs` and `Event::NodeFailed::outputs` are
    `Variables` (same JSON).
  - `VariablePool::namespace()` returns `Option<&Variables>` and
    `VariablePool::set_outputs` takes `&Variables`.

  Migration: most code compiles unchanged, since `Variables` has the familiar
  `get`, `insert`, `remove`, `iter`, `keys`, `values`, `len`, `contains_key`,
  `extend` and `collect()` API. Iteration yields `(&str, &Value)` instead of
  `(&String, &Value)`; use `.to_owned()` where a `String` is needed. Pass a
  non-`'static` `&str` name as `name.to_owned()`. `outputs.insert("x".into(), v)`
  becomes `outputs.insert("x", v)`. Convert with `Variables::from(index_map)`,
  `variables.to_index_map()` or `IndexMap::from(variables)`. Indexing a missing
  name (`outputs["x"]`) yields `Value::Null` instead of panicking. `remove`
  keeps the order of the remaining entries (like `IndexMap::shift_remove`).

### Fixed
- `http-request` `timeout: {connect, read, write}` in a document was silently
  dropped (the loader treated `timeout` as a node policy key), so documents'
  HTTP timeouts never applied.
- The generated Kotlin bindings (`bindings/kotlin`) did not compile: every
  `GraphrtException` variant declared a `message` field that clashes with
  `Throwable.message`. Kotlin now names the field `detail`
  (`crates/graphrt-ffi/uniffi.toml`); `Throwable.message` still carries the
  text, and Swift and Python are unchanged. The `bindgen-cli` feature now
  enables UniFFI's `cargo-metadata` so the generator reads that file.

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
  `RangeError`. Also fixed in 0.2.1.
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
  `graphrt_core::redact`. Also fixed in 0.2.1.
- LLM providers: error bodies of failed provider requests are scrubbed of the
  credentials the request carried (`Authorization`, `x-api-key`,
  `x-goog-api-key`, `api-key` headers, `key` query parameters) before they
  become error messages. OpenAI-compatible gateways and self-hosted servers
  often echo the full key. Also fixed in 0.2.1.
- `code` node: returned strings containing a secret input (`key`, or
  `"Bearer " + key`) are re-tagged as secrets, so they are masked in
  `node_succeeded` events, SSE, run records and snapshots while staying usable
  downstream; captured Python stdout is scrubbed. Previously they were stored
  in plaintext. Also fixed in 0.2.1.
- HTTP egress policy: also blocks the IPv4-translated form `::ffff:0:a.b.c.d`
  (judged by the embedded IPv4 address), the local-use NAT64 prefix
  `64:ff9b:1::/48`, Teredo `2001::/32` and the Azure WireServer address
  `168.63.129.16`. Also fixed in 0.2.1.
- OTel HTTP exporter: redirects are no longer followed, so vendor key headers
  cannot follow a redirect to another host.
- Bundles: `graphrt run/serve --bundle` extracted configs to a fixed,
  predictable directory in the shared temp dir and reused it if it existed, so
  another local user could pre-create it and plant symlinks or swap
  `providers.json` (e.g. pointing `base_url` at their server). Each invocation
  now extracts into a new owner-only (`0700`) directory. Signatures are checked
  with strict Ed25519 verification. Also fixed in 0.2.1.
- Plugins: `limits.max_message_bytes` is capped at 256 MiB; WebAssembly
  plugins get at most 4 tables of 100,000 elements (was 8 x 1,000,000), which
  lived outside the `memory_bytes` budget.
- `graphrt serve`: new `--max-active-runs` (default 256, `0` = unlimited)
  caps runs executing at once; further starts and resumes get `429`.
  Previously `"async": true` requests could start unlimited parallel runs
  (`--max-runs` only bounds kept records). Also fixed in 0.2.1.
- `graphrt serve` without an API key on a loopback address now accepts only
  loopback `Host` names (`localhost`, `127.0.0.1`, `[::1]`), blocking DNS
  rebinding from web pages; a non-loopback address without a key prints an
  explicit warning. Run store I/O errors no longer return server file paths to
  clients, and `top_k` on `/v1/knowledge/retrieve` is capped at 100. Also in
  0.2.0.
- `Debug` output of `OtelConfig` (OTLP headers) and the MCP `StdioConfig`
  (`env`) / `HttpConfig` (headers, bearer token) shows names only, never values.
- Docs: new [docs/security.md](docs/security.md) (sandbox limits, egress
  policy, masking guarantees and their limits, deployment knobs); SECURITY.md
  now has a threat model, explicit scope and supported versions (0.2.x, 0.3.x).
- Dependencies: `rusqlite` 0.40 -> 0.41 (bundled SQLite 3.54), fixing
  RUSTSEC-2026-0459 / CVE-2026-119375 (uninitialized page-cache memory read
  through a crafted database and `-wal` pair) in `graphrt-rag`'s `SqliteStore`.
  `graphrt-rag` now needs Rust 1.89. `paste` (unmaintained, build-time only, via
  candle) is acknowledged in `deny.toml`. Also fixed in 0.2.1.

### Performance
- Engine, second round (shared outputs map, batched commits, one clock read
  per node, faster ids, direct template variables). Benchmark harness, Apple
  M3 Pro, against the first round below (p50, median of 21 alternating runs on
  a shared machine): 1,000-node chain 0.63 -> 0.40 µs per node, 1,000-way
  fan-out 0.99 -> 0.52 µs, 100-way fan-out 0.79 -> 0.46 µs, template chain
  1.35 -> 1.00 µs, branches 0.60 -> 0.44 µs, parallel iteration 1.25 -> 1.03
  µs, loop x iteration 1.20 -> 1.00 µs; allocations per node 9.1 -> 4.1 (noop
  chain), 10.9 -> 5.4 (fan-out), 16.7 -> 11.0 (template chain). Concurrent
  instant LLM runs (median of 15): 239k -> 282k runs/s at 1,000 in flight,
  206k -> 230k at 100; runs bound by a 50 ms model are unchanged.
- Engine: a node's outputs become its pool namespace and its `NodeSucceeded`
  payload without being copied (see **Changed (Breaking)**), string-literal
  output names no longer allocate, and namespace lookups scan a few entries
  instead of hashing with SipHash.
- Engine: nodes that have already finished are committed together before new
  nodes start. In a wide fan-out every commit used to copy the namespace table
  and a shard because a freshly started sibling held a snapshot of it; a batch
  of completions now shares one copy.
- Engine: the clock is read about once per node instead of twice. The time a
  finished node already read is reused for the next node's start and for any
  nodes skipped in the same step, and a batch of nodes started together shares
  one reading.
- Engine: execution ids are written as characters into a reused per-thread
  buffer, which drops the UTF-8 validation pass each id needed (same format).
- `template-transform`: string, number, boolean and null variables are handed
  to the template directly (a string shares its allocation) instead of
  through serde; other values still go through serde, which masks secrets.
- Engine: fewer allocations and less work per node execution. Node task
  futures are stored unboxed and are less than half their former size (584 ->
  264 bytes); a node that runs alone (chains, fan-ins) is polled without a
  `FuturesUnordered` task; the clock is read once per node start instead of
  twice; pool namespace lookups use a keyed FxHash-style hasher instead of
  SipHash; output names are shared across commits through a per-node cache
  (since replaced by `Variables`, whose literal names never allocate);
  the root pool's shards are sized from the graph. `template-transform`
  resolves its variables when the template reads them instead of building and
  converting a map per render. Benchmark harness, Apple M3 Pro, same machine
  before and after (p50, median of 11 runs): 1,000-node chain 0.74 -> 0.56 µs
  per node, 1,000-way fan-out 0.93 -> 0.86 µs, template node 1.38 -> 1.06 µs;
  allocations per node 12.2 -> 9.1 (noop chain), 13.1 -> 10.9 (fan-out),
  23.8 -> 16.7 (template chain). Concurrent instant LLM runs are unchanged
  (~285k runs/s at 11 workers): their cost is outside the per-node path.
- `bench/run.sh` records allocations per run and per node (`graphrt-bench
  allocs`).
- LLM path: fewer allocations and copies per model call and per streamed
  chunk. Chunks share their field name and move the provider's text instead
  of copying it; the `llm` node renders its prompt into one pre-sized message
  list, builds `usage` directly, moves the answer out of the response and
  builds its metadata once per node. Instant mock model, 10 chunks, Apple M3
  Pro (11 cores), against 7444368: 133 -> 103 allocations per run;
  -13% instructions per run (sequential and with 1,000 runs in flight);
  sequential latency 6.5 -> 5.8 µs. Throughput of runs whose events are
  discarded (`run_to_end`) is unchanged at 11 workers (~265k runs/s at 100 in
  flight, ~290k at 1,000): with the macOS system allocator it is bound by the
  allocator and by the efficiency cores, not by this work.
- `ResponseAssembler`: no allocation per chunk on the streaming path (stream
  buffers looked up by `&str`, template parts borrowed, only referenced
  outputs copied), and `push_into` lets consumers reuse one chunk buffer.
  Instant LLM run consumed through the assembler: 253 -> 161 allocations, -25%
  instructions, time to first answer chunk 11.1 -> 9.2 µs, concurrent runs
  +24% (161k -> 200k runs/s at 100 in flight, 171k -> 214k at 1,000).
- `graphrt serve`: live events are shared as `Arc<Event>` between the run
  feed, the broadcast channel and SSE listeners instead of deep-copied, and a
  streaming run's 4,096-slot channel holds 8-byte slots instead of ~256-byte
  events (it was over 1 MiB per streaming request). LLM workflow against a
  local instant OpenAI-compatible endpoint, 50 connections, macOS: synchronous
  32.7k -> 35.8k req/s, streaming 16.3k -> 21.8k req/s, server RSS after 40k
  requests 120 -> 39 MiB. Static musl build in a 16 MiB, 1-CPU container
  with 8 streaming clients and the default `--max-runs`: the server was
  OOM-killed before (15.1 MiB peak even with `--max-runs 100`); it now peaks at
  13.8 MiB (7.0 MiB with `--max-runs 100`), streaming 2.2k -> 3.6k req/s.

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

### Changed
- `graphrt_core::tool::ToolOutput` has a new `images` field and derives
  `Default`: struct literals must add `images: Vec::new()` or
  `..ToolOutput::default()` (`ToolOutput::text` / `ToolOutput::error` are
  unchanged). `graphrt_plugin_sdk::ToolResult` likewise gains `images`.
- Plugin SDKs (Python, TypeScript) answer `message_too_large` instead of
  writing a reply line above the host's `max_message_bytes`, which used to
  break the connection.

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

[Unreleased]: https://github.com/BimaPangestu28/graphrt/compare/v0.3.2...HEAD
[0.3.2]: https://github.com/BimaPangestu28/graphrt/compare/v0.3.1...v0.3.2
[0.3.1]: https://github.com/BimaPangestu28/graphrt/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/BimaPangestu28/graphrt/compare/v0.2.1...v0.3.0
[0.2.1]: https://github.com/BimaPangestu28/graphrt/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/BimaPangestu28/graphrt/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/BimaPangestu28/graphrt/releases/tag/v0.1.0
