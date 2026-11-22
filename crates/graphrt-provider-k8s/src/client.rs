//! Minimal Kubernetes REST client on the shared provider HTTP layer
//! ([`ProviderHttp`]: token bucket per API server, jittered retry of
//! 429 / 5xx / transport errors, one token refresh on 401). This module adds
//! what is Kubernetes-specific: the cluster CA and client certificate on the
//! TLS config, Kubernetes status classification, and newline-delimited JSON
//! watch streams.
//!
//! Every operation the provider sends is idempotent (CAS scale patches with a
//! `resourceVersion`, annotation merge patches, server-side apply), so all of
//! them may be retried. 409 Conflict is not retried here: the caller re-reads
//! and retries the compare-and-swap itself.

use std::sync::Arc;
use std::time::Duration;

use graphrt_provider_api::http::{ProviderHttp, RetryPolicy, TokenBucket, TokenSource};
use graphrt_provider_api::{ProviderError, ProviderErrorKind};
use reqwest::Method;

use crate::auth::ClusterConfig;

/// Tuning of the REST client.
#[derive(Debug, Clone)]
pub struct ClientOptions {
    /// Sustained requests per second to the API server (client-go default is 5).
    pub qps: f64,
    /// Burst above `qps`.
    pub burst: u32,
    /// Retries of 429 / 5xx / transport errors.
    pub retry: RetryPolicy,
    /// Timeout of one non-watch request.
    pub request_timeout: Duration,
}

impl Default for ClientOptions {
    fn default() -> Self {
        Self {
            qps: 20.0,
            burst: 40,
            retry: RetryPolicy {
                max_attempts: 4,
                base_delay: Duration::from_millis(100),
                max_delay: Duration::from_secs(2),
            },
            request_timeout: Duration::from_secs(10),
        }
    }
}

/// Kubernetes status → error kind. 409 is `Fatal` so the shared retry loop
/// leaves compare-and-swap conflicts to the caller; a 403 for an exceeded
/// `ResourceQuota` is `Capacity`.
fn classify(status: u16, body: &str) -> ProviderErrorKind {
    match status {
        409 => ProviderErrorKind::Fatal,
        403 if body.contains("exceeded quota") => ProviderErrorKind::Capacity,
        _ => ProviderError::kind_for_status(status),
    }
}

/// HTTP status of an error returned by the API server (`None` for transport
/// errors). `ProviderHttp` messages start with `HTTP <status>:`.
pub fn http_status(error: &ProviderError) -> Option<u16> {
    error.message.strip_prefix("HTTP ")?.get(..3)?.parse().ok()
}

/// Body of a request: content type and bytes.
#[derive(Debug, Clone)]
pub struct RequestBody {
    pub content_type: &'static str,
    pub bytes: Vec<u8>,
}

impl RequestBody {
    /// JSON merge patch (`application/merge-patch+json`).
    pub fn merge_patch(value: &serde_json::Value) -> Self {
        Self { content_type: "application/merge-patch+json", bytes: value.to_string().into_bytes() }
    }

    /// Server-side apply (`application/apply-patch+yaml`; JSON is valid YAML).
    pub fn apply_patch(value: &serde_json::Value) -> Self {
        Self { content_type: "application/apply-patch+yaml", bytes: value.to_string().into_bytes() }
    }
}

/// Client for one API server.
#[derive(Clone)]
pub struct KubeClient {
    inner: Arc<ClientInner>,
}

struct ClientInner {
    http: ProviderHttp,
    server: String,
    tokens: Option<Arc<dyn TokenSource>>,
    request_timeout: Duration,
}

impl std::fmt::Debug for KubeClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("KubeClient")
            .field("server", &self.inner.server)
            .field("bearer_auth", &self.inner.tokens.is_some())
            .finish_non_exhaustive()
    }
}

