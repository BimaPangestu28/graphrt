# The `graphrt` CLI

```
graphrt run <workflow>       Run a workflow and stream its output
graphrt validate <workflow>  Parse and validate without running (every error, with positions)
graphrt schema               JSON Schema of workflow documents for this build's node kinds
graphrt inspect <workflow>   Show nodes, inputs, env variables and missing node kinds
graphrt nodes [--json] [--kind K]  The node catalog: built-ins plus plugins (with config JSON Schemas)
graphrt serve <file|dir>     HTTP + SSE API
graphrt eval <file.eval.yaml>  Evaluate a workflow against test cases; gate CI on the pass rate
graphrt kb ingest|query      Fill and query a knowledge base
graphrt plugin list|install|search|remove|info|new   Manage plugins
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
| 5 | `graphrt eval`: the gate failed (pass rate below `gate.pass_rate`); configurable with `--fail-exit-code` |

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
| `--session-id ID`, `--user-id ID`, `--tag TAG`, `--metadata key=value` | Caller context stamped on every OpenTelemetry span of the run (`session.id`, `user.id`, `langfuse.*`); see [observability](observability.md#sessions-users-tags-and-metadata). `--tag` and `--metadata` repeat. |
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

A `kind: replay` entry serves recorded responses ("cassettes") instead of
calling a model, for deterministic, offline runs; `mode: record` or `auto`
records them from an `upstream` provider. See
[evals.md](evals.md#replay-deterministic-model-responses).

## `graphrt eval`

Runs a workflow against the test cases of an eval file (Promptfoo-shaped
`tests` / `assert`, plus an optional JSONL dataset), prints a line per test as
it finishes and a summary, and exits with 5 when the pass rate is below the
file's `gate.pass_rate`. The workflow is compiled once and the tests run
in-process, `-j` at a time.

| Flag | Description |
| --- | --- |
| `-j, --concurrency N` | Tests running at the same time (default 4). |
| `--filter PATTERN` | Only tests whose id or a tag matches (`*` wildcards). Repeatable. |
| `--repeat N` | Run every test N times. |
| `-o, --output FILE` | JSON results (`schema_version: 1`). |
| `--junit FILE` | JUnit XML. |
| `--pass-rate RATE` | Override the gate. |
| `--fail-exit-code CODE` | Exit code of a failed gate (default 5). |
| `--include-events` | Add the run events to the JSON results. |
| `--timeout-ms`, `--providers`, `--model`, `--knowledge`, `--mcp`, `--plugins`, `--env-file`, ... | As for `run`; `--timeout-ms` applies per test. |

See [evals.md](evals.md) for the file format, the assertion types, replay
cassettes, CI and Promptfoo interop.

## `graphrt serve`

```
graphrt serve <workflow-file|directory> [--addr 127.0.0.1:8080] [--api-key-env GRAPHRT_API_KEY]
              [--max-body-bytes N] [--env-file] [--mcp] [--allow-private-network]
              [--providers FILE] [--model PROVIDER/NAME] [--allow-python-subprocess]
              [--timeout-ms] [--max-steps] [--input-header INPUT=HEADER]...
              [--verify-jwt INPUT [--jwt-jwks-url URL] [--jwt-secret-env VAR]
               [--jwt-issuer ISS] [--jwt-audience AUD] [--user-rate-limit N]]
```

Each workflow file is served under its file stem and compiled once at startup.
If the variable named by `--api-key-env` is set, every `/v1` request needs
`Authorization: Bearer <key>`.

`--input-header user_jwt=X-User-Authorization` passes a per-user credential from
a request header (leading `Bearer ` stripped) to the start input `user_jwt` of
new and resumed runs, as a secret. The input must be declared `type: secret`;
a missing header for a required input is `401`. See
[serving.md](serving.md#per-user-credentials-from-headers).

`--verify-jwt user_jwt` also verifies that header as a JWT (JWKS with
`--jwt-jwks-url`, or an HS256 secret with `--jwt-secret-env`; `--jwt-issuer`,
`--jwt-audience`) and lets a valid token start runs without the API key; an
invalid one is `401`. `--user-rate-limit N` caps runs per minute per token
`sub` (`429` with `Retry-After`). See
[serving.md](serving.md#verifying-end-user-jwts).

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

## `graphrt validate`

Checks a workflow without running it and reports every problem at once, not
just the first. `graphrt run` performs the same checks before running.

```
error[unknown-field]: unknown field `templat` for node type `template-transform`
  --> hello.yaml:16:7
   |
