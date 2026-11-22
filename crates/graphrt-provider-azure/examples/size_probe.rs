//! Size probe for conformance C13: the binary delta of `provider-aca`
//! (`auth-azure` included) on top of what the gateway links anyway (tokio,
//! reqwest + rustls/ring, serde_json). Built twice by
//! `crates/graphrt-provider-azure/measure-size.sh`; never run in CI.
#![allow(clippy::print_stdout)]

use std::time::Duration;

fn main() {
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build();
    let Ok(runtime) = runtime else { return };
    let url = std::env::args().nth(1).unwrap_or_else(|| "http://127.0.0.1:9".into());
    runtime.block_on(async move {
        baseline(&url).await;
        #[cfg(feature = "provider-aca")]
        azure(&url).await;
    });
}

/// What every gateway build already contains.
async fn baseline(url: &str) {
    let _already_installed = rustls::crypto::ring::default_provider().install_default();
    let Ok(client) = reqwest::Client::builder().timeout(Duration::from_secs(1)).build() else { return };
    if let Ok(response) = client.get(url).send().await
        && let Ok(bytes) = response.bytes().await
    {
        let parsed: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or_default();
        let digest = ring::digest::digest(&ring::digest::SHA256, parsed.to_string().as_bytes());
        println!("{:?}", digest.as_ref().first());
    }
}

/// Every public entry point of the Azure family, so nothing is dead-code eliminated.
#[cfg(feature = "provider-aca")]
async fn azure(url: &str) {
    use std::sync::Arc;

    use graphrt_provider_azure::aca::{AcaConfig, AcaDeployDriver, AcaProvider};
    use graphrt_provider_azure::api::{DeployDriver, DeploymentRef, Provider, ReleaseMode, ReplicaRef, SecretString};
    use graphrt_provider_azure::arm::{ArmClient, ArmConfig};
    use graphrt_provider_azure::auth::{AuthEndpoints, AzureCredential, CertificateKey, TokenProvider};
    use graphrt_provider_azure::keyvault::{KeyVaultClient, KeyVaultConfig};

    let env = |name: &str| std::env::var(name).ok();
    let Ok(credential) = AzureCredential::from_env(&env, &AuthEndpoints::default()) else { return };
    let _ = std::hint::black_box(CertificateKey::from_pem(url));
    let Ok(tokens) = TokenProvider::new(credential) else { return };
    let tokens = Arc::new(tokens);
    let Ok(arm) = ArmClient::new(ArmConfig { endpoint: url.into(), ..ArmConfig::default() }, tokens.clone()) else {
        return;
    };
    let config = AcaConfig::new("sub", "rg");
    let Ok(provider) = AcaProvider::new(arm.clone(), config.clone()) else { return };
    let deployment = DeploymentRef { deployment: "d".into(), version: 1, resource: "d".into() };
    println!("{:?}", provider.wake(&deployment, 1).await);
    println!("{:?}", provider.list(&deployment).await);
    let replica = ReplicaRef { deployment: deployment.clone(), replica: "r".into() };
    println!("{:?}", provider.release(&replica, ReleaseMode::ScaleIn).await);
    println!("{:?}", provider.retire_old_versions("d", 0).await);
    if let Ok(driver) = AcaDeployDriver::new(arm, config, KeyVaultConfig::default()) {
        let spec = std::hint::black_box(None::<graphrt_provider_azure::aca::AcaAppSpec>);
        if let Some(spec) = spec {
            println!("{:?}", driver.apply(&spec).await);
        }
    }
    if let Ok(vault) = KeyVaultClient::new(KeyVaultConfig::default(), tokens) {
        println!("{:?}", vault.set_secret(url, "n", &SecretString::new("v")).await);
    }
}
