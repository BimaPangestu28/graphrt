//! In-cluster config: the pod's service account token and CA, the API server
//! from `KUBERNETES_SERVICE_HOST` / `KUBERNETES_SERVICE_PORT`.

use std::path::Path;
use super::{AuthError, ClusterConfig, TokenFile, cached};

/// Where Kubernetes mounts the service account of a pod.
pub const IN_CLUSTER_DIR: &str = "/var/run/secrets/kubernetes.io/serviceaccount";

/// In-cluster config from the environment and [`IN_CLUSTER_DIR`].
///
/// @throws AuthError::NotInCluster - when the service env vars or token are missing
pub fn in_cluster_config() -> Result<ClusterConfig, AuthError> {
    let host = std::env::var("KUBERNETES_SERVICE_HOST").ok();
    let port = std::env::var("KUBERNETES_SERVICE_PORT").ok();
    in_cluster_config_from(host.as_deref(), port.as_deref(), Path::new(IN_CLUSTER_DIR))
}

/// In-cluster config from explicit values (testable form of [`in_cluster_config`]).
///
/// @param host - `KUBERNETES_SERVICE_HOST` (an IPv6 address is bracketed)
/// @param port - `KUBERNETES_SERVICE_PORT`
/// @param account_dir - directory holding `token`, `ca.crt`, `namespace`
pub fn in_cluster_config_from(
    host: Option<&str>,
    port: Option<&str>,
    account_dir: &Path,
) -> Result<ClusterConfig, AuthError> {
    let (Some(host), Some(port)) = (host.filter(|host| !host.is_empty()), port.filter(|port| !port.is_empty())) else {
        return Err(AuthError::NotInCluster("KUBERNETES_SERVICE_HOST / KUBERNETES_SERVICE_PORT not set".into()));
    };
    let token_path = account_dir.join("token");
    if !token_path.is_file() {
        return Err(AuthError::NotInCluster(format!("no service account token at {}", token_path.display())));
    }
    let ca_path = account_dir.join("ca.crt");
    let ca_pem = std::fs::read(&ca_path)
        .map_err(|error| AuthError::NotInCluster(format!("read {}: {error}", ca_path.display())))?;
    let namespace = std::fs::read_to_string(account_dir.join("namespace"))
        .ok()
        .map(|namespace| namespace.trim().to_owned())
        .filter(|namespace| !namespace.is_empty());
    let host = if host.contains(':') && !host.starts_with('[') { format!("[{host}]") } else { host.to_owned() };
    Ok(ClusterConfig {
        server: format!("https://{host}:{port}"),
        ca_pem: Some(ca_pem),
        client_identity_pem: None,
        token_source: Some(cached(TokenFile(token_path))),
        namespace,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("graphrt-k8s-sa-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("token"), "sa-token\n").unwrap();
        std::fs::write(dir.join("ca.crt"), "-----BEGIN CERTIFICATE-----\n").unwrap();
        std::fs::write(dir.join("namespace"), "agents\n").unwrap();
        dir
    }

    #[tokio::test]
    async fn reads_service_account_mount() {
        let dir = account_dir("ok");
        let config = in_cluster_config_from(Some("10.96.0.1"), Some("443"), &dir).unwrap();
        assert_eq!(config.server, "https://10.96.0.1:443");
        assert_eq!(config.namespace.as_deref(), Some("agents"));
        let token = config.token_source.unwrap().token(false).await.unwrap();
        assert_eq!(token.expose(), "sa-token");
        let ipv6 = in_cluster_config_from(Some("fd00::1"), Some("443"), &dir).unwrap();
        assert_eq!(ipv6.server, "https://[fd00::1]:443");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn outside_a_pod_is_not_in_cluster() {
        let missing = Path::new("/nonexistent/graphrt-sa");
        assert!(matches!(in_cluster_config_from(None, None, missing), Err(AuthError::NotInCluster(_))));
        assert!(matches!(in_cluster_config_from(Some("10.0.0.1"), Some("443"), missing), Err(AuthError::NotInCluster(_))));
    }
}
