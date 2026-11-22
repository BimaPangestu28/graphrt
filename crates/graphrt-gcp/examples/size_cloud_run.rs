//! C13 size probe: baseline + `auth-gcp` + `provider-cloud-run` (provider,
//! deploy driver, Secret Manager).

use graphrt_gcp::cloud_run::{CloudRunConfig, CloudRunDeployDriver, CloudRunProvider};
use graphrt_gcp::secret_manager::SecretManager;
use graphrt_provider_api::http::SecretString;
use graphrt_provider_api::{DeployDriver, DeploymentRef, Provider};

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let mut arguments = std::env::args().skip(1);
    let project = arguments.next().unwrap_or_default();
    let spec_json = arguments.next().unwrap_or_default();
    let _provider = rustls::crypto::ring::default_provider().install_default();
    let Ok(auth) = graphrt_gcp::auth::GcpAuth::from_env() else { return };
    let config = CloudRunConfig::new(project, "europe-west1");
    let Ok(provider) = CloudRunProvider::new(config.clone(), auth.clone()) else { return };
    let deployment = DeploymentRef::new("chat", "v1", "");
    if let Ok(endpoints) = provider.wake(&deployment, 1).await {
        eprintln!("{endpoints:?}");
    }
    eprintln!("{:?}", provider.list(&deployment).await.map(|replicas| replicas.len()));
    eprintln!("{:?}", provider.invoke_authorization(&deployment, false).await.map(|header| header.is_some()));
    let Ok(driver) = CloudRunDeployDriver::new(config.clone(), &auth) else { return };
    if let Ok(spec) = serde_json::from_str(&spec_json) {
        eprintln!("{:?}", driver.apply(&spec).await);
        eprintln!("{:?}", driver.retire_versions(&deployment, &["v0".to_owned()]).await);
    }
    if let Ok(secrets) = SecretManager::new(&config, &auth) {
        eprintln!("{:?}", secrets.put_secret("k", &SecretString::new("v")).await);
    }
}