16 |       templat: "Hello, {{ name }}"
   |       ^^^^^^^ did you mean `template`?
   |
   = at: graph.nodes[1].templat
```

Each finding has a code (`unknown-field`, `missing-field`, `unknown-node`,
`unknown-node-type`, `unknown-reference`, `unknown-handle`, `deprecated`, ...),
the document path, the position and, when a known name is close, a
suggestion. Warnings (deprecated spellings, unreachable nodes, dead edges) do
not fail validation.

`--format json` prints one report on stdout, for editors and builder apps:

```json
{
  "file": "hello.yaml",
  "valid": false,
  "errors": 1,
  "warnings": 0,
  "diagnostics": [
    {
      "severity": "error",
      "code": "unknown-field",
      "message": "unknown field `templat` for node type `template-transform`",
      "path": "graph.nodes[1].templat",
      "line": 16,
      "column": 7,
      "length": 7,
      "node": "greet",
      "suggestion": "template"
    }
  ]
}
```

`line`, `column` (1-based, in characters) and `length` are absent when the
position is unknown; `nodes` and `edges` counts are added when the document
is valid. The exit code is 2 when there are errors.

## `graphrt schema`

Prints the JSON Schema (draft 2020-12) of workflow documents for the node
kinds in this binary, including plugins found in the plugin search paths
(`--plugins`, `--no-default-plugins`). The repository ships the schema of the
default build as `schema/workflow-v1.schema.json`
(`graphrt schema --no-default-plugins`); point your editor at a generated
file to get completion for your plugins' node kinds too. See
[editor setup](workflow-format.md#editor-setup).

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
| `graphrt plugin install <dir\|git-url> [--rev R] [--subdir P] [--dir D]` | Copy a plugin into `~/.graphrt/plugins/<name>` (or `D`), verify its checksums and check that it loads. Git sources (`https://…`, `git@…`, `*.git`) are cloned with the `git` executable; `--rev` picks a branch or tag. |
| `graphrt plugin install <name>[@version] [--index I] [--yes] [--dir D]` | Install from the plugin catalog: the latest version, or the exact one. The tarball is downloaded over https, checked against the catalog's sha256, extracted safely and installed like a directory. Plugins not labelled stable/beta with a none/low/medium ToS risk (including unlabelled ones) need `--yes`. |
| `graphrt plugin search [query] [--index I]` | Catalog plugins whose name or description contains `query`: name, latest version, status and a one-line description. |
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

### The plugin catalog

`graphrt plugin install <name>` and `graphrt plugin search` read the
[graphrt-plugins](https://github.com/BimaPangestu28/graphrt-plugins) catalog
(`index.json` of its `catalog` release). A source that is not an existing
directory and looks like `name` or `name@1.2.3` is a catalog name.

| Option | Description |
| --- | --- |
| `--index URL\|PATH` | Catalog index: an `https://` URL, a `file://` URL or a local path. Overrides `$GRAPHRT_PLUGIN_INDEX`; the default is the graphrt-plugins `catalog` release. Plain `http://` is refused. |
| `--yes`, `-y` | Install a plugin that is not labelled status `stable`/`beta` with ToS risk `none`/`low`/`medium`: `experimental`, ToS risk `high`, an unrecognised label, or no label at all. Without it such installs stop with exit code 2 (graphrt has no interactive prompt). |

`--index` and `--yes` only apply to catalog names: with a directory or git
source they are usage errors (exit code 2) rather than silently ignored.

```sh
graphrt plugin search weather
graphrt plugin install open-meteo           # latest version
graphrt plugin install open-meteo@0.1.0     # exact version
graphrt plugin install whatsapp-web --yes   # experimental, ToS risk high (or unlabelled)
GRAPHRT_PLUGIN_INDEX=/srv/mirror/index.json graphrt plugin install open-meteo
```

The plugin's status and ToS risk are printed before anything is downloaded.
Downloads are https-only with a timeout and a size cap, the tarball must match
the catalog's sha256, and extraction refuses absolute paths, `..`, links and
device files. See [Installing from the plugin catalog](plugins.md#installing-from-the-plugin-catalog).
Unknown names and versions are usage errors (exit code 2); download, checksum
and extraction failures exit with 1.
