//! The one request path of every Google REST call graphrt makes (Admin API,
//! Secret Manager). Funnelling them through a single non-generic function keeps
//! one copy of the shared retry loop in the binary instead of one per call site.

use graphrt_provider_api::ProviderError;
use graphrt_provider_api::http::{ProviderHttp, TokenSource};
use serde_json::Value;

/// HTTP method of a Google API call.
// POST/PATCH are only sent by the deploy half (`deploy-cloud-run`).
#[cfg_attr(not(feature = "deploy-cloud-run"), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Method {
    Get,
    Post,
    Patch,
}

/// Sends one authenticated JSON request (bucket, jittered retry, one refresh on
/// 401) and returns the parsed body (`Null` when empty).
pub(crate) async fn send_json(
    http: &ProviderHttp,
    tokens: &dyn TokenSource,
    method: Method,
    url: &str,
    body: Option<&str>,
) -> Result<Value, ProviderError> {
    let build = |client: &reqwest::Client| {
        let request = match method {
            Method::Get => client.get(url),
            Method::Post => client.post(url),
            Method::Patch => client.patch(url),
        };
        match body {
            Some(body) => request.header("content-type", "application/json").body(body.to_owned()),
            None => request,
        }
    };
    let response = http.send(build, Some(tokens)).await?;
    let bytes = response.bytes().await.map_err(|_| ProviderError::transient("API response was cut off"))?;
    if bytes.is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_slice(&bytes).map_err(|_| ProviderError::transient("API response is not JSON"))
}
