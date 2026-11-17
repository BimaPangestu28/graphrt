# The `graphrt` CLI

```
graphrt run <workflow>       Run a workflow and stream its output
graphrt validate <workflow>  Parse and validate without running
graphrt inspect <workflow>   Show nodes, inputs, env variables and missing node kinds
graphrt nodes [--json] [--kind K]  The node catalog: built-ins plus plugins (with config JSON Schemas)
graphrt serve <file|dir>     HTTP + SSE API
graphrt kb ingest|query      Fill and query a knowledge base
graphrt plugin list|install|remove|info|new   Manage plugins
graphrt version              Version and enabled features
```

Workflows can be YAML (`.yaml`, `.yml`) or JSON.

## Exit codes

| Code | Meaning |
| --- | --- |
| 0 | Success |
| 1 | The run failed |
| 2 | Invalid workflow or command-line usage |
| 3 | The run paused (snapshot written) |
| 4 | The run was aborted (e.g. Ctrl-C, timeout) |

## `graphrt run`

| Flag | Description |
| --- | --- |
| `-i, --input key=value` | Start input. Values are strings (converted to the declared type by the start node); values starting with `[` or `{` are parsed as JSON. Use `key:=json` to force JSON (`--input n:=42`). Repeatable. |
| `--inputs-json FILE\|-` | JSON object with start inputs, from a file or stdin. `--input` flags override it. |
| `--sys key=value` | System variable (`sys.*`), e.g. `--sys query="hi"`, `--sys 'history:=[...]'`. |
| `-o, --output text\|json\|events` | `text` (default): live ordered answer text on stdout, or the outputs as JSON when the workflow has no answer; status on stderr. `json`: one result object. `events`: every event as NDJSON. |
| `--env-file FILE` | dotenv file. Supplies model keys and the workflow's declared `env.*` variables. |
| `--mcp FILE` | MCP servers (`{"mcpServers": {...}}`) available to `tool` and `agent` nodes. |
| `--allow-private-network` | Allow `http-request` to reach private / loopback addresses. |
| `--providers FILE` | Model providers file (YAML/JSON), layered over the environment providers. Defaults to `$GRAPHRT_PROVIDERS`. See below. |
| `--model PROVIDER/NAME` | Use this model for every `llm`, `question-classifier`, `parameter-extractor` and `agent` node, e.g. `--model ollama/qwen2.5:14b`. The name may contain `/`. |
| `--allow-python-subprocess` | Run `python3` code nodes in a local subprocess (isolated interpreter, empty environment, temp working dir, CPU/memory/file-size `ulimit`s, timeout). **Not a security sandbox** — only for trusted workflows. |
| `--timeout-ms MS` | Abort the run after this long. |
| `--max-steps N` | Maximum node executions. |
| `--resume SNAPSHOT` | Resume a paused run. |
| `--resume-input node=json` | Input for a paused node, e.g. `review={"action":"approve","values":{}}`. Repeatable. |
| `--snapshot-out FILE` | Where to write the snapshot on pause (default `<workflow>.snapshot.json`). |
| `--plugins DIR` | Plugin directory: one plugin, a directory of plugins, or a `.wasm` module. Searched before `$GRAPHRT_PLUGIN_PATH` and `~/.graphrt/plugins`. Repeatable. Also accepted by `serve`, `validate`, `inspect`, `nodes` and `plugin list/info`. |
| `--no-default-plugins` | Ignore `$GRAPHRT_PLUGIN_PATH` and `~/.graphrt/plugins`. |

Ctrl-C aborts the run and cancels in-flight nodes.

### Environment variables

Only variables declared in the workflow's `env` section are exposed to it,
taken from `--env-file` first, then the process environment.

Model providers are configured from the environment:

| Variables | Provider name in workflows |
| --- | --- |
| `OPENAI_API_KEY`, optional `OPENAI_BASE_URL` | `openai` |
| `ANTHROPIC_API_KEY`, optional `ANTHROPIC_BASE_URL` | `anthropic` |
| `OLLAMA_HOST` | `ollama` |
| `GRAPHRT_OPENAI_COMPAT_<NAME>_BASE_URL`, optional `..._API_KEY` | `<name>` |

### Providers file

`--providers` declares any number of endpoints without code. Keys are read from
the environment (`api_key_env`), never from the file; `${VAR}` in `base_url`,
`headers` and `query` values is substituted from the environment.

