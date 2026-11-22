//! Gateway → agent calls of protocol 1 (`/internal/v1/*`, plan §3.1), over
//! HTTP/1.1 keep-alive on a Unix socket or a private network, every request
//! signed with the deployment's key. Agent addresses come only from the
//! provider (never from the agent itself, plan §3.2).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use graphrt_provider_api::{Endpoint, EndpointAddress, InvokeRequest, Provider};

use crate::signature::{
    DEPLOYMENT_HEADER, EPOCH_HEADER, GatewayKey, PROTOCOL_HEADER, PROTOCOL_VERSION, RUN_ID_HEADER, SIGNATURE_HEADER,
    Signed, sign, unix_now,
};
use crate::sse::{Frame, Parser, SseError};

/// Failure talking to an agent.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AgentError {
    /// Connection refused / reset / timeout: the replica may be gone.
    #[error("agent unreachable: {0}")]
    Unreachable(String),
    /// The agent answered with an error status (`409 epoch_conflict`, `503 draining`...).
    #[error("agent refused ({status}): {body}")]
    Refused {
        /// HTTP status.
        status: u16,
        /// Body excerpt (a protocol error JSON).
        body: String,
    },
    /// The stream broke or was malformed.
    #[error("agent stream broke: {0}")]
    Stream(String),
    /// The platform ended the request at its own limit (Lambda): the run can
    /// neither be preempted nor continued elsewhere.
    #[error("platform timeout: {0}")]
    PlatformTimeout(String),
}

/// An invoke-transport error as an [`AgentError`]; `broken` is used for
/// every kind other than `PlatformTimeout`.
fn invoke_error(error: &graphrt_provider_api::ProviderError, broken: fn(String) -> AgentError) -> AgentError {
    match error.kind {
        graphrt_provider_api::ProviderErrorKind::PlatformTimeout => AgentError::PlatformTimeout(error.message.clone()),
        _ => broken(error.to_string()),
    }
}

impl From<SseError> for AgentError {
    fn from(error: SseError) -> Self {
        Self::Stream(error.to_string())
    }
}

/// Who a request is for: run id and epoch, both optional.
#[derive(Debug, Clone, Copy, Default)]
pub struct RunScope<'a> {
    /// Run id.
    pub run_id: &'a str,
    /// Epoch.
    pub epoch: Option<u64>,
}

/// Signed client of the agents of all deployments.
#[derive(Debug, Default)]
pub struct AgentClient {
    clients: Mutex<HashMap<EndpointAddress, reqwest::Client>>,
}

enum StreamSource {
    Http(reqwest::Response),
    Invoke(graphrt_provider_api::ByteStream),
}

/// An SSE stream from an agent (over HTTP, or a provider's own transport).
pub struct AgentStream {
    source: StreamSource,
    parser: Parser,
    pending: std::collections::VecDeque<Frame>,
}

impl std::fmt::Debug for AgentStream {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("AgentStream").finish_non_exhaustive()
    }
}

impl AgentStream {
    fn http(response: reqwest::Response) -> Self {
        Self { source: StreamSource::Http(response), parser: Parser::default(), pending: Default::default() }
    }

    /// The next frame; `Ok(None)` when the agent closed the stream.
    ///
    /// # Errors
    /// [`AgentError::Stream`] on a broken connection or oversized event.
    pub async fn next_frame(&mut self) -> Result<Option<Frame>, AgentError> {
        use futures::StreamExt as _;
        loop {
            if let Some(frame) = self.pending.pop_front() {
                return Ok(Some(frame));
            }
            let chunk = match &mut self.source {
                StreamSource::Http(response) => response
                    .chunk()
                    .await
                    .map(|chunk| chunk.map(|bytes| bytes.to_vec()))
                    .map_err(|error| AgentError::Stream(error.to_string())),
                StreamSource::Invoke(stream) => {
                    stream.next().await.transpose().map_err(|error| invoke_error(&error, AgentError::Stream))
                }
            };
            match chunk {
                Ok(Some(chunk)) => self.pending.extend(self.parser.feed(&chunk)?),
                Ok(None) => return Ok(None),
                Err(error) => return Err(error),
            }
        }
    }
}

fn base_url(address: &EndpointAddress) -> String {
    match address {
        // The host is ignored on a Unix socket; any valid one will do.
        EndpointAddress::Unix(_) => "http://agent".to_owned(),
        EndpointAddress::Http(base) => base.trim_end_matches('/').to_owned(),
    }
}

