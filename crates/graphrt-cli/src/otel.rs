//! OpenTelemetry wiring for `run` and `serve`.
//!
//! Enabled by `--otel` (OTLP/HTTP to `$OTEL_EXPORTER_OTLP_ENDPOINT`, default
//! `http://localhost:4318`), automatically when an OTLP endpoint variable is set,
//! or `--otel-stdout` (span JSON lines on stderr). `OTEL_SDK_DISABLED=true` turns
//! everything off. Without the `otel` feature these flags are rejected.

use std::sync::Arc;

use graphrt_core::{Event, Graph};

use crate::args::ServiceArgs;
use crate::error::CliError;

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
    capture_content: bool,
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
}

impl Telemetry {
    /// Build from flags and variables (process environment plus env file).
    /// Must be called inside the tokio runtime when OTLP export is enabled.
    pub fn from_args(args: &ServiceArgs, vars: &[(String, String)]) -> Result<Option<Arc<Self>>, CliError> {
        #[cfg(feature = "otel")]
        {
            let config = graphrt_otel::OtelConfig::from_vars(vars)
                .map_err(|e| CliError::usage(format!("OpenTelemetry configuration: {e}")))?;
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
            Ok(Some(Arc::new(Self {
                exporter: Arc::new(exporters),
                http,
                metrics,
                capture_content: config.capture.is_enabled(),
            })))
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

    /// Tracer for one run of `graph`, optionally continuing an upstream `traceparent`.
    pub fn tracer(&self, graph: &Arc<Graph>, traceparent: Option<&str>) -> RunTracer {
        #[cfg(feature = "otel")]
        {
            let mut tracer = graphrt_otel::EventTracer::new(self.exporter.clone())
                .with_graph(graph.clone())
                .with_metrics(self.metrics.clone())
                .capture_content(self.capture_content);
            if let Some(tp) = traceparent {
                tracer = tracer.with_parent(tp);
            }
            RunTracer { inner: Some(tracer) }
        }
        #[cfg(not(feature = "otel"))]
        {
            let _ = (graph, traceparent);
            RunTracer::default()
        }
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

/// Convenience: a tracer from optional telemetry.
pub fn tracer_for(telemetry: Option<&Arc<Telemetry>>, graph: &Arc<Graph>, traceparent: Option<&str>) -> RunTracer {
    telemetry.map(|t| t.tracer(graph, traceparent)).unwrap_or_default()
}
