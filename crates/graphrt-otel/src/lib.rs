//! # graphrt-otel
//!
//! OpenTelemetry for graphrt runs, without the OpenTelemetry SDK: an
//! [`EventTracer`] turns run [`graphrt_core::Event`]s into spans (one per run,
//! one per node execution, one per container body run) with GenAI semantic
//! convention attributes, and exporters ship them as OTLP/HTTP JSON.
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
//! Content (node outputs, progress payloads) is not recorded unless
//! [`EventTracer::capture_content`] is enabled; secrets are always masked.

pub mod config;
pub mod exporter;
#[cfg(feature = "http")]
pub mod http;
pub mod metrics;
pub mod otlp;
pub mod span;
pub mod tracer;

pub use config::{ConfigError, OtelConfig};
pub use exporter::{InMemoryExporter, MultiExporter, SpanExporter, StdoutExporter, WriterExporter};
#[cfg(feature = "http")]
pub use http::{ExportError, OtlpHttpExporter};
pub use metrics::Metrics;
pub use otlp::Resource;
pub use span::{AttrValue, Span, SpanId, StatusCode, TraceId, TraceParent};
pub use tracer::EventTracer;
