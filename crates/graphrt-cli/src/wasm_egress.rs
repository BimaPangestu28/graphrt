//! Outbound HTTP of WebAssembly plugins (`graphrt::http`) through graphrt-http.
//!
//! The guest's requests go through the same SSRF-safe [`HttpClient`] as the
//! `http-request` node: public addresses only unless `--allow-private-network`,
//! DNS answers checked before connecting (no rebinding), proxy variables
//! ignored. Redirects are never followed: a 3xx goes back to the guest, so a
//! credential header such as Supabase's `apikey` cannot be replayed to another
//! host. The body is read up to the plugin's limit.

use std::time::Duration;

use graphrt_http::{EgressPolicy, HttpClient};
use graphrt_wasm::{HttpEgress, HttpError, HttpRequest, HttpResponse};

/// [`HttpEgress`] over an [`HttpClient`].
#[derive(Clone, Debug)]
pub struct ClientEgress {
    client: HttpClient,
}

impl ClientEgress {
    /// Egress with the default policy; `allow_private_network` also allows
    /// loopback, private and link-local addresses.
    ///
    /// @throws String - The HTTP client could not be built.
    pub fn new(allow_private_network: bool) -> Result<Self, String> {
        let policy = EgressPolicy { allow_private: allow_private_network, max_redirects: 0, ..EgressPolicy::default() };
        HttpClient::new(policy).map(|client| Self { client }).map_err(|e| e.to_string())
    }
}

impl HttpEgress for ClientEgress {
    fn send(&self, request: HttpRequest) -> Result<HttpResponse, HttpError> {
        let handle = tokio::runtime::Handle::try_current()
            .map_err(|_| HttpError::new("http_error", "outbound HTTP needs the host's Tokio runtime"))?;
        let timeout = request.timeout;
        handle.block_on(async move {
            match tokio::time::timeout(timeout, self.exchange(request)).await {
                Ok(result) => result,
                Err(_) => {
                    Err(HttpError::new("timeout", format!("no complete response within {} ms", timeout.as_millis())))
                }
            }
        })
    }
}

impl ClientEgress {
    async fn exchange(&self, request: HttpRequest) -> Result<HttpResponse, HttpError> {
        let url = reqwest::Url::parse(&request.url)
            .map_err(|e| HttpError::new("invalid_request", format!("invalid URL: {e}")))?;
        self.client.check_url(&url).map_err(|e| HttpError::new("egress_denied", e.to_string()))?;
        let method = reqwest::Method::from_bytes(request.method.as_bytes())
            .map_err(|e| HttpError::new("invalid_request", e.to_string()))?;
        let client =
            self.client.manual_redirect_client(true).map_err(|e| HttpError::new("http_error", e.to_string()))?;
        let mut builder = client.request(method, url).timeout(request.timeout.max(Duration::from_millis(1)));
        for (name, value) in &request.headers {
            builder = builder.header(name.as_str(), value.as_str());
        }
        if let Some(body) = request.body {
            builder = builder.body(body);
        }
        let mut response = builder.send().await.map_err(|e| send_error(&e))?;
        let status = response.status().as_u16();
        let headers = response
            .headers()
            .iter()
            .filter_map(|(name, value)| value.to_str().ok().map(|v| (name.as_str().to_owned(), v.to_owned())))
            .collect();
        let limit = request.max_response_bytes.min(self.client.policy().max_response_bytes);
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|e| send_error(&e))? {
            if body.len().saturating_add(chunk.len()) > limit {
                return Err(HttpError::new("too_large", format!("response body is above the {limit}-byte limit")));
            }
            body.extend_from_slice(&chunk);
        }
        let body =
            String::from_utf8(body).map_err(|_| HttpError::new("http_error", "response body is not UTF-8 text"))?;
        Ok(HttpResponse { status, headers, body })
    }
}

/// Classify a reqwest failure. The guarded resolver refuses non-public
/// addresses while connecting, so that surfaces as a connect error naming them.
fn send_error(error: &reqwest::Error) -> HttpError {
    let message = error_chain(error);
    if error.is_timeout() {
        return HttpError::new("timeout", message);
    }
    if message.contains("not a public address") || message.contains("not allowed by the egress policy") {
        return HttpError::new("egress_denied", message);
    }
    HttpError::new("http_error", message)
}

fn error_chain(error: &(dyn std::error::Error + 'static)) -> String {
    let mut message = error.to_string();
    let mut source = error.source();
    while let Some(inner) = source {
        message.push_str(": ");
        message.push_str(&inner.to_string());
        source = inner.source();
    }
    message
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::io::{Read, Write};
    use std::net::TcpListener;

    use super::*;

    /// One-shot HTTP server answering `response`; returns its URL and the raw request.
    fn serve_once(response: &'static str) -> (String, std::thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/rest/v1/rpc/get_balance", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buffer = [0u8; 4096];
            let read = stream.read(&mut buffer).unwrap();
            stream.write_all(response.as_bytes()).unwrap();
            String::from_utf8_lossy(&buffer[..read]).into_owned()
        });
        (url, handle)
    }

    fn request(url: &str) -> HttpRequest {
        HttpRequest {
            method: "POST".into(),
            url: url.into(),
            headers: vec![("apikey".into(), "pk".into())],
            body: Some("{}".into()),
            timeout: Duration::from_secs(5),
            max_response_bytes: 1024,
        }
    }

    fn send(egress: ClientEgress, request: HttpRequest) -> Result<HttpResponse, HttpError> {
        let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build().unwrap();
        runtime.block_on(async move { tokio::task::spawn_blocking(move || egress.send(request)).await.unwrap() })
    }

    #[test]
    fn sends_through_the_policy_and_reads_the_body() {
        let (url, server) =
            serve_once("HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 2\r\n\r\n[]");
        let response = send(ClientEgress::new(true).unwrap(), request(&url)).unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(response.body, "[]");
        let raw = server.join().unwrap().to_ascii_lowercase();
        assert!(raw.starts_with("post /rest/v1/rpc/get_balance") && raw.contains("apikey: pk"), "{raw}");
    }

    #[test]
    fn loopback_is_refused_by_default_and_redirects_are_not_followed() {
        let error = send(ClientEgress::new(false).unwrap(), request("http://127.0.0.1:9/x")).unwrap_err();
        assert_eq!(error.kind, "egress_denied", "{error:?}");
        let (url, _server) =
            serve_once("HTTP/1.1 302 Found\r\nlocation: http://169.254.169.254/\r\ncontent-length: 0\r\n\r\n");
        assert_eq!(send(ClientEgress::new(true).unwrap(), request(&url)).unwrap().status, 302);
    }

    #[test]
    fn large_bodies_are_refused() {
        let (url, _server) = serve_once("HTTP/1.1 200 OK\r\ncontent-length: 20\r\n\r\nxxxxxxxxxxxxxxxxxxxx");
        let mut big = request(&url);
        big.max_response_bytes = 10;
        assert_eq!(send(ClientEgress::new(true).unwrap(), big).unwrap_err().kind, "too_large");
    }
}
