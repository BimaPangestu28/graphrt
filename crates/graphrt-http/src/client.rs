//! Policy-enforcing HTTP client service.

use std::net::SocketAddr;
use std::sync::{Arc, Once, OnceLock};
use std::time::Duration;

use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use reqwest::redirect;

use crate::policy::{EgressError, EgressPolicy};

/// Error building an [`HttpClient`].
#[derive(Clone, Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ClientError {
    #[error("invalid proxy `{0}`: {1}")]
    Proxy(String, String),
    #[error("cannot build HTTP client: {0}")]
    Build(String),
}

/// HTTP client shared by HTTP-calling nodes. Register it in
/// [`graphrt_core::Services`]; when absent, nodes use a client with the default
/// (restrictive) [`EgressPolicy`].
///
/// Every connection goes through the policy:
/// - literal-IP URLs and every redirect hop are validated before connecting;
/// - host names are resolved by a guarded resolver that refuses non-public
///   addresses, and the connection is made to exactly the addresses it returned
///   (so DNS rebinding cannot swap in a private address afterwards);
/// - environment proxy variables are ignored.
#[derive(Clone, Debug)]
pub struct HttpClient {
    policy: Arc<EgressPolicy>,
    clients: Arc<Clients>,
}

/// The four reqwest clients, each built on first use: building one loads the
/// system trust store, and most runs need one variant or none at all.
#[derive(Debug, Default)]
struct Clients {
    verified: OnceLock<Result<reqwest::Client, ClientError>>,
    unverified: OnceLock<Result<reqwest::Client, ClientError>>,
    /// Clients that never follow redirects; the `http-request` node follows them
    /// itself so it can strip credentials on cross-origin hops.
    manual_verified: OnceLock<Result<reqwest::Client, ClientError>>,
    manual_unverified: OnceLock<Result<reqwest::Client, ClientError>>,
}

impl HttpClient {
    /// Build a client enforcing `policy`. The proxy setting is validated here;
    /// the underlying connections are set up on first use.
    ///
    /// @throws `ClientError::Proxy` when `policy.proxy` is not a valid proxy URL.
    pub fn new(policy: EgressPolicy) -> Result<Self, ClientError> {
        install_crypto_provider();
        if let Some(proxy_url) = &policy.proxy {
            reqwest::Proxy::all(proxy_url).map_err(|e| ClientError::Proxy(proxy_url.clone(), e.to_string()))?;
        }
        Ok(Self { policy: Arc::new(policy), clients: Arc::default() })
    }

    /// Client with [`EgressPolicy::trusted`], for operator-configured endpoints.
    pub fn trusted() -> Result<Self, ClientError> {
        Self::new(EgressPolicy::trusted())
    }

    pub fn policy(&self) -> &EgressPolicy {
        &self.policy
    }

    /// Underlying client, built on first use. `verify_tls = false` disables
    /// certificate verification.
    ///
    /// Redirects are followed automatically (each hop re-validated). reqwest strips only
    /// `authorization`, `cookie` and `proxy-authorization` on cross-host hops, so callers
    /// sending other credential headers should use [`HttpClient::manual_redirect_client`].
    ///
    /// @throws `ClientError::Build` when the TLS backend cannot be initialised.
    pub fn client(&self, verify_tls: bool) -> Result<&reqwest::Client, ClientError> {
        let cell = if verify_tls { &self.clients.verified } else { &self.clients.unverified };
        self.get(cell, verify_tls, true)
    }

    /// Client that never follows redirects: 3xx responses are returned to the caller.
    ///
    /// @throws `ClientError::Build` when the TLS backend cannot be initialised.
    pub fn manual_redirect_client(&self, verify_tls: bool) -> Result<&reqwest::Client, ClientError> {
        let cell = if verify_tls { &self.clients.manual_verified } else { &self.clients.manual_unverified };
        self.get(cell, verify_tls, false)
    }

    fn get<'a>(
        &self,
        cell: &'a OnceLock<Result<reqwest::Client, ClientError>>,
        verify_tls: bool,
        follow_redirects: bool,
    ) -> Result<&'a reqwest::Client, ClientError> {
        cell.get_or_init(|| build(&self.policy, verify_tls, follow_redirects)).as_ref().map_err(Clone::clone)
    }

    /// Validate a URL against the policy (scheme, host lists, literal addresses).
    pub fn check_url(&self, url: &reqwest::Url) -> Result<(), EgressError> {
        self.policy.check_url(url)
    }
}

