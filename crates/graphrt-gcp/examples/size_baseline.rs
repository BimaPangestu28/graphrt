//! C13 size probe, baseline: what the gateway links anyway (tokio, reqwest +
//! rustls/ring, serde_json, the shared `ProviderHttp`). Compare its min-size
//! binary with `size_auth_gcp` / `size_cloud_run` to get the feature deltas.
//! `cargo build -p graphrt-gcp --profile min-size --features provider-cloud-run --examples`

use std::sync::Arc;

use graphrt_provider_api::http::{ProviderHttp, SecretString, StaticToken, TokenBucket};

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let url = std::env::args().nth(1).unwrap_or_else(|| "https://example.invalid".to_owned());
    let _provider = rustls::crypto::ring::default_provider().install_default();
    let Ok(client) = reqwest::Client::builder().build() else { return };
    let http = ProviderHttp::new(client, Arc::new(TokenBucket::new(5.0, 10)));
    let token = StaticToken(SecretString::new(std::env::args().nth(2).unwrap_or_default()));
    let response = http.send(|client| client.get(&url), Some(&token)).await;
    if let Ok(response) = response {
        let bytes = response.bytes().await.unwrap_or_default();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or_default();
        eprintln!("{}", value.get("name").and_then(serde_json::Value::as_str).unwrap_or_default());
    }
}
