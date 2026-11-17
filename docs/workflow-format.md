# Workflow format

A workflow is a JSON or YAML document. YAML is parsed with YAML 1.2 boolean
rules: only `true` / `false` are booleans, so `yes`, `no`, `y`, `n` stay strings.

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

The loader also accepts nodes in the `{id, parentId, data: {type, title, ...}}`
layout, camelCase `sourceHandle` on edges, and documents nested under a
top-level `workflow` key (with `environment_variables` /
`conversation_variables`).

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
    - { label: draft, var_type: string, value_type: constant, value: "" }
    - { label: source, var_type: string, value_type: variable, value: [start, text] }
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
```

Pauses the run with kind `human_input`. Resume with
`{"action": "approve", "values": {"note": "..."}}`; the node outputs the values
and `action`, and follows the edges whose handle is the action id.

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
memory: { window: { enabled: true, size: 10 }, variable_selector: [sys, history] }
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
```

Runs a tool-calling loop until the model answers without tools (the last
allowed iteration runs without tools). Tools in `require_approval` pause the run
(kind `tool_approval`); resume with `{"approved": true}` or
`{"approvals": {"<call id>": true}}`. Outputs `text`, `tool_calls`,
`iterations`, `usage`.

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
```

JSON bodies insert values JSON-escaped (inside strings) or as typed JSON
(outside strings); invalid JSON fails the node. Outputs `status_code`, `body`,
`headers`, `json` (for JSON responses) and `files` (small binary responses).

### `code`

```yaml
code_language: javascript       # python3 requires a remote sandbox service
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
