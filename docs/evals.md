# Evaluating workflows

`graphrt eval` runs a workflow against a set of test cases, checks each run with
assertions, and gates CI on the pass rate. The same harness is a Rust library,
[`graphrt-eval`](../crates/graphrt-eval), for `cargo test`.

```sh
graphrt eval examples/evals/hello.eval.yaml
graphrt eval evals/support.eval.yaml -j 8 --output results.json --junit junit.xml
```

```
graphrt eval: examples/evals/branching.eval.yaml (5 test case(s), workflow examples/branching.yaml, -j 4)
  PASS  boundary-8-is-urgent               0 ms  score 1.00
  PASS  urgent                             0 ms  score 1.00
  ...
5 test(s): 5 passed, 0 failed, 0 error(s); pass rate 100.0% (gate 100.0%) in 1 ms, 0 tokens
gate: PASSED
```

What it is for: deterministic checks you can run on every commit, as the CI
layer below LLM-judged metrics. Answers, structured outputs, latency and token
budgets, the tools an agent called and in what order, and whether retrieval
found the labelled documents. Model responses can be replayed from recorded
cassettes (see [Replay](#replay-deterministic-model-responses)), so an agent
suite runs offline, for free, with the same result every time.

The workflow is loaded and compiled **once**; every test runs in-process on
the same engine and host services, up to `-j` tests at a time. The harness only
reads the run's event stream: it adds nothing to the engine's hot path, and an
evaluated run behaves exactly like a production run.

## The eval file

Eval configuration lives in its own file, never in the workflow (workflow
format v1 is frozen, and `x-` keys are dropped at parse time). The `tests` /
`assert` / `defaultTest` shape is Promptfoo's, so test cases move between the
two tools. JSON Schema:
[`schema/eval-v1.schema.json`](../schema/eval-v1.schema.json).

```yaml
# yaml-language-server: $schema=https://raw.githubusercontent.com/BimaPangestu28/graphrt/main/schema/eval-v1.schema.json
description: Support agent regression suite
workflow: ../workflows/support.yaml       # paths are relative to this file
providers: providers.replay.yaml          # optional: model providers (e.g. replay)
knowledge: kb.yaml                        # optional: knowledge base config
mcp: mcp.json                             # optional: MCP servers
plugins: [plugins]                        # optional: plugin directories
dataset: cases.jsonl                      # optional: more test cases, one per line

defaultTest:                              # merged into every test
  vars: { locale: en }                    # default inputs
  assert:                                 # prepended to every test's assertions
    - { type: latency, threshold: 5000, weight: 0 }

tests:
  - id: refund-window                     # default: test-<n>
    description: Refund policy question
    tags: [refund, smoke]
    vars: { question: "How long is the refund window?" }   # the workflow's start inputs
    threshold: 0.8                        # optional: pass on weighted score instead
    assert:
      - { type: contains, value: "30 days" }
      - { type: trajectory:tool-used, value: search_policies }
      - { type: tokens, threshold: 4000, weight: 0 }

gate:
  pass_rate: 0.95                         # default 1.0
```

| Key | Meaning |
| --- | --- |
| `workflow` | The workflow under test (required). |
| `providers`, `knowledge`, `mcp`, `plugins` | Host configuration, as the `run` flags of the same names. Flags on the command line win. |
| `dataset` | JSONL file: one `{id?, vars, expected?, tags?, metadata?, assert?, description?}` object per line. Blank lines and `//` lines are skipped. Rows without an `id` are named `<file stem>:<line>`. |
| `defaultTest` | `vars` and `metadata` used as defaults, `tags` and `assert` prepended, `threshold` inherited. |
| `tests[]` | Inline test cases (run before the dataset rows). |
| `gate.pass_rate` | Minimum fraction of passing tests; below it the command exits with code 5. |

Keys starting with `x-` are ignored at the file and test level; any other
unknown key is an error, so typos do not silently disable checks.

`expected`: a test (or dataset row) without its own `assert` list but with an
`expected` value gets `equals: <expected>` (Promptfoo's `__expected`). The
value is also available to assertions as `{{expected}}`.

### Variables in assertion values

Strings in `value` (and other assertion keys) may reference the test:
`{{name}}` (a var, as in Promptfoo), `{{vars.name}}`, `{{expected}}`,
`{{metadata.key}}`, `{{id}}`, `{{tags}}`. A string that is a single reference
becomes the referenced value itself, so a list in `metadata` can feed a
retrieval assertion:

```yaml
- { type: retrieval:recall-at-k, value: "{{metadata.relevant}}", k: 3 }
```

### Scoring

Each assertion yields a `score` in 0..1, `pass`, a `reason` and an optional
`metric` name. A test's score is the weighted mean of its assertion scores.

- Without a test `threshold`, every assertion with a non-zero weight must pass.
- With a `threshold`, the weighted score must reach it.
- `weight: 0` reports an assertion (in the results and the metric summary)
  without failing the test. Use it for efficiency signals such as token use.
- A run that fails, aborts, pauses or times out makes the test an **error**
  (it counts as not passed).

The summary aggregates every assertion under its `metric` (or its type):
count, passed, mean score.

## Assertions

Any type can be negated with a `not-` prefix (`not-contains`,
`not-trajectory:tool-used`): pass flips and the score becomes `1 - score`.

By default an assertion checks the run's **output**: the streamed answer when
the workflow has answer nodes, else the single output of the end node, else
the outputs object. `transform` selects something else with a path (no
JavaScript): `output.field`, `outputs.text`, `outputs.items[0].name`,
`answer`, `usage.total_tokens`, `vars.question`. Strings holding JSON are
parsed on the way, so `output.score` works when the output is JSON text.

### Output

| Type | `value` | Passes when |
| --- | --- | --- |
| `equals` | string or any JSON | The output equals it. Strings compare as text; other values as JSON (numbers by value, so `1 == 1.0`). |
| `contains`, `icontains` | string | The output contains it (`i`: case-insensitive). |
| `contains-any`, `icontains-any` | list (or comma-separated string) | At least one is contained. |
| `contains-all`, `icontains-all` | list (or comma-separated string) | All are contained (score: fraction found). |
| `starts-with` | string | The output starts with it. |
| `regex` | pattern | The output matches (Rust `regex` syntax; `(?i)` for case-insensitive). |
| `is-json` | optional JSON Schema | The output is JSON (and matches the schema, if given). |
| `json-schema` | JSON Schema | The output is JSON matching the schema. |

The JSON Schema subset is the one `llm` structured output uses (`type`,
`properties`, `required`, `items`, `enum`, `const`, ...).

### Budgets

| Type | Keys | Passes when |
| --- | --- | --- |
| `latency` | `threshold`: ms | The run took at most that long (wall time measured by the harness). |
| `tokens` | `threshold`: tokens; `value`: `total` (default), `prompt` or `completion` | The run's usage is within budget. |

### Trajectory

Computed from the run's event stream: every `tool_call` progress event of
`agent` nodes, enriched with the provider and tool from the agent's
`tool_calls` output. No OpenTelemetry needed. Tool names match the name the
model called, the tool name, or `provider/tool`, with `*` wildcards (as in the
agent's `require_approval`). The value shapes follow Promptfoo's
`trajectory:*` assertions.

| Type | `value` | Passes when |
| --- | --- | --- |
| `trajectory:tool-used` | `name`, `[names]` or `{name, min?, max?}` | Each tool was called at least once (or between `min` and `max` times). Score: fraction of tools satisfied. |
| `trajectory:tool-sequence` | `[names]` or `{steps: [...], mode: in_order \| exact}` | `in_order` (default): the steps appear in this order, other calls in between allowed. `exact`: the calls are exactly these. |
| `trajectory:tool-args-match` | `{name, args: {...}, mode: partial \| exact}` | Some call of `name` had these arguments (`partial`: a subset, `exact`: equal). `"123"` matches `123`. |
| `trajectory:step-count` | `{type: tool \| step \| node, min?, max?, pattern?}` or a number (max tool calls) | The count is in range. `tool`: tool calls (optionally matching `pattern`); `step`: agent model turns (`agent_step` events); `node`: node executions. |

Trajectories are diagnostics more than specifications: prefer outcome checks
(the answer, the final state) and use `weight: 0` for efficiency counts.

### Retrieval

Computed from the hits of every `knowledge-retrieval` node in the run (rank
order). `value` lists the relevant ids; a hit is relevant when its
`document_id` or chunk `id` is listed.

| Type | Score |
| --- | --- |
| `retrieval:recall-at-k` | Relevant ids found in the top `k` / relevant ids. Passes at 1 by default. |
| `retrieval:precision-at-k` | Relevant hits in the top `k` / hits in the top `k`. Passes above 0 by default. |
| `retrieval:mrr` | 1 / rank of the first relevant hit in the top `k`. Passes above 0 by default. |

Keys: `k` (default: every hit), `node` (only this retrieval node),
`threshold` (minimum score).

### Adding assertion types

In Rust, implement [`Assertion`](../crates/graphrt-eval/src/assertion/mod.rs)
(evaluation is async, so a type may call a model or run a judge workflow) and
register a factory on an `AssertionRegistry` passed to `Runner::with_registry`.

## Running

```
graphrt eval <file.eval.yaml> [flags]
```

| Flag | Description |
| --- | --- |
| `-j, --concurrency N` | Tests running at the same time (default 4). |
| `--filter PATTERN` | Only tests whose id or a tag matches (`*` wildcards). Repeatable. |
| `--repeat N` | Run every test N times; each run is a separate result (`id#1`, `id#2`...). |
| `-o, --output FILE` | Write the JSON results. |
| `--junit FILE` | Write JUnit XML (one `<testcase>` per test; failures and run errors). |
| `--pass-rate RATE` | Override `gate.pass_rate`. |
| `--fail-exit-code CODE` | Exit code when the gate fails (default 5). |
| `--include-events` | Add every run event (except token chunks) to the JSON results. |
| `--timeout-ms MS` | Per-test run timeout (a timed-out run is an error). |
| `--providers`, `--knowledge`, `--mcp`, `--plugins`, `--model`, `--env-file`, ... | As for `graphrt run`; they win over the eval file. `--model` points every model node at another model, for A/B runs. |

Exit codes: `0` the gate passed, `5` (or `--fail-exit-code`) the gate failed,
`2` invalid eval file, dataset, assertion or workflow, `1` other failures.
Individual test failures never change the exit code on their own; only the gate does.

### JSON results

`--output` writes one object; `schema_version` is `1`, and fields are only
added within a version.

```jsonc
{
  "schema_version": 1,
  "graphrt_version": "0.3.2",
  "suite": "evals/support.eval.yaml",
  "description": "...",
  "workflow": { "path": "...", "name": "support-agent", "fingerprint": "600df109733a51e0" },
  "started_at_ms": 1795157658827,
  "summary": {
    "total": 3, "passed": 2, "failed": 1, "errors": 0, "pass_rate": 0.667,
    "duration_ms": 245,
    "usage": { "prompt_tokens": 1302, "completion_tokens": 126, "total_tokens": 1428 },
    "latency_ms": { "mean": 81.0, "p50": 3, "p95": 242, "max": 242 },
    "metrics": { "recall@1": { "count": 4, "passed": 4, "mean_score": 1.0 } },
    "gate": { "pass_rate": 0.95, "passed": false }
  },
  "tests": [
    {
      "id": "refund-on-request", "description": null, "source": "inline",  // or "<dataset>:<line>"
      "tags": ["refund"], "repeat": 0,
      "status": "passed",                 // passed | failed | error
      "pass": true, "score": 1.0,
      "reason": null,                     // one-line explanation when not passed
      "vars": { "question": "Please refund order 456." },
      "expected": null,
      "run": {
        "run_id": "...", "status": "succeeded",   // succeeded | failed | aborted | paused
        "error": null, "error_type": null,
        "latency_ms": 3,
        "usage": { "prompt_tokens": 600, "completion_tokens": 54, "total_tokens": 654 },
        "output": "Done: order 456 has been refunded.",
        "outputs": { "answer": "..." },
        "answer": "Done: order 456 has been refunded.",
        "tool_calls": [ { "node_id": "agent", "id": "call_2", "name": "search_orders",
                          "provider": "orders", "tool": "search_orders",
                          "arguments": { "order_id": "456" }, "is_error": false } ],
        "retrieval": [ { "node_id": "retrieve", "id": "...", "document_id": "...", "score": 0.24 } ],
        "events": []                      // only with --include-events
      },
      "assertions": [
        { "type": "trajectory:tool-sequence", "metric": null, "pass": true, "score": 1.0,
          "weight": 1.0, "reason": "called search_orders -> refund_order in order" }
      ]
    }
  ]
}
```

Fields that are null or empty are omitted.

## Replay: deterministic model responses

A `kind: replay` entry in a providers file serves model responses from
**cassettes**: one JSON file per request, named by a SHA-256 of the normalized
request (model, messages, tools, parameters; object keys sorted). Streaming is
replayed delta by delta, tool calls and token usage included.

```yaml
# providers.replay.yaml
providers:
  - name: openai                  # the provider name the workflow's nodes use
    kind: replay
    cassettes: cassettes          # directory, relative to this providers file
    mode: replay                  # replay (default) | record | auto
    upstream: { kind: openai, api_key_env: OPENAI_API_KEY }   # or the name of another provider in this file
    aliases: []
```

| Mode | Cassette exists | Cassette missing |
| --- | --- | --- |
| `replay` | served | error: `replay: no cassette for this request (model ..., key ...)` |
| `record` | re-recorded from the upstream | recorded from the upstream |
| `auto` | served | recorded from the upstream (a response cache) |

`GRAPHRT_REPLAY_MODE=replay|record|auto` overrides the mode of every replay
provider, so one committed file serves CI (`replay`) and re-recording
(`GRAPHRT_REPLAY_MODE=record`). The upstream is only built when the mode can
record, so replaying needs no API key. Only complete, successful responses are
recorded; files are written atomically.

Replay works wherever providers files do: `graphrt eval`, `graphrt run
--providers`, `graphrt serve --providers`, and `ModelRegistry` in Rust
(`graphrt_llm::replay::ReplayModel`). Builds without the `replay` feature
(such as the small-device build) do not include it.

A cassette is keyed on the exact request, so a changed prompt, tool list or
model parameter is a miss: re-record after changing the workflow. Cassettes
contain the full prompts and answers; review them like any test fixture and
do not record prompts that carry secrets.

## Examples

[`examples/evals`](../examples/evals) runs fully offline (from the repository root):

| Suite | Shows |
| --- | --- |
| `hello.eval.yaml` | `equals`, `transform` paths, `regex`, templated values, a report-only latency metric. |
| `branching.eval.yaml` + `branching-cases.jsonl` | A JSONL dataset with `expected`, tags, `defaultTest` assertions. |
| `agent/agent.eval.yaml` | An agent judged on answers and trajectory: model responses replayed from committed cassettes, tools from a local plugin (needs `python3`). The cassettes were recorded from a scripted OpenAI-compatible server; re-record against a real model with `GRAPHRT_REPLAY_MODE=record OPENAI_API_KEY=...`. |
| `retrieval/retrieval.eval.yaml` | recall@1, MRR and precision@3 over a committed, hash-embedded knowledge store. |

## CI

```yaml
# .github/workflows/evals.yml
name: evals
on: [pull_request]
jobs:
  eval:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - name: Install graphrt
        run: curl -fsSL https://raw.githubusercontent.com/BimaPangestu28/graphrt/main/install.sh | sh
      - name: Evaluate
        env:
          GRAPHRT_REPLAY_MODE: replay       # cassettes only: no keys, no network, no cost
        run: graphrt eval evals/support.eval.yaml --output results.json --junit junit.xml
      - name: Test report
        if: always()
        uses: mikepenz/action-junit-report@v5
        with: { report_paths: junit.xml }
      - uses: actions/upload-artifact@v4
        if: always()
        with: { name: eval-results, path: results.json }
```

A failed gate fails the step (exit code 5). To run against a live model
instead, drop `GRAPHRT_REPLAY_MODE`, pass the API key as a secret, and cache
the cassette directory with `mode: auto` so unchanged requests are not paid
twice.

## From `cargo test`

```rust
use graphrt_eval::{EngineTarget, EvalSuite, Runner, RunnerOptions};

#[tokio::test]
async fn support_agent_regressions() {
    let engine = build_engine(); // your graphrt_core::Engine (graph, services, TokioClock)
    let suite = EvalSuite::load("evals/support.eval.yaml".as_ref()).unwrap();
    let report = Runner::new(RunnerOptions::default())
        .run(&suite, &EngineTarget::new(engine), |_| {})
        .await
        .unwrap();
    assert!(report.gate_passed(), "{}", graphrt_eval::report::format_summary(&report));
}
```

`EngineTarget` runs on any `Engine`, so the models can be `MockModel`s,
`ReplayModel`s or real providers. A custom `Target` can evaluate something
other than an in-process engine (for example a `graphrt serve` deployment).
See [crates/graphrt-eval/tests/engine.rs](../crates/graphrt-eval/tests/engine.rs).

## Promptfoo interop

The `tests` section of an eval file is valid Promptfoo, and Promptfoo's
deterministic assertions keep their names here. Two ways to drive graphrt
from Promptfoo, for its LLM-graded metrics, red-teaming or web viewer:

**HTTP, through `graphrt serve`** (structured outputs available to assertions):

```yaml
# promptfooconfig.yaml
providers:
  - id: https
    config:
      url: http://127.0.0.1:8080/v1/workflows/support/runs
      method: POST
      headers: { Content-Type: application/json, Authorization: "Bearer {{env.GRAPHRT_API_KEY}}" }
      body: { inputs: { question: "{{question}}" } }
      transformResponse: json.answer        # or json.outputs
prompts: ["{{question}}"]
tests: file://tests.yaml                    # the same tests as your eval file
```

```sh
graphrt serve workflows/ --providers providers.replay.yaml &
npx promptfoo@latest eval
```

**Subprocess, through `graphrt run`** (stdout only):

```yaml
providers:
  - id: 'exec: graphrt run workflows/support.yaml -o json --input question="{{question}}"'
    config:
      transformResponse: 'JSON.parse(output).outputs.answer'
```

The other way around: a Promptfoo test suite whose assertions are all
deterministic types listed above runs under `graphrt eval` after adding
`workflow:` and mapping its `vars` to the workflow's start inputs. JavaScript
`transform`s are not supported; simple paths (`output.field`) are. Model-graded
types (`llm-rubric`, `factuality`, ...) are reported as unknown assertion types.
