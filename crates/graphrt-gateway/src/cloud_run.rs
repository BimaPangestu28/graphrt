//! Cloud Run in the gateway (feature `gw-cloud-run`): the provider of
//! `graphrt-gcp`, plus the invoker credential on every request.
//!
//! Cloud Run only lets a request through with an ID token for the service
//! (`X-Serverless-Authorization: Bearer <id token>`, so the agent's own
//! `Authorization` stays free). [`CloudRunGatewayProvider::invoke`] sends the
//! gateway's signed protocol request over HTTPS to the service URL with that
//! header, refreshing the token once after a 401/403, and streams the agent's
//! SSE bytes back. Everything else is the `graphrt-gcp` provider unchanged.

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt as _;
use graphrt_gcp::cloud_run::CloudRunProvider;
use graphrt_provider_api::{
    BoxFuture, Capabilities, DeploymentRef, Endpoint, EndpointAddress, InvokeRequest, InvokeResponse, Provider,
    ProviderError, ReleaseMode, ReplicaRef, ReplicaState,
};

/// [`CloudRunProvider`] with ID-token invocations.
#[derive(Debug, Clone)]
pub struct CloudRunGatewayProvider {
    inner: Arc<CloudRunProvider>,
    client: reqwest::Client,
}

impl CloudRunGatewayProvider {
    /// Wraps `inner`.
    ///
    /// # Errors
    /// When the HTTP client cannot be built.
    pub fn new(inner: CloudRunProvider) -> Result<Self, ProviderError> {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .pool_idle_timeout(Duration::from_secs(90))
            // The invoker token must only ever reach the service.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|error| ProviderError::fatal(error.to_string()))?;
        Ok(Self { inner: Arc::new(inner), client })
    }

    async fn send(
        &self,
        base_url: &str,
        deployment: &DeploymentRef,
        request: &InvokeRequest,
        force_refresh: bool,
    ) -> Result<reqwest::Response, ProviderError> {
        let method = reqwest::Method::from_bytes(request.method.as_bytes())
            .map_err(|error| ProviderError::fatal(error.to_string()))?;
        let mut builder = self.client.request(method, format!("{}{}", base_url.trim_end_matches('/'), request.path));
        for (name, value) in &request.headers {
            builder = builder.header(name.as_str(), value.as_str());
        }
        if let Some((header, value)) = self.inner.invoke_authorization(deployment, force_refresh).await? {
            builder = builder.header(header, value.expose());
        }
        builder
            .body(request.body.clone())
            .send()
            .await
            .map_err(|error| ProviderError::transient(format!("cloud run invoke: {}", error.without_url())))
    }
}

impl Provider for CloudRunGatewayProvider {
    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }

    fn wake<'a>(
        &'a self,
        deployment: &'a DeploymentRef,
        replicas: u32,
    ) -> BoxFuture<'a, Result<Vec<Endpoint>, ProviderError>> {
        self.inner.wake(deployment, replicas)
    }

    fn release<'a>(&'a self, replica: &'a ReplicaRef, mode: ReleaseMode) -> BoxFuture<'a, Result<(), ProviderError>> {
        self.inner.release(replica, mode)
    }

    fn list<'a>(&'a self, deployment: &'a DeploymentRef) -> BoxFuture<'a, Result<Vec<ReplicaState>, ProviderError>> {
        self.inner.list(deployment)
    }

    fn invoke<'a>(
        &'a self,
        endpoint: &'a Endpoint,
        request: InvokeRequest,
    ) -> Option<BoxFuture<'a, Result<InvokeResponse, ProviderError>>> {
        let EndpointAddress::Http(base_url) = &endpoint.address else { return None };
        Some(Box::pin(async move {
            let deployment = &endpoint.replica.deployment;
            let mut response = self.send(base_url, deployment, &request, false).await?;
            if matches!(response.status().as_u16(), 401 | 403) {
                response = self.send(base_url, deployment, &request, true).await?;
            }
            let status = response.status().as_u16();
            let body = futures::stream::unfold(response, |mut response| async move {
                match response.chunk().await {
                    Ok(Some(chunk)) => Some((Ok(chunk.to_vec()), response)),
                    Ok(None) => None,
                    Err(error) => Some((Err(ProviderError::transient(error.to_string())), response)),
                }
            })
            .boxed();
            Ok(InvokeResponse { status, body })
        }))
    }
}
