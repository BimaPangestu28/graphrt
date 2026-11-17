//! Span exporters: the [`SpanExporter`] contract plus in-memory, writer (stdout /
//! stderr) and fan-out implementations.

use std::fmt;
use std::io::Write;
use std::sync::{Arc, Mutex, PoisonError};

use futures::future::BoxFuture;

use crate::otlp;
use crate::span::Span;

/// Receives finished spans. `export` must never block the run: implementations
/// queue or write quickly and drop data when overloaded.
pub trait SpanExporter: Send + Sync + fmt::Debug {
    fn export(&self, span: Span);

    /// Deliver everything queued so far (and current metrics, when supported).
    fn flush(&self) -> BoxFuture<'_, ()> {
        Box::pin(futures::future::ready(()))
    }
}

/// Keeps spans in memory. For tests and embedding hosts that ship spans themselves.
#[derive(Debug, Default, Clone)]
pub struct InMemoryExporter {
    spans: Arc<Mutex<Vec<Span>>>,
}

impl InMemoryExporter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Spans exported so far, in export order.
    pub fn spans(&self) -> Vec<Span> {
        self.spans.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    /// Remove and return the exported spans.
    pub fn take(&self) -> Vec<Span> {
        std::mem::take(&mut *self.spans.lock().unwrap_or_else(PoisonError::into_inner))
    }
}

impl SpanExporter for InMemoryExporter {
    fn export(&self, span: Span) {
        self.spans.lock().unwrap_or_else(PoisonError::into_inner).push(span);
    }
}

/// Writes each span as one OTLP-JSON line to a writer (stdout / stderr by default).
pub struct WriterExporter {
    out: Mutex<Box<dyn Write + Send>>,
}

/// Alias kept for discoverability: the debugging exporter.
pub type StdoutExporter = WriterExporter;

impl fmt::Debug for WriterExporter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("WriterExporter")
    }
}

impl WriterExporter {
    pub fn new(out: Box<dyn Write + Send>) -> Self {
        Self { out: Mutex::new(out) }
    }

    pub fn stdout() -> Self {
        Self::new(Box::new(std::io::stdout()))
    }

    /// Stderr, so span lines never mix with a run's answer on stdout.
    pub fn stderr() -> Self {
        Self::new(Box::new(std::io::stderr()))
    }
}

impl SpanExporter for WriterExporter {
    fn export(&self, span: Span) {
        let line = otlp::span(&span).to_string();
        let mut out = self.out.lock().unwrap_or_else(PoisonError::into_inner);
        // Debug output: a failed write is not worth failing the run over.
        let _ignored = writeln!(out, "{line}");
    }

    fn flush(&self) -> BoxFuture<'_, ()> {
        let _ignored = self.out.lock().unwrap_or_else(PoisonError::into_inner).flush();
        Box::pin(futures::future::ready(()))
    }
}

/// Sends every span to several exporters.
#[derive(Debug, Default, Clone)]
pub struct MultiExporter {
    exporters: Vec<Arc<dyn SpanExporter>>,
}

impl MultiExporter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with(mut self, exporter: Arc<dyn SpanExporter>) -> Self {
        self.exporters.push(exporter);
        self
    }

    pub fn is_empty(&self) -> bool {
        self.exporters.is_empty()
    }
}

impl SpanExporter for MultiExporter {
    fn export(&self, span: Span) {
        if let Some((last, rest)) = self.exporters.split_last() {
            for e in rest {
                e.export(span.clone());
            }
            last.export(span);
        }
    }

    fn flush(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            for e in &self.exporters {
                e.flush().await;
            }
        })
    }
}
