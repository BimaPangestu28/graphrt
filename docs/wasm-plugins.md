# WebAssembly plugins

`graphrt-wasm` lets anyone add node kinds as a single portable `.wasm` file.
The same file runs on Linux, macOS, Windows, x86_64 and ARM, inside the CLI,
the server, or any host embedding graphrt — with no recompilation and no
native code execution.

```rust
use graphrt_core::Registry;
use graphrt_wasm::{WasmLimits, WasmPlugin};

let plugin = WasmPlugin::load_file("plugins/text_tools.wasm", WasmLimits::default())?;
let mut registry = Registry::with_builtins();
plugin.register(&mut registry)?;         // node kinds + catalog descriptors; fails on existing kinds
for d in plugin.descriptors() { println!("{} ({})", d.kind, d.title); }
```

A workflow then uses the plugin's kinds like any built-in node:

```yaml
- id: slug
  type: slugify
  text: "{{#start.title#}}"
  separator: "-"
```

## Using WebAssembly plugins

Two ways to ship a module to the `graphrt` CLI:

- **Bare module:** drop `my_plugin.wasm` directly into a plugin search path
  (`--plugins <dir>`, `$GRAPHRT_PLUGIN_PATH` or `~/.graphrt/plugins`). The
  module describes its own nodes.
- **Manifest:** a directory with `graphrt-plugin.json` using `"runtime": "wasm"`
  and `"module": "my_plugin.wasm"`. This adds versioned installs
  (`graphrt plugin install`), `checksums`, host variables and outbound HTTP
  through `permissions` (below), and `permissions.secrets` (granted
  only to plugins the operator trusts: `--trust-plugin <name>`). Node kinds
  must start with `<name>.`; with `allow_unprefixed_kinds` plain names without
  `.` are also accepted. The CLI ships the loader for `runtime: wasm`.
  See [`plugins/examples/wasm-text-tools`](../plugins/examples/wasm-text-tools).

A manifest's `permissions` decide what the module gets:

| Permission | WebAssembly plugin gets |
| --- | --- |
| `env: [NAME, ...]` | Those host variables (when set) in every request's `env`, like a process plugin's environment. |
| `network: true` | The `graphrt::http` import, served by graphrt's SSRF-safe egress (builds with the `http` feature). Without it, a module importing `http` is refused at load. |
| `secrets: true` + `--trust-plugin <name>` | Plaintext secrets in node configs and the agent's per-call `tool_context` (e.g. an end user's JWT) in tool calls. |

Either way, a module's kinds never replace built-in nodes or another plugin's
nodes (`WasmPlugin::register` fails with `KindConflict`), and a namespaced kind
(`a.b`) must use the module's own name as its namespace.

```sh
graphrt nodes --plugins plugins/examples --kind slugify
graphrt run plugins/examples/demo.yaml --plugins plugins/examples --input review="I love it"
```

## Engine choice

| Option | Verdict |
| --- | --- |
| **wasmi** (pure-Rust interpreter) | **Chosen.** ~1 MB added to a size-optimized binary, no JIT, no native code generation, deterministic fuel metering, store-level memory/table/instance limits, builds everywhere Rust builds (including inside a wasm32 host). |
| wasmtime | Fastest (JIT) and the only option with the component model, but adds roughly an order of magnitude more code (Cranelift), and its interpreter mode (Pulley) still carries the full runtime. Too heavy as a default for a runtime whose goal is a small binary. |
| Extism | Convenient multi-language PDKs, but the Rust host crate depends on wasmtime, and its ABI (`extism:host/env`: alloc/free/load/store/input/output/vars/config/http/log...) is far larger than what node plugins need. Implementing that ABI on wasmi ourselves would mean tracking an external, evolving surface. |

The ABI below is deliberately tiny (4 exports, 3 optional imports) so it can be
implemented from any language that compiles to `wasm32-unknown-unknown`
(Rust, C/C++, Zig, TinyGo, AssemblyScript) without a dedicated SDK. A Rust SDK
is provided in [`sdk/wasm/rust`](../sdk/wasm/rust/graphrt-wasm-guest).

