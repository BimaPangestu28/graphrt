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
`OTEL_EXPORTER_OTLP_TRACES_ENDPOINT`) is set. For debugging without a
collector, `--otel-stdout` prints each span as one OTLP-JSON line on **stderr**
(stdout keeps carrying the run's answer).

## What a trace looks like

```text
workflow support-triage                 (one span per run)
├── Start
├── Classify ticket                     gen_ai.request.model, gen_ai.usage.*
├── each                                (iteration node)
│   ├── iteration each[0]               (one span per body run)
│   │   └── Summarize
│   └── iteration each[1]
│       └── Summarize
└── Answer
```

| Span | Key attributes |
| --- | --- |
| Run (`workflow <name>`, kind server) | `graphrt.run.id`, `graphrt.run.status` (`succeeded`, `failed`, `aborted`, `paused`, `abandoned`), `graphrt.run.handled_errors`, `graphrt.workflow.fingerprint`, `gen_ai.usage.input_tokens` / `output_tokens` |
| Node | `graphrt.node.id`, `graphrt.node.kind`, `graphrt.node.status`, `graphrt.node.attempt`, `graphrt.node.branch`, `graphrt.node.elapsed_ms`, `graphrt.node.chunks`, `error.type` |
| Model nodes (`llm`, `agent`, `question-classifier`, `parameter-extractor`, `system-one`) | `gen_ai.operation.name`, `gen_ai.system`, `gen_ai.request.model`, `gen_ai.usage.input_tokens`, `gen_ai.usage.output_tokens` |
| Container body (`iteration each[3]`) | `graphrt.frame`, `graphrt.container.id`, `graphrt.frame.index` |

Span events: `retry` (attempt, delay, error), `exception` (failures), node
progress events such as `tool_call` / `agent_step`, `variable_updated`, and
`node_skipped` on the run span. Failed nodes and failed / aborted runs carry an
`Error` status; paused runs end with status `Unset` and `graphrt.run.paused=true`.

## Metrics

Pushed to `/v1/metrics` every 30 s and at shutdown (cumulative):

| Metric | Type | Attributes |
| --- | --- | --- |
| `graphrt.runs` | counter | `graphrt.workflow.name`, `graphrt.run.status` |
| `graphrt.run.duration` | histogram (ms) | same |
| `graphrt.nodes` | counter | `graphrt.node.kind`, `graphrt.node.status` |
| `graphrt.node.duration` | histogram (ms) | same |
| `graphrt.tokens` | counter | `gen_ai.request.model`, `gen_ai.token.type` (`input` / `output`) |

## Privacy

Prompts, node outputs and tool payloads are **not** recorded by default. Set
`GRAPHRT_OTEL_CAPTURE_CONTENT=true` to attach node outputs and progress data,
truncated to 4 KB per attribute. Secret values are always masked (`******`),
also when content capture is on. `OTEL_EXPORTER_OTLP_HEADERS` values (vendor API
keys) are never logged.

## Environment variables

| Variable | Meaning |
| --- | --- |
| `OTEL_EXPORTER_OTLP_ENDPOINT` | Base URL; `/v1/traces` and `/v1/metrics` are appended (default `http://localhost:4318`) |
| `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT`, `OTEL_EXPORTER_OTLP_METRICS_ENDPOINT` | Full per-signal URLs, used as-is |
| `OTEL_EXPORTER_OTLP_HEADERS`, `OTEL_EXPORTER_OTLP_TRACES_HEADERS` | `key=value,key2=value2` (percent-decoded) |
| `OTEL_SERVICE_NAME` | `service.name` (default `graphrt`) |
| `OTEL_RESOURCE_ATTRIBUTES` | Extra resource attributes, e.g. `deployment.environment=prod` |
| `OTEL_SDK_DISABLED` | `true` disables all export |
| `GRAPHRT_OTEL_CAPTURE_CONTENT` | `true` records truncated outputs (see Privacy) |

## Distributed tracing

`graphrt serve` continues the caller's trace when a request carries a W3C
`traceparent` header: the run span becomes a child of the caller's span, and an
unsampled parent (`-00` flags) disables export for that run.

## Backends

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

**Langfuse** (LLM observability over OTLP)

```sh
export OTEL_EXPORTER_OTLP_TRACES_ENDPOINT=https://cloud.langfuse.com/api/public/otel/v1/traces
export OTEL_EXPORTER_OTLP_HEADERS="Authorization=Basic%20$(echo -n "$LANGFUSE_PUBLIC_KEY:$LANGFUSE_SECRET_KEY" | base64 -w0)"
```

Langfuse reads the `gen_ai.*` attributes for model names and token usage.

**An OpenTelemetry Collector** in front of anything else:

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
use graphrt_otel::{EventTracer, Metrics, OtelConfig, OtlpHttpExporter};

let config = OtelConfig::from_env()?;
let metrics = Arc::new(Metrics::new());
let exporter = Arc::new(OtlpHttpExporter::start(&config, Some(metrics.clone()))?); // inside tokio
let mut tracer = EventTracer::new(exporter.clone())
    .with_graph(graph.clone())
    .with_metrics(metrics)
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
when it is full; requests are batched (512 spans, 2 s), retried with backoff on
429/502/503/504 (honoring `Retry-After`), and flushed at shutdown.
