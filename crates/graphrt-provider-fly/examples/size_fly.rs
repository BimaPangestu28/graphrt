//! Size probe: `size_baseline` plus the Fly provider and deploy driver,
//! every trait method called through `dyn Provider` / `dyn DeployDriver`.
//!
//! `cargo build -p graphrt-provider-fly --profile min-size --examples`

#![allow(clippy::print_stdout, clippy::unwrap_used)]

#[path = "size_baseline.rs"]
#[allow(dead_code)]
mod size_baseline;

use std::collections::BTreeMap;
use std::sync::Arc;

use graphrt_provider_api::{DeployDriver, DeploymentRef, DeploymentSpec, Provider, ReleaseMode, ReplicaRef, Resources};
use graphrt_provider_fly::{
    ClientPolicy, FlyDeployConfig, FlyDeployDriver, FlyProvider, FlyProviderConfig, TokenOrigin,
};

fn main() {
    size_baseline::main();
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let base_url = std::env::args().nth(1).unwrap_or_else(|| "http://127.0.0.1:9".to_owned());
    runtime.block_on(async move {
        let mut config = FlyProviderConfig::single_app("probe", TokenOrigin::Env("FLY_API_TOKEN".to_owned()));
        config.api_base_url = base_url.clone();
        let provider: Arc<dyn Provider> = Arc::new(FlyProvider::new(config).unwrap());
        let deployment = DeploymentRef::new("probe", "v1", "probe");
        std::hint::black_box(provider.wake(&deployment, 1).await.is_ok());
        let replica = ReplicaRef { deployment: deployment.clone(), id: "abc".to_owned() };
        std::hint::black_box(provider.release(&replica, ReleaseMode::Suspend).await.is_ok());
        std::hint::black_box(provider.list(&deployment).await.is_ok());
        let driver: Box<dyn DeployDriver> = Box::new(FlyDeployDriver::new(FlyDeployConfig {
            api_base_url: base_url,
            token_origin: TokenOrigin::Env("FLY_API_TOKEN".to_owned()),
            client_policy: ClientPolicy::default(),
            env_lookup: Arc::new(|name| std::env::var(name).ok()),
        }));
        let spec = DeploymentSpec {
            name: "probe".into(),
            version: "v1".into(),
            image: Some("img".into()),
            command: Vec::new(),
            env: BTreeMap::new(),
            secrets: Vec::new(),
            max_active_runs: 1,
            min_replicas: 0,
            max_replicas: 1,
            grace_secs: 0,
            resources: Resources::default(),
            workflow_fingerprint: String::new(),
            provider_settings: BTreeMap::new(),
        };
        std::hint::black_box(driver.apply(&spec).await.is_ok());
    });
}
