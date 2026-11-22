//! Size probe for conformance C13: the binary delta of `auth-azure`,
//! `provider-aca` and `deploy-aca` on top of what the gateway links anyway (tokio,
//! reqwest + rustls/ring, serde_json). Built twice by
//! `crates/graphrt-provider-azure/measure-size.sh`; never run in CI.
#![allow(clippy::print_stdout, unused_variables)]

use std::time::Duration;

fn main() {
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build();
    let Ok(runtime) = runtime else { return };
    let url = std::env::args().nth(1).unwrap_or_else(|| "http://127.0.0.1:9".into());
    runtime.block_on(async move {
        baseline(&url).await;
        #[cfg(feature = "auth-azure")]
        let Some(tokens) = azure_auth(&url).await else { return };
        #[cfg(feature = "provider-aca")]
        let Some(arm) = azure_runtime(&url, tokens.clone()).await else { return };
        #[cfg(feature = "deploy-aca")]
        azure_deploy(&url, arm, tokens).await;
    });
}

/// What every gateway build already contains: the shared provider HTTP layer
/// (`ProviderHttp` over reqwest/rustls/ring), serde_json, ring digests.
async fn baseline(url: &str) {
    use std::sync::Arc;

    use graphrt_provider_api::http::{ProviderHttp, StaticToken, TokenBucket};

    let _already_installed = rustls::crypto::ring::default_provider().install_default();
    let Ok(client) = reqwest::Client::builder().timeout(Duration::from_secs(1)).build() else { return };
    let http = ProviderHttp::new(client, Arc::new(TokenBucket::new(10.0, 10)));
    let token = StaticToken(graphrt_provider_api::http::SecretString::new(url));
    if let Ok(response) = http.send(|client| client.get(url), Some(&token)).await
        && let Ok(bytes) = response.bytes().await
    {
        let parsed: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or_default();
        let digest = ring::digest::digest(&ring::digest::SHA256, parsed.to_string().as_bytes());
        println!("{:?}", digest.as_ref().first());
    }
}

/// `auth-azure`: every credential kind and the token cache.
#[cfg(feature = "auth-azure")]
async fn azure_auth(url: &str) -> Option<std::sync::Arc<graphrt_provider_azure::auth::TokenProvider>> {
    use graphrt_provider_azure::auth::{ARM_SCOPE, AuthEndpoints, AzureCredential, CertificateKey, TokenProvider};

    let env = |name: &str| std::env::var(name).ok();
    let credential = AzureCredential::from_env(&env, &AuthEndpoints::default()).ok()?;
    let _ = std::hint::black_box(CertificateKey::from_pem(url));
    let tokens = std::sync::Arc::new(TokenProvider::new(credential).ok()?);
    println!("{:?}", tokens.token(ARM_SCOPE).await.is_ok());
    Some(tokens)
}

/// `provider-aca`: the run-time provider the gateway links.
#[cfg(feature = "provider-aca")]
async fn azure_runtime(
    url: &str,
    tokens: std::sync::Arc<graphrt_provider_azure::auth::TokenProvider>,
) -> Option<graphrt_provider_azure::arm::ArmClient> {
    use graphrt_provider_api::{DeploymentRef, Provider, ReleaseMode, ReplicaRef};
    use graphrt_provider_azure::aca::{AcaConfig, AcaProvider};
    use graphrt_provider_azure::arm::{ArmClient, ArmConfig};

    let arm = ArmClient::new(ArmConfig { endpoint: url.into(), ..ArmConfig::default() }, tokens).ok()?;
    let provider = AcaProvider::new(arm.clone(), AcaConfig::new("sub", "rg")).ok()?;
    let deployment = DeploymentRef::new("d", "v1", "d");
    println!("{:?}", provider.wake(&deployment, 1).await);
    println!("{:?}", provider.list(&deployment).await);
    let replica = ReplicaRef { deployment: deployment.clone(), id: "url".into() };
    println!("{:?}", provider.release(&replica, ReleaseMode::ScaleIn).await);
    Some(arm)
}

/// `deploy-aca`: the deploy driver and Key Vault client `graphrt deploy` links.
#[cfg(feature = "deploy-aca")]
async fn azure_deploy(
    url: &str,
    arm: graphrt_provider_azure::arm::ArmClient,
    tokens: std::sync::Arc<graphrt_provider_azure::auth::TokenProvider>,
) {
    use graphrt_provider_api::http::SecretString;
    use graphrt_provider_api::{DeployDriver, DeploymentSpec};
    use graphrt_provider_azure::aca::{AcaConfig, AcaDeployDriver};
    use graphrt_provider_azure::keyvault::{KeyVaultClient, KeyVaultConfig};

    if let Ok(driver) = AcaDeployDriver::new(arm, AcaConfig::new("sub", "rg"), KeyVaultConfig::default()) {
        println!("{:?}", driver.retire_old_versions("d", 0).await);
        if let Some(spec) = std::hint::black_box(None::<DeploymentSpec>) {
            println!("{:?}", driver.apply(&spec).await);
        }
    }
    if let Ok(vault) = KeyVaultClient::new(KeyVaultConfig::default(), tokens) {
        println!("{:?}", vault.set_secret(url, "n", &SecretString::new("v")).await);
    }
}
