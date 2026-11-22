//! Binary-size probe for conformance C13 (plan §9 budgets).
//!
//! Build it under `--profile min-size` with no features (baseline) and with each
//! provider feature, then compare. The baseline already links what the gateway
//! links anyway (reqwest + rustls/ring + tokio + serde_json), so the delta is the
//! cost of the feature alone:
//!
//! ```text
//! cargo build -p graphrt-provider-aws --example size_probe --profile min-size [--features provider-ecs]
//! ```
#![allow(clippy::print_stdout)]

fn main() {
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build();
    let Ok(runtime) = runtime else { return };
    runtime.block_on(probe());
}

/// Uses an HTTP client and JSON the way the gateway does (baseline).
async fn baseline() {
    let _already_installed = rustls::crypto::ring::default_provider().install_default();
    if let Ok(client) = reqwest::Client::builder().build() {
        let response = client.get("http://127.0.0.1:9/").send().await;
        let text = match response {
            Ok(r) => r.text().await.unwrap_or_default(),
            Err(e) => e.to_string(),
        };
        let value = serde_json::json!({ "probe": text });
        println!("{}", serde_json::to_string(&value).unwrap_or_default());
    }
}

#[cfg(not(feature = "auth-aws"))]
async fn probe() {
    baseline().await;
}

#[cfg(feature = "auth-aws")]
async fn probe() {
    use std::sync::Arc;

    use graphrt_provider_aws::auth::{ChainConfig, Clock, CredentialChain};

    baseline().await;
    let Ok(chain) = CredentialChain::new(ChainConfig::from_env(), Clock::default()) else { return };
    let chain = Arc::new(chain);
    match chain.credentials().await {
        Ok(creds) => println!("{}", graphrt_provider_aws::auth::eks_token(&creds, "eu-west-1", "sts", "c", 0).len()),
        Err(e) => println!("{e}"),
    }
    #[cfg(feature = "provider-ecs")]
    probe_ecs(Arc::clone(&chain)).await;
    #[cfg(feature = "deploy-ecs")]
    probe_deploy_ecs(Arc::clone(&chain)).await;
    #[cfg(feature = "deploy-lambda")]
    probe_deploy_lambda(Arc::clone(&chain)).await;
    #[cfg(feature = "provider-lambda")]
    probe_lambda(chain).await;
}

#[cfg(any(feature = "deploy-ecs", feature = "deploy-lambda"))]
fn probe_spec() -> graphrt_provider_aws::api::DeploymentSpec {
    graphrt_provider_aws::api::DeploymentSpec {
        name: std::env::args().next().unwrap_or_default(),
        version: "v1".into(),
        image: Some("i".into()),
        command: Vec::new(),
        env: Default::default(),
        secrets: Vec::new(),
        max_active_runs: 1,
        min_replicas: 0,
        max_replicas: 1,
        grace_secs: 0,
        resources: Default::default(),
        workflow_fingerprint: String::new(),
        provider_settings: Default::default(),
    }
}

#[cfg(feature = "deploy-ecs")]
async fn probe_deploy_ecs(chain: std::sync::Arc<graphrt_provider_aws::auth::CredentialChain>) {
    use graphrt_provider_aws::api::DeployDriver;
    use graphrt_provider_aws::deploy::ecs::{EcsDeployConfig, EcsDeployDriver};

    let Ok(driver) = EcsDeployDriver::new(chain, EcsDeployConfig::new("eu-west-1")) else { return };
    println!("{:?}", driver.apply(&probe_spec()).await.err());
}

#[cfg(feature = "deploy-lambda")]
async fn probe_deploy_lambda(chain: std::sync::Arc<graphrt_provider_aws::auth::CredentialChain>) {
    use graphrt_provider_aws::api::DeployDriver;
    use graphrt_provider_aws::deploy::lambda::{LambdaDeployConfig, LambdaDeployDriver};

    let Ok(driver) = LambdaDeployDriver::new(chain, LambdaDeployConfig::new("eu-west-1")) else { return };
    println!("{:?}", driver.apply(&probe_spec()).await.err());
}

#[cfg(feature = "provider-ecs")]
async fn probe_ecs(chain: std::sync::Arc<graphrt_provider_aws::auth::CredentialChain>) {
    use graphrt_provider_aws::api::{DeploymentRef, Provider};
    use graphrt_provider_aws::ecs::{EcsConfig, EcsProvider};

    let Ok(provider) = EcsProvider::new(chain, EcsConfig::new("eu-west-1")) else { return };
    let deployment = DeploymentRef::new("d", "1", "");
    println!("{:?} {:?}", provider.wake(&deployment, 1).await.err(), provider.list(&deployment).await.err());
}

#[cfg(feature = "provider-lambda")]
async fn probe_lambda(chain: std::sync::Arc<graphrt_provider_aws::auth::CredentialChain>) {
    use futures::StreamExt;
    use graphrt_provider_aws::api::DeploymentRef;
    use graphrt_provider_aws::lambda::{AgentRequest, LambdaConfig, LambdaProvider};

    let Ok(provider) = LambdaProvider::new(chain, LambdaConfig::new("eu-west-1")) else { return };
    let deployment = DeploymentRef::new("d", "1", "fn");
    match provider.invoke(&deployment, &AgentRequest::default()).await {
        Ok(mut response) => println!("{:?}", response.body.next().await),
        Err(e) => println!("{e}"),
    }
}
