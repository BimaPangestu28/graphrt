//! `ProviderHttp`: the one HTTP layer of all providers (reqwest + rustls,
//! already linked by graphrt). Adds the per-API token bucket, jittered retry
//! of 429/5xx/network errors, and one forced token refresh on 401.

use std::sync::Arc;

use crate::error::{ProviderError, ProviderErrorKind};
use crate::http::bucket::TokenBucket;
use crate::http::retry::{JitterRng, RetryPolicy, parse_retry_after};
use crate::http::token::TokenSource;

/// Longest response body excerpt kept in an error message.
const ERROR_BODY_LIMIT: usize = 512;

/// Maps a non-2xx status and body excerpt to an error kind. Providers pass
/// their own to recognise capacity codes; the default is
/// [`ProviderError::kind_for_status`].
pub type StatusClassifier = fn(status: u16, body: &str) -> ProviderErrorKind;

/// HTTP client of one provider API.
#[derive(Debug, Clone)]
pub struct ProviderHttp {
    client: reqwest::Client,
    bucket: Arc<TokenBucket>,
    policy: RetryPolicy,
    rng: Arc<JitterRng>,
    classifier: StatusClassifier,
}

fn default_classifier(status: u16, _body: &str) -> ProviderErrorKind {
    ProviderError::kind_for_status(status)
}

impl ProviderHttp {
    /// Wraps `client` with a bucket and the default retry policy.
    pub fn new(client: reqwest::Client, bucket: Arc<TokenBucket>) -> Self {
        Self {
            client,
            bucket,
            policy: RetryPolicy::default(),
            rng: Arc::new(JitterRng::from_entropy()),
            classifier: default_classifier,
        }
    }

    /// Replaces the retry policy.
    pub fn with_policy(mut self, policy: RetryPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Replaces the jitter generator (deterministic tests).
    pub fn with_rng(mut self, rng: Arc<JitterRng>) -> Self {
        self.rng = rng;
        self
    }

    /// Replaces the status classifier.
    pub fn with_classifier(mut self, classifier: StatusClassifier) -> Self {
        self.classifier = classifier;
        self
    }

    /// The underlying client, for building requests.
    pub fn client(&self) -> &reqwest::Client {
        &self.client
    }

    /// Sends the request `build` makes (called again for every attempt) and
    /// returns the 2xx response. With `auth`, attaches `Authorization: Bearer`
    /// and refreshes the token once after a 401.
    pub async fn send<B>(&self, build: B, auth: Option<&dyn TokenSource>) -> Result<reqwest::Response, ProviderError>
    where
        B: Fn(&reqwest::Client) -> reqwest::RequestBuilder,
    {
        let mut refreshed = false;
        loop {
            let force_refresh = refreshed;
            let result = self.policy.run(&self.rng, |_| self.attempt(&build, auth, force_refresh)).await;
            match result {
                Err(error) if error.kind == ProviderErrorKind::Auth && auth.is_some() && !refreshed => refreshed = true,
                other => return other,
            }
        }
    }

    async fn attempt<B>(
        &self,
        build: &B,
        auth: Option<&dyn TokenSource>,
        force_refresh: bool,
    ) -> Result<reqwest::Response, ProviderError>
    where
        B: Fn(&reqwest::Client) -> reqwest::RequestBuilder,
    {
        self.bucket.acquire().await;
        let mut request = build(&self.client);
        if let Some(source) = auth {
            let token = source.token(force_refresh).await?;
            request = request.bearer_auth(token.expose());
        }
        let response = request.send().await.map_err(|error| ProviderError::transient(redact_reqwest_error(&error)))?;
        let status = response.status().as_u16();
        if (200..300).contains(&status) {
            return Ok(response);
        }
        let retry_after =
            response.headers().get("retry-after").and_then(|value| value.to_str().ok()).and_then(parse_retry_after);
        let body = response.text().await.unwrap_or_default();
        let excerpt: String = body.chars().take(ERROR_BODY_LIMIT).collect();
        let kind = (self.classifier)(status, &body);
        Err(ProviderError::new(kind, format!("HTTP {status}: {excerpt}")).with_retry_after(retry_after))
    }
}

/// A reqwest error without the URL's query string (which may carry a
/// presigned signature) and without headers.
fn redact_reqwest_error(error: &reqwest::Error) -> String {
    let kind = if error.is_timeout() {
        "timeout"
    } else if error.is_connect() {
        "connect error"
    } else {
        "request error"
    };
    match error.url() {
        Some(url) => format!("{kind} for {}://{}{}", url.scheme(), url.host_str().unwrap_or(""), url.path()),
        None => kind.to_owned(),
    }
}