```yaml
providers:
  - name: openrouter
    kind: openai                     # openai (any compatible API) | anthropic
    base_url: https://openrouter.ai/api/v1
    api_key_env: OPENROUTER_API_KEY
    headers: { HTTP-Referer: "${SITE_URL}" }
    defaults: { temperature: 0.2, max_tokens: 1024 }   # used when a node sets none
  - name: azure
    kind: openai
    base_url: https://my.openai.azure.com/openai/deployments/gpt4o
    api_key_env: AZURE_OPENAI_KEY
    auth_header: api-key
    query: { api-version: "2024-06-01" }
  # Run workflows written for `openai/gpt-4o-mini` on a local model:
  - name: local
    kind: openai
    base_url: http://localhost:11434/v1
    api_key_env: OLLAMA_KEY
    api_key_optional: true
    aliases: [openai]                # `provider: openai` now resolves here
    models: { gpt-4o-mini: "qwen2.5:14b" }   # model-name aliases
```

Providers from the file win over environment providers with the same name,
and an alias shadows an environment provider of that name. Unknown keys are
errors.

## `graphrt serve`

```
graphrt serve <workflow-file|directory> [--addr 127.0.0.1:8080] [--api-key-env GRAPHRT_API_KEY]
              [--max-body-bytes N] [--env-file] [--mcp] [--allow-private-network]
              [--providers FILE] [--model PROVIDER/NAME] [--allow-python-subprocess]
              [--timeout-ms] [--max-steps]
```

Each workflow file is served under its file stem and compiled once at startup.
If the variable named by `--api-key-env` is set, every `/v1` request needs
`Authorization: Bearer <key>`.

| Method | Path | Body |
| --- | --- | --- |
| GET | `/healthz` | – |
| GET | `/v1/workflows` | – |
| POST | `/v1/workflows/{name}/runs` | `{"inputs": {}, "sys": {}, "conversation": {}, "stream": false}` |
| POST | `/v1/runs/{run_id}/abort` | – |
| POST | `/v1/runs/{run_id}/resume` | `{"snapshot": {...}, "inputs": {"<node>": {...}}, "secrets": {"ns.name": "..."}, "stream": false}` |
| GET | `/v1/nodes` | – (node catalog: `{"nodes": [descriptor...]}`) |
| POST | `/v1/validate` | a workflow document (JSON), or `{"document": "<YAML or JSON text>"}` → `{"valid", "errors", "warnings"}` |

Blocking responses:

```json
{"run_id": "...", "status": "succeeded", "outputs": {}, "answer": "...", "usage": {}, "handled_errors": 0, "elapsed_ms": 12}
{"run_id": "...", "status": "paused", "reasons": [...], "snapshot": {...}}
{"run_id": "...", "status": "failed", "error": "..."}
```

With `"stream": true` the response is an SSE stream. Each engine event is sent
with its `type` as the event name (`run_started`, `node_started`,
`node_chunk`, `node_succeeded`, …, `run_succeeded` / `run_paused` /
`run_failed` / `run_aborted`). `response` events carry the ordered answer text:
`{"node_id": "answer", "text": "...", "is_final": false}`.

Closing the connection cancels the run.

## `graphrt nodes`

Prints the node catalog: every node kind compiled into this binary with its
category, source crate and title. `--kind K` shows one kind's outputs, branch
handles and config fields; `--json` prints full descriptors including the
config JSON Schema (draft 2020-12), as served by `GET /v1/nodes`. Visual
builders use the catalog to render node palettes and config forms.

Plugin kinds are included (their `source` is `plugin:<name>`) for every plugin
found in the plugin search paths; see [`graphrt plugin`](#graphrt-plugin).

## `graphrt plugin`

Plugins add node kinds and tools without recompiling graphrt. They are found in,
in order: `--plugins <dir>` flags, `$GRAPHRT_PLUGIN_PATH` (`:`-separated) and
`~/.graphrt/plugins`. A plugin that fails to load is skipped with a warning, so
one broken plugin never blocks unrelated workflows.

| Command | Description |
| --- | --- |
| `graphrt plugin list [--json]` | Every plugin found: name, version, runtime, node kinds, tools, and load errors. |
| `graphrt plugin install <dir\|git-url> [--rev R] [--dir D]` | Copy a plugin into `~/.graphrt/plugins/<name>` (or `D`), verify its checksums and check that it loads. Git sources (`https://…`, `git@…`, `*.git`) are cloned with the `git` executable; `--rev` picks a branch or tag. |
| `graphrt plugin remove <name> [--dir D]` | Delete an installed plugin. |
| `graphrt plugin info <name> [--json]` | Manifest summary and node descriptors. |
| `graphrt plugin new <name> --lang python\|typescript\|rust [--dir D]` | Scaffold a working plugin (manifest, code, vendored SDK, `example.yaml`). |

```sh
graphrt plugin new acme --lang python
graphrt run acme/example.yaml --plugins acme --input who=World   # try it in place
graphrt plugin install ./acme                                    # make it available everywhere
graphrt nodes --kind acme.greet
```

Workflows that use a kind no loaded plugin provides fail validation with a hint
pointing at `graphrt plugin install` and `--plugins`.
