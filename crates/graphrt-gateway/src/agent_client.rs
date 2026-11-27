//! Gateway → agent calls of protocol 1 (`/internal/v1/*`, plan §3.1), over
//! HTTP/1.1 keep-alive on a Unix socket, a Windows named pipe or a private
//! network, every request signed with the deployment's key. Agent addresses
//! come only from the provider (never from the agent itself, plan §3.2).
//!
//! Run streams (start and reattach) to a Unix socket, named pipe or `http://`
//! agent go through the lean client of [`crate::stream_conn`] (one small
//! buffer per stream); `https://` agents and the short control calls through
//! reqwest, except on named pipes (reqwest has no pipe transport), where the
//! control calls take the lean client too.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use graphrt_provider_api::{Endpoint, EndpointAddress, InvokeRequest, Provider};

use crate::signature::{
    DEPLOYMENT_HEADER, EPOCH_HEADER, GatewayKey, PROTOCOL_HEADER, PROTOCOL_VERSION, RUN_ID_HEADER, SIGNATURE_HEADER,
    Signed, sign, unix_now,
};
use crate::sse::{Frame, Parser, SseError};
use crate::stream_conn::{self, IdlePool, OpenError, ResponseBody, Target};

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

/// How long [`AgentStream::recycle`] waits for the end of a finished stream.
const RECYCLE_WAIT: Duration = Duration::from_secs(2);
/// How much of a finished stream [`AgentStream::recycle`] reads before it gives up on it.
const RECYCLE_BYTES: usize = 64 * 1024;

/// Signed client of the agents of all deployments.
#[derive(Debug, Default)]
pub struct AgentClient {
    clients: Mutex<HashMap<EndpointAddress, reqwest::Client>>,
    /// Keep-alive connections of the lean run streams.
    streams: Arc<IdlePool>,
}

enum StreamSource {
    /// Unix socket / `http://`: the lean client.
    Lean(ResponseBody),
    /// `https://`: reqwest.
    Http(reqwest::Response),
    Invoke(graphrt_provider_api::ByteStream),
}

/// An SSE stream from an agent (over HTTP, or a provider's own transport).
pub struct AgentStream {
    source: StreamSource,
    parser: Parser,
    pending: std::collections::VecDeque<Frame>,
    /// The last frame went to client streams and the reader has not waited
    /// for the agent since.
    handed_out: bool,
}

impl std::fmt::Debug for AgentStream {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("AgentStream").finish_non_exhaustive()
    }
}

impl AgentStream {
    fn new(source: StreamSource) -> Self {
        Self { source, parser: Parser::default(), pending: Default::default(), handed_out: false }
    }

    /// Ends the use of a stream after its terminal event. A lean stream reads
    /// the rest of its body in the background (bounded in bytes and time) so
    /// its connection can serve the next run: a new connection per run costs a
    /// connect and the agent's accept on the latency path of the next request.
    pub fn recycle(self) {
        let StreamSource::Lean(body) = self.source else { return };
        graphrt_core::task::spawn(async move {
            let _kept = tokio::time::timeout(RECYCLE_WAIT, body.drain(RECYCLE_BYTES)).await;
        });
    }

    /// The frame returned last was handed to client streams (they are woken):
    /// the next frame, when already buffered, waits for them to run first.
    pub fn handed_to_clients(&mut self) {
        self.handed_out = true;
    }

