# Observability

graphrt exports **OpenTelemetry** traces and metrics over OTLP/HTTP (JSON), so
every run of an agent can be inspected in Jaeger, Grafana Tempo, Honeycomb,
Langfuse or any OTLP-compatible backend. No OpenTelemetry SDK is linked: the
`graphrt-otel` crate encodes OTLP JSON itself and adds a few hundred KB.

## Quick start

```sh
# Local Jaeger (UI on :16686, OTLP/HTTP on :4318)
docker run --rm -p 16686:16686 -p 4318:4318 jaegertracing/jaeger:2.5.0

graphrt run examples/hello.yaml --input name=Ada --otel
graphrt serve examples/ --otel
```

Export also turns on automatically whenever `OTEL_EXPORTER_OTLP_ENDPOINT` (or
`OTEL_EXPORTER_OTLP_TRACES_ENDPOINT`) is set. Ready-made settings for Langfuse,
Phoenix, Opik, MLflow, LangSmith, Braintrust and others are under
[Backends](#backends). For debugging without a
collector, `--otel-stdout` prints each span as one OTLP-JSON line on **stderr**
(stdout keeps carrying the run's answer).

## What a trace looks like

Spans follow the [OpenTelemetry GenAI semantic conventions](https://github.com/open-telemetry/semantic-conventions-genai)
(still "Development"; graphrt keeps every attribute name in one module,
`graphrt_otel::semconv`, and dual-emits renamed attributes for one release):

```text
invoke_workflow support-triage          (one span per run)
├── Start
├── retrieval                           (knowledge-retrieval)
├── chat gpt-4o                         (llm, question-classifier, parameter-extractor, system-one)
├── invoke_agent Researcher             (agent)
│   ├── execute_tool search             (one span per tool call)
│   └── execute_tool fetch_page
├── each                                (iteration node)
│   ├── iteration each[0]               (one span per body run)
│   │   └── Summarize
│   └── iteration each[1]
│       └── Summarize
└── Answer
```

| Span | Key attributes |
| --- | --- |
| Run (`invoke_workflow <name>`; kind internal, server when it continues a caller's `traceparent`) | `gen_ai.operation.name=invoke_workflow`, `gen_ai.agent.name` (workflow name), `graphrt.workflow.name`, `graphrt.run.id`, `graphrt.run.status` (`succeeded`, `failed`, `aborted`, `paused`, `abandoned`), `graphrt.run.handled_errors`, `graphrt.workflow.fingerprint`, `gen_ai.usage.input_tokens` / `output_tokens` |
| Node (named after its title) | `graphrt.node.id`, `graphrt.node.kind`, `graphrt.node.status`, `graphrt.node.attempt`, `graphrt.node.branch`, `graphrt.node.elapsed_ms`, `graphrt.node.chunks`, `error.type` |
| Model nodes (`chat <model>`) | `gen_ai.operation.name=chat`, `gen_ai.provider.name`, `gen_ai.system` (deprecated, same provider, emitted for one more release), `gen_ai.request.model`, `gen_ai.usage.*`, `gen_ai.response.finish_reasons`, `graphrt.node.title`; `graphrt.operation.kind` is `classify` for `system-one` and `question-classifier`, `extract` for `parameter-extractor` |
| Agent (`invoke_agent <title>`) | `gen_ai.operation.name=invoke_agent`, `gen_ai.agent.name`, `gen_ai.provider.name`, `gen_ai.request.model`, `gen_ai.usage.*` |
| Tool call (`execute_tool <tool>`, child of the agent) | `gen_ai.operation.name=execute_tool`, `gen_ai.tool.name`, `gen_ai.tool.call.id`, `gen_ai.tool.type=function`, `gen_ai.agent.name`; `Error` status and `error.type=tool_error` when the tool failed, `tool_denied` when a person rejected the call |
| Retrieval (`retrieval`) | `gen_ai.operation.name=retrieval`, `gen_ai.retrieval.top_k`, `gen_ai.data_source.id` (single dataset) |
| Container body (`iteration each[3]`) | `graphrt.frame`, `graphrt.container.id`, `graphrt.frame.index` |

Span events: `retry` (attempt, delay, error), `exception` (failures), node
progress events such as `agent_step`, `variable_updated`, and `node_skipped` on
the run span. Failed nodes and failed / aborted runs carry an `Error` status;
paused runs end with status `Unset` and `graphrt.run.paused=true`.

### Sessions, users, tags and metadata

A run can carry caller context: `RunRequest` fields `session_id`, `user_id`,
`tags` and `metadata` in [`graphrt serve`](serving.md#run-context-and-evaluation-data),
or `graphrt run --session-id ID --user-id ID --tag TAG --metadata key=value`.
It is stamped on **every** span of the run, because Langfuse v4 and Opik only
filter on attributes present on each observation:

| Context | Attributes |
| --- | --- |
| `session_id` | `session.id`, `langfuse.session.id`, and `gen_ai.conversation.id` on GenAI spans |
| `user_id` | `user.id`, `langfuse.user.id` |
| `tags` | `langfuse.trace.tags` (string array); `graphrt.run.tags` on the run span |
| `metadata` | `langfuse.trace.metadata.<key>`; `graphrt.run.metadata.<key>` on the run span |

### Root spans are never dropped

The exporter queue holds 4096 spans and drops new ones when it is full, except a
run's root span: that one goes to a priority buffer flushed with the next
batch. LangSmith drops child spans whose parent never arrives, and Braintrust
and Opik hide traces without a root.

## Metrics

Pushed to `/v1/metrics` every 30 s and at shutdown (cumulative):

| Metric | Type | Attributes |
| --- | --- | --- |
| `graphrt.runs` | counter | `graphrt.workflow.name`, `graphrt.run.status` |
| `graphrt.run.duration` | histogram (ms) | same |
| `graphrt.nodes` | counter | `graphrt.node.kind`, `graphrt.node.status` |
| `graphrt.node.duration` | histogram (ms) | same |
| `graphrt.tokens` | counter | `gen_ai.request.model`, `gen_ai.token.type` (`input` / `output`) |
| `graphrt.eval.score` | histogram (`0..1`) | `gen_ai.evaluation.name`, `graphrt.workflow.name`, `graphrt.evaluation.source` (`online` / `human`) |

## Content capture (privacy)

Prompts, messages, inputs, outputs and tool payloads are **not** recorded by
default. Opt in with the standard GenAI variable
`OTEL_INSTRUMENTATION_GENAI_CAPTURE_CONTENT` (case-insensitive; unknown values
mean `NO_CONTENT`):

| Value | Effect |
| --- | --- |
| `NO_CONTENT` (default) | No content anywhere |
| `SPAN_ONLY` | Content as span attributes (what Langfuse, Phoenix and Opik read) |
| `EVENT_ONLY` | Content on a `gen_ai.client.inference.operation.details` span event instead |
| `SPAN_AND_EVENT` | Both |

`GRAPHRT_OTEL_CAPTURE_CONTENT=true` still works and means `SPAN_ONLY`; the
standard variable wins when both are set. With capture on:

| Span | Content |
| --- | --- |
| Run | `graphrt.run.input` (the start node's validated inputs) and `graphrt.run.output` (JSON text), plus `langfuse.observation.input` / `langfuse.observation.output`, since Langfuse v4 evaluators only see one observation and run on the root |
| `chat` | `gen_ai.system_instructions`, `gen_ai.input.messages` (rendered prompt), `gen_ai.output.messages`, as structured values (OTLP `kvlistValue` / `arrayValue`); `langfuse.observation.input` / `output` |
| `execute_tool` | `gen_ai.tool.call.arguments`, `gen_ai.tool.call.result` |
| `retrieval` | `gen_ai.retrieval.query.text`, `gen_ai.retrieval.documents` (`[{id, score}]`) |
| Any node | `graphrt.node.outputs`, `graphrt.progress.data` on progress events |

Every value is capped at 4 KB (strings inside structured values share that
budget, so the structure stays valid). Secret values are always masked
(`******`): prompts are rendered with secrets masked, `secret`-typed run inputs
serialize masked, and inline image data is reduced to its media type.

Rendered messages only exist when the model node is asked for them. When
capture is on, `graphrt run` and `graphrt serve` register the
`graphrt_core::node::CaptureContent` service, and `llm` nodes then emit a
`model_request` progress event (`{"messages": [...]}`), agents add the result
text to `tool_result` progress, and `knowledge-retrieval` adds its `query` to
the node metadata. These also appear in `-o events` output and in stored run
events. With capture off none of this is built. Embedders that want the same
must register the service themselves:

```rust
let services = services.with(graphrt_core::node::CaptureContent);
```

`OTEL_EXPORTER_OTLP_HEADERS` values (vendor API keys) are never logged.

## Environment variables

| Variable | Meaning |
| --- | --- |
| `OTEL_EXPORTER_OTLP_ENDPOINT` | Base URL; `/v1/traces` and `/v1/metrics` are appended (default `http://localhost:4318`) |
| `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT`, `OTEL_EXPORTER_OTLP_METRICS_ENDPOINT` | Full per-signal URLs, used as-is |
| `OTEL_EXPORTER_OTLP_HEADERS`, `OTEL_EXPORTER_OTLP_TRACES_HEADERS` | `key=value,key2=value2` (percent-decoded) |
| `OTEL_SERVICE_NAME` | `service.name` (default `graphrt`) |
| `OTEL_RESOURCE_ATTRIBUTES` | Extra resource attributes, e.g. `deployment.environment=prod` |
| `OTEL_SDK_DISABLED` | `true` disables all export |
| `OTEL_INSTRUMENTATION_GENAI_CAPTURE_CONTENT` | `NO_CONTENT` (default), `SPAN_ONLY`, `EVENT_ONLY` or `SPAN_AND_EVENT` (see Content capture) |
| `GRAPHRT_OTEL_CAPTURE_CONTENT` | `true` is an alias for `SPAN_ONLY` |
| `GRAPHRT_OTEL_PRESET` | A vendor preset (same as `--otel-preset`, see Vendor presets) |

## Distributed tracing

`graphrt serve` continues the caller's trace when a request carries a W3C
`traceparent` header: the run span becomes a child of the caller's span, and an
unsampled parent (`-00` flags) disables export for that run.

## Backends

graphrt sends OTLP/HTTP **JSON**. Point `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` at
the vendor's full traces URL and put the vendor's headers in
`OTEL_EXPORTER_OTLP_HEADERS` (comma-separated `key=value`, values
percent-encoded, so a space is `%20`). For LLM backends, also set
`OTEL_INSTRUMENTATION_GENAI_CAPTURE_CONTENT=SPAN_ONLY` if they should see
prompts and answers. Vendors that only accept protobuf (W&B Weave) need an
OpenTelemetry Collector in between (see the end of this section).

### Vendor presets

`--otel-preset <name>` (or `GRAPHRT_OTEL_PRESET=<name>`; `run`, `serve` and
`eval`) fills the traces endpoint, the auth and routing headers and the
resource attributes a vendor needs from **that vendor's own environment
variables**, so keys never go on a command line where `ps` shows them. It
implies `--otel`, and turns the metrics push off (none of these backends
ingests OTLP metrics) unless `OTEL_EXPORTER_OTLP_METRICS_ENDPOINT` is set.
Explicit `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` and `OTEL_EXPORTER_OTLP_HEADERS`
values still win over the preset's, as an escape hatch.

| Preset | Base URL (default) | Traces path | Required | Optional |
| --- | --- | --- | --- | --- |
| `langfuse` | `LANGFUSE_BASE_URL`, else `LANGFUSE_HOST` (`https://cloud.langfuse.com`) | `/api/public/otel/v1/traces` | `LANGFUSE_PUBLIC_KEY`, `LANGFUSE_SECRET_KEY` (sent as `Authorization: Basic base64(pk:sk)`, plus `x-langfuse-ingestion-version: 4`) | – |
| `langsmith` | `LANGSMITH_ENDPOINT` (`https://api.smith.langchain.com`) | `/otel/v1/traces` | `LANGSMITH_API_KEY` (`x-api-key`) | `LANGSMITH_PROJECT` (`Langsmith-Project`) |
| `braintrust` | `BRAINTRUST_API_URL` (`https://api.braintrust.dev`) | `/otel/v1/traces` | `BRAINTRUST_API_KEY` (`Authorization: Bearer`), `BRAINTRUST_PARENT` (`x-bt-parent`: `project_name:<name>`, `project_id:<id>` or `experiment_id:<id>`) | – |
| `opik` | `OPIK_URL_OVERRIDE` (`https://www.comet.com/opik/api`) | `/v1/private/otel/v1/traces` | – | `OPIK_API_KEY` (`Authorization`, no `Bearer`), `OPIK_WORKSPACE` (`Comet-Workspace`), `OPIK_PROJECT_NAME` (`projectName` and the `opik.project_name` resource attribute, so every span has it) |
| `phoenix` | `PHOENIX_COLLECTOR_ENDPOINT` (`http://localhost:6006`) | `/v1/traces` | – | `PHOENIX_API_KEY` (`Authorization: Bearer`), `PHOENIX_PROJECT_NAME` (`openinference.project.name` resource attribute) |
| `mlflow` | `MLFLOW_TRACKING_URI` (`http://localhost:5000`) | `/v1/traces` | `MLFLOW_EXPERIMENT_ID` (`x-mlflow-experiment-id`) | `MLFLOW_WORKSPACE` (`x-mlflow-workspace`), `MLFLOW_TRACKING_TOKEN` (`Authorization: Bearer`) |

A base URL that already ends with the traces path is used as is. A missing
required variable fails at startup and names it.

```sh
export LANGFUSE_PUBLIC_KEY=pk-lf-... LANGFUSE_SECRET_KEY=sk-lf-...
export OTEL_INSTRUMENTATION_GENAI_CAPTURE_CONTENT=SPAN_ONLY   # let evaluators see inputs and outputs
graphrt serve flows/ --otel-preset langfuse
```

The sections below show what each preset sets, for configuring the same
backends by hand or through a collector.

**Langfuse v4** (HTTP only, no gRPC)

```sh
# EU cloud; US: https://us.cloud.langfuse.com, JP: https://jp.cloud.langfuse.com,
# HIPAA: https://hipaa.cloud.langfuse.com, self-hosted (v3.22.0+): http://<host>:3000
export OTEL_EXPORTER_OTLP_TRACES_ENDPOINT=https://cloud.langfuse.com/api/public/otel/v1/traces
export OTEL_EXPORTER_OTLP_HEADERS="Authorization=Basic%20$(echo -n "$LANGFUSE_PUBLIC_KEY:$LANGFUSE_SECRET_KEY" | base64 | tr -d '\n'),x-langfuse-ingestion-version=4"
export OTEL_INSTRUMENTATION_GENAI_CAPTURE_CONTENT=SPAN_ONLY
```

`x-langfuse-ingestion-version: 4` makes ingestion real time (without it data
can lag up to 10 minutes). The run span is the trace's root observation and
carries `langfuse.observation.input` / `output`, which observation-level
evaluators ("Is Root Observation") read. Session, user, tags and metadata are
on every observation. Langfuse bills per trace, observation and score: a run
with N nodes and M tool calls costs about 1 + N + M units.

**Arize Phoenix** (self-hosted, one container)

```sh
docker run --rm -p 6006:6006 arizephoenix/phoenix:latest
export OTEL_EXPORTER_OTLP_TRACES_ENDPOINT=http://localhost:6006/v1/traces
export OTEL_INSTRUMENTATION_GENAI_CAPTURE_CONTENT=SPAN_ONLY
```

Phoenix Cloud uses `https://app.phoenix.arize.com/s/<space-name>/v1/traces`
plus the API key header from your Phoenix settings. Phoenix 15.10.0 and later
convert `gen_ai.*` attributes (messages, tool calls, token usage, retrieval
documents) to OpenInference at ingest.

**Comet Opik** (HTTP only)

```sh
# Self-hosted: http://localhost:5173/api/v1/private/otel/v1/traces
export OTEL_EXPORTER_OTLP_TRACES_ENDPOINT=https://www.comet.com/opik/api/v1/private/otel/v1/traces
export OTEL_EXPORTER_OTLP_HEADERS="Authorization=$OPIK_API_KEY,projectName=my-agents,Comet-Workspace=$COMET_WORKSPACE"
export OTEL_RESOURCE_ATTRIBUTES="opik.project_name=my-agents"
```

The API key goes in `Authorization` without a `Bearer` prefix. Opik needs
`opik.project_name` on every span; as a resource attribute it is. It maps
`gen_ai.conversation.id` (the run's `session_id`) to threads.

**MLflow** (3.6.0+, HTTP only, a SQL backend store such as SQLite)

```sh
mlflow server --backend-store-uri sqlite:///mlflow.db --port 5000
export OTEL_EXPORTER_OTLP_TRACES_ENDPOINT=http://localhost:5000/v1/traces
export OTEL_EXPORTER_OTLP_HEADERS="x-mlflow-experiment-id=$EXPERIMENT_ID"
```

The lightest backend for trying traces on a laptop or in CI.

**LangSmith**

```sh
# EU: https://eu.api.smith.langchain.com/otel, APAC: https://apac.api.smith.langchain.com/otel,
# self-hosted: https://<host>/api/v1/otel
export OTEL_EXPORTER_OTLP_TRACES_ENDPOINT=https://api.smith.langchain.com/otel/v1/traces
export OTEL_EXPORTER_OTLP_HEADERS="x-api-key=$LANGSMITH_API_KEY,Langsmith-Project=my-agents"
```

Routing is by header only (`Langsmith-Project`, or the
`Langsmith-Agent-Id` / `Langsmith-Agent-Environment` pair, never both). The
endpoint answers 200 before processing and silently drops child spans whose
parent never arrives, which is why graphrt never drops a root span.

**Braintrust**

```sh
# EU: https://api-eu.braintrust.dev/otel/v1/traces
export OTEL_EXPORTER_OTLP_TRACES_ENDPOINT=https://api.braintrust.dev/otel/v1/traces
export OTEL_EXPORTER_OTLP_HEADERS="Authorization=Bearer%20$BRAINTRUST_API_KEY,x-bt-parent=project_name:my-agents"
```

`x-bt-parent` also accepts `project_id:<id>` and `experiment_id:<id>`; the latter
sends runs straight into an offline experiment. The logs view lists only traces
with a root span.

**Grafana Tempo / Grafana Cloud**

```sh
export OTEL_EXPORTER_OTLP_ENDPOINT=https://otlp-gateway-prod-eu-west-2.grafana.net/otlp
export OTEL_EXPORTER_OTLP_HEADERS="Authorization=Basic%20$(echo -n "$INSTANCE_ID:$TOKEN" | base64 -w0)"
graphrt serve flows/
```

**Honeycomb**

```sh
export OTEL_EXPORTER_OTLP_ENDPOINT=https://api.honeycomb.io
export OTEL_EXPORTER_OTLP_HEADERS="x-honeycomb-team=$HONEYCOMB_API_KEY"
export OTEL_SERVICE_NAME=agents
```

With only `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` set, metrics still go to the
default local collector address and are dropped quietly if nothing listens
there; LLM backends ignore OTLP metrics anyway.

**An OpenTelemetry Collector** in front of anything else (also the way to reach
protobuf-only backends such as W&B Weave):

```yaml
receivers:
  otlp:
    protocols:
      http: { endpoint: 0.0.0.0:4318 }
exporters:
  debug: {}
service:
  pipelines:
    traces: { receivers: [otlp], exporters: [debug] }
    metrics: { receivers: [otlp], exporters: [debug] }
```

## Embedding

```rust
use std::sync::Arc;
use futures::StreamExt;
use graphrt_otel::{EventTracer, Metrics, OtelConfig, OtlpHttpExporter, RunContext};

let config = OtelConfig::from_env()?;
let metrics = Arc::new(Metrics::new());
let exporter = Arc::new(OtlpHttpExporter::start(&config, Some(metrics.clone()))?); // inside tokio
let mut tracer = EventTracer::new(exporter.clone())
    .with_graph(graph.clone())
    .with_metrics(metrics)
    .capture(config.capture) // content capture policy from the environment
    .with_context(RunContext { session_id: Some(session), ..RunContext::default() }) // optional
    .with_parent(incoming_traceparent); // optional
let mut run = engine.run(input);
while let Some(event) = run.next().await {
    tracer.on_event(&event);
}
drop(tracer);
exporter.shutdown().await;
```

`InMemoryExporter` collects spans for tests, `WriterExporter` writes JSON lines
to any writer, and `MultiExporter` fans out to several. The exporter never
blocks a run: spans go into a bounded queue (4096) and are dropped and counted
when it is full (run root spans excepted, see above); requests are batched (512 spans, 2 s), retried with backoff on
429/502/503/504 (honoring `Retry-After`), and flushed at shutdown.
