//! Size probe, baseline: what the gateway links anyway (tokio, reqwest +
//! rustls, serde_json, the shared `ProviderHttp`) exercised through a
//! `dyn Provider`. `size_fly` adds the Fly provider and deploy driver on top;
//! the difference of the two `min-size` binaries is the `provider-fly` delta
//! reported against the §9 budget (`gw-fly` ≤ 80 KiB).
//!
//! `cargo build -p graphrt-provider-fly --profile min-size --examples`

#![allow(clippy::print_stdout, clippy::unwrap_used)]

use std::sync::Arc;

use graphrt_provider_api::http::{ProviderHttp, TokenBucket};
use graphrt_provider_api::{
    Addressing, BoxFuture, Capabilities, DeploymentRef, Endpoint, Provider, ProviderError, ReleaseMode, ReplicaRef,
    ReplicaState, Scaler,
};

struct HttpProbe {
    http: ProviderHttp,
    url: String,
}

impl Provider for HttpProbe {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            addressing: Addressing::Replica,
            scaler: Scaler::Gateway,
            max_request_secs: None,
            idle_timeout_secs: None,
            max_grace_secs: 30,
            background_cpu: true,
            supports_suspend: false,
            wake_p95_target_ms: 1,
        }
    }
    fn wake<'a>(
        &'a self,
        _deployment: &'a DeploymentRef,
        _replicas: u32,
    ) -> BoxFuture<'a, Result<Vec<Endpoint>, ProviderError>> {
        Box::pin(async move {
            let response = self.http.send(|client| client.get(&self.url), None).await?;
            let body = response.bytes().await.map_err(|error| ProviderError::transient(error.to_string()))?;
            let value: serde_json::Value =
                serde_json::from_slice(&body).map_err(|error| ProviderError::fatal(error.to_string()))?;
            println!("{value}");
            Ok(Vec::new())
        })
    }
    fn release<'a>(&'a self, _replica: &'a ReplicaRef, _mode: ReleaseMode) -> BoxFuture<'a, Result<(), ProviderError>> {
        Box::pin(async { Ok(()) })
    }
    fn list<'a>(&'a self, _deployment: &'a DeploymentRef) -> BoxFuture<'a, Result<Vec<ReplicaState>, ProviderError>> {
        Box::pin(async { Ok(Vec::new()) })
    }
}

pub fn main() {
    let _installed = rustls::crypto::ring::default_provider().install_default();
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let url = std::env::args().nth(1).unwrap_or_else(|| "http://127.0.0.1:9".to_owned());
    runtime.block_on(async move {
        let client = reqwest::Client::builder().build().unwrap();
        let provider: Arc<dyn Provider> =
            Arc::new(HttpProbe { http: ProviderHttp::new(client, Arc::new(TokenBucket::new(10.0, 10))), url });
        let deployment = DeploymentRef::new("probe", "v1", "probe");
        println!("{:?}", provider.wake(&deployment, 1).await.err());
    });
}
