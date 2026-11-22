//! reqwest clients for Google APIs (rustls with the ring provider graphrt already links).

use std::time::Duration;

use graphrt_provider_api::ProviderError;

/// Connection timeout of every Google API call.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Options of [`build_client`].
#[derive(Debug, Clone, Copy)]
pub(crate) struct ClientOptions {
    /// Whole-request timeout; `None` for long-lived streams (Cloud Run invocations).
    pub request_timeout: Option<Duration>,
    /// Ignore `HTTP(S)_PROXY`: the metadata server must never be reached through a
    /// proxy, which would see the tokens it returns.
    pub bypass_proxy: bool,
}

/// Builds a client that never follows redirects (a redirect could carry a bearer
/// token or an assertion to another host) and speaks HTTP/1.1 only.
///
/// @throws `ProviderError` (fatal) when the TLS backend cannot be initialised.
pub(crate) fn build_client(options: ClientOptions) -> Result<reqwest::Client, ProviderError> {
    // reqwest is built without a bundled crypto provider; install ring once per process.
    // An error only means a provider is already installed, which is fine.
    let _already_installed = rustls::crypto::ring::default_provider().install_default();
    let mut builder = reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .http1_only();
    if let Some(timeout) = options.request_timeout {
        builder = builder.timeout(timeout);
    }
    if options.bypass_proxy {
        builder = builder.no_proxy();
    }
    builder.build().map_err(|error| ProviderError::fatal(format!("cannot build HTTP client: {error}")))
}
