//! OpenTelemetry wiring for `run` and `serve`.
//!
//! Enabled by `--otel` (OTLP/HTTP to `$OTEL_EXPORTER_OTLP_ENDPOINT`, default
//! `http://localhost:4318`), automatically when an OTLP endpoint variable is set,
//! or `--otel-stdout` (span JSON lines on stderr). `OTEL_SDK_DISABLED=true` turns
//! everything off. Without the `otel` feature these flags are rejected.
//!
//! Content capture follows `OTEL_INSTRUMENTATION_GENAI_CAPTURE_CONTENT`
//! (`GRAPHRT_OTEL_CAPTURE_CONTENT=true` is an alias for `SPAN_ONLY`).
//!
//! `--otel-preset <vendor>` (or `$GRAPHRT_OTEL_PRESET`) fills the endpoint,
//! headers and resource attributes of a vendor from its own environment
//! variables (`graphrt_otel::preset`); credentials never go on the command line.

use std::sync::Arc;

use graphrt_core::{Event, Graph, IndexMap};

use crate::args::ServiceArgs;
use crate::error::CliError;

/// Caller context of a run (session, user, tags, metadata), stamped on its
/// spans and stored on its run record.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RunContext {
    pub session_id: Option<String>,
    pub user_id: Option<String>,
    pub tags: Vec<String>,
    pub metadata: IndexMap<String, String>,
}

/// Per-process telemetry: exporters and shared metrics.
#[derive(Debug)]
pub struct Telemetry {
    #[cfg(feature = "otel")]
    exporter: Arc<dyn graphrt_otel::SpanExporter>,
    #[cfg(feature = "otel")]
    http: Option<Arc<graphrt_otel::OtlpHttpExporter>>,
    #[cfg(feature = "otel")]
    metrics: Arc<graphrt_otel::Metrics>,
    #[cfg(feature = "otel")]
    capture: graphrt_otel::CaptureMode,
}

/// Traces one run. A no-op when telemetry is off.
#[derive(Debug, Default)]
pub struct RunTracer {
    #[cfg(feature = "otel")]
    inner: Option<graphrt_otel::EventTracer>,
}

impl RunTracer {
    /// End tracing: closes any span still open (as abandoned) and hands everything
    /// to the exporter.
    pub fn finish(self) {}

    /// Feed one run event.
    pub fn on_event(&mut self, event: &Event) {
        #[cfg(feature = "otel")]
        if let Some(tracer) = self.inner.as_mut() {
            tracer.on_event(event);
        }
        #[cfg(not(feature = "otel"))]
        let _ = event;
    }

    /// Whether this run is traced.
    pub fn is_active(&self) -> bool {
        #[cfg(feature = "otel")]
        {
            self.inner.is_some()
        }
        #[cfg(not(feature = "otel"))]
        {
            false
        }
    }

    /// `(trace id, root span id)` as hex, once the run started.
    pub fn ids(&self) -> Option<(String, String)> {
        #[cfg(feature = "otel")]
        {
            let tracer = self.inner.as_ref()?;
            Some((tracer.trace_id().to_hex(), tracer.root_span_id()?.to_hex()))
        }
        #[cfg(not(feature = "otel"))]
        {
            None
        }
    }
}

impl Telemetry {
    /// Build from flags and variables (process environment plus env file).
    /// Must be called inside the tokio runtime when OTLP export is enabled.
    pub fn from_args(args: &ServiceArgs, vars: &[(String, String)]) -> Result<Option<Arc<Self>>, CliError> {
        #[cfg(feature = "otel")]
        {
            let config = otel_config(args, vars)?;
            if config.disabled {
                return Ok(None);
            }
            let use_otlp = args.otel || config.endpoint_configured;
            if !use_otlp && !args.otel_stdout {
                return Ok(None);
            }
            let metrics = Arc::new(graphrt_otel::Metrics::new());
            let mut exporters = graphrt_otel::MultiExporter::new();
            let mut http = None;
            if use_otlp {
                let exporter = Arc::new(
                    graphrt_otel::OtlpHttpExporter::start(&config, Some(metrics.clone()))
                        .map_err(|e| CliError::failed(format!("OpenTelemetry exporter: {e}")))?,
                );
                exporters = exporters.with(exporter.clone());
                http = Some(exporter);
            }
            if args.otel_stdout {
                exporters = exporters.with(Arc::new(graphrt_otel::WriterExporter::stderr()));
            }
            Ok(Some(Arc::new(Self { exporter: Arc::new(exporters), http, metrics, capture: config.capture })))
        }
        #[cfg(not(feature = "otel"))]
        {
            let _ = vars;
            if args.otel || args.otel_stdout {
                return Err(CliError::usage("`--otel` / `--otel-stdout` require a build with the `otel` feature"));
            }
            Ok(None)
        }
    }

