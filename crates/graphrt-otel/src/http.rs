//! OTLP/HTTP JSON exporter with batching, a bounded queue and retries.
//!
//! `export` never blocks: spans go into a bounded channel and are dropped (and
//! counted) when it is full. A background tokio task batches spans by size and
//! interval, POSTs them to `.../v1/traces`, retries 429/502/503/504 and network
//! errors with backoff (honoring `Retry-After`), and periodically pushes metrics.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use futures::future::BoxFuture;
use tokio::sync::{mpsc, oneshot};

use crate::config::OtelConfig;
use crate::exporter::SpanExporter;
use crate::metrics::Metrics;
use crate::otlp::{self, Resource};
use crate::span::Span;

/// Exporter construction error.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ExportError {
    #[error("the OTLP exporter needs a running tokio runtime")]
    NoRuntime,
    #[error("cannot build HTTP client: {0}")]
    Client(String),
}

/// Tuning knobs.
#[derive(Clone, Debug)]
pub struct ExportSettings {
    /// Spans per request.
    pub batch_size: usize,
    /// Spans buffered before new ones are dropped.
    pub queue_capacity: usize,
    /// Maximum delay before a partial batch is sent.
    pub flush_interval: Duration,
    /// Interval between metric pushes.
    pub metrics_interval: Duration,
    /// Per-request timeout.
    pub timeout: Duration,
    /// Retries after the first attempt for retryable failures.
    pub max_retries: u32,
    /// First retry delay (doubled each retry, capped at 30 s).
    pub initial_backoff: Duration,
}

impl Default for ExportSettings {
    fn default() -> Self {
        Self {
            batch_size: 512,
            queue_capacity: 4096,
            flush_interval: Duration::from_secs(2),
            metrics_interval: Duration::from_secs(30),
            timeout: Duration::from_secs(10),
            max_retries: 3,
            initial_backoff: Duration::from_millis(500),
        }
    }
}

enum Msg {
    Span(Box<Span>),
    Flush(oneshot::Sender<()>),
    Shutdown(oneshot::Sender<()>),
}

#[derive(Debug, Default)]
struct Counters {
    dropped: AtomicU64,
    exported: AtomicU64,
    failed: AtomicU64,
}

/// OTLP/HTTP JSON span (and metric) exporter.
#[derive(Debug)]
pub struct OtlpHttpExporter {
    tx: mpsc::Sender<Msg>,
    counters: Arc<Counters>,
}

impl OtlpHttpExporter {
    /// Start the exporter (spawns its batching task on the current tokio runtime).
    pub fn start(config: &OtelConfig, metrics: Option<Arc<Metrics>>) -> Result<Self, ExportError> {
        Self::start_with(config, metrics, ExportSettings::default())
    }

    /// [`OtlpHttpExporter::start`] with explicit settings.
    pub fn start_with(
        config: &OtelConfig,
        metrics: Option<Arc<Metrics>>,
        settings: ExportSettings,
    ) -> Result<Self, ExportError> {
        let handle = tokio::runtime::Handle::try_current().map_err(|_| ExportError::NoRuntime)?;
        install_crypto_provider();
        let client = reqwest::Client::builder()
            .timeout(settings.timeout)
            .no_proxy()
            .build()
            .map_err(|e| ExportError::Client(e.without_url().to_string()))?;
        let (tx, rx) = mpsc::channel(settings.queue_capacity.max(1));
        let counters = Arc::new(Counters::default());
        let worker = Worker {
            client,
            traces_url: config.traces_endpoint.clone(),
            metrics_url: config.metrics_endpoint.clone(),
            headers: config.headers.clone(),
            resource: config.resource.clone(),
            metrics,
            settings,
            counters: counters.clone(),
        };
        handle.spawn(worker.run(rx));
        Ok(Self { tx, counters })
    }

    /// Spans dropped because the queue was full or the exporter stopped.
    pub fn dropped(&self) -> u64 {
        self.counters.dropped.load(Ordering::Relaxed)
    }

    /// Spans delivered successfully.
    pub fn exported(&self) -> u64 {
        self.counters.exported.load(Ordering::Relaxed)
    }

    /// Spans lost after exhausting retries or on non-retryable errors.
    pub fn failed(&self) -> u64 {
        self.counters.failed.load(Ordering::Relaxed)
    }

    /// Send everything queued, push metrics, and stop the background task.
    pub async fn shutdown(&self) {
        let (done, wait) = oneshot::channel();
        if self.tx.send(Msg::Shutdown(done)).await.is_ok() {
            let _closed = wait.await;
        }
    }
}