    /// The next frame; `Ok(None)` when the agent closed the stream.
    ///
    /// # Errors
    /// [`AgentError::Stream`] on a broken connection or oversized event.
    pub async fn next_frame(&mut self) -> Result<Option<Frame>, AgentError> {
        use futures::StreamExt as _;
        loop {
            if !self.pending.is_empty() {
                if self.handed_out {
                    // The next frame is already here (one read carries many)
                    // and the previous one woke client streams: let them write
                    // it first. Without this the reader handles (and stores)
                    // every buffered frame before any client sees the first:
                    // +45 µs TTFB on a hello run, where hyper's body channel
                    // used to hand over one frame at a time. Before the frame
                    // is taken: the pump races this future against its flush
                    // timer, and a dropped future must not lose a frame.
                    reschedule().await;
                    self.handed_out = false;
                }
                return Ok(self.pending.pop_front());
            }
            let buffered = match &self.source {
                StreamSource::Lean(body) => body.has_buffered(),
                StreamSource::Http(_) | StreamSource::Invoke(_) => false,
            };
            if !buffered {
                self.handed_out = false;
            }
            let fed = match &mut self.source {
                // Parsed straight out of the connection's read buffer.
                StreamSource::Lean(body) => match body.next_data().await {
                    Ok(Some(data)) => self.parser.feed_into(data, &mut self.pending).map(|()| true),
                    Ok(None) => Ok(false),
                    Err(error) => return Err(AgentError::Stream(error.to_string())),
                },
                StreamSource::Http(response) => match response.chunk().await {
                    Ok(Some(chunk)) => self.parser.feed_into(&chunk, &mut self.pending).map(|()| true),
                    Ok(None) => Ok(false),
                    Err(error) => return Err(AgentError::Stream(error.to_string())),
                },
                StreamSource::Invoke(stream) => match stream.next().await {
                    Some(Ok(chunk)) => self.parser.feed_into(&chunk, &mut self.pending).map(|()| true),
                    None => Ok(false),
                    Some(Err(error)) => return Err(invoke_error(&error, AgentError::Stream)),
                },
            };
            if !fed? {
                return Ok(None);
            }
        }
    }
}

/// Returns `Pending` once with the task woken at once: it goes to the back of
/// its worker's queue, behind the tasks it just woke. Unlike
/// `tokio::task::yield_now`, which waits until the worker ran out of work and
/// polled the driver: under load (1,000 cron runs) that held runs back.
async fn reschedule() {
    let mut yielded = false;
    std::future::poll_fn(|cx| {
        if std::mem::replace(&mut yielded, true) {
            return std::task::Poll::Ready(());
        }
        cx.waker().wake_by_ref();
        std::task::Poll::Pending
    })
    .await;
}

/// The protocol headers of a signed request (plan §3.1).
fn signed_headers(
    key: &GatewayKey,
    deployment: &str,
    method: &str,
    path: &str,
    scope: RunScope<'_>,
    body: &[u8],
) -> Vec<(&'static str, String)> {
    let epoch = scope.epoch.map(|epoch| epoch.to_string()).unwrap_or_default();
    let signed = Signed { deployment, method, path, run_id: scope.run_id, epoch: &epoch, body };
    let mut headers = vec![
        (PROTOCOL_HEADER, PROTOCOL_VERSION.to_string()),
        (SIGNATURE_HEADER, sign(key, unix_now(), &signed)),
        (DEPLOYMENT_HEADER, deployment.to_owned()),
    ];
    if !scope.run_id.is_empty() {
        headers.push((RUN_ID_HEADER, scope.run_id.to_owned()));
    }
    if !epoch.is_empty() {
        headers.push((EPOCH_HEADER, epoch));
    }
    headers
}

fn base_url(address: &EndpointAddress) -> String {
    match address {
        // The host is ignored on a Unix socket; any valid one will do.
        EndpointAddress::Unix(_) | EndpointAddress::Pipe(_) => "http://agent".to_owned(),
        EndpointAddress::Http(base) => base.trim_end_matches('/').to_owned(),
        _ => "http://agent".to_owned(),
    }
}

impl AgentClient {
    fn client(&self, address: &EndpointAddress) -> Result<reqwest::Client, AgentError> {
        let mut clients = self.clients.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(client) = clients.get(address) {
            return Ok(client.clone());
        }
        let builder = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .pool_idle_timeout(Duration::from_secs(90))
            .http1_only();
        #[cfg(unix)]
        let builder = match address {
            EndpointAddress::Unix(path) => builder.unix_socket(path.as_path()),
            _ => builder,
        };
        let client = builder.build().map_err(|error| AgentError::Unreachable(error.to_string()))?;
        if clients.len() > 4096 {
            clients.clear();
        }
        clients.insert(address.clone(), client.clone());
        Ok(client)
    }

