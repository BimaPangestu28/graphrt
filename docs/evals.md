# Evaluating workflows

`graphrt eval` runs a workflow against a set of test cases, checks each run with
assertions, and gates CI on the pass rate. The same harness is a Rust library,
[`graphrt-eval`](../crates/graphrt-eval), for `cargo test`.

The release binaries, the Homebrew formula and the GHCR image include it. It is
an opt-in cargo feature, so a build from source needs it named:
`cargo install graphrt-cli --features eval` (add `online-eval` for
[online evaluation](#online-evaluation) in `graphrt serve`), or
`docker build --build-arg EXTRA_FEATURES=eval,online-eval .`.

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

What it is for: deterministic checks you can run on every commit (answers,
structured outputs, latency and token budgets, the tools an agent called and in
what order, whether retrieval found the labelled documents), plus
[LLM judges](#llm-judges-judges-are-workflows) written as ordinary graphrt
workflows, [repeated trials](#trials-passk-and-passk) with pass@k / pass^k,
[baseline comparison](#baselines-regressions-with-a-tolerance) with tolerances,
and [judge calibration](#calibrating-a-judge) against human labels. Model
responses can be replayed from recorded cassettes (see
[Replay](#replay-deterministic-model-responses)), so agent and judge suites run
offline, for free, with the same result every time.

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
# yaml-language-server: $schema=https://raw.githubusercontent.com/Graphrt/graphrt/main/schema/eval-v1.schema.json
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
  pass_hat_k: 0.8                         # optional, with --trials: mean pass^k
  k: 3                                    # optional: k of pass@k / pass^k (default: --trials)
  baseline:                               # with --baseline
    tolerance: 0.05                       # max drop of a metric's mean score (default 0.05)
    test_tolerance: 0                     # max drop of a test's pass rate (default 0)
    metrics: { faithfulness: 0.1 }        # per-metric tolerances
```

| Key | Meaning |
| --- | --- |
| `workflow` | The workflow under test (required). |
| `providers`, `knowledge`, `mcp`, `plugins` | Host configuration, as the `run` flags of the same names. Flags on the command line win. |
| `dataset` | JSONL file: one `{id?, vars, expected?, tags?, metadata?, assert?, description?}` object per line. Blank lines and `//` lines are skipped. Rows without an `id` are named `<file stem>:<line>`. |
| `defaultTest` | `vars` and `metadata` used as defaults, `tags` and `assert` prepended, `threshold` inherited. |
| `tests[]` | Inline test cases (run before the dataset rows). |
| `gate.pass_rate` | Minimum fraction of passing test runs (every trial counts); below it the command exits with code 5. |
| `gate.pass_hat_k`, `gate.k` | Minimum mean pass^k over the test cases, and its k. See [Trials](#trials-passk-and-passk). |
| `gate.baseline` | Tolerances for `--baseline`. See [Baselines](#baselines-regressions-with-a-tolerance). |
| `calibration` | Makes the suite a judge calibration. See [Calibrating a judge](#calibrating-a-judge). |

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
(evaluation is async, so a type may call a model or run a workflow) and
register a factory on an `AssertionRegistry` passed to `Runner::with_registry`.

## LLM judges: judges are workflows

A judge is an ordinary graphrt workflow: a start node, usually one `llm` node
with structured output, and an end node. The `judge` assertion runs it on what
the test produced and reads its verdict:

```yaml
assert:
  - type: judge
    workflow: ../judges/faithfulness.yaml     # relative to the eval file
    metric: faithfulness
    inputs: { question: "{{question}}" }      # extra start inputs (templated)
    unknown: exclude                          # fail (default) | exclude
    threshold: 0.5                            # score-only judges pass at this score
```

Because the judge is a workflow it is versioned by its fingerprint (recorded
with every result, so a changed prompt or model is visible), it can use any
provider including `replay` cassettes, and it is calibrated like any other
workflow ([below](#calibrating-a-judge)). Each judge file is compiled **once**
and shared by every test; judges run inside their test, so they share the `-j`
limit. They use the suite's providers, knowledge, MCP servers and plugins.

**Input contract.** The judge's start node receives these inputs, by name; only
the variables it declares are passed:

| Input | Value |
| --- | --- |
| `output` | The value under test: the run output, or the `transform` path. |
| `vars` | The test's vars (an object). |
| `expected` | The test's `expected` value (absent when there is none). |
| `contexts` | The text of every chunk the run's `knowledge-retrieval` nodes returned, in rank order (absent when there were none). |
| `trajectory` | The agent tool calls: `[{node_id, id, name, provider, tool, arguments, is_error}]`. |
| keys of `inputs` | Anything else, e.g. the question; these win over the names above. |

Text inputs (`paragraph`, `text-input`, `string`, `select`) receive text:
strings as is, a list of strings joined by blank lines (so `contexts` reads
naturally in a prompt), anything else as indented JSON. `json`, `object` and
array inputs receive the value itself.

**Output contract.** The judge's outputs (or its single output when that is an
object, such as an `llm` node's `structured_output`) carry:

| Output | Meaning |
| --- | --- |
| `verdict` | `pass`, `fail` or `unknown` (also `yes` / `no`, `true` / `false`). Decides when present. |
| `score` | A number in 0..1. Reported as the assertion score; alone, it passes at `threshold` (default 0.5). |
| `reasoning` | Shown in the reason and the report (`reason` and `explanation` also work). |

`unknown` is explicit: with `unknown: fail` (the default) it fails the
assertion; with `unknown: exclude` the result is reported (`"excluded": true`,
counted under `excluded` in the metric summary) but takes no part in the test
score, the gate or the metric means. A test whose only results are excluded
passes. A judge run that fails, or outputs neither a verdict nor a score, fails
the assertion (the test is a failure, not an error).

**Models.** `--model` rewrites the workflow under test only. `--judge-model
PROVIDER/NAME` rewrites the judges (and the target of a calibration suite,
which is a judge).

**Reference judges** live in [`examples/evals/judges`](../examples/evals/judges):
`faithfulness.yaml` (every claim supported by `contexts`), `answer-relevance.yaml`
(the answer addresses `question`), `goal-success.yaml` (the `goal` input is
achieved, `trajectory` as evidence) and `refusal.yaml` (the answer declines;
use `not-judge` to catch over-refusal). Each is binary, judges one criterion,
asks for the reasoning before the verdict, and has an `unknown` way out.
`refusal.system-one.yaml` is the same judge on TypeSafe System One: the
calibrated probability of `pass` becomes the score, and a low-confidence answer
routes to an explicit `unknown`. Copy them and adapt the prompts to your domain;
then calibrate.

## Trials: pass@k and pass^k

`--trials N` runs every test N times (each trial is a result, `id#1`,
`id#2`...) and adds per-case statistics to the report and the summary. With
`n` trials of which `c` passed, graphrt uses the unbiased estimators:

| Statistic | Estimator | Meaning |
| --- | --- | --- |
| pass@k | `1 - C(n-c, k) / C(n, k)` | At least one of k trials passes (capability). Unbiased for `1 - (1-p)^k` (Chen et al. 2021). |
| pass^k | `C(c, k) / C(n, k)` | All k trials pass (reliability, τ-bench). Unbiased for `p^k`: the chance that k trials drawn without replacement all passed. |

`k` is `gate.k`, else N (clamped to `1..=n`). The summary reports the mean of
each over the test cases; `gate.pass_hat_k` gates on the mean pass^k. A 75%
per-trial pass rate is roughly 42% pass^3, so reliability gates are much
stricter than pass rates. `gate.pass_rate` still counts every trial.

```
  test case                      passed  pass@3  pass^3
  refund-window                    3/3    1.000   1.000
pass@3 1.000, pass^3 1.000 over 4 case(s), up to 3 trial(s) each
```

## Baselines: regressions with a tolerance

The `--output` file of one run is the baseline of the next:

```sh
graphrt eval evals/support.eval.yaml -o baseline.json          # on main
graphrt eval evals/support.eval.yaml --baseline baseline.json  # on the branch
```

LLM metrics are noisy, so comparisons use tolerances (`gate.baseline`), not
equality. A **metric** regresses when its mean score drops by more than its
tolerance (`metrics.<name>`, else `tolerance`, default 0.05); a **test**
regresses when its pass rate across its trials drops by more than
`test_tolerance` (default 0: a test that passed and now fails). Any regression
fails the gate. Improvements beyond the tolerance are reported too; tests or
metrics on one side only are `new` / `missing`, never regressions. When a
judge's fingerprint differs from the one in the baseline the summary says so,
because its scores are no longer strictly comparable. Reports of schema
versions 1 and 2 load as baselines.

```
baseline baseline.json: 1 regression(s), 0 improvement(s)
  REGRESSED metric faithfulness                 0.925 -> 0.700 (tolerance 0.05)
gate: FAILED (1 regression(s) vs the baseline)
```

## Calibrating a judge

Calibration is just another eval: the workflow under test is the judge, each
test's vars are the judge's inputs, and `expected` is the label a person gave
(`pass` / `fail`). A `calibration:` block turns on the agreement report; tests
without their own assertions then check `verdict: <expected>` instead of
`equals`.

```yaml
workflow: ../judges/faithfulness.yaml
dataset: labels.jsonl        # {id, vars: {question, output, contexts}, expected: pass|fail}
calibration:
  unknown: exclude           # exclude (default): counted apart | fail: counted as the judge saying fail
  threshold: 0.5             # for score-only judges
  min_kappa: 0.5             # gates (optional)
  min_tpr: 0.8
  min_tnr: 0.7
gate: { pass_rate: 0 }       # disagreements are expected; gate on kappa instead
```

```
calibration: 10 labelled, 9 judged, 1 unknown (excluded), 0 error(s)
  agreement 0.778  TPR 0.800  TNR 0.750  kappa 0.550
                 label pass  label fail
  judge pass              4           1
  judge fail              1           3
```

TPR is the share of good outputs the judge passes, TNR the share of bad ones
it catches (often the weak side), and Cohen's kappa
`(po - pe) / (1 - pe)` corrects the raw agreement `po` for the agreement `pe`
expected by chance from the marginals: 80% agreement can be a kappa of 0.6.
A metric with no examples of its class is undefined (`null`) and fails its
gate. Judge runs that fail count as `errors`. Calibration rows pass the judge's
inputs directly as vars, so give `contexts` as text. Collect 100-200 labels
per failure mode, keep a held-out split, and re-run the calibration whenever
the judge's fingerprint changes; `--judge-model` measures what a cheaper judge
model costs in agreement.

## Running

```
graphrt eval <file.eval.yaml> [flags]
```

| Flag | Description |
| --- | --- |
| `-j, --concurrency N` | Tests running at the same time (default 4). |
| `--filter PATTERN` | Only tests whose id or a tag matches (`*` wildcards). Repeatable. |
| `--trials N` (alias `--repeat`) | Run every test N times; each trial is a separate result (`id#1`, `id#2`...) and the report adds pass@k / pass^k. |
| `-o, --output FILE` | Write the JSON results (also the input of a later `--baseline`). |
| `--baseline FILE` | Compare with earlier JSON results; regressions beyond `gate.baseline` fail the gate. |
| `--judge-model PROVIDER/NAME` | Point every model node of the judge workflows at another model. |
| `--junit FILE` | Write JUnit XML (one `<testcase>` per test; failures and run errors). |
| `--pass-rate RATE` | Override `gate.pass_rate`. |
| `--fail-exit-code CODE` | Exit code when the gate fails (default 5). |
| `--include-events` | Add every run event (except token chunks) to the JSON results. |
| `--timeout-ms MS` | Per-test run timeout (a timed-out run is an error). |
| `--otel`, `--otel-stdout`, `--otel-preset NAME` | One trace per test run, as a Langfuse experiment item, with an `evaluate` span per assertion (below). |
| `--providers`, `--knowledge`, `--mcp`, `--plugins`, `--model`, `--env-file`, ... | As for `graphrt run`; they win over the eval file. `--model` points every model node at another model, for A/B runs. |

With `--otel` (or `--otel-stdout`, an OTLP endpoint variable, or
`--otel-preset`), every test run is its own trace, shaped as a
[Langfuse experiment over OpenTelemetry](https://langfuse.com/integrations/native/opentelemetry/experiments):
`langfuse.experiment.id` (`<suite>-<start time>`, one per invocation),
`langfuse.experiment.name` (the suite file stem) and
`langfuse.experiment.dataset.id` (the suite path) on every span; the test id as
`langfuse.experiment.item.id`, the root span id as
`langfuse.experiment.item.root_observation_id`, `expected` as
`langfuse.experiment.item.expected_output`, test metadata and the suite
description on the root span. Each assertion result is an `evaluate {metric}`
span under the root with a `gen_ai.evaluation.result` event (source
`offline`). Inputs and outputs follow content capture
(`OTEL_INSTRUMENTATION_GENAI_CAPTURE_CONTENT=SPAN_ONLY`), as for any run.
Judge runs are not traced. With `--otel-preset braintrust` and
`BRAINTRUST_PARENT=experiment_id:<id>` the traces land in a Braintrust
experiment.

Exit codes: `0` the gate passed, `5` (or `--fail-exit-code`) the gate failed
(pass rate, pass^k, calibration thresholds or baseline regressions; the summary
line `gate: FAILED (...)` lists each unmet condition),
`2` invalid eval file, dataset, assertion or workflow, `1` other failures.
Individual test failures never change the exit code on their own; only the gate does.

### JSON results

`--output` writes one object; `schema_version` is `2`, and fields are only
added within a version. Version 2 renamed each test's `repeat` to `trial` and
added `cases`, `calibration`, `baseline`, `summary.trials`,
`summary.judge_usage`, `gate.failures`, and `judge` / `excluded` on assertion
results. Version 1 files still load as baselines.

```jsonc
{
  "schema_version": 2,
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
    "judge_usage": { "prompt_tokens": 822, "completion_tokens": 84, "total_tokens": 906 },  // judges' tokens
    "metrics": { "faithfulness": { "count": 3, "passed": 3, "mean_score": 1.0, "excluded": 1 } },
    "trials": { "trials": 3, "k": 3, "pass_at_k": 1.0, "pass_hat_k": 0.667 },   // with --trials
    "gate": { "pass_rate": 0.95, "pass_hat_k": 0.8, "passed": false,
              "failures": ["pass rate 0.667 < 0.950", "pass^3 0.667 < 0.800"] }
  },
  "cases": [                              // with --trials: one entry per test case
    { "id": "refund-on-request", "trials": 3, "passed": 2, "pass_rate": 0.667,
      "pass_at_k": 1.0, "pass_hat_k": 0.0, "mean_score": 0.667 }
  ],
  "calibration": {                        // calibration suites
    "labelled": 10, "judged": 9, "unknown": 1, "errors": 0, "unknown_policy": "exclude",
    "confusion": { "tp": 4, "fp": 1, "tn": 3, "fn": 1 },
    "agreement": 0.778, "tpr": 0.8, "tnr": 0.75, "kappa": 0.55
  },
  "baseline": {                           // with --baseline
    "path": "baseline.json", "workflow_fingerprint": "600df109733a51e0",
    "judges_changed": [], "regressions": 1, "improvements": 0,
    "metrics": [ { "name": "faithfulness", "baseline": 0.925, "current": 0.7, "tolerance": 0.05,
                   "change": "regressed" } ],           // regressed | improved | unchanged | new | missing
    "tests": [ { "name": "refund-on-request", "baseline": 1.0, "current": 0.667, "tolerance": 0.0,
                 "change": "regressed" } ]              // changed tests only
  },
  "tests": [
    {
      "id": "refund-on-request", "description": null, "source": "inline",  // or "<dataset>:<line>"
      "tags": ["refund"], "trial": 0,
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
        "retrieval": [ { "node_id": "retrieve", "id": "...", "document_id": "...", "score": 0.24,
                         "content": "..." } ],
        "events": []                      // only with --include-events
      },
      "assertions": [
        { "type": "trajectory:tool-sequence", "metric": null, "pass": true, "score": 1.0,
          "weight": 1.0, "reason": "called search_orders -> refund_order in order" },
        { "type": "judge", "metric": "faithfulness", "pass": true, "score": 1.0, "weight": 1.0,
          "reason": "judge: pass: every claim appears in the context",
          "excluded": false,              // true: reported only (unknown with `unknown: exclude`)
          "judge": { "workflow": "evals/judges/faithfulness.yaml", "fingerprint": "b97e80df555ecba9",
                     "verdict": "pass", "score": null, "reasoning": "...", "run_id": "...",
                     "latency_ms": 412, "usage": { "prompt_tokens": 274, "completion_tokens": 28, "total_tokens": 302 } } }
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
| `judged/judged.eval.yaml` | A RAG workflow graded by the reference judges (faithfulness, relevance, goal success, refusal), an `unknown` excluded, `gate.pass_hat_k`. The answers and the verdicts replay from cassettes recorded against a scripted OpenAI-compatible server. Try `--trials 3`. |
| `calibration/faithfulness.eval.yaml` + `labels.jsonl` | Calibrating the faithfulness judge against ten human labels: agreement, TPR, TNR, kappa, confusion matrix, a `min_kappa` gate. |
| `online/evaluators.yaml` | [Online evaluation](#online-evaluation) of the RAG workflow in `graphrt serve` with the same judges, a token budget, a negated text check and a tag filter, replayed offline. |

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
        run: curl -fsSL https://raw.githubusercontent.com/Graphrt/graphrt/main/install.sh | sh
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

## Online evaluation

`graphrt serve --evaluators evaluators.yaml` scores a **sample of production
runs** after they finish, with the same assertion types and judge workflows as
`graphrt eval`. The config is a file of its own (the workflow format is frozen):

```yaml
concurrency: 2        # evaluations running at once (default 2, max 256)
queue: 256            # sampled runs waiting for a slot (default 256); more are dropped and counted
evaluators:
  - name: faithfulness                    # the score name, unique
    workflows: [support, "chat-*"]        # served workflow names, `*` wildcards (default: all)
    sample: 0.05                          # fraction of matching runs (default 1)
    status: [succeeded]                   # succeeded / failed / aborted (default: succeeded, failed)
    tags: [prod]                          # the run has at least one of these tags (optional)
    metadata: { tier: gold }              # and every one of these metadata values (optional)
    assert:                               # one assertion, as in an eval file
      type: judge
      workflow: judges/faithfulness.yaml  # relative to this file
      unknown: exclude
      inputs: { question: "{{question}}" } # the run's own inputs, metadata, tags and id
  - name: no-apology
    assert: { type: not-icontains, value: "i apologize" }
  - name: budget
    assert: { type: tokens, threshold: 4000 }
```

- **Assertions.** Only types that need no expected answer run online:
  `judge`, `regex`, `contains` and its `i`/`-any`/`-all` forms, `starts-with`,
  `is-json`, `json-schema`, `latency`, `tokens` and `trajectory:*` (and their
  `not-` forms). `equals`, `verdict`, the retrieval metrics and any
  `{{expected}}` reference are refused when the server starts, as are unknown
  keys, a judge workflow that does not compile and a sample rate outside
  `0..1`. `{{name}}` references resolve against the run: its inputs (also as
  `{{vars.name}}`), `{{metadata.key}}`, `{{tags}}` and `{{id}}` (the run id).
  Judges receive the inputs listed in [LLM judges](#llm-judges-judges-are-workflows),
  with `vars` being the run's inputs (secrets masked) and no `expected`.
- **Sampling** is deterministic: a hash of the run id and the evaluator name
  decides, so the same run id always gets the same decision (reproducible) and
  evaluators sample independently.
- **When.** After a run of a matching workflow ends (succeeded, failed or
  aborted; paused runs wait until they finish), the matching evaluators that
  sample it are queued as one job. One background task runs at most
  `concurrency` jobs at a time (interleaved: they wait on models and judge
  runs) from a queue of `queue` jobs; a job that finds the queue full
  is dropped, counted in the `graphrt.eval.dropped` metric, and logged (first
  drop, then every thousandth). The run's response never waits: the job is
  queued after the run is recorded and its callers are answered.
- **Cost.** Without `--evaluators` there is no task, queue or check: a run
  pays a test of an empty `Option` (measured against the previous release:
  `serve` sync throughput on the hello workflow within noise, medians -1.4%
  to -3% with ±10% spread between rounds). Scoring every run with two
  deterministic evaluators cost about 10% throughput in the same test. With it, a run of a workflow no evaluator
  covers pays the same; a covered one pays a hash per evaluator when it ends.
  The feature (`online-eval`, opt-in; in the release binaries and image) is
  not in the IoT build (`--no-default-features --features llm,http,wasm,serve`).
- **Results** are `online` [scores](serving.md#scores-and-feedback) of the run
  (`GET /v1/runs/{id}/scores`): the assertion score as `value`, `label`
  (`pass`, `fail`, or `unknown` for an excluded judge verdict), the reason as
  `comment`, the assertion type, and the judge's workflow, fingerprint and
  verdict. With OpenTelemetry each becomes an `evaluate {name}` span in the
  run's trace with a `gen_ai.evaluation.result` event and feeds the
  `graphrt.eval.score` histogram; with `--langfuse-scores` it is also uploaded
  to Langfuse. Judge runs themselves are not traced or stored as runs.

A resumed run is evaluated on the events of its last execution (the part after
the pause). [`examples/evals/online`](../examples/evals/online/evaluators.yaml)
scores the RAG workflow of `examples/evals/judged` with the reference judges,
replayed offline.

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
types (`llm-rubric`, `factuality`, ...) are reported as unknown assertion types;
replace them with a `judge` workflow (see [LLM judges](#llm-judges-judges-are-workflows)).
