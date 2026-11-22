//! C13 size probe: baseline + `auth-gcp` (metadata server, service-account JWT).

use std::sync::Arc;

use graphrt_provider_api::http::{ProviderHttp, TokenBucket};

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let url = std::env::args().nth(1).unwrap_or_else(|| "https://example.invalid".to_owned());
    let _provider = rustls::crypto::ring::default_provider().install_default();
    let Ok(client) = reqwest::Client::builder().build() else { return };
    let http = ProviderHttp::new(client, Arc::new(TokenBucket::new(5.0, 10)));
    let Ok(auth) = graphrt_gcp::auth::GcpAuth::from_env() else { return };
    let tokens = auth.access_tokens();
    let response = http.send(|client| client.get(&url), Some(tokens.as_ref())).await;
    if let Ok(response) = response {
        let bytes = response.bytes().await.unwrap_or_default();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or_default();
        eprintln!("{}", value.get("name").and_then(serde_json::Value::as_str).unwrap_or_default());
    }
    let audience = std::env::args().nth(2).unwrap_or_default();
    if let Ok(token) = auth.id_tokens(&audience).token(false).await {
        eprintln!("{}", token.expose().len());
    }
}