    /// Telemetry exporting to `exporter` (e.g. an in-memory one, for tests and
    /// embedding hosts).
    #[cfg(feature = "otel")]
    pub fn with_exporter(exporter: Arc<dyn graphrt_otel::SpanExporter>, capture: graphrt_otel::CaptureMode) -> Self {
        Self { exporter, http: None, metrics: Arc::new(graphrt_otel::Metrics::new()), capture }
    }

    /// Whether spans record content, so nodes should report it
    /// ([`graphrt_core::node::CaptureContent`]).
    pub fn captures_content(&self) -> bool {
        #[cfg(feature = "otel")]
        {
            self.capture.is_enabled()
        }
        #[cfg(not(feature = "otel"))]
        {
            false
        }
    }

    /// Tracer for one run of `graph`, optionally continuing an upstream `traceparent`.
    pub fn tracer(&self, graph: &Arc<Graph>, traceparent: Option<&str>, context: &RunContext) -> RunTracer {
        #[cfg(feature = "otel")]
        {
            let mut tracer = graphrt_otel::EventTracer::new(self.exporter.clone())
                .with_graph(graph.clone())
                .with_metrics(self.metrics.clone())
                .capture(self.capture);
            if *context != RunContext::default() {
                tracer = tracer.with_context(graphrt_otel::RunContext {
                    session_id: context.session_id.clone(),
                    user_id: context.user_id.clone(),
                    tags: context.tags.clone(),
                    metadata: context.metadata.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
                });
            }
            if let Some(tp) = traceparent {
                tracer = tracer.with_parent(tp);
            }
            RunTracer { inner: Some(tracer) }
        }
        #[cfg(not(feature = "otel"))]
        {
            let _ = (graph, traceparent, context);
            RunTracer::default()
        }
    }

    /// Record an evaluation score of a run of `workflow`: the
    /// `graphrt.eval.score` metric, and an `evaluate {name}` span (with its
    /// `gen_ai.evaluation.result` event) under the run's root span when the run
    /// was traced (`ids` are its stored trace and root span ids, as hex).
    #[cfg(feature = "otel")]
    pub fn record_evaluation(
        &self,
        ids: (Option<&str>, Option<&str>),
        workflow: &str,
        result: &graphrt_otel::EvaluationResult,
        timing: Option<(u64, u64)>,
    ) {
        if let Some(score) = result.score {
            self.metrics.record_score(&result.name, workflow, result.source.as_deref().unwrap_or_default(), score);
        }
        let Some(trace_id) = ids.0.and_then(graphrt_otel::TraceId::from_hex) else { return };
        let parent = ids.1.and_then(graphrt_otel::SpanId::from_hex);
        let (start, end) = timing.unwrap_or_else(|| {
            let now = graphrt_otel::span::now_unix_nanos();
            (now, now)
        });
        self.exporter.export(graphrt_otel::evaluation_span(trace_id, parent, result, start, end));
    }

    /// Shared metrics (tests and embedders).
    #[cfg(feature = "otel")]
    pub fn metrics(&self) -> &Arc<graphrt_otel::Metrics> {
        &self.metrics
    }

    /// Deliver queued spans and metrics.
    pub async fn shutdown(&self) {
        #[cfg(feature = "otel")]
        {
            self.exporter.flush().await;
            if let Some(http) = &self.http {
                http.shutdown().await;
                let dropped = http.dropped() + http.failed();
                if dropped > 0 {
                    eprintln!("warning: {dropped} OpenTelemetry span(s) could not be exported");
                }
            }
        }
    }
}

/// The exporter configuration of `vars`, with `--otel-preset` applied.
#[cfg(feature = "otel")]
pub fn otel_config(args: &ServiceArgs, vars: &[(String, String)]) -> Result<graphrt_otel::OtelConfig, CliError> {
    let with_preset;
    let vars = match &args.otel_preset {
        Some(name) => {
            let mut all = vars.to_vec();
            all.push((graphrt_otel::preset::PRESET_ENV.to_owned(), name.clone()));
            with_preset = all;
            &with_preset
        }
        None => vars,
    };
    graphrt_otel::OtelConfig::from_vars(vars).map_err(|e| CliError::usage(format!("OpenTelemetry configuration: {e}")))
}

/// Convenience: a tracer from optional telemetry.
pub fn tracer_for(
    telemetry: Option<&Arc<Telemetry>>,
    graph: &Arc<Graph>,
    traceparent: Option<&str>,
    context: &RunContext,
) -> RunTracer {
    telemetry.map(|t| t.tracer(graph, traceparent, context)).unwrap_or_default()
}

/// Whether `telemetry` records content.
pub fn captures_content(telemetry: Option<&Arc<Telemetry>>) -> bool {
    telemetry.is_some_and(|t| t.captures_content())
}
