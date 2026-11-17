# The `graphrt` CLI

```
graphrt run <workflow>       Run a workflow and stream its output
graphrt validate <workflow>  Parse and validate without running
graphrt inspect <workflow>   Show nodes, inputs, env variables and missing node kinds
graphrt serve <file|dir>     HTTP + SSE API
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
| `--timeout-ms MS` | Abort the run after this long. |
| `--max-steps N` | Maximum node executions. |
| `--resume SNAPSHOT` | Resume a paused run. |
| `--resume-input node=json` | Input for a paused node, e.g. `review={"action":"approve","values":{}}`. Repeatable. |
| `--snapshot-out FILE` | Where to write the snapshot on pause (default `<workflow>.snapshot.json`). |

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

## `graphrt serve`

```
graphrt serve <workflow-file|directory> [--addr 127.0.0.1:8080] [--api-key-env GRAPHRT_API_KEY]
              [--max-body-bytes N] [--env-file] [--mcp] [--allow-private-network]
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
| POST | `/v1/runs/{run_id}/resume` | `{"snapshot": {...}, "inputs": {"<node>": {...}}, "stream": false}` |

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