impl KubeClient {
    /// Build a client from resolved cluster settings.
    ///
    /// @throws ProviderError(Fatal) - bad CA / identity PEM, or a server URL that is not http(s)
    pub fn new(config: &ClusterConfig, options: ClientOptions) -> Result<Self, ProviderError> {
        let server = config.server.trim_end_matches('/').to_owned();
        if !(server.starts_with("https://") || server.starts_with("http://")) {
            return Err(ProviderError::fatal(format!("server {server:?} is not an http(s) URL")));
        }
        let _already_installed = rustls::crypto::ring::default_provider().install_default();
        // No total timeout on the client: watches are long-lived. Requests set their own.
        let mut builder = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .pool_idle_timeout(Duration::from_secs(90))
            // Never follow redirects: the bearer token must only reach the API server.
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!("graphrt-gateway/", env!("CARGO_PKG_VERSION")));
        if let Some(ca_pem) = &config.ca_pem {
            let certificates = reqwest::Certificate::from_pem_bundle(ca_pem)
                .map_err(|error| ProviderError::fatal(format!("cluster CA: {error}")))?;
            builder = builder.tls_certs_only(certificates);
        }
        if let Some(identity_pem) = &config.client_identity_pem {
            let identity = reqwest::Identity::from_pem(identity_pem.expose().as_bytes())
                .map_err(|_| ProviderError::fatal("client certificate / key is not valid PEM"))?;
            builder = builder.identity(identity);
        }
        let client = builder.build().map_err(|error| ProviderError::fatal(format!("HTTP client: {error}")))?;
        let bucket = Arc::new(TokenBucket::new(options.qps, options.burst));
        let http = ProviderHttp::new(client, bucket).with_policy(options.retry).with_classifier(classify);
        Ok(Self {
            inner: Arc::new(ClientInner {
                http,
                server,
                tokens: config.token_source.clone(),
                request_timeout: options.request_timeout,
            }),
        })
    }

    async fn send_raw(
        &self,
        method: &Method,
        path: &str,
        body: Option<&RequestBody>,
        timeout: Duration,
    ) -> Result<reqwest::Response, ProviderError> {
        let url = format!("{}{}", self.inner.server, path);
        let build = |client: &reqwest::Client| {
            let request = client.request(method.clone(), &url).timeout(timeout).header("Accept", "application/json");
            match body {
                Some(body) => request.header("Content-Type", body.content_type).body(body.bytes.clone()),
                None => request,
            }
        };
        self.inner.http.send(build, self.inner.tokens.as_deref()).await.map_err(|mut error| {
            // Keep the `HTTP <status>:` prefix first (see `http_status`), add the call.
            error.message = format!("{} ({method} {path})", error.message);
            error
        })
    }

    /// Send a request (retrying transient failures) and return the success body.
    pub async fn send(&self, method: Method, path: &str, body: Option<RequestBody>) -> Result<Vec<u8>, ProviderError> {
        let response = self.send_raw(&method, path, body.as_ref(), self.inner.request_timeout).await?;
        let bytes = response
            .bytes()
            .await
            .map_err(|error| ProviderError::transient(format!("read body of {method} {path}: {error}")))?;
        Ok(bytes.to_vec())
    }

    /// `GET` and parse the JSON body.
    pub async fn get(&self, path: &str) -> Result<serde_json::Value, ProviderError> {
        let bytes = self.send(Method::GET, path, None).await?;
        serde_json::from_slice(&bytes).map_err(|error| ProviderError::fatal(format!("decode {path}: {error}")))
    }

    /// Open a watch (`?watch=1&timeoutSeconds=N` must be in `path`).
    pub async fn watch(&self, path: &str, timeout: Duration) -> Result<WatchStream, ProviderError> {
        let response = self.send_raw(&Method::GET, path, None, timeout + Duration::from_secs(10)).await?;
        Ok(WatchStream { response, buffer: Vec::new(), path: path.to_owned() })
    }
}

/// One event of a watch stream.
#[derive(Debug, Clone, PartialEq)]
pub struct WatchEvent {
    /// `ADDED`, `MODIFIED`, `DELETED`, `BOOKMARK` or `ERROR`.
    pub kind: String,
    pub object: serde_json::Value,
}

/// Newline-delimited JSON watch events, read chunk by chunk.
#[derive(Debug)]
pub struct WatchStream {
    response: reqwest::Response,
    buffer: Vec<u8>,
    path: String,
}

impl WatchStream {
    /// The next event, or `None` when the server closed the watch.
    pub async fn next(&mut self) -> Result<Option<WatchEvent>, ProviderError> {
        loop {
            if let Some(newline) = self.buffer.iter().position(|byte| *byte == b'\n') {
                let line: Vec<u8> = self.buffer.drain(..=newline).collect();
                let line = line.trim_ascii();
                if line.is_empty() {
                    continue;
                }
                let mut event: serde_json::Value = serde_json::from_slice(line)
                    .map_err(|error| ProviderError::fatal(format!("decode watch {}: {error}", self.path)))?;
                let kind = event.get("type").and_then(serde_json::Value::as_str).unwrap_or_default().to_owned();
                let object = event.get_mut("object").map(serde_json::Value::take).unwrap_or_default();
                return Ok(Some(WatchEvent { kind, object }));
            }
            match self.response.chunk().await {
                Ok(Some(chunk)) => self.buffer.extend_from_slice(&chunk),
                // A body without a trailing newline still ends with one event.
                Ok(None) if !self.buffer.trim_ascii().is_empty() => self.buffer.push(b'\n'),
                Ok(None) => return Ok(None),
                Err(error) => return Err(ProviderError::transient(format!("watch {} dropped: {error}", self.path))),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_kubernetes_statuses() {
        assert_eq!(classify(409, ""), ProviderErrorKind::Fatal, "CAS conflicts are the caller's to retry");
        assert_eq!(
            classify(403, r#"{"message":"pods \"x\" is forbidden: exceeded quota: q"}"#),
            ProviderErrorKind::Capacity
        );
        assert_eq!(classify(403, "forbidden"), ProviderErrorKind::Auth);
        assert_eq!(classify(429, ""), ProviderErrorKind::RateLimited);
        assert_eq!(classify(503, ""), ProviderErrorKind::Transient);
        assert_eq!(classify(422, ""), ProviderErrorKind::Fatal);
    }

    #[test]
    fn reads_the_status_back_from_errors() {
        assert_eq!(http_status(&ProviderError::fatal("HTTP 409: conflict (PATCH /x)")), Some(409));
        assert_eq!(http_status(&ProviderError::transient("connect error for http://k")), None);
    }

    #[test]
    fn rejects_non_http_servers() {
        let error = KubeClient::new(&ClusterConfig::new("ftp://k8s"), ClientOptions::default()).unwrap_err();
        assert_eq!(error.kind, ProviderErrorKind::Fatal);
    }
}