    /// Forgets the connection pools of a replica that went away.
    pub fn forget(&self, address: &EndpointAddress) {
        self.clients.lock().unwrap_or_else(std::sync::PoisonError::into_inner).remove(address);
        self.streams.forget(address);
    }

    /// Idle keep-alive connections of finished lean run streams (all agents).
    pub fn idle_stream_connections(&self) -> usize {
        self.streams.idle_count()
    }

    /// Opens a run stream: the lean client for Unix sockets and `http://`,
    /// reqwest for the rest.
    #[allow(clippy::too_many_arguments)]
    async fn open_stream(
        &self,
        address: &EndpointAddress,
        key: &GatewayKey,
        deployment: &str,
        method: reqwest::Method,
        path: &str,
        scope: RunScope<'_>,
        body: Vec<u8>,
    ) -> Result<AgentStream, AgentError> {
        let Some(target) = Target::of(address) else {
            let response = self.send_http(address, key, deployment, method, path, scope, body).await?;
            return Ok(AgentStream::new(StreamSource::Http(response)));
        };
        let headers = signed_headers(key, deployment, method.as_str(), path, scope, &body);
        let headers: Vec<(&str, &str)> = headers.iter().map(|(name, value)| (*name, value.as_str())).collect();
        let request = stream_conn::Request { method: method.as_str(), path, headers: &headers, body: &body };
        match stream_conn::open(&self.streams, &target, request).await {
            Ok(response) => Ok(AgentStream::new(StreamSource::Lean(response))),
            Err(OpenError::Unreachable(error)) => Err(AgentError::Unreachable(error)),
            Err(OpenError::Refused { status, body }) => Err(AgentError::Refused { status, body }),
        }
    }

