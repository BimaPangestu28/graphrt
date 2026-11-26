# Workflow format

A workflow is a JSON or YAML document. YAML is parsed with YAML 1.2 boolean
rules: only `true` / `false` are booleans, so `yes`, `no`, `y`, `n` stay strings.

This page describes **format version 1**, which is frozen (see
[Stability](#stability)). Its JSON Schema is
[`schema/workflow-v1.schema.json`](../schema/workflow-v1.schema.json); see
[editor setup](#editor-setup).

```yaml
version: "1"                 # document format version
name: my-workflow            # optional
description: ...             # optional
env:                         # optional: variables read as {{#env.NAME#}}
  - { name: API_TOKEN, type: secret }            # value supplied by the host
  - { name: REGION, type: string, value: eu-west-1 }
conversation:                # optional: variables read as {{#conversation.NAME#}}
  - { name: summary, type: string, value: "" }
graph:
  nodes: [ ... ]
  edges: [ ... ]
```

`version` may be omitted (it means `"1"`); any other value is rejected, so a
document written for a newer format fails clearly instead of half-working.

### Strictness

v1 documents are strict: an unknown key anywhere (document, `graph`, node,
edge, `retry`, `on_error`, variable declarations, node config) is an error,
reported with a "did you mean" suggestion. Node configs are checked against
the node kind's catalog schema (`graphrt nodes --kind <kind> --json`). A
plugin node whose config schema does not set `additionalProperties: false`
accepts any config key.

Keys starting with `x-` are free for tools (editor state, node positions,
comments) at every level that has fixed keys: the document, `graph`, nodes and
edges. The runtime ignores them; they are not part of the run fingerprint and
are not kept when graphrt re-serializes a document. The builder keys `desc`,
`selected`, `position`, `positionAbsolute`, `width`, `height`,
`sourcePosition`, `targetPosition`, `zIndex`, `isInIteration` and `isInLoop`
are ignored on nodes too; new tools should use `x-` keys.

### Import layout

The loader also accepts nodes in the `{id, parentId, data: {type, title, ...}}`
layout, camelCase `sourceHandle` on edges, and documents nested under a
top-level `workflow` key (with `environment_variables` /
`conversation_variables`). Documents with a top-level `workflow` or `app` key,
and nodes wrapped in `data`, are read leniently: unknown keys are ignored, so
documents exported by other tools load unchanged. The JSON Schema describes
the strict layout only.

### Deprecated spellings

Accepted, with a warning from `graphrt validate`:

| Deprecated | Use |
| --- | --- |
| `environment_variables`, `conversation_variables` | `env`, `conversation` |
| `value_type` in `env` / `conversation` declarations | `type` |
| node `parentId`, `container_id`, `iteration_id`, `loop_id` | `parent` |
| node `error_strategy`, `default_value` | `on_error` |
| node `retry_config` | `retry` |
| edge `sourceHandle`, `source_handle` | `handle` |
| loop variable `label`, `var_type`, `value_type` | `name`, `type`, `input_type` |

### Reserved names

These keys are reserved for future versions and rejected today: top-level
`imports`, `inputs`, `outputs`, `triggers`, `metadata`, `requires`,
`subgraphs`; node-level `disabled`, `cache`, `metadata` (node configs,
including plugin configs, cannot use them).

## Value types

`string`, `secret`, `number`, `integer`, `float`, `boolean`, `object`, `file`,
`array[any]`, `array[string]`, `array[number]`, `array[boolean]`,
`array[object]`, `array[file]`, `any`.

`secret` values behave like strings for nodes that need them (HTTP auth, code
inputs) but are masked (`******`) in events, logs and templates, and are never
written to snapshots.

## Referencing values

- Selectors: `[node_id, output]`, `[node_id, output, key, 0]` or `"node_id.output.key"`.
  Paths walk into objects (by key), arrays (by index, negative from the end,
  `length`) and files (`name`, `type`, `extension`, `mime_type`, `size`, `url`,
  `transfer_method`).
- Templates: `"Hello {{#start.name#}}"`. A missing value renders as empty
  text. A template that is exactly one placeholder keeps the value's type.
  Resolved values are never parsed again.
- Namespaces: node ids, `sys` (host-provided: `query`, `files`, `history`,
  `user_id`, `run_id`...), `env`, `conversation`.

### Conversation history (`sys.history`)

graphrt keeps no conversation state between runs: a chat host sends the
previous turns with every request, and `llm` / `agent` nodes with `memory`
read them. The contract:

```json
{
  "sys": {
    "query": "And on Friday?",
    "history": [
      { "role": "user", "content": "What's on my calendar Thursday?" },
      { "role": "assistant", "content": "A dentist appointment at 10:00." }
    ]
  }
}
```

- `sys.history` is an array of `{role, content}` objects, **oldest first**,
  holding the earlier turns only. The current message is **not** in it: it is
  `sys.query`, or a start input the node's prompt / `query` references.
- `role` is `user` (or `human`), `assistant` (or `ai`), or `system`; anything
  else (including `tool`) fails the node with `invalid_input`. `content` is
  text; other values are converted to text (objects and arrays as JSON).
- A missing, `null` or empty history means a fresh conversation. A JSON string
  holding such an array is also accepted (handy for `--sys history=...` on the
  command line); any other type is `invalid_input`.
- `memory: { window: { enabled: true, size: N } }` keeps only the last `N`
  turns (`2N` messages; `N` defaults to 10). Without a window every message is
  sent, so hosts should trim long chats themselves.
- `memory.variable_selector` reads the history from another variable (default
  `[sys, history]`).
- `llm`: the history goes after the system prompt, then the current query as a
  user message (`memory.query_prompt_template`, default `{{#sys.query#}}`; not
  added again when a prompt message already references `sys.query`). `agent`:
  instruction, history, then the node's `query`.
- The history is client input. A client can put anything in it, including
  `system` entries, so it can steer its own conversation; do not use the system
  prompt as an authorization boundary (enforce access in tools and data, as in
  [examples/supabase-chatbot](../examples/supabase-chatbot/)).

## Nodes

Every node has:

| Field | Description |
| --- | --- |
| `id` | Unique id (also the namespace of its outputs) |
| `type` | Node kind |
| `title` | Optional label |
| `parent` | Owning `iteration` / `loop` for nodes in a container body |
| `retry` | `{max_retries, interval_ms, backoff}`; applies to every node kind; only retryable errors are retried |
| `timeout_ms` | Per-attempt timeout |
| `on_error` | `{strategy: fail}` (default), `{strategy: fail-branch}`, or `{strategy: default-value, values: {...}}` |

Node configs cannot use these field names or the [reserved names](#reserved-names).

With `fail-branch`, failures follow edges whose `handle` is `fail-branch`;
outputs are `error_message` and `error_type`. With `default-value`, the node
outputs `values` plus `error_message` / `error_type` and continues normally.

## Edges

```yaml
- { source: a, target: b }                   # handle defaults to "source"
- { source: check, target: c, handle: urgent }
- { source: fetch, target: fallback, handle: fail-branch }
```

A node runs once all incoming edges are resolved and at least one was taken.
Branch nodes take only the edges matching their selected handle; other paths
are skipped.

A `handle` must be one the source node can select: `source` for non-branch
nodes, the case ids plus `false` for `if-else`, class ids for
`question-classifier`, action ids for `human-input`, the declared handles of
plugin nodes. `fail-branch` is always accepted; on a node without
`on_error: {strategy: fail-branch}` such an edge is never taken (a warning).

---

### `start`

```yaml
variables:
  - { variable: name, type: text-input, required: true, max_length: 64, label: Name }
  - { variable: tier, type: select, options: [free, pro], default: free }
  - { variable: count, type: number }
```

Types: `text-input`, `paragraph`, `select`, `number`, `checkbox`, `json`,
`file`, `file-list`, or any value type name. Inputs are validated, converted
(`"42"` → `42` for numbers) and output under the start node's id.

### `end`

```yaml
outputs:
  - { variable: summary, value_selector: [llm, text], value_type: string }
```

The run's outputs.

### `answer`

```yaml
answer: "Here you go: {{#llm.text#}}"
```

Output `answer`. LLM tokens referenced by the template stream to the user in
template order.

### `if-else` (branch)

```yaml
cases:
  - case_id: vip
    logical_operator: and          # and | or
    conditions:
      - { variable_selector: [start, tier], comparison_operator: is, value: pro }
      - { variable_selector: [start, count], comparison_operator: "≥", value: "10" }
```

Selects the first matching `case_id`, otherwise handle `false`.

Operators: `contains`, `not contains`, `start with`, `end with`, `is`,
`is not`, `empty`, `not empty`, `in`, `not in`, `all of`, `=`, `≠`, `>`, `<`,
`≥`, `≤`, `null`, `not null`, `exists`, `not exists` (ASCII aliases `==`, `!=`,
`>=`, `<=`, `starts with`, `ends with`). Missing variables count as null.
Numbers compare exactly (no truncation). `value` may contain `{{#..#}}`. For
file lists, `sub_variable_condition: {logical_operator, conditions: [{key, comparison_operator, value}]}`
checks file attributes.

### `template-transform`

```yaml
template: "{% for x in items %}- {{ x }}\n{% endfor %}"
variables:
  - { variable: items, value_selector: [start, items] }
```

Jinja2 (sandboxed: no file access, fuel-limited, 1 MiB output cap). Output `output`.

### `variable-aggregator`

```yaml
variables: [[branch_a, output], [branch_b, output]]
# or grouped:
advanced_settings:
  group_enabled: true
  groups:
    - { group_name: text, variables: [[a, text], [b, text]] }
```

Outputs the first non-null value as `output` (or `{group_name: {output}}`).

### `assigner`

```yaml
items:
  - { variable_selector: [conversation, notes], operation: append, input_type: variable, value: [llm, text] }
  - { variable_selector: [conversation, turns], operation: "+=", input_type: constant, value: 1 }
```

Operations: `over-write`, `clear`, `set`, `append`, `extend`, `remove-first`,
`remove-last`, `+=`, `-=`, `*=`, `/=`. All items succeed or none are applied.
The legacy single-assignment shape (`assigned_variable_selector`,
`input_variable_selector`, `write_mode`) is accepted too, also under the kind
name `variable-assigner`.

### `list-operator`

```yaml
variable: [start, products]
filter_by: { enabled: true, conditions: [{ key: price, comparison_operator: "<", value: "50" }] }
order_by: { enabled: true, key: price, value: asc }
limit: { enabled: true, size: 3 }
extract_by: { enabled: false, serial: "1" }
```

Outputs `result`, `first_record`, `last_record`.

### `iteration` (container)

```yaml
- id: each
  type: iteration
  iterator_selector: [start, items]
  output_selector: [work, result]
  is_parallel: true
  parallel_nums: 5                   # max 100
  error_handle_mode: terminated      # terminated | continue-on-error | remove-abnormal-output
  flatten_output: true
- { id: each_start, type: iteration-start, parent: each }
- { id: work, type: llm, parent: each, ... }   # reads {{#each.item#}} and {{#each.index#}}
```

Output `output`: one result per item, in input order.

### `loop` (container)

```yaml
- id: refine
  type: loop
  loop_count: 5
  loop_variables:
    - { name: draft, type: string, input_type: constant, value: "" }
    - { name: source, type: string, input_type: variable, value: [start, text] }
  break_conditions:
    - { variable_selector: [refine, draft], comparison_operator: contains, value: DONE }
  logical_operator: and
- { id: refine_start, type: loop-start, parent: refine }
- { id: stop, type: loop-end, parent: refine }     # optional: reaching it breaks the loop
```

Body nodes update loop variables with an `assigner` targeting `[refine, draft]`.
Outputs: the loop variables and `loop_round`.

### `human-input` (branch, pauses)

```yaml
prompt: "Approve {{#start.amount#}}?"
form:
  fields:
    - { name: note, label: Comment, type: text, required: false }
actions:
  - { id: approve, title: Approve }
  - { id: reject, title: Reject }
expire_after_ms: 86400000      # optional; reported to the host as `timeout_ms`
card: "{{#review_card.data#}}"   # optional: a UI block (see "UI blocks") shown with the question
```

Pauses the run with kind `human_input`. Resume with
`{"action": "approve", "values": {"note": "..."}}`; the node outputs the values
and `action`, and follows the edges whose handle is the action id.

With `card`, the pause data also has `card`: the UI block itself (inline or the
referenced value; `null` when it is not a block). Hosts that render it answer
with the card action's id as `action` and the card's input values as `values`,
so the card's submit actions use the node's action ids; leave `form` empty so
every card input becomes an output.

### `tool`

```yaml
provider_id: mcp:github          # a ToolRegistry provider
tool_name: create_issue
tool_parameters:
  title: { type: mixed, value: "Bug: {{#start.title#}}" }
  repo: { type: constant, value: acme/app }
  labels: { type: variable, value: [classify, labels] }
ignore_tool_errors: false
```

Outputs `text`, `json`, `files`.

### `llm`

```yaml
model:
  provider: openai               # a ModelRegistry provider name
  name: gpt-4o-mini
  completion_params: { temperature: 0.2, max_tokens: 800 }
prompt_template:
  - { role: system, text: "You are helpful. Context: {{#context#}}" }
  - { role: user, text: "{{#sys.query#}}" }
context: { enabled: true, variable_selector: [search, text] }
vision: { enabled: true, variable_selector: [sys, files] }
memory: { window: { enabled: true, size: 10 }, variable_selector: [sys, history] }   # see "Conversation history"
structured_output_enabled: true
structured_output: { schema: { type: object, properties: { score: { type: number } } } }
```

Streams `text` (and `reasoning`). Outputs `text`, `reasoning`, `usage`,
`finish_reason`, and `structured_output` when enabled. `{{#context#}}` inserts
the context value as data.

### `agent`

```yaml
model: { provider: anthropic, name: claude-sonnet-4-5 }
instruction: "You are a support agent for {{#env.COMPANY#}}."
query: "{{#sys.query#}}"
tools: [crm/*, mcp:github/create_issue]   # or [{provider, name}]
max_iterations: 10
max_tool_calls_per_turn: 8
parallel_tool_calls: true
require_approval: [mcp:github/create_issue]   # or ["*"]
memory: { window: { enabled: true, size: 20 } }
keep_tool_images: 3   # optional (default 3): images of only the last 3 image-bearing tool results; null keeps all
stream_tool_turns: true   # optional (default true): stream every turn's text per token; false buffers turns that may call tools
tool_context:         # optional: per-run values sent to a provider's tools outside the arguments
  crm: { user_jwt: "{{#start.user_jwt#}}" }
skills: [refund-policy]   # optional (preview): installed skills the agent loads on demand
```

Runs a tool-calling loop until the model answers without tools (the last
allowed iteration runs without tools). Tools in `require_approval` pause the run
(kind `tool_approval`); resume with `{"approved": true}` or
`{"approvals": {"<call id>": true}}`. Tool results can include images, which
the model sees (see [tool-images.md](tool-images.md)). Outputs `text`,
`tool_calls`, `ui`, `iterations`, `usage`. `ui` lists the UI blocks (see
"UI blocks") found in the structured data of successful tool results; they are
never sent to the model, so a tool can return a short text for the model and a
chart or card for the person.

Streams `text` (and `reasoning`) token by token on every turn, including turns
that may still call tools, so the answer reaches the client while it is
generated. Tool-call arguments are never streamed as text, and once a turn
starts a tool call the rest of its text is not streamed. Text a turn streams
before calling tools (a preamble such as "Let me look that up.") stays in the
answer: `text` is the preambles followed by the final answer, separated by a
blank line (streamed as its own `"\n\n"` chunk), so the streamed text always
equals the `text` output. Each tool-calling turn's full text is also reported
as an `agent_thought` progress event (`streamed: true` when part of it was
streamed). `stream_tool_turns: false` buffers tool-calling turns instead: only
the final answer is streamed (as one chunk unless it was the forced last turn)
and `text` is the final turn's text alone. See
[serving.md](serving.md#streaming-sse) for the SSE format.

`tool_context` passes per-run values, such as the end user's JWT, to the tools
of one provider without the model seeing them: they travel in the `context` of
each `tool.call`, never in the arguments, and are kept out of events, outputs,
snapshots and traces (any echo in a tool result is replaced with `******`).
Keys are tool providers that appear in `tools`; values map names to templates,
rendered with secrets in plaintext. Only plugins that request
`permissions.secrets` and are trusted (`--trust-plugin`) may receive it; any
other provider fails the node with `tool_context_denied`. See
[plugins.md](plugins.md#tool-call-context).

`skills` (preview) lists installed skills. The system prompt gets one line per
skill, the model loads a skill's instructions with the `load_skill` tool when a
task needs them, and tools the owner granted to a skill are offered only after
that skill was loaded. It needs a host with a skill store (the `skills` cargo
feature of the CLI). See [skills.md](skills.md).

### `question-classifier` (branch)

```yaml
model: { provider: openai, name: gpt-4o-mini }
query_variable_selector: [sys, query]
classes:
  - { id: billing, name: Billing questions }
  - { id: tech, name: Technical problems }
instruction: "Classify support messages."
fallback_class: tech           # optional; otherwise an unknown answer fails the node
```

Selects the class id as the branch handle. Outputs `class_id`, `class_name`, `usage`.

### `parameter-extractor`

```yaml
model: { provider: openai, name: gpt-4o-mini }
query: [sys, query]
parameters:
  - { name: city, type: string, description: Destination city, required: true }
  - { name: nights, type: number }
  - { name: class, type: select, options: [economy, business] }
reasoning_mode: function_call  # or prompt
```

Outputs each parameter plus `__is_success` and `__reason`.

### `http-request`

```yaml
method: post
url: "https://api.example.com/items/{{#start.id#}}"
headers: "Accept: application/json\nX-Trace: {{#sys.run_id#}}"   # or [{key, value}] or a map
params: [{ key: verbose, value: "true" }]
authorization:
  type: api-key                  # no-auth | api-key
  config: { type: bearer, api_key: "{{#env.API_TOKEN#}}" }   # bearer | basic | custom (+ header)
body:
  type: json                     # none | json | raw-text | form-data | x-www-form-urlencoded | binary
  data: '{"name": "{{#start.name#}}", "tags": {{#start.tags#}}}'
timeout: { connect: 5, read: 30, write: 30 }   # seconds
fail_on_error_status: true       # non-2xx fails the node (5xx / 429 are retryable)
max_response_bytes: 10485760
retry_non_idempotent: false
idempotency_key: true            # Idempotency-Key on POST / PUT / PATCH / DELETE
```

JSON bodies insert values JSON-escaped (inside strings) or as typed JSON
(outside strings); invalid JSON fails the node. Outputs `status_code`, `body`,
`headers`, `json` (for JSON responses) and `files` (small binary responses).

A node can run more than once: after a retry, and after a run that was
preempted (a shutdown drain, a lost gateway stream) resumes, when the node
that was in flight runs again. Execution is **at-least-once per node**. So
that the receiving service can apply a side effect once, POST, PUT, PATCH and
DELETE requests carry `Idempotency-Key: <run_id>:<exec_id>`: the same value
for every attempt of this node execution, including its re-execution after a
resume, and a different one for any other execution (another run, another loop
iteration). A header named `Idempotency-Key` in `headers` replaces it;
`idempotency_key: false` turns it off. Plugins receive the same key as
`idempotency_key` in `node.run` (see [plugins.md](plugins.md#noderun)).

### `code`

```yaml
code_language: javascript       # or python3 (see below)
code: |
  function main({ a, b }) {
    return { sum: a + b };
  }
variables:
  - { variable: a, value_selector: [start, a] }
  - { variable: b, value_selector: [start, b] }
outputs:
  sum: { type: number }
limits: { memory_bytes: 67108864, time_ms: 10000 }
```

`main` may be async. No filesystem, network, timers or modules. Declared
outputs are type-checked.

`python3` code defines `def main(**inputs)` returning a `dict`. It needs a
sandbox service registered by the host (`Arc<dyn RemoteSandbox>`): either a
remote sandbox (feature `remote`, `HttpSandbox`) or, for trusted workflows only,
the local subprocess runner (feature `python`, `SubprocessPython`; the CLI flag
`--allow-python-subprocess`). The subprocess runner isolates the interpreter
(`-I -S`, empty environment, temporary working directory) and applies CPU,
memory and file-size limits plus a timeout, but it is **not a security
sandbox**: the code runs with the host user's privileges.

```yaml
code_language: python3
code: |
  def main(items):
      return {"total": sum(i["qty"] for i in items)}
variables: [{ variable: items, value_selector: [start, items] }]
outputs: { total: { type: number } }
```

## Model providers

Model-backed nodes name a provider (`model.provider`) and a model
(`model.name`). Hosts register providers in a `ModelRegistry`: from the
environment (`ModelRegistry::from_env`) and/or a providers file
(`ProvidersConfig` + `ModelRegistry::from_config`; see `docs/cli.md` for the
format). Provider aliases and model-name aliases let a workflow written for one
provider run on another without editing it.

## Node catalog

Every node kind publishes a descriptor (title, category, outputs, branch
handles, config JSON Schema). Run `graphrt nodes --kind <kind> --json` or
`GET /v1/nodes` to read them, or `Registry::descriptors()` when embedding.
Descriptors list the handles a node selects itself; the `fail-branch` handle
comes from the node policy (`on_error: {strategy: fail-branch}`), is available
on every node kind and is therefore not listed.

## Editor setup

The schema gives completion, hover docs and inline errors for every node kind
and config field.

**VS Code** with the [YAML extension](https://marketplace.visualstudio.com/items?itemName=redhat.vscode-yaml)
(Red Hat): put this line at the top of a workflow file:

```yaml
# yaml-language-server: $schema=https://raw.githubusercontent.com/Graphrt/graphrt/main/schema/workflow-v1.schema.json
```

or map files to the schema in `.vscode/settings.json`:

```json
{
  "yaml.schemas": {
    "https://raw.githubusercontent.com/Graphrt/graphrt/main/schema/workflow-v1.schema.json": ["workflows/**/*.yaml"]
  },
  "json.schemas": [
    { "fileMatch": ["workflows/**/*.json"], "url": "https://raw.githubusercontent.com/Graphrt/graphrt/main/schema/workflow-v1.schema.json" }
  ]
}
```

**JetBrains IDEs** (IntelliJ IDEA, PyCharm, RustRover, ...): the
`# yaml-language-server: $schema=...` comment is honoured as well. To map a
directory instead, open *Settings → Languages & Frameworks → Schemas and DTDs
→ JSON Schema Mappings*, add a mapping with the schema URL above (or a local
file), *Schema version: JSON Schema version 2020-12*, and add your workflow
files or a glob such as `workflows/*.yaml`.

**Plugins.** The published schema covers the built-in node kinds of the
default build. For plugin node kinds, generate a schema for your setup and
point the editor at the file:

```sh
graphrt schema --plugins ./plugins > .graphrt/workflow.schema.json
```

`graphrt validate` performs the same checks and more (node ids in edges and
selectors, handles, required inputs), so run it in CI even with editor
support.

## Stability

Format version 1 is frozen. Within v1:

**Frozen** (a v1 document that is valid today stays valid and means the same
thing in every later release that reads v1):

- the document structure: `version`, `name`, `description`, `env`,
  `conversation`, `graph.nodes`, `graph.edges`;
- the common node fields `id`, `type`, `title`, `parent`, `retry`,
  `timeout_ms`, `on_error` and their semantics, edge fields `id`, `source`,
  `target`, `handle`, and the `source` / `fail-branch` handles;
- selector and template syntax (`[node, output, ...]`, `"node.output"`,
  `{{#node.output#}}`), the `sys`, `env` and `conversation` namespaces, value
  type names;
- built-in node types, their config fields and output names as documented on
  this page; deprecated spellings keep working within v1;
- the `x-` extension prefix and the reserved names.

**May still change** within v1, without a new format version:

- additions: new optional fields, new node types, new outputs, new enum values
  (comparison operators, body types...), new `sys` variables. Old documents
  stay valid; a document using an addition needs a graphrt that knows it;
- error messages, diagnostic codes' wording and the set of warnings;
- the behaviour of documents that are invalid today (they may become valid
  through additions, never the other way round);
- the import layout, which follows the tools it imports from;
- plugin node configs, which are owned by their plugins.

Removing or renaming a field, changing what a field means, or making a valid
document invalid needs format version `"2"`; graphrt will then keep reading v1
documents for at least one minor release.

## UI blocks

Any node output or tool result can carry rich visual output for a host to
render: an object whose `"$ui"` key names its kind.

```json
{"$ui": "chart", "version": 1, "type": "bar", "title": "Sales by month",
 "x": {"values": ["Jan", "Feb", "Mar"]},
 "series": [{"name": "2026", "values": [120, 150, 90]}]}
```

The engine passes blocks through as ordinary values;
`graphrt_core::ui::collect_ui_blocks` finds them in an output (the value
itself, or up to three levels down in arrays and objects). Kinds, JSON Schemas
and validation (`adaptive_card`, `chart`, `table`, `stats`) are in the
`graphrt-ui` crate: hosts validate every block there before rendering it and
show nothing for blocks that fail. Blocks reach the host as node outputs, the
`agent` node's `ui` output, and the `human-input` node's `card`.

