//! Size probe: `size_baseline` plus the run-time Fly provider only (what the
//! gateway links), every trait method called through `dyn Provider`.
//!
//! `cargo build -p graphrt-provider-fly --profile min-size --examples`

#![allow(clippy::print_stdout, clippy::unwrap_used)]

#[path = "size_baseline.rs"]
#[allow(dead_code)]
mod size_baseline;

use std::sync::Arc;

use graphrt_provider_api::{DeploymentRef, Provider, ReleaseMode, ReplicaRef};
use graphrt_provider_fly::{FlyProvider, FlyProviderConfig, TokenOrigin};

fn main() {
    size_baseline::main();
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let base_url = std::env::args().nth(1).unwrap_or_else(|| "http://127.0.0.1:9".to_owned());
    runtime.block_on(async move {
        let mut config = FlyProviderConfig::single_app("probe", TokenOrigin::Env("FLY_API_TOKEN".to_owned()));
        config.api_base_url = base_url;
        let provider: Arc<dyn Provider> = Arc::new(FlyProvider::new(config).unwrap());
        let deployment = DeploymentRef::new("probe", "v1", "probe");
        std::hint::black_box(provider.wake(&deployment, 1).await.is_ok());
        let replica = ReplicaRef { deployment: deployment.clone(), id: "abc".to_owned() };
        std::hint::black_box(provider.release(&replica, ReleaseMode::Suspend).await.is_ok());
        std::hint::black_box(provider.list(&deployment).await.is_ok());
    });
}