/// Use the small `ring` crypto backend. Installing fails harmlessly if the
/// process already chose a provider.
fn install_crypto_provider() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let _already_installed = rustls::crypto::ring::default_provider().install_default();
    });
}

fn build(policy: &Arc<EgressPolicy>, verify_tls: bool, follow_redirects: bool) -> Result<reqwest::Client, ClientError> {
    let redirect_policy = if !follow_redirects {
        redirect::Policy::none()
    } else {
        let policy = Arc::clone(policy);
        redirect::Policy::custom(move |attempt| {
            if attempt.previous().len() >= policy.max_redirects {
                return attempt.error(format!("too many redirects (limit {})", policy.max_redirects));
            }
            match policy.check_url(attempt.url()) {
                Ok(()) => attempt.follow(),
                Err(e) => attempt.error(format!("redirect blocked: {e}")),
            }
        })
    };
    let mut builder = reqwest::Client::builder()
        .no_proxy()
        .redirect(redirect_policy)
        .dns_resolver(Arc::new(GuardedResolver { policy: Arc::clone(policy) }))
        .connect_timeout(policy.connect_timeout)
        .read_timeout(policy.read_timeout)
        .pool_idle_timeout(Duration::from_secs(90))
        .tls_danger_accept_invalid_certs(!verify_tls);
    if let Some(proxy_url) = &policy.proxy {
        // The proxy resolves target hosts itself; it is expected to enforce its own egress rules.
        let proxy = reqwest::Proxy::all(proxy_url).map_err(|e| ClientError::Proxy(proxy_url.clone(), e.to_string()))?;
        builder = builder.proxy(proxy);
    }
    builder.build().map_err(|e| ClientError::Build(e.to_string()))
}

/// DNS resolver that refuses names resolving to non-public addresses.
#[derive(Debug)]
pub(crate) struct GuardedResolver {
    policy: Arc<EgressPolicy>,
}

impl Resolve for GuardedResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let policy = Arc::clone(&self.policy);
        let host = name.as_str().to_owned();
        Box::pin(async move {
            policy.check_host_name(&host)?;
            let resolved: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), 0)).await?.collect();
            let allowed = filter_addresses(&policy, &host, resolved)?;
            Ok(Box::new(allowed.into_iter()) as Addrs)
        })
    }
}

/// Keep only permitted addresses. If any resolved address is blocked the whole
/// name is refused: a name mixing public and private records is suspicious.
pub(crate) fn filter_addresses(
    policy: &EgressPolicy,
    host: &str,
    resolved: Vec<SocketAddr>,
) -> Result<Vec<SocketAddr>, EgressError> {
    if resolved.is_empty() {
        return Err(EgressError::InvalidUrl(format!("`{host}` did not resolve to any address")));
    }
    for addr in &resolved {
        policy.check_ip(addr.ip())?;
    }
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(s: &str) -> SocketAddr {
        SocketAddr::new(s.parse().unwrap(), 0)
    }

    #[test]
    fn resolver_filter_blocks_private_records() {
        let p = EgressPolicy::default();
        assert!(filter_addresses(&p, "h", vec![addr("93.184.216.34")]).is_ok());
        assert!(filter_addresses(&p, "h", vec![addr("127.0.0.1")]).is_err());
        assert!(filter_addresses(&p, "h", vec![addr("93.184.216.34"), addr("10.0.0.1")]).is_err());
        assert!(filter_addresses(&p, "h", vec![addr("::ffff:192.168.0.1")]).is_err());
        assert!(filter_addresses(&p, "h", vec![]).is_err());
        assert!(filter_addresses(&EgressPolicy::trusted(), "h", vec![addr("127.0.0.1")]).is_ok());
    }

    #[tokio::test]
    async fn resolver_refuses_localhost_name() {
        let r = GuardedResolver { policy: Arc::new(EgressPolicy::default()) };
        let name: Name = "localhost".parse().unwrap();
        assert!(r.resolve(name).await.is_err());
    }

    #[tokio::test]
    async fn resolver_respects_denied_hosts() {
        let policy = EgressPolicy { denied_hosts: vec!["localhost".into()], ..EgressPolicy::trusted() };
        let r = GuardedResolver { policy: Arc::new(policy) };
        let name: Name = "localhost".parse().unwrap();
        assert!(r.resolve(name).await.is_err());
    }

    #[test]
    fn builds() {
        assert!(HttpClient::new(EgressPolicy::default()).is_ok());
    }
}