    /// A control call: through reqwest, or the lean client on a named pipe.
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
    ) -> Result<Reply, AgentError> {
        if let EndpointAddress::Pipe(_) = address
            && let Some(target) = Target::of(address)
        {
            let headers = signed_headers(key, deployment, method.as_str(), path, scope, &body);
            let headers: Vec<(&str, &str)> = headers.iter().map(|(name, value)| (*name, value.as_str())).collect();
            let request = stream_conn::Request { method: method.as_str(), path, headers: &headers, body: &body };
            return match stream_conn::open(&self.streams, &target, request).await {
                Ok(response) => Ok(Reply::Lean(response)),
                Err(OpenError::Unreachable(error)) => Err(AgentError::Unreachable(error)),
                Err(OpenError::Refused { status, body }) => Err(AgentError::Refused { status, body }),
            };
        }
        self.send_http(address, key, deployment, method, path, scope, body).await.map(Reply::Http)
    }

    // One private call site per protocol operation; a parameter struct would only rename them.
    #[allow(clippy::too_many_arguments)]
    async fn send_http(
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
        let mut request = client.request(method.clone(), format!("{}{path}", base_url(address)));
        for (name, value) in signed_headers(key, deployment, method.as_str(), path, scope, &body) {
            request = request.header(name, value);
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
        self.open_stream(address, key, deployment, reqwest::Method::POST, "/internal/v1/runs", scope, bytes).await
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
        Ok(AgentStream::new(StreamSource::Invoke(response.body)))
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
        self.open_stream(address, key, deployment, reqwest::Method::GET, &path, scope, Vec::new()).await
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

    /// `POST` of `body` to `path` (`/internal/v1/runs/{id}/feedback`), answered with JSON.
    ///
    /// # Errors
    /// See [`AgentError`] (an agent older than the route answers `404`).
    pub async fn call_json(
        &self,
        address: &EndpointAddress,
        key: &GatewayKey,
        deployment: &str,
        path: &str,
        scope: RunScope<'_>,
        body: &serde_json::Value,
    ) -> Result<serde_json::Value, AgentError> {
        let body = serde_json::to_vec(body).map_err(|error| AgentError::Stream(error.to_string()))?;
        let reply = self.send(address, key, deployment, reqwest::Method::POST, path, scope, body).await?;
        let bytes = reply.bytes().await?;
        serde_json::from_slice(&bytes).map_err(|error| AgentError::Stream(error.to_string()))
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
        let bytes = response.bytes().await?;
        serde_json::from_slice(&bytes).map_err(|error| AgentError::Stream(error.to_string()))
    }

    /// `POST /internal/v1/warm`: a warm hint (runs are expected soon).
    ///
    /// # Errors
    /// See [`AgentError`] (an agent older than the hint answers `404`).
    pub async fn warm(&self, address: &EndpointAddress, key: &GatewayKey, deployment: &str) -> Result<(), AgentError> {
        self.send(address, key, deployment, reqwest::Method::POST, "/internal/v1/warm", RunScope::default(), Vec::new())
            .await
            .map(|_| ())
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

    /// `POST /internal/v1/stop`: the agent drains and exits, as on `SIGTERM`
    /// (how the local-process provider stops agents on Windows).
    ///
    /// # Errors
    /// See [`AgentError`].
    pub async fn stop(&self, address: &EndpointAddress, key: &GatewayKey, deployment: &str) -> Result<(), AgentError> {
        self.send(address, key, deployment, reqwest::Method::POST, "/internal/v1/stop", RunScope::default(), Vec::new())
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
        let bytes = response.bytes().await?;
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

/// The answer to a control call.
enum Reply {
    Http(reqwest::Response),
    Lean(ResponseBody),
}

impl Reply {
    /// The whole body.
    async fn bytes(self) -> Result<Vec<u8>, AgentError> {
        match self {
            Self::Http(response) => response
                .bytes()
                .await
                .map(|bytes| bytes.to_vec())
                .map_err(|error| AgentError::Stream(error.to_string())),
            Self::Lean(mut body) => {
                let mut bytes = Vec::new();
                while let Some(data) = body.next_data().await.map_err(|error| AgentError::Stream(error.to_string()))? {
                    bytes.extend_from_slice(data);
                }
                Ok(bytes)
            }
        }
    }
}

/// Shared handle.
pub type SharedAgentClient = Arc<AgentClient>;

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    /// The supervisor's pump races `next_frame` against its flush timer: a
    /// call dropped while it yields between buffered frames loses none.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn next_frame_is_cancel_safe() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = EndpointAddress::Http(format!("http://{}", listener.local_addr().unwrap()));
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = vec![0u8; 4096];
            let _ = socket.read(&mut request).await.unwrap();
            let mut body = String::new();
            for id in 0..50 {
                let frame = format!("event: node_succeeded\nid: {id}\ndata: {{}}\n\n");
                body.push_str(&format!("{:x}\r\n{frame}\r\n", frame.len()));
            }
            body.push_str("0\r\n\r\n");
            let response = format!("HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n{body}");
            socket.write_all(response.as_bytes()).await.unwrap();
            tokio::time::sleep(Duration::from_secs(5)).await;
        });
        let client = AgentClient::default();
        let key = crate::signature::GatewayKey::new("k1", &[7u8; 32]).unwrap();
        let scope = RunScope { run_id: "r", epoch: Some(1) };
        let mut stream = client.start_run(&address, &key, "d", scope, &serde_json::json!({})).await.unwrap();
        let (mut ids, mut cancelled) = (Vec::new(), 0);
        for round in 0.. {
            if ids.len() == 50 {
                break;
            }
            if round % 2 == 0 {
                ids.push(stream.next_frame().await.unwrap().unwrap().event_index().unwrap());
                stream.handed_to_clients();
                continue;
            }
            // Dropped at its first `Pending` (the yield between buffered frames).
            tokio::select! {
                biased;
                frame = stream.next_frame() => {
                    ids.push(frame.unwrap().unwrap().event_index().unwrap());
                    stream.handed_to_clients();
                }
                () = std::future::ready(()) => cancelled += 1,
            }
        }
        assert!(cancelled > 0, "the race never dropped a pending call");
        assert_eq!(ids, (0..50).collect::<Vec<u64>>());
    }
}
