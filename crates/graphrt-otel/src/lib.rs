//! # graphrt-otel
//!
//! OpenTelemetry for graphrt runs, without the OpenTelemetry SDK: an
//! [`EventTracer`] turns run [`graphrt_core::Event`]s into spans (one per run,
//! one per node execution, one per tool call, one per container body run) that
//! follow the OpenTelemetry GenAI semantic conventions (`invoke_workflow`,
//! `chat`, `invoke_agent`, `execute_tool`, `retrieval`; names in [`semconv`]),
//! and exporters ship them as OTLP/HTTP JSON. Evaluation scores become
//! `evaluate {name}` spans with a `gen_ai.evaluation.result` event
//! ([`evaluation`]), and [`preset`]s fill vendor endpoints and headers
//! (Langfuse, LangSmith, Braintrust, Opik, Phoenix, MLflow) from their
//! environment variables.
//!
//! ```no_run
//! # async fn demo(graph: std::sync::Arc<graphrt_core::Graph>, mut run: graphrt_core::RunStream) {
//! use std::sync::Arc;
//! use futures::StreamExt;
//! use graphrt_otel::{EventTracer, OtelConfig, OtlpHttpExporter};
//!
//! let config = OtelConfig::from_env().expect("valid OTEL_* variables");
//! let exporter = Arc::new(OtlpHttpExporter::start(&config, None).expect("inside tokio"));
//! let mut tracer = EventTracer::new(exporter.clone()).with_graph(graph);
//! while let Some(event) = run.next().await {
//!     tracer.on_event(&event);
//! }
//! exporter.shutdown().await;
//! # }
//! ```
//!
//! Content (run input and output, model messages, tool arguments and results,
//! retrieval queries and documents, node outputs) is not recorded unless
//! [`EventTracer::capture`] enables it ([`CaptureMode`], from
//! `OTEL_INSTRUMENTATION_GENAI_CAPTURE_CONTENT`); secrets are always masked.
//! Model nodes only report their rendered messages when the host also registers
//! the [`graphrt_core::node::CaptureContent`] service.

pub mod config;
pub mod evaluation;
pub mod exporter;
#[cfg(feature = "http")]
pub mod http;
pub mod metrics;
pub mod otlp;
pub mod preset;
pub mod semconv;
pub mod span;
pub mod tracer;

pub use config::{CaptureMode, ConfigError, OtelConfig};
pub use evaluation::{EvaluationResult, evaluation_span};
pub use exporter::{InMemoryExporter, MultiExporter, SpanExporter, StdoutExporter, WriterExporter};
#[cfg(feature = "http")]
pub use http::{ExportError, OtlpHttpExporter};
pub use metrics::Metrics;
pub use otlp::Resource;
pub use span::{AttrValue, Span, SpanId, StatusCode, TraceId, TraceParent};
pub use tracer::{EventTracer, RunContext};
