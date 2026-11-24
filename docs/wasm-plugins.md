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
  (`graphrt plugin install`), `checksums`, and `permissions.secrets` (granted
  only to plugins the operator trusts: `--trust-plugin <name>`). Node kinds
  must start with `<name>.`; with `allow_unprefixed_kinds` plain names without
  `.` are also accepted. The CLI ships the loader for `runtime: wasm`.
  See [`plugins/examples/wasm-text-tools`](../plugins/examples/wasm-text-tools).

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

The interpreter costs binary size instead: the agent with `wasm` is 6.62 MiB
(6,937,800 B) against 6.10 MiB (6,395,096 B) with process plugins
(`serve,llm,plugins,trust-gateway`, aarch64 musl), +530 KiB for wasmi.

For a plugin on the hot path of a scale-to-zero deployment (every request a
possible cold wake), prefer a WebAssembly build when the plugin is pure
computation (parsing, formatting, validation, routing). Plugins that need
network access, a native SDK or a JavaScript library stay process plugins:
keep them warm with [`serve --plugin-pool`](plugins.md#warm-process-pool),
`min_replicas` or a prewarm, or run them as a shared HTTP plugin. None of the
[graphrt-plugins](https://github.com/BimaPangestu28/graphrt-plugins) catalog
plugins has a WebAssembly build yet (all are Node.js process plugins, most of
them API clients that need the network), so the example above is the
reference measurement.

## Security model

- **No ambient capabilities.** Guests can only import `graphrt::log`,
  `graphrt::chunk` and `graphrt::progress`. Modules importing anything else
  (WASI, `env`, ...) are rejected at load. No filesystem, network, clock or
  randomness.
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

Guests never need to free: the instance is discarded after each call.

### Host imports (module `graphrt`, all optional)

| Import | Signature | Purpose |
| --- | --- | --- |
| `log` | `(level: i32, ptr: i32, len: i32)` | Log line, level 0 (trace) .. 4 (error). Returned in the node's `metadata.logs`; when the run fails, the last lines are appended to the error message. |
| `chunk` | `(field_ptr, field_len, ptr, len: i32)` | Stream text for output `field` (emits `node_chunk` events immediately). |
| `progress` | `(name_ptr, name_len, ptr, len: i32)` | Structured progress (`node_progress` event); payload is JSON. |

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

`resume` is present only when the node previously paused.

### Response

One of:

```json
{"outputs": {"slug": "hello-world"}, "branch": "short", "metadata": {}, "usage": {"prompt_tokens": 0}}
{"pause": {"kind": "approval", "data": {"question": "..."}, "state": {"step": 1}}}
{"error": {"kind": "invalid_input", "message": "`text` is required", "retryable": false}}
```

`error` wins over `pause`, which wins over `outputs`. A response with none of
them is rejected (`wasm_invalid_output`).

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

With the `tokio` feature (on by default), guest calls run on Tokio's blocking
pool when a Tokio runtime is active, so long computations never stall other
nodes or their timeouts. Builds without it run guests inline on the calling
task. Node timeouts do not interrupt an interpreter mid-call; fuel is the CPU
bound.