impl AgentClient {
    fn client(&self, address: &EndpointAddress) -> Result<reqwest::Client, AgentError> {
        let mut clients = self.clients.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(client) = clients.get(address) {
            return Ok(client.clone());
        }
        let mut builder = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .pool_idle_timeout(Duration::from_secs(90))
            .http1_only();
        if let EndpointAddress::Unix(path) = address {
            builder = builder.unix_socket(path.as_path());
        }
        let client = builder.build().map_err(|error| AgentError::Unreachable(error.to_string()))?;
        if clients.len() > 4096 {
            clients.clear();
        }
        clients.insert(address.clone(), client.clone());
        Ok(client)
    }

    /// Forgets the connection pool of a replica that went away.
    pub fn forget(&self, address: &EndpointAddress) {
        self.clients.lock().unwrap_or_else(std::sync::PoisonError::into_inner).remove(address);
    }

    // One private call site per protocol operation; a parameter struct would only rename them.
    #[allow(clippy::too_many_arguments)]
    async fn send(
        &self,
        address: &EndpointAddress,
        key: &GatewayKey,
        deployment: &str,
        method: reqwest::Method,
        path: &str,
        scope: RunScope<'_>,
        body: Vec<u8>,
    ) -> Result<reqwest::Response, AgentError> {
        let client = self.client(address)?;
        let epoch = scope.epoch.map(|epoch| epoch.to_string()).unwrap_or_default();
        let signed =
            Signed { deployment, method: method.as_str(), path, run_id: scope.run_id, epoch: &epoch, body: &body };
        let mut request = client
            .request(method.clone(), format!("{}{path}", base_url(address)))
            .header(PROTOCOL_HEADER, PROTOCOL_VERSION.to_string())
            .header(SIGNATURE_HEADER, sign(key, unix_now(), &signed))
            .header(DEPLOYMENT_HEADER, deployment);
        if !scope.run_id.is_empty() {
            request = request.header(RUN_ID_HEADER, scope.run_id);
        }
        if !epoch.is_empty() {
            request = request.header(EPOCH_HEADER, &epoch);
        }
        if !body.is_empty() {
            request = request.header("content-type", "application/json").body(body);
        }
        let response = request.send().await.map_err(|error| AgentError::Unreachable(error.to_string()))?;
        if response.status().is_success() {
            return Ok(response);
        }
        let status = response.status().as_u16();
        let text = response.text().await.unwrap_or_default();
        Err(AgentError::Refused { status, body: text.chars().take(512).collect() })
    }

    /// `POST /internal/v1/runs`: start or resume run `scope` and stream it.
    ///
    /// # Errors
    /// See [`AgentError`].
    pub async fn start_run(
        &self,
        address: &EndpointAddress,
        key: &GatewayKey,
        deployment: &str,
        scope: RunScope<'_>,
        body: &serde_json::Value,
    ) -> Result<AgentStream, AgentError> {
        let bytes = serde_json::to_vec(body).map_err(|error| AgentError::Stream(error.to_string()))?;
        let response =
            self.send(address, key, deployment, reqwest::Method::POST, "/internal/v1/runs", scope, bytes).await?;
        Ok(AgentStream::http(response))
    }

    /// `POST /internal/v1/runs` through `provider`'s own transport when it has
    /// one ([`Provider::invoke`], e.g. Lambda), else over HTTP.
    ///
    /// # Errors
    /// See [`AgentError`].
    pub async fn start_run_on(
        &self,
        provider: &dyn Provider,
        endpoint: &Endpoint,
        key: &GatewayKey,
        deployment: &str,
        scope: RunScope<'_>,
        body: &serde_json::Value,
    ) -> Result<AgentStream, AgentError> {
        let bytes = serde_json::to_vec(body).map_err(|error| AgentError::Stream(error.to_string()))?;
        let path = "/internal/v1/runs";
        let epoch = scope.epoch.map(|epoch| epoch.to_string()).unwrap_or_default();
        let signed = Signed { deployment, method: "POST", path, run_id: scope.run_id, epoch: &epoch, body: &bytes };
        let headers = vec![
            (PROTOCOL_HEADER.to_owned(), PROTOCOL_VERSION.to_string()),
            (SIGNATURE_HEADER.to_owned(), sign(key, unix_now(), &signed)),
            (DEPLOYMENT_HEADER.to_owned(), deployment.to_owned()),
            (RUN_ID_HEADER.to_owned(), scope.run_id.to_owned()),
            (EPOCH_HEADER.to_owned(), epoch.clone()),
            ("content-type".to_owned(), "application/json".to_owned()),
        ];
        let request = InvokeRequest { method: "POST".into(), path: path.into(), headers, body: bytes };
        let Some(invocation) = provider.invoke(endpoint, request) else {
            return self.start_run(&endpoint.address, key, deployment, scope, body).await;
        };
        let response = invocation.await.map_err(|error| invoke_error(&error, AgentError::Unreachable))?;
        if !(200..300).contains(&response.status) {
            return Err(AgentError::Refused { status: response.status, body: String::new() });
        }
        Ok(AgentStream {
            source: StreamSource::Invoke(response.body),
            parser: Parser::default(),
            pending: Default::default(),
        })
    }

