//! `graphrt-llm-proxy`: run the example proxy + token broker.
//!
//! ```sh
//! PROXY_JWT_SECRET=$(openssl rand -hex 32) UPSTREAM_API_KEY=sk-... \
//! DEMO_USER_TOKEN=demo PUBLIC_BASE_URL=http://localhost:8787/v1 \
//! ALLOWED_MODELS=gpt-4o-mini cargo run --release
//! ```

use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    let config = match graphrt_llm_proxy::Config::from_env() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("config error: {e}");
            return ExitCode::from(2);
        }
    };
    let addr = std::env::var("LISTEN_ADDR").unwrap_or_else(|_| "127.0.0.1:8787".into());
    let router = match graphrt_llm_proxy::router(config) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("startup error: {e}");
            return ExitCode::FAILURE;
        }
    };
    let listener = match tokio::net::TcpListener::bind(&addr).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("cannot bind {addr}: {e}");
            return ExitCode::FAILURE;
        }
    };
    eprintln!("llm proxy listening on http://{addr} (POST /token, POST /v1/...)");
    let shutdown = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    match axum::serve(listener, router).with_graceful_shutdown(shutdown).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("server error: {e}");
            ExitCode::FAILURE
        }
    }
}
