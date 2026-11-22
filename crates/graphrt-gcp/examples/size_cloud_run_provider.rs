//! C13 size probe: baseline + `auth-gcp` + the run-time Cloud Run provider only
//! (what the gateway links; the deploy driver and Secret Manager are deploy-time).

use graphrt_gcp::cloud_run::{CloudRunConfig, CloudRunProvider};
use graphrt_provider_api::{DeploymentRef, Provider};

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let project = std::env::args().nth(1).unwrap_or_default();
    let _provider = rustls::crypto::ring::default_provider().install_default();
    let Ok(auth) = graphrt_gcp::auth::GcpAuth::from_env() else { return };
    let config = CloudRunConfig::new(project, "europe-west1");
    let Ok(provider) = CloudRunProvider::new(config, auth) else { return };
    let deployment = DeploymentRef::new("chat", "v1", "");
    if let Ok(endpoints) = provider.wake(&deployment, 1).await {
        eprintln!("{endpoints:?}");
    }
    eprintln!("{:?}", provider.list(&deployment).await.map(|replicas| replicas.len()));
    eprintln!("{:?}", provider.invoke_authorization(&deployment, false).await.map(|header| header.is_some()));
}