    /// `GET /internal/v1/runs/{id}/stream?from=N`: reattach after a lost connection.
    ///
    /// # Errors
    /// See [`AgentError`].
    pub async fn reattach(
        &self,
        address: &EndpointAddress,
        key: &GatewayKey,
        deployment: &str,
        scope: RunScope<'_>,
        from: u64,
    ) -> Result<AgentStream, AgentError> {
        let path = format!("/internal/v1/runs/{}/stream?from={from}", scope.run_id);
        let response = self.send(address, key, deployment, reqwest::Method::GET, &path, scope, Vec::new()).await?;
        Ok(AgentStream::http(response))
    }

    /// `POST /internal/v1/runs/{id}/abort`.
    ///
    /// # Errors
    /// See [`AgentError`].
    pub async fn abort(
        &self,
        address: &EndpointAddress,
        key: &GatewayKey,
        deployment: &str,
        scope: RunScope<'_>,
    ) -> Result<(), AgentError> {
        let path = format!("/internal/v1/runs/{}/abort", scope.run_id);
        self.send(address, key, deployment, reqwest::Method::POST, &path, scope, Vec::new()).await.map(|_| ())
    }

    /// `GET /internal/v1/hello`.
    ///
    /// # Errors
    /// See [`AgentError`].
    pub async fn hello(
        &self,
        address: &EndpointAddress,
        key: &GatewayKey,
        deployment: &str,
    ) -> Result<serde_json::Value, AgentError> {
        let response = self
            .send(address, key, deployment, reqwest::Method::GET, "/internal/v1/hello", RunScope::default(), Vec::new())
            .await?;
        let bytes = response.bytes().await.map_err(|error| AgentError::Stream(error.to_string()))?;
        serde_json::from_slice(&bytes).map_err(|error| AgentError::Stream(error.to_string()))
    }

    /// `POST /internal/v1/drain` with a deadline.
    ///
    /// # Errors
    /// See [`AgentError`].
    pub async fn drain(
        &self,
        address: &EndpointAddress,
        key: &GatewayKey,
        deployment: &str,
        deadline_ms: u64,
    ) -> Result<(), AgentError> {
        let body = serde_json::to_vec(&serde_json::json!({ "deadline_ms": deadline_ms })).unwrap_or_default();
        self.send(address, key, deployment, reqwest::Method::POST, "/internal/v1/drain", RunScope::default(), body)
            .await
            .map(|_| ())
    }

    /// `GET /internal/v1/orphans`: executions whose gateway stream was lost.
    ///
    /// # Errors
    /// See [`AgentError`].
    pub async fn orphans(
        &self,
        address: &EndpointAddress,
        key: &GatewayKey,
        deployment: &str,
    ) -> Result<Vec<serde_json::Value>, AgentError> {
        let response = self
            .send(
                address,
                key,
                deployment,
                reqwest::Method::GET,
                "/internal/v1/orphans",
                RunScope::default(),
                Vec::new(),
            )
            .await?;
        let bytes = response.bytes().await.map_err(|error| AgentError::Stream(error.to_string()))?;
        let value: serde_json::Value =
            serde_json::from_slice(&bytes).map_err(|error| AgentError::Stream(error.to_string()))?;
        Ok(value.get("orphans").and_then(serde_json::Value::as_array).cloned().unwrap_or_default())
    }

    /// `DELETE /internal/v1/orphans/{id}`: the gateway took the orphan over.
    ///
    /// # Errors
    /// See [`AgentError`].
    pub async fn release_orphan(
        &self,
        address: &EndpointAddress,
        key: &GatewayKey,
        deployment: &str,
        scope: RunScope<'_>,
    ) -> Result<(), AgentError> {
        let path = format!("/internal/v1/orphans/{}", scope.run_id);
        self.send(address, key, deployment, reqwest::Method::DELETE, &path, scope, Vec::new()).await.map(|_| ())
    }
}

/// Shared handle.
pub type SharedAgentClient = Arc<AgentClient>;