Interpreted execution is slower than a JIT, which is the right trade-off for
plugin nodes: they do small amounts of deterministic work (parsing,
formatting, routing, validation) between I/O-bound model and HTTP calls.
Measured per-run overhead for the example plugin (fresh instance + JSON
request/response, release build): **~0.13 ms**.

## Cold start: the path for hot plugins

A WebAssembly plugin runs in the agent's process: there is no second process
to start, so a cold wake pays only the module's instantiation. Measured with
`bench/targets.sh` (`wake-wasm` and `wake-node`, Linux aarch64, static musl,
2026-11-24, 3 rounds of 100 wakes, median round):

| Cold wake of a new agent (request → plugin node's result) | p50 / p95 |
| --- | ---: |
| no plugin (`wake`, first event) | 5.6 / 6.7 ms |
| [`wasm-text-tools`](../plugins/examples/wasm-text-tools) (`slugify`, 152 KiB module), agent `serve,llm,wasm,trust-gateway` | 13.4 / 15.1 ms |
| [`node-uppercase`](../plugins/examples/node-uppercase) (Node.js 22 process plugin), agent `serve,llm,plugins,trust-gateway` | 56.4 / 59.7 ms |

The interpreter costs binary size instead. `wasm` on the slim agents
(min-size; optional profile of `bench/budgets.toml`, measured 2026-11-24):

| Build | x86_64 musl | aarch64 musl | macOS arm64 | idle RSS (aarch64) |
| --- | ---: | ---: | ---: | ---: |
| `serve,llm,plugins` (agent-slim) | 7.22 MiB | 6.03 MiB | 5.33 MiB | 4.80 MiB |
| + `wasm` | +1050.6 KiB | +848.9 KiB | +684.5 KiB | 5.27 MiB |
| + `trust-gateway,wasm` (over agent-slim-gateway) | +1050.3 KiB | +849.5 KiB | +684.5 KiB | 5.32 MiB |
| + `trust-gateway,wasm,http` (egress for `graphrt::http`) | +1269.1 KiB | +1022.6 KiB | +846.9 KiB | 5.28 MiB |

`wasm` stays off in the default agent builds; turn it on for agents whose hot
plugins have a WebAssembly build. Tools and `graphrt::http` themselves cost
+57.7 KiB (aarch64 musl `serve,llm,wasm,trust-gateway`: 6,937,800 →
6,996,928 B).

### Node.js vs WebAssembly: the supabase tool

The [supabase](https://github.com/BimaPangestu28/graphrt-plugins/tree/main/supabase)
plugin's per-user `call_function` tool (`SUPABASE_TOOL_MODE=user_rpc`) has a
WebAssembly build ([`wasm/supabase-rpc`](https://github.com/BimaPangestu28/graphrt-plugins/tree/main/wasm/supabase-rpc),
Rust, ≈214 KB module) with the same env, tool, errors and HTTP requests (the
same test cases run against both). Measured with
[`bench/wasm-plugin/supabase_tool.py`](../bench/wasm-plugin/supabase_tool.py):
`examples/supabase-chatbot/agent.yaml`, one tool call per run, mock model and
Supabase (sub-millisecond), agent `serve,llm,plugins,trust-gateway,wasm,http`,
Linux aarch64 musl in a 4-vCPU VM, Node.js 22:

| | Node.js plugin | WebAssembly build |
| --- | ---: | ---: |
| Plugin on disk | 370 KB (+ Node.js runtime, 61 MiB `libnode`) | 220 KB (`.wasm` 214 KB; runtime in graphrt, +1.0 MiB) |
| Cold run, `graphrt run` exec → exit, p50 / p95 (n=50) | 118.8 / 122.2 ms | 18.4 / 19.4 ms |
| First run on a fresh `serve` | 172.9 ms | 10.9 ms |
| Warm run on `serve`, p50 / p95 (n=300) | 1.80 / 2.75 ms | 1.79 / 2.06 ms |
| RSS (serve + plugin processes), idle → after the runs | 6.6 → 127.0 MiB | 6.8 → 9.2 MiB |

For a plugin on the hot path of a scale-to-zero deployment (every request a
possible cold wake), prefer a WebAssembly build: pure computation (parsing,
formatting, validation, routing) and HTTP API clients both fit now. Plugins
that need a native SDK, a JavaScript library, a browser or the filesystem stay
process plugins: keep them warm with [`serve --plugin-pool`](plugins.md#warm-process-pool),
`min_replicas` or a prewarm, or run them as a shared HTTP plugin.

## Security model

- **No ambient capabilities.** Guests can only import `graphrt::log`,
  `graphrt::chunk`, `graphrt::progress` and, when granted, `graphrt::http`.
  Modules importing anything else (WASI, `env`, ...) are rejected at load. No
  filesystem, clock or randomness, and no network unless granted.
- **Network (`graphrt::http`, `permissions.network`).** Every request goes
  through the same egress layer as the `http-request` node (graphrt-http):
  http/https only, public addresses only unless the operator passes
  `--allow-private-network`, DNS answers checked before connecting (no
  rebinding), proxy variables ignored, and **no redirects** (a 3xx goes back
  to the guest, so a credential header such as Supabase's `apikey` is never
  replayed to another host). The host also checks each request: methods GET,
  HEAD, POST, PUT, PATCH, DELETE; framing and hop-by-hop headers (`host`,
  `content-length`, `transfer-encoding`, `connection`, ...) belong to the host;
  at most `max_http_requests` per call; the guest's timeout is capped by
  `http_timeout_ms`; request and response bodies are capped
  (`http_request_bytes`, `http_response_bytes`) and must be UTF-8. Bare
  `.wasm` modules never get the network. Time spent waiting on the network
  does not consume fuel; the request count and timeout bound it instead.
- **Tool context.** A tool call carries the agent's `tool_context` only to a
  plugin with `permissions.secrets` that the operator trusts (otherwise the
  call is refused, as for process plugins), and the host replaces every
  context value found in the tool's result or error with `******`.
- **CPU:** fuel metering (≈1 unit per instruction). Exhausting fuel aborts the
  run with `wasm_fuel_exhausted` (fatal, not retried). Fuel also bounds the
  module's start function. Host imports are charged too: every `log`, `chunk`
  or `progress` call costs 1 000 fuel plus 1 per byte copied, so a guest cannot
  make the host do unbounded work inside its budget.
- **Events:** at most `max_events` `chunk` + `progress` calls and
  `max_stream_bytes` streamed bytes per call; going past either fails the run.
  Log lines past `max_logs` are counted, not copied.
- **Memory:** linear memory is capped per instance (`memory_bytes`, default
  64 MiB); growing past it traps with `wasm_memory_limit`. One memory, one
  instance, bounded tables.
- **I/O:** request, response, log, chunk and progress buffers are size-capped
  and bounds-checked against guest memory.
- **Stack:** wasm call depth is capped (`max_recursion_depth`).
- **Isolation:** every run instantiates a fresh copy of the module; no state
  survives between runs or leaks between concurrent runs.
- **Validation:** modules are fully validated and compiled once at load, with
  wasmi's strict enforced limits for untrusted input.
- **Secrets:** placeholders referencing secrets resolve to the mask unless the
  operator enables `WasmLimits::expose_secrets` for that plugin.
- **Untrusted output:** guest JSON never becomes `File` values.

Limits are set by the operator through `WasmLimits`; workflow documents cannot
raise them.

## ABI version 1

All buffers are UTF-8 JSON in the guest's exported `memory`. A packed return
value is `(ptr << 32) | len` as `i64`.

### Guest exports

| Export | Signature | Purpose |
| --- | --- | --- |
| `memory` | memory | Linear memory. |
| `graphrt_abi_version` | `() -> i32` | Must return `1`. |
| `graphrt_alloc` | `(len: i32) -> i32` | Return a buffer of `len` bytes the host writes the request into. |
| `graphrt_describe` | `() -> i64` | Packed pointer to the manifest JSON. |
| `graphrt_run` | `(ptr: i32, len: i32) -> i64` | Run one node; packed pointer to the response JSON. |
| `graphrt_tool_call` | `(ptr: i32, len: i32) -> i64` | Optional; required when the manifest lists `tools`. One tool call ([below](#tools)). |

Guests never need to free: the instance is discarded after each call.

### Host imports (module `graphrt`, all optional)

| Import | Signature | Purpose |
| --- | --- | --- |
| `log` | `(level: i32, ptr: i32, len: i32)` | Log line, level 0 (trace) .. 4 (error). Returned in the node's `metadata.logs`; when the run fails, the last lines are appended to the error message. |
| `chunk` | `(field_ptr, field_len, ptr, len: i32)` | Stream text for output `field` (emits `node_chunk` events immediately). |
| `progress` | `(name_ptr, name_len, ptr, len: i32)` | Structured progress (`node_progress` event); payload is JSON. |
| `http` | `(ptr, len: i32) -> i64` | Outbound HTTP; plugins with `permissions.network` only ([below](#outbound-http)). |

`tools`, `graphrt_tool_call`, `env` and `http` were added to version 1 in
graphrt 0.6: older hosts ignore `tools` and refuse a module importing `http`.

### Manifest (`graphrt_describe`)

```json
{
  "name": "text-tools",
  "version": "0.1.0",
  "description": "Small text utilities",
  "nodes": [
    {
      "kind": "slugify",
      "title": "Slugify",
      "category": "transform",
      "config_schema": {"type": "object", "required": ["text"], "properties": {"text": {"type": "string"}}},
      "outputs": [{"name": "slug", "type": "string"}],
      "handles": {"kind": "single"}
    }
  ]
}
```

Each entry is a full catalog `NodeDescriptor`, so plugin nodes appear in the
node palette exactly like built-in ones. `handles` of kind `fixed` or
`dynamic` makes the node a branching node; with `fixed` the host rejects any
`branch` not in the list. Container nodes are not supported.

### Request (`graphrt_run` input)

```json
{
  "kind": "slugify",
  "node_id": "slug",
  "config": {"text": "Hello World", "separator": "-"},
  "attempt": 1,
  "idempotency_key": "run-18c2f0a1d3e-4b1f0c2a-7:18c2f0a1d40-4b1f0c2a-9",
  "resume": {"state": {}, "input": {}}
}
```

`idempotency_key` (`<run_id>:<exec_id>`) is the same for every attempt of a
node execution and for its re-execution after a preempted run resumes; key
external side effects on it (see [plugins.md](plugins.md#noderun)). The guest
crate exposes it as `Request::idempotency_key`.

`config` is the node's config from the workflow document after host-side
resolution:

- strings containing `{{#ns.name#}}` placeholders are resolved; a string that
  is exactly one placeholder becomes the typed value (number, object, array...);
  missing variables resolve to `null` (single placeholder) or empty text;
- every key ending in `selector` whose value is a selector (`["ns", "name"]` or
  `"ns.name"`) keeps its value and gains `<key>_value` with the resolved value,
  e.g. `input_selector` → `input_selector_value`.

The host also uses these references for build-time validation: a plugin node
referencing an unknown namespace fails `Graph::build`.

`resume` is present only when the node previously paused. `env` holds the
host variables of the manifest's `permissions.env` (omitted when empty).

### Response

One of:

```json
{"outputs": {"slug": "hello-world"}, "branch": "short", "metadata": {}, "usage": {"prompt_tokens": 0}}
{"pause": {"kind": "approval", "data": {"question": "..."}, "state": {"step": 1}}}
{"error": {"kind": "invalid_input", "message": "`text` is required", "retryable": false}}
```

`error` wins over `pause`, which wins over `outputs`. A response with none of
them is rejected (`wasm_invalid_output`).

### Tools

A manifest may list tools for `agent` nodes next to (or instead of) node kinds:

```json
{"name": "supabase", "nodes": [], "tools": [{"name": "call_function", "description": "...", "parameters": {"type": "object"}}]}
```

They are registered under the plugin's name (`supabase/call_function` in an
agent's `tools`), like a process plugin's. Each call instantiates the module and
calls `graphrt_tool_call` with:

```json
{"name": "call_function", "arguments": {"function_name": "get_balance"}, "context": {"secrets": {"user_jwt": "eyJ..."}}, "env": {"SUPABASE_URL": "https://..."}}
```

and expects a process plugin's `tool.call` result, or `error` for a refused call:

```json
{"text": "...", "data": {}, "is_error": false, "images": []}
{"error": {"kind": "permission_denied", "message": "Function \"x\" is not allowed"}}
```

### Outbound HTTP

`graphrt::http` takes a request JSON and returns a packed pointer to the reply,
written into a buffer the host gets from `graphrt_alloc`:

```json
{"method": "POST", "url": "https://x.supabase.co/rest/v1/rpc/get_balance", "headers": {"apikey": "..."}, "body": "{}", "timeout_ms": 30000}
{"status": 200, "headers": {"content-type": "application/json"}, "body": "[...]"}
{"error": {"kind": "egress_denied", "message": "address 127.0.0.1 is not a public address ..."}}
```

Error kinds: `invalid_request`, `egress_denied`, `timeout`, `too_large`,
`http_error`. The call blocks the guest (which runs on Tokio's blocking pool),
not the host's async workers.

## Writing a plugin in Rust

```toml
[lib]
crate-type = ["cdylib"]

[dependencies]
graphrt-wasm-guest = { path = "sdk/wasm/rust/graphrt-wasm-guest" }

[profile.release]
opt-level = "z"
lto = true
panic = "abort"
strip = true
```

```rust
use graphrt_wasm_guest::{export_plugin, json, Manifest, NodeSpec, Output, Plugin, PluginError, Request};

struct Upper;

impl Plugin for Upper {
    fn manifest() -> Manifest {
        Manifest::new("upper", "0.1.0").node(NodeSpec::new("upper", "Uppercase").output("text", "string"))
    }
    fn run(req: Request) -> Result<Output, PluginError> {
        let text = req.config_str("text").ok_or_else(|| PluginError::invalid_input("`text` is required"))?;
        Ok(Output::new().with("text", json!(text.to_uppercase())))
    }
}

export_plugin!(Upper);
```

```sh
rustup target add wasm32-unknown-unknown
cargo build --release --target wasm32-unknown-unknown
```

See [`sdk/wasm/examples/text-tools`](../sdk/wasm/examples/text-tools) for a
complete plugin with a streaming node and a branching node (≈150 KB `.wasm`).

Tools: list them with `Manifest::tool(name, description, parameters)`,
implement `Plugin::call_tool(ToolCall) -> Result<ToolResult, PluginError>`
(`call.env("NAME")`, `call.secret("user_jwt")`) and export with
`export_plugin!(MyPlugin, tools)`. Outbound HTTP:
`graphrt_wasm_guest::http(&HttpRequest { .. })`. The
[supabase WebAssembly build](https://github.com/BimaPangestu28/graphrt-plugins/tree/main/wasm/supabase-rpc)
is a complete tool plugin with HTTP.

## Other languages

Any toolchain that emits a `wasm32-unknown-unknown` module (no WASI imports)
can implement the ABI directly: export the five items, parse the request JSON,
write the response JSON into memory and return `(ptr << 32) | len`. The
integration tests build guests by hand in WAT to exercise every failure mode,
which doubles as a minimal reference.

## Limits reference

| `WasmLimits` field | Default | Meaning |
| --- | --- | --- |
| `fuel` | 1 000 000 000 | CPU budget per call. |
| `memory_bytes` | 64 MiB | Linear memory cap. |
| `module_bytes` | 32 MiB | Maximum `.wasm` size. |
| `input_bytes` / `output_bytes` | 8 MiB | Request / response JSON caps. |
| `message_bytes` | 256 KiB | Per log line / chunk / progress payload. |
| `max_logs` | 100 | Log lines kept per run. |
| `max_events` | 10 000 | `chunk` + `progress` calls per call. |
| `max_stream_bytes` | 16 MiB | Bytes streamed through `chunk` + `progress` per call. |
| `max_recursion_depth` | 1024 | Wasm call depth. |
| `expose_secrets` | `false` | Hand plaintext secrets to the guest. |
| `max_http_requests` | 16 | `graphrt::http` requests per call. |
| `http_timeout_ms` | 30 000 | Upper bound of a guest request's timeout. |
| `http_request_bytes` | 1 MiB | Request body cap. |
| `http_response_bytes` | 4 MiB | Response body cap. |

With the `tokio` feature (on by default), guest calls run on Tokio's blocking
pool when a Tokio runtime is active, so long computations never stall other
nodes or their timeouts. Builds without it run guests inline on the calling
task. Node timeouts do not interrupt an interpreter mid-call; fuel is the CPU
bound.
