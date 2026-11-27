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

The full, versioned reference (every field, type, default and constraint, plus
the compatibility policy) is [Plugin manifest specification](plugin-manifest.md),
with a JSON Schema in [`schemas/plugin-manifest.schema.json`](schemas/plugin-manifest.schema.json).

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
| `permissions.env` | Host environment variables passed to the process (in addition to `PATH`, `HOME`, `LANG`, `TMPDIR`, and `SYSTEMROOT`, `USERPROFILE`, `APPDATA`, `TEMP`, `TMP`, `PATHEXT` on Windows). An HTTP plugin's `bearer_token_env` must be listed here. |
| `permissions.secrets` | Requests plaintext secret values in node config, resume input and the `context` of tool calls ([Tool call context](#tool-call-context)). **Granted only when the operator trusts the plugin** (`--trust-plugin <name>`); otherwise secrets are sent masked as `******` and a warning is printed. |
| `permissions.network` | Informational: whether the plugin makes network calls. Shown to users before install. |
| `limits.timeout_ms` | Per-request timeout (default 60 000), counted from when the plugin is up: starting a process plugin (its process and `initialize` handshake) has its own bound of 15 s and does not use up a request's timeout, so the first request after a cold start of a slow Node.js or Python process is not cut short. The node's own deadline covers both. After 3 consecutive timeouts a process plugin is considered stuck: it is killed and restarted on the next request. |
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
   {"jsonrpc":"2.0","id":"init","method":"initialize","params":{"protocol_version":1,"host":{"name":"graphrt","version":"0.1.0"},"max_message_bytes":16777216,"features":["tool_images","tool_context"]}}
   ```

   `max_message_bytes` is the largest line the host will send; the SDKs adopt it.
   It also bounds what the host reads: a longer line from the plugin breaks the
   connection, so the SDKs answer `message_too_large` instead of sending one.
   `features` lists optional protocol features the host supports (absent on
   older hosts): `tool_images` means `tool.call` results may carry `images`;
   `tool_context` means `tool.call` requests may carry a `context` (see
   [Tool call context](#tool-call-context)).
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
  "idempotency_key":"run-18c2f0a1d3e-4b1f0c2a-7:18c2f0a1d40-4b1f0c2a-9",
  "config":{"text":"I love it","text_raw":"{{#start.review#}}"},
  "resume":null
}}
```

`idempotency_key` is `<run_id>:<exec_id>`. A node can run more than once: on a
retry (`attempt` increments) and when a run that was preempted (a shutdown
drain, a lost gateway stream) resumes and runs the in-flight node again.
Execution is at-least-once per node, and the key is the same in all those
cases and different for every other execution, so key external side effects
on it (send it as an `Idempotency-Key` header, or use it as a unique id). The
SDKs expose it as `RunRequest.idempotency_key` (Rust, Python) and
`idempotencyKey` (TypeScript); it is empty with hosts older than 0.5.

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
- `tool.call {request_id, name, arguments, context?}` → `{"text": "...", "data": <json>?, "is_error": false, "images": [...]?}`.

`images` (only when the host announced `tool_images`) shows images to the
model, for example a screenshot for a computer-use loop:

```json
{"text": "1280x800 screenshot", "images": [{"media_type": "image/png", "data_base64": "iVBORw0KGgo..."}, {"url": "https://example.com/chart.png"}]}
```

At most 4 images, `image/png`, `image/jpeg`, `image/webp` or `image/gif`, up
to 5 MiB each (decoded); URLs must be `https://`. A result that breaks these
rules fails the call with a `plugin_protocol` error. The whole result must
still fit in `max_message_bytes`. Older hosts reject unknown result fields, so
the SDKs drop images (adding a note to `text`) when `tool_images` was not
announced. Per-provider encoding is described in [tool-images.md](tool-images.md).

### Tool call context

An `agent` node can send per-run values to a plugin's tools without putting
them in the model-written `arguments`, for example the end user's JWT so a
database tool queries with the user's own permissions:

```yaml
# agent node
tools: [supabase/call_function]
tool_context:
  supabase:                              # tool provider (the plugin name)
    user_jwt: "{{#start.user_jwt#}}"     # any template; secrets are rendered in plaintext
```

The host then adds a `context` object to every `tool.call` for that plugin:

```json
{"jsonrpc":"2.0","id":"r-9","method":"tool.call","params":{
  "request_id":"r-9",
  "name":"call_function",
  "arguments":{"function_name":"get_balance"},
  "context":{"secrets":{"user_jwt":"eyJhbGciOiJIUzI1NiJ9..."}}
}}
```

- `context` is `{"secrets": {<name>: <string>}}`. Values are always strings.
  Names whose template renders empty (e.g. a missing variable) are left out,
  so the tool can refuse the call instead of running without credentials.
- It is only sent to plugins that request `permissions.secrets` **and** are
  trusted by the operator (`--trust-plugin <name>`). An `agent` whose
  `tool_context` names any other provider (an untrusted plugin, an MCP server,
  native tools) fails with `tool_context_denied` before calling the model.
- It is omitted when empty, so plugins that predate it see the same requests
  as before. Hosts that may send it list `tool_context` in `initialize`
  `features`.
- The values never reach the model, events, SSE, run records, snapshots or
  traces. The host registers them as run secrets and replaces them with
  `******` in tool results, logged tool arguments and node error messages.
  On resume (e.g. after a tool approval) they are rendered again from the
  run's variables; secret inputs must be re-supplied as for any resume.
- Plugins must not log the values or return them in results.

Every `tool.call` from an `agent` node also carries `idempotency_key`
(`<run_id>:<exec_id>:<turn>:<call id>`), to every plugin. It is the same for every
attempt of that call: a retry, or the run resumed after a preempt. A plugin
that charges, sends or writes can keep the keys it handled and do the effect
once. It is not a secret.

SDKs pass the context as the tool handler's second argument (handlers that
take only the arguments keep working):

| SDK | Handler | Context |
| --- | --- | --- |
| TypeScript | `async (args, context) => ...` | `context.secrets.user_jwt` (`{ secrets: {} }` when none; frozen) |
| Python | `async def handler(args, context)` | `context.secrets.get("user_jwt")` (`ToolContext`; passed only to handlers with two positional parameters) |
| Rust | `\|call: ToolCall\| async move { ... }` | `call.context.secret("user_jwt")` (`ToolCallContext`) |

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
of up to 5 seconds after repeated crashes; with `serve --plugin-pool` a
replacement starts in the background at once, see
[Warm process pool](#warm-process-pool)). On Unix the plugin runs in its own
process group, and the whole group is killed on crash, restart or shutdown, so
wrapper commands (`sh run.sh`, `npx`, `uv run`) do not leave the real plugin
running.

HTTP plugins: a non-2xx response whose body is a JSON-RPC error keeps that
error's `kind` and `retryable`; otherwise the first 4 KiB of the body are
included in the message. 4xx statuses (except 408 and 429) are not retried.

## Warm process pool

A process plugin starts at its first node and then serves every run of the
process that loaded it. Starting it is the plugin's own start-up: a Node.js
host takes ~50 ms (~51 ms spawn → `initialize` for
[`node-uppercase`](../plugins/examples/node-uppercase), Linux aarch64), and
graphrt adds ~5 ms around it. `graphrt serve --plugin-pool <n>` moves that
start off the request path whenever the agent is already running:

```sh
graphrt serve app.yaml --plugins ./plugins --plugin-pool 1                    # started at startup, kept up
graphrt serve app.yaml --plugins ./plugins --plugin-pool 2 --plugin-idle 5m   # two per plugin, stopped after 5 min idle
```

- **Members.** Each process plugin gets `n` processes (up to 32), started at
  startup. A request goes to a started process with the fewest requests in
  flight, so CPU-bound plugins (one Node.js thread each) run `n` requests in
  parallel. A request id names the process that served it: three timeouts in
  a row restart that process only.
- **Replacement.** A process that exits (crash, or killed after
  [three timeouts](#manifest)) is started again in the background at once, so
  the next run does not pay the start. A process that dies within a second of
  its start counts as a failure: the pool backs off (200 ms doubling to 5 s)
  and after six in a row stops restarting; requests then start the plugin on
  demand, as without a pool, until the next warm hint.
- **Idle stop.** With `--plugin-idle <d>`, a pool that served no request for
  `d` stops its processes, freeing their memory. The next request starts its
  process (and waits for it); a **warm hint** starts them ahead of it: the
  gateway sends one (`POST /internal/v1/warm`, see
  [gateway-protocol.md](gateway-protocol.md#endpoints)) to the agents a
  prewarm (`POST …/warm`, cron `prewarm_secs`, predictive prewarm) finds
  already up. A hint counts as use: the pool stays up for `d` after it (counted
  from when its processes are up, so a slow start never ends in an idle stop).
- **Isolation.** The processes share nothing, and the protocol keeps no state
  in a process between requests: a paused node's state travels with the
  host's resume request, and cancellation goes to the process that runs the
  request. Any process may serve any request, including a resume. A plugin
  that keeps its own state in memory (a cache, a connection) holds one copy
  per process.
- **Default.** `0`: one process, started at its first node (behind a gateway,
  `--trust-gateway`, at startup, since a run is on its way), kept until the
  server stops. HTTP and WebAssembly plugins have no start to hide and are
  not pooled.

**Memory.** Each pooled process costs the plugin's resident memory. Measured
with `bench/targets.sh` (`plugin-pool`, Linux aarch64, static musl agent
`serve,llm,plugins,trust-gateway`, Node.js 22, 2026-11-24):

| Agent behind a gateway, kept warm | RSS of the agent and its plugin processes |
| --- | ---: |
| default (`--plugin-pool 0`: one process, started at startup) | 73.5 MiB |
| `--plugin-pool 1` | 73.5 MiB |
| `--plugin-pool 2` | 141.0 MiB |
| `--plugin-pool 1 --plugin-idle 1s`, after the idle stop | 5.9 MiB |

| Run with the Node.js plugin node, through the gateway (n = 20) | p50 / p95 |
| --- | ---: |
| on a warm pool | 1.71 / 3.94 ms |
| after the idle stop, no hint (the run starts Node.js) | 87.3 / 96.2 ms |
| after the idle stop, 300 ms after a warm hint | 6.34 / 10.46 ms |
| cold wake of a new agent (`wake-node`, unchanged by the pool) | 56.4 / 59.7 ms |

What a pool does not change: the cold wake of a *new* agent (scale to zero,
no prediction). That agent starts its plugins at startup either way, and the
run waits for Node.js itself (`wake-node` with and without the pool code,
three to four runs of 3 × 100 wakes each: p50 55–59 ms on both, the plugin's
own spawn → `initialize` p50 51–54 ms in the quiet runs). Hiding it needs the plugin process up before the wake
(`min_replicas`, a prewarm, or a pool on an agent that stays up) or a runtime
that starts in microseconds: [WebAssembly](wasm-plugins.md#cold-start-the-path-for-hot-plugins)
(a cold wake with the WebAssembly example plugin: p95 15 ms). A Node.js start
on an idle machine is also slower than back to back (85–200 ms in the
benchmark VM after 2 s idle, which is the "no hint" row above): a hint ahead
of predicted traffic avoids both.

From Rust: `PluginHost::new().with_pool(PoolConfig { size, idle_timeout })`
before loading, then `PluginHost::prewarm()` (also the warm hint);
`PluginHost::warm_plugins()` lists the plugins whose process is up.

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
  `HOME`, `LANG`, `TMPDIR`; on Windows also `SYSTEMROOT`, `USERPROFILE`, `APPDATA`,
  `TEMP`, `TMP`, `PATHEXT`). This is hygiene, not a sandbox: native plugin code
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
plugin.tool({ name: "screenshot", description: "Capture the screen" }, async () => ({
  text: "1280x800",
  images: [{ media_type: "image/png", data: await capturePng() }], // Buffer, or data_base64
}));
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
graphrt plugin search weather                        # browse the plugin catalog
graphrt plugin install open-meteo                    # install from the catalog (latest version)
graphrt plugin install open-meteo@0.1.0              # ...or an exact version
graphrt plugin install ./acme                        # copy into ~/.graphrt/plugins
graphrt plugin install https://github.com/acme/graphrt-acme --rev v1.2.0
graphrt plugin install https://github.com/acme/plugins --rev v1.2.0 --subdir acme   # monorepo
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

`--subdir <path>` installs the plugin in a subdirectory of a git repository,
for collections with one plugin per directory (such as
[graphrt-plugins](https://github.com/Graphrt/graphrt-plugins)):

```sh
graphrt plugin install https://github.com/Graphrt/graphrt-plugins --rev main --subdir whatsapp-api
```

The path must be relative to the repository root; absolute paths, `..`
components and symlinks along the path are rejected. Only that directory is
copied into the install directory, after the usual manifest, name and checksum
checks. From Rust, the same is `graphrt_plugin::install_from_git_subdir`.

### Installing from the plugin catalog

[graphrt-plugins](https://github.com/Graphrt/graphrt-plugins) publishes
a catalog: its `catalog` release carries `index.json` (every published plugin
with its version, tarball URL, sha256, size, status and ToS risk), its
minisign signature `index.json.minisig` and `SHA256SUMS`, and each plugin is released as `<name>-<version>.tar.gz`.
`graphrt plugin install <name>` and `<name>@<version>` install from it:

```sh
$ graphrt plugin search
NAME                      VERSION     STATUS        DESCRIPTION
open-meteo                0.1.0       stable        Open-Meteo weather (no API key): forecasts with current, hourly and d...
whatsapp-web              0.1.0       experimental  Your personal WhatsApp through WhatsApp Web in a headless browser: re...

$ graphrt plugin install open-meteo
open-meteo 0.1.0: status stable, ToS risk none
  Public no-auth API; the example read a real Jakarta forecast live (PR #27, rechecked 2026-11-19).
installed open-meteo 0.1.0 into ~/.graphrt/plugins/open-meteo (open-meteo.api_request, open-meteo.get_forecast, open-meteo.search_locations)

$ graphrt plugin install whatsapp-web
whatsapp-web 0.1.0: status experimental, ToS risk high
  Unofficial WhatsApp Web automation of a personal account; only get_status ran live, no real send.
warning: whatsapp-web: automating a personal WhatsApp account breaks WhatsApp's Terms of Service. ...
error: `whatsapp-web` 0.1.0 is labelled status experimental and ToS risk high; re-run with `--yes` to install it anyway (there is no interactive prompt)
```

How a name is resolved and installed:

1. The source is a catalog name when it is not an existing directory and has
   the form `name` or `name@MAJOR.MINOR.PATCH[-pre]` (plugin names are
   `[a-z0-9_-]{1,64}`). A directory of the same name in the current directory
   wins; write `./name` to make a local install explicit.
2. The index is read from `--index <url|path>`, else `$GRAPHRT_PLUGIN_INDEX`,
   else `https://github.com/Graphrt/graphrt-plugins/releases/download/catalog/index.json`.
   It may be an `https://` URL, a `file://` URL (`file:///C:/catalog/index.json`
   on Windows) or a local path; plain
   `http://` is refused because the index carries the checksums.
3. The signature is read from the same place with `.minisig` appended
   (`index.json.minisig`, at most 4 KiB) and must be a minisign signature of
   exactly the index bytes, made with the graphrt-plugins catalog key that is
   built into `graphrt`. This happens before the index is parsed. Without a
   valid signature the command stops (exit code 1) and nothing of the index is
   printed or installed. There is no unverified fallback. See
   [Catalog signatures](#catalog-signatures) for other publishers and for
   local catalogs.
4. Catalogs with a `schema` other than 1 are refused (upgrade graphrt). An
   index older than the newest one already accepted from the same place and
   key is refused as a rollback.
5. Without a version, the highest version by semver precedence is picked,
   skipping prereleases (`1.1.0-rc.1`) unless the plugin has nothing else;
   ask for a prerelease with `name@1.1.0-rc.1`.
   Entries with an invalid plugin name or a non-semver version are skipped,
   and every index string graphrt prints (descriptions, labels, reasons,
   warnings, URLs) has its control and bidirectional formatting characters
   replaced by `?`, as do install errors that quote the tarball (entry paths,
   manifest fields), so a catalog cannot send terminal escape sequences or
   visually reorder a line.
6. The plugin's `status` and `tos_risk` are printed (trimmed and lowercased;
   `unknown` when the catalog predates the labels), plus the README warning
   when there is one. The gate fails closed: only status `stable` or `beta`
   with ToS risk `none`, `low` or `medium` installs without `--yes`. Everything
   else needs `--yes`: `experimental`, ToS risk `high`, a label this graphrt
   does not recognise, and an entry with no labels at all ("this catalog entry
   has no status/ToS label", which is every entry of catalogs published before
   the labels existed). graphrt has no interactive prompt.
7. The tarball is downloaded over `https://` only (redirects included, at most
   five), with a 300 s timeout and a size cap: the entry's `size_bytes` when
   the catalog declares it, never more than 256 MiB. A `file://` tarball URL is
   accepted only when the index itself is local (mirrors and tests).
8. Its sha256 must equal the `sha256` in the verified index. A mismatch is
   refused.
9. It is extracted into a scratch directory under the system temporary
   directory (`$TMPDIR`, else `/tmp`), so up to 1 GiB may be written there
   while installing; the directory is removed afterwards. Only regular files and
   directories are accepted: absolute paths, `..`, `:` and reserved Windows
   device names (`CON`, `NUL`, `COM1`...), symlinks and hard links, device
   files, FIFOs and duplicate paths are refused, as are archives over
   1 GiB of contents or 100,000 entries. Files keep only their executable bit.
10. The tarball's top-level `<name>/` directory, whose manifest must name the
   same plugin and version, is installed exactly like `graphrt plugin install
   <dir>`: manifest validation, `checksums` verification, a copy into a
   staging directory next to the target, removal of the previous install and a
   rename of the staging copy into place, then a load check. This replace is
   not atomic: if it is interrupted between the removal and the rename, the
   plugin is left uninstalled (run the install again).

#### Catalog signatures

The graphrt-plugins release workflow signs `index.json` with
[minisign](https://jedisct1.github.io/minisign/) and uploads the detached
signature `index.json.minisig` next to it. The public key is committed in that
repository as `catalog-signing.pub` and is built into `graphrt`
(`graphrt_plugin::catalog_signature::TRUSTED_CATALOG_PUBLIC_KEYS`, key id
`CB1AEE339F05C2BD`). The desktop app checks the same signature with the same
key.

| Index | Used when |
| --- | --- |
| The default catalog, or any `--index` / `$GRAPHRT_PLUGIN_INDEX` | `<index>.minisig` is a valid signature of the built in key. A mirror works when it serves `index.json.minisig` too. |
| With `--index-key <KEY\|PATH>` or `$GRAPHRT_PLUGIN_INDEX_KEY` | The signature is valid for that key, and only that key. A note on stderr names the key. |
| A local file with `--allow-unsigned-index` | Always, with a warning on stderr. Never for an `https://` index, and not together with an index key. |

What is refused, each with exit code 1 and without printing or installing
anything from the index:

- an index with no `.minisig` next to it, or one that is not a minisign
  signature (an error page, a file over 4 KiB)
- a signature made with any other key, including a key the publisher of a
  fake index made for it
- an index that was changed after it was signed, even by one byte, and a
  signature copied from another index
- a tarball whose sha256 differs from the one in the verified index
- an index of a newer format (`index-v2.json`) served as `index.json`
- a validly signed index that is older than the newest one already accepted
  from the same place and key (its `generated_at` is earlier or gone). Graphrt
  remembers the newest `generated_at` per index location and key in
  `~/.graphrt/plugin-catalog-state.json`. This stops a replay of an old index
  that still lists a withdrawn plugin version. The first index from a place
  is accepted as is. If a publisher really went back to an older catalog,
  remove that file.

A catalog of another publisher needs their minisign public key, named
explicitly:

```sh
minisign -G -p acme.pub -s acme.key            # the publisher, once
minisign -S -s acme.key -m index.json          # writes index.json.minisig
graphrt plugin install acme-crm --index https://plugins.acme.example/index.json --index-key acme.pub
```

`--index-key` takes the base64 key or the path of a `.pub` file and replaces
the built in key for that command. Only prehashed signatures are accepted,
which is what current minisign writes.

**Trust model.** Installing by name trusts the holder of the catalog signing
key. Whoever can replace the index or its signature (a mirror, a
misconfigured proxy, a swapped release asset) cannot make `graphrt` install
anything the key holder did not publish, because the signature covers the
index and the index pins every tarball by sha256. The signature does not
review plugins: a published plugin still runs with the permissions its
manifest asks for, so the status and ToS labels and the manifest matter as
before. Installing from a directory or a git URL reads no catalog and is
trusted like any local file. `--allow-unsigned-index` and `--index-key` change
who is trusted, so both are explicit flags that print what they do. A
compromised signing key needs a new `graphrt` release with the new key, and
the list of built in keys can hold the old and the new key during a rotation.

To test a catalog before publishing it, pack it locally and point `--index` at
the result (rewrite `tarball_url` to `file://` paths first, since the packed
index points at release URLs). A local catalog packed without the signing key
has no signature, so say so:

```sh
node scripts/pack-plugins.mjs open-meteo --out /tmp/catalog   # in graphrt-plugins
graphrt plugin install open-meteo --index /tmp/catalog/index.json --allow-unsigned-index
```

A `.wasm` module placed directly in a plugin search path is loaded as a plugin
on its own (it describes itself; see [WebAssembly plugins](wasm-plugins.md)).

## Publishing a plugin

A plugin is just a directory, so any git repository works:

1. Put `graphrt-plugin.json` at the repository root, with a unique `name` and
   node kinds prefixed `<name>.`. A monorepo can instead hold one plugin per
   directory (each with its own manifest); users then add `--subdir <dir>`.
   Checksum paths are relative to that plugin's directory.
2. Vendor the SDK file (`graphrt_plugin.py` / `graphrt-plugin.mjs`) or document
   how to install it; `graphrt plugin new` vendors it for you.
3. For compiled plugins, commit release binaries (or a build step users run)
   and pin them with `checksums` in the manifest; `graphrt plugin install`
   verifies them before copying, and the host verifies them before every start.
4. Tag releases; users install with `graphrt plugin install <git-url> --rev <tag>`
   (plus `--subdir <dir>` for a plugin in a monorepo). Plugins published to the
   graphrt-plugins catalog install by name (`graphrt plugin install <name>`).

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
