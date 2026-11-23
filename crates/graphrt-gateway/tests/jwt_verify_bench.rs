//! Microbenchmark of end-user JWT verification on the gateway's hot path
//! (audit §4.1: RS256 ≤ 50 µs with the JWKS key cached).
//!
//! The token below is signed with a throw-away 2048-bit RSA key whose private
//! half was never stored; only the public JWK is here. It expires in 2100.
//!
//! Ignored by default (timing): `bench/targets.sh` runs it on Linux, or by hand:
//!
//! ```sh
//! cargo test --release -p graphrt-gateway --test jwt_verify_bench -- --ignored --nocapture
//! ```
//!
//! The budget comes from `GRAPHRT_JWT_BUDGET_US` (default 50, the
//! `jwt_verify_rs256_us` target in bench/budgets.toml).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::print_stdout, clippy::indexing_slicing)]

use std::time::Instant;

use graphrt_gateway::auth::{DeploymentAuth, JwtConfig, Principal};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const ISSUER: &str = "https://auth.bench.invalid";
const AUDIENCE: &str = "bench";
const TOKEN: &str = "eyJhbGciOiJSUzI1NiIsInR5cCI6IkpXVCIsImtpZCI6ImJlbmNoIn0.\
eyJpc3MiOiJodHRwczovL2F1dGguYmVuY2guaW52YWxpZCIsImF1ZCI6ImJlbmNoIiwic3ViIjoidXNlci00MiIsImV4cCI6NDEwMjQ0NDgwMH0.\
M50GVyofiQVA7yOH7gB_zHRx0WeqWWJluhfHar5KySXMN54oAQeS8CfxOBzlowd6ivqXrQUXmtRnuo_SwzreILAWuRqqwgu1oBVYGxPvz3KI1ZjSKsE\
q-_pUmh3u65eKf49oWN-6bnqnX9gjJOrjEtbSTfBfmeGUzHuSeEI-i26n7j5WFEdQF2oo4v_MMscLBU9L86t6OP1YTtmWpX7Wes-WKfjy4yH-45srfO1a\
iOUV342l0rIWsaagM17OlcEWmbxVFLzg4XjX8yjhnh0jT4HmrEwaVxqRggNIcql3ublPmLHlEHhs0SAZIzdmHovFdF9FrgTxtJ9nShxlu13ScQ";
const MODULUS: &str = "pCcAMh3ER16Aigv8m77ztFuivC3qbrDCP1JB_geID9koEmZd7jcDbrdmUvqtO_x2i-5QfCh2Kq12SI3K5-H5pIR3I0UWPvj\
1hXycjpeANsMVSh_kh_y-3WNsCG8sdLKdSbyuuaMGt2DTF_NvJomH6R23OpIqYGTa8PhN5wXYNNQWXpbFwzeAoTROnyZmPmNPKhRaSEt2I0kc851jYA17\
nuiMCTYd8Y43mOWTmNtV8He-eS9B6fAKRrqt8gKXbThc6SZ4Eiv1FAHohlYI4c7R9dstgTvrRl8oxoP9m3LKwd-92ERc2M23-IMmFXVVEbzwSptKYPd1i\
6H2cmNhf4IvGQ";
const ITERATIONS: usize = 5_000;
const NOW_SECS: u64 = 1_800_000_000;

/// Serves the JWKS document on loopback (fetched once, then cached).
async fn serve_jwks() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/jwks.json", listener.local_addr().unwrap());
    let body = format!(r#"{{"keys":[{{"kty":"RSA","kid":"bench","alg":"RS256","n":"{MODULUS}","e":"AQAB"}}]}}"#);
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let mut request = [0_u8; 2048];
            let _read = stream.read(&mut request).await;
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _written = stream.write_all(response.as_bytes()).await;
        }
    });
    url
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "timing benchmark; run with --release --ignored"]
async fn rs256_verification_stays_under_budget() {
    graphrt_gateway::install_crypto_provider();
    let config = JwtConfig {
        issuer: ISSUER.to_owned(),
        audience: AUDIENCE.to_owned(),
        jwks_url: Some(serve_jwks().await),
        hs256_secret_env: None,
    };
    let auth = DeploymentAuth::new(Vec::new(), Some(config), &reqwest::Client::new()).unwrap();
    let principal = auth.authenticate(Some(TOKEN), None, NOW_SECS).await.unwrap();
    assert_eq!(principal, Principal::User { subject: "user-42".to_owned(), expires_at: 4_102_444_800 });

    let mut samples_ns = Vec::with_capacity(ITERATIONS);
    let began = Instant::now();
    for _ in 0..ITERATIONS {
        let started = Instant::now();
        let verified = auth.authenticate(Some(TOKEN), None, NOW_SECS).await;
        samples_ns.push(started.elapsed().as_nanos() as f64);
        assert!(verified.is_ok());
    }
    let mean_us = began.elapsed().as_secs_f64() * 1e6 / ITERATIONS as f64;
    samples_ns.sort_by(f64::total_cmp);
    let p50_us = samples_ns[ITERATIONS / 2] / 1000.0;
    let p95_us = samples_ns[ITERATIONS * 95 / 100] / 1000.0;
    println!(
        "{{\"jwt_verify_rs256_mean_us\": {mean_us:.2}, \"p50_us\": {p50_us:.2}, \"p95_us\": {p95_us:.2}, \"iterations\": {ITERATIONS}}}"
    );
    let budget_us: f64 = std::env::var("GRAPHRT_JWT_BUDGET_US").ok().and_then(|v| v.parse().ok()).unwrap_or(50.0);
    assert!(mean_us <= budget_us, "RS256 verification {mean_us:.2} µs > budget {budget_us} µs");
}
