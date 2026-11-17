# Plugins

Plugins add node kinds (and tools) to graphrt without recompiling it. A plugin is
a directory with a manifest and an executable written in any language, a remote
HTTP service, or a WebAssembly module. Once installed, its nodes appear in the
node catalog and can be used in workflow documents like built-in nodes:

```yaml
- id: mood
  type: acme.sentiment          # <plugin name>.<kind>
  text: "{{#start.review#}}"
```

SDKs: [Rust](../crates/graphrt-plugin-sdk) · [Python](../sdk/python) ·
[TypeScript / JavaScript](../sdk/typescript). Examples live in
[`plugins/examples`](../plugins/examples) and
[`crates/graphrt-plugin-sdk/examples`](../crates/graphrt-plugin-sdk/examples).

## Layout and discovery

```
~/.graphrt/plugins/
  acme/
    graphrt-plugin.json         # or graphrt-plugin.yaml
    plugin.py
```

The host scans, in order:

1. directories listed in `GRAPHRT_PLUGIN_PATH` (`:`-separated; `;` on Windows),
2. `~/.graphrt/plugins`.

Each direct subdirectory containing `graphrt-plugin.json` or
`graphrt-plugin.yaml` (or `.yml`) is one plugin. A directory passed explicitly
may also itself be a plugin directory.

## Manifest

```json
{
  "name": "acme",
  "version": "1.2.0",
  "description": "Sentiment analysis nodes",
  "author": "ACME Corp",
  "license": "MIT",
  "homepage": "https://example.com/acme",
  "protocol_version": 1,

  "runtime": "process",
  "command": "python3",
  "args": ["plugin.py"],
  "env": { "ACME_MODE": "fast" },
  "checksums": { "plugin.py": "<sha256 hex>" },

  "permissions": { "network": false, "env": ["ACME_API_KEY"], "secrets": false },
  "limits": { "timeout_ms": 30000, "max_concurrency": 8, "max_message_bytes": 16777216 },

  "nodes": [
    {
      "kind": "acme.sentiment",
      "title": "Sentiment",
      "description": "Classify the sentiment of a text",
      "category": "model",
      "config_schema": { "type": "object", "properties": { "text": { "type": "string" } }, "required": ["text"] },
      "outputs": [
        { "name": "label", "type": "string" },
        { "name": "score", "type": "number" },
        { "name": "explanation", "type": "string", "streaming": true }
      ]
    }
  ],
  "tools": [
    { "name": "score", "description": "Score a text", "parameters": { "type": "object", "properties": { "text": { "type": "string" } } } }
  ]
}
```

| Field | Meaning |
| --- | --- |
| `name` | Plugin id: lowercase letters, digits, `-` and `_`, 1–64 characters. |
| `version` | Plugin version (free-form, semver recommended). |
| `protocol_version` | Protocol version the plugin speaks. Currently `1`. |
| `runtime` | `process`, `http` or `wasm`. |
| `nodes` | Node descriptors (see the node catalog). Each `kind` must start with `<name>.`. With `allow_unprefixed_kinds: true`, plain names without `.` (e.g. `slugify`) are also allowed; another plugin's `<other>.` namespace never is. Unknown descriptor keys are errors. |
| `tools` | Tools exposed to `tool` and `agent` nodes under provider `<name>`. |
| `permissions.env` | Host environment variables passed to the process (in addition to `PATH`, `HOME`, `LANG`, `TMPDIR` and their Windows equivalents). An HTTP plugin's `bearer_token_env` must be listed here. |
| `permissions.secrets` | Requests plaintext secret values in node config and resume input. **Granted only when the operator trusts the plugin** (`--trust-plugin <name>`); otherwise secrets are sent masked as `******` and a warning is printed. |
| `permissions.network` | Informational: whether the plugin makes network calls. Shown to users before install. |
| `limits.timeout_ms` | Per-request timeout (default 60 000). The node's own deadline still applies. After 3 consecutive timeouts a process plugin is considered stuck: it is killed and restarted on the next request. |
| `limits.max_concurrency` | Concurrent requests per plugin (default 16). |
| `limits.max_message_bytes` | Maximum size of one protocol message (default 16 MiB). Sent to the plugin in `initialize` so SDKs read lines of the same size. |
| `checksums` | Optional `relative path -> sha256` map, verified before every process start. When present it must include the entrypoint (a `command` inside the plugin directory, or the wasm `module`). Pinned files must be regular files, not symlinks. |