impl SpanExporter for OtlpHttpExporter {
    fn export(&self, span: Span) {
        if self.tx.try_send(Msg::Span(Box::new(span))).is_err() {
            self.counters.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn flush(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            let (done, wait) = oneshot::channel();
            if self.tx.send(Msg::Flush(done)).await.is_ok() {
                let _closed = wait.await;
            }
        })
    }
}

struct Worker {
    client: reqwest::Client,
    traces_url: String,
    metrics_url: String,
    headers: Vec<(String, String)>,
    resource: Resource,
    metrics: Option<Arc<Metrics>>,
    settings: ExportSettings,
    counters: Arc<Counters>,
}

impl Worker {
    async fn run(self, mut rx: mpsc::Receiver<Msg>) {
        let mut batch: Vec<Span> = Vec::new();
        // `interval_at` skips the immediate first tick `interval` would fire.
        let start = tokio::time::Instant::now();
        let mut flush_tick =
            tokio::time::interval_at(start + self.settings.flush_interval, self.settings.flush_interval);
        let mut metrics_tick =
            tokio::time::interval_at(start + self.settings.metrics_interval, self.settings.metrics_interval);
        flush_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        metrics_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                msg = rx.recv() => match msg {
                    Some(Msg::Span(span)) => {
                        batch.push(*span);
                        if batch.len() >= self.settings.batch_size {
                            self.send_spans(std::mem::take(&mut batch)).await;
                        }
                    }
                    Some(Msg::Flush(done)) => {
                        self.send_spans(std::mem::take(&mut batch)).await;
                        self.send_metrics().await;
                        let _receiver_gone = done.send(());
                    }
                    Some(Msg::Shutdown(done)) => {
                        rx.close();
                        while let Ok(Msg::Span(span)) = rx.try_recv() {
                            batch.push(*span);
                        }
                        self.send_spans(std::mem::take(&mut batch)).await;
                        self.send_metrics().await;
                        let _receiver_gone = done.send(());
                        return;
                    }
                    None => {
                        self.send_spans(std::mem::take(&mut batch)).await;
                        self.send_metrics().await;
                        return;
                    }
                },
                _ = flush_tick.tick() => {
                    if !batch.is_empty() {
                        self.send_spans(std::mem::take(&mut batch)).await;
                    }
                }
                _ = metrics_tick.tick() => self.send_metrics().await,
            }
        }
    }

    async fn send_spans(&self, spans: Vec<Span>) {
        if spans.is_empty() {
            return;
        }
        for chunk in spans.chunks(self.settings.batch_size.max(1)) {
            let body = otlp::traces_request(&self.resource, chunk);
            let n = u64::try_from(chunk.len()).unwrap_or(u64::MAX);
            if self.post(&self.traces_url, &body).await {
                self.counters.exported.fetch_add(n, Ordering::Relaxed);
            } else {
                self.counters.failed.fetch_add(n, Ordering::Relaxed);
            }
        }
    }

    async fn send_metrics(&self) {
        let Some(metrics) = &self.metrics else { return };
        let encoded = metrics.encode();
        if encoded.is_empty() {
            return;
        }
        let body = otlp::metrics_request(&self.resource, encoded);
        // Metrics are cumulative: a failed push is superseded by the next one.
        let _delivered = self.post(&self.metrics_url, &body).await;
    }

    /// POST with retries. Returns whether the collector accepted the request.
    async fn post(&self, url: &str, body: &serde_json::Value) -> bool {
        let Ok(bytes) = serde_json::to_vec(body) else { return false };
        let mut delay = self.settings.initial_backoff;
        for attempt in 0..=self.settings.max_retries {
            let mut request = self.client.post(url).header("content-type", "application/json").body(bytes.clone());
            for (k, v) in &self.headers {
                request = request.header(k.as_str(), v.as_str());
            }
            let retry_after = match request.send().await {
                Ok(response) if response.status().is_success() => return true,
                Ok(response) => {
                    let status = response.status().as_u16();
                    if !matches!(status, 429 | 502 | 503 | 504) {
                        return false;
                    }
                    response
                        .headers()
                        .get("retry-after")
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| v.trim().parse::<u64>().ok())
                        .map(Duration::from_secs)
                }
                Err(_) => None,
            };
            if attempt == self.settings.max_retries {
                break;
            }
            let wait = retry_after.unwrap_or(delay).min(Duration::from_secs(30));
            tokio::time::sleep(wait).await;
            delay = (delay * 2).min(Duration::from_secs(30));
        }
        false
    }
}

fn install_crypto_provider() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let _already_installed = rustls::crypto::ring::default_provider().install_default();
    });
}