Runtime-specific fields:

| Runtime | Fields |
| --- | --- |
| `process` | `command`, `args`, `env` (fixed values), `inherit_env` (default `false`), `cwd` (relative to the plugin directory; default: the plugin directory) |
| `http` | `url`, `headers`, `bearer_token_env` (name of a host environment variable holding the token; it must be listed in `permissions.env`, the plugin must be trusted, and `url` must be `https://` unless it is a loopback address) |
| `wasm` | `module` (path relative to the plugin directory, no `..`). The graphrt CLI ships the loader (`graphrt_cli::wasm_loader::WasmLoader`); embedders register it with `PluginHost::with_loader`. |

A plugin's process is started lazily, the first time one of its nodes or tools
runs, and is reused for all later requests.

## Protocol

Messages are [JSON-RPC 2.0](https://www.jsonrpc.org/specification).

- **process**: newline-delimited JSON over the plugin's stdin/stdout. One message
  per line, UTF-8, no embedded newlines. Anything the plugin writes to stderr is
  kept (the last 64 KiB) and attached to crash errors. Never print anything else
  to stdout.
- **http**: every request is a `POST` of one JSON-RPC request to `url`. The
  response is either `application/json` (a single JSON-RPC response) or
  `application/x-ndjson` (zero or more notifications, one per line, followed by
  the final JSON-RPC response). Cancellation closes the connection.

### Lifecycle

1. Host → plugin `initialize`

   ```json
   {"jsonrpc":"2.0","id":"init","method":"initialize","params":{"protocol_version":1,"host":{"name":"graphrt","version":"0.1.0"},"max_message_bytes":16777216}}
   ```

   `max_message_bytes` is the largest line the host will send; the SDKs adopt it.
   A plugin that receives a longer line should skip it up to the newline and
   answer that request (id recovered from the line when possible) with an
   error such as `{"kind": "message_too_large"}`.

   Result: `{"protocol_version": 1, "nodes": [...]?, "tools": [...]?}`. `nodes` and
   `tools` are optional; when present they replace the manifest's lists for
   hosts that refresh descriptors (`PluginHost::refresh`).

2. Any number of `node.run`, `tool.list` and `tool.call` requests, possibly
   concurrent (match responses by `id`).
3. Host → plugin `shutdown` notification before the process is stopped.

### `node.run`

```json
{"jsonrpc":"2.0","id":"r-7","method":"node.run","params":{
  "request_id":"r-7",
  "kind":"acme.sentiment",
  "node_id":"mood",
  "attempt":1,
  "config":{"text":"I love it","text_raw":"{{#start.review#}}"},
  "resume":null
}}
```

`config` is the node's configuration **with variables already resolved**:

- Every string containing `{{#namespace.name#}}` placeholders is rendered. A string
  that is exactly one placeholder becomes the typed value (number, object,
  array...); otherwise the placeholders are interpolated as text. The original
  template is kept in a sibling key `<key>_raw`.
- Every key whose name ends in `selector` (`value_selector`,
  `query_variable_selector`, ...) holding a selector (`["node","var"]` or
  `"node.var"`) gets a sibling `<key>_value` with the selected value (`null` when
  missing).
- This applies recursively inside objects and arrays.

The host also uses these placeholders and selectors to validate references when
the workflow is loaded, so typos fail early.

`resume` is `{"state": <your saved state>, "input": <host input or null>}` when the
node is resumed after pausing, else `null`. `attempt` starts at 1 and increments
on retries.

Result (success):

```json
{"outputs":{"label":"positive","score":0.93},
 "branch":null,
 "writes":[{"selector":["conversation","last_mood"],"value":"positive"}],
 "usage":{"prompt_tokens":0,"completion_tokens":0,"total_tokens":0},
 "metadata":{"model":"tiny"}}
```

Only `outputs` is required. `branch` selects the outgoing edge handle for nodes
declaring `handles` of kind `fixed` or `dynamic`. `writes` may target
`conversation.*` or container variables; writes to `sys.*` and `env.*` are
rejected.

Result (pause, e.g. waiting for a human):

```json
{"pause":{"kind":"approval","data":{"message":"Publish this post?"},"state":{"draft":"..."}}}
```

The run pauses with a snapshot; on resume the node receives `resume.state` and
the host-supplied `resume.input`.

Outputs are plain JSON. Objects are never turned into file values, whatever keys
they contain.

### Notifications from the plugin

Sent while a `node.run` is in progress (no `id`):

| Method | Params | Effect |
| --- | --- | --- |
| `node.chunk` | `{request_id, field, chunk}` | Streams text for output `field` (`node_chunk` event). |
| `node.progress` | `{request_id, name, data}` | Structured progress (`node_progress` event). |
| `log` | `{level, message, request_id?}` | Plugin log line (`debug`/`info`/`warn`/`error`). Written to the host's stderr as `[plugin <name>] <level>: <message>` (`debug` only with `GRAPHRT_PLUGIN_LOG=debug`); with a `request_id` it also becomes a `plugin_log` progress event of that node. |

Every response must carry `result` or `error`; a response with neither, a
`node.run` result with unknown keys (e.g. `output` instead of `outputs`), or an
`id: null` error (the plugin lost a request) is a protocol error. A branching
node with `fixed` handles must return one of the declared handles.

### Cancellation

Host → plugin notification `node.cancel {request_id}` when a run is aborted or a
request times out. The plugin should stop work for that request; any later
response for it is ignored.

### Tools

- `tool.list` → `{"tools": [ToolSpec...]}` (optional; the manifest's `tools` are
  used when the method is not implemented).
- `tool.call {name, arguments}` → `{"text": "...", "data": <json>?, "is_error": false}`.

### Errors

A JSON-RPC error response fails the node:

```json
{"jsonrpc":"2.0","id":"r-7","error":{"code":-32000,"message":"model unavailable","data":{"kind":"upstream","retryable":true,"retry_after_ms":2000}}}
```

`data.kind` becomes the node error type (default `plugin_error`), `data.retryable`
controls retries (default `false`), `data.retry_after_ms` delays the retry.
Standard JSON-RPC codes (`-32601` method not found, `-32602` invalid params...)
are reported as non-retryable `plugin_protocol` errors.

Transport failures are retryable: if the process exits, every in-flight request
fails with `plugin_crashed` (the message includes the exit code or signal and
the end of stderr), and the next request restarts the process (with a backoff
of up to 5 seconds after repeated crashes). On Unix the plugin runs in its own
process group, and the whole group is killed on crash, restart or shutdown, so
wrapper commands (`sh run.sh`, `npx`, `uv run`) do not leave the real plugin
running.

HTTP plugins: a non-2xx response whose body is a JSON-RPC error keeps that
error's `kind` and `retryable`; otherwise the first 4 KiB of the body are
included in the message. 4xx statuses (except 408 and 429) are not retried.

## Security model

Manifest permissions are **requests**; the operator grants them:

- Plaintext secrets (`permissions.secrets`) and HTTP bearer tokens
  (`bearer_token_env`) need the plugin to be trusted by name:
  `graphrt run ... --trust-plugin acme` (repeatable), or
  `GRAPHRT_TRUSTED_PLUGINS=acme,other`; embedders call `PluginHost::trust`.
  Untrusted plugins get masked secrets (a warning is printed), and an untrusted
  HTTP plugin asking for a bearer token does not load.
- Credentials and secrets are only sent to HTTP plugins over `https://`
  (loopback addresses excepted).
- Plugin processes start with a **clean environment**: only the variables listed
  in `permissions.env`, the fixed `env` map and a few basic variables (`PATH`,
  `HOME`, `LANG`, `TMPDIR`). This is hygiene, not a sandbox: native plugin code
  runs with the privileges of the graphrt process and can read anything the
  process can.
- Plugin node kinds never replace built-in nodes or another plugin's nodes, and
  a plugin cannot claim another plugin's `<name>.` namespace.
- `checksums` detect corrupted or partially modified files. They are **not** a
  signature: the hashes live in the manifest they protect, so anyone who can
  rewrite the plugin directory can update both, and files are hashed before
  they are executed by path. Keep plugin directories writable only by the
  operator, and pin the entrypoint (required when `checksums` is used).
- Only install native (`process`) plugins you trust. For untrusted code, use the
  `wasm` runtime: modules run in an interpreter with fuel, memory and output
  limits and no file system or network access.

## Writing a plugin

### Python

```python
from graphrt_plugin import Plugin

plugin = Plugin("acme")

@plugin.node("acme.sentiment", title="Sentiment")
async def sentiment(ctx, req):
    text = req.config.get("text", "")
    await ctx.chunk("explanation", "thinking...")
    return {"outputs": {"label": "positive" if "love" in text else "neutral"}}

plugin.run()
```

### TypeScript / JavaScript

```js
import { Plugin } from "graphrt-plugin";

const plugin = new Plugin("acme");
plugin.node("acme.upper", async (ctx, req) => ({ outputs: { text: String(req.config.text).toUpperCase() } }));
plugin.run();
```

### Rust

```rust
use graphrt_plugin_sdk::{Plugin, RunResult};

#[tokio::main(flavor = "current_thread")]
async fn main() -> std::io::Result<()> {
    Plugin::new("acme")
        .node("acme.len", |_ctx, req| async move {
            let text = req.config["text"].as_str().unwrap_or_default().to_owned();
            Ok(RunResult::new().output("length", text.chars().count()))
        })
        .serve_stdio()
        .await
}
```

## Using plugins from the CLI

```sh
graphrt run workflow.yaml --plugins ./my-plugins     # try plugins in place
graphrt plugin install ./acme                        # copy into ~/.graphrt/plugins
graphrt plugin install https://github.com/acme/graphrt-acme --rev v1.2.0
graphrt plugin list                                  # what is installed, and load errors
graphrt plugin info acme                             # node descriptors
graphrt plugin remove acme
graphrt plugin new acme --lang python|typescript|rust   # scaffold a working plugin
```

Every command that builds graphs (`run`, `serve`, `validate`, `inspect`,
`nodes`) loads plugins from `--plugins` flags, `$GRAPHRT_PLUGIN_PATH` and
`~/.graphrt/plugins` (skip the last two with `--no-default-plugins`). Plugin
nodes appear in `graphrt nodes` and `GET /v1/nodes` with `source:
"plugin:<name>"`, and plugin tools join MCP tools for `tool` and `agent` nodes
(provider name = plugin name). A plugin that fails to load is skipped with a
warning. Grant secrets / bearer tokens with `--trust-plugin <name>` (see the
security model).

A `.wasm` module placed directly in a plugin search path is loaded as a plugin
on its own (it describes itself; see [WebAssembly plugins](wasm-plugins.md)).

## Publishing a plugin

A plugin is just a directory, so any git repository works:

1. Put `graphrt-plugin.json` at the repository root, with a unique `name` and
   node kinds prefixed `<name>.`.
2. Vendor the SDK file (`graphrt_plugin.py` / `graphrt-plugin.mjs`) or document
   how to install it; `graphrt plugin new` vendors it for you.
3. For compiled plugins, commit release binaries (or a build step users run)
   and pin them with `checksums` in the manifest; `graphrt plugin install`
   verifies them before copying, and the host verifies them before every start.
4. Tag releases; users install with `graphrt plugin install <git-url> --rev <tag>`.

## Host API

```rust
use graphrt_core::{Registry, tool::ToolRegistry};
use graphrt_plugin::PluginHost;

let mut host = PluginHost::new().trust("acme"); // grants requested secrets / bearer tokens
host.load_dirs(&graphrt_plugin::default_plugin_paths())?;
let mut registry = Registry::with_builtins();
let mut tools = ToolRegistry::new();
host.register(&mut registry, &mut tools)?;
```

`PluginHost::plugins()` reports the effective permissions, whether each plugin
is trusted, and warnings for requests that were not granted.

Custom runtimes (such as `wasm`) are added with `PluginHost::with_loader`. The
CLI's adapter for `runtime: wasm` (`graphrt_cli::wasm_loader::WasmLoader`) is a
complete example: it loads the module with graphrt-wasm, applies the granted
`permissions.secrets`, and returns the node factories. The host checks every
kind a loader returns against the same namespace rules as manifest kinds.
