//! App -> token broker -> proxy -> fake provider, using graphrt's own
//! `TokenBroker` and OpenAI-compatible client as the "app".

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::http::HeaderMap;
use axum::routing::post;
use graphrt_llm::credentials::{BrokerAuth, StaticCredential, TokenBroker, TokenBrokerConfig};
use graphrt_llm::openai::{OpenAiConfig, OpenAiModel};
use graphrt_llm::{ChatModel, ChatRequest, LlmError, Message, collect};
use graphrt_llm_proxy::{Config, UpstreamAuth, router};

const SSE: &str = concat!(
    "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"proxied\"},\"finish_reason\":null}]}\n\n",
    "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
    "data: [DONE]\n\n"
);

async fn spawn(app: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}")
}

#[tokio::test]
async fn app_reaches_provider_only_through_broker_tokens() {
    // Fake provider: records the authorization header the proxy sends.
    let seen: Arc<Mutex<Vec<String>>> = Arc::default();
    let seen2 = seen.clone();
    let upstream = Router::new().route(
        "/v1/chat/completions",
        post(move |headers: HeaderMap| {
            let seen = seen2.clone();
            async move {
                let auth = headers.get("authorization").and_then(|v| v.to_str().ok()).unwrap_or_default().to_owned();
                seen.lock().unwrap().push(auth);
                ([("content-type", "text/event-stream")], SSE)
            }
        }),
    );
    let upstream_url = spawn(upstream).await;

    let proxy_url = spawn(
        router(Config {
            jwt_secret: b"0123456789abcdef0123456789abcdef".to_vec(),
            upstream_base_url: format!("{upstream_url}/v1"),
            upstream_key: "sk-server-side".into(),
            upstream_auth: UpstreamAuth::Bearer,
            allowed_models: vec!["allowed-model".into()],
            allowed_paths: vec!["/chat/completions".into()],
            token_ttl: Duration::from_secs(600),
            demo_user_token: Some("user-session".into()),
            public_base_url: None,
        })
        .unwrap(),
    )
    .await;

    // The app: knows only its user session, never the provider key.
    let broker = TokenBroker::cached(
        TokenBrokerConfig::new(format!("{proxy_url}/token"))
            .auth(BrokerAuth::Bearer(Arc::new(StaticCredential::new("user-session")))),
    )
    .unwrap();
    let model = OpenAiModel::new(OpenAiConfig::new(format!("{proxy_url}/v1"), None).with_credentials(Arc::new(broker)))
        .unwrap();

    let stream = model.chat(ChatRequest::new("allowed-model", vec![Message::user("hi")])).await.unwrap();
    assert_eq!(collect(stream, |_| {}).await.unwrap().text, "proxied");
    assert_eq!(*seen.lock().unwrap(), vec!["Bearer sk-server-side".to_owned()]);

    // Models outside the allow-list are refused by the proxy.
    let err = model.chat(ChatRequest::new("expensive-model", vec![Message::user("hi")])).await.err().unwrap();
    assert!(matches!(err, LlmError::BadRequest(_)), "{err:?}");

    // A wrong user session cannot obtain a token.
    let bad = TokenBroker::new(
        TokenBrokerConfig::new(format!("{proxy_url}/token"))
            .auth(BrokerAuth::Bearer(Arc::new(StaticCredential::new("someone-else")))),
    )
    .unwrap();
    let bad_model =
        OpenAiModel::new(OpenAiConfig::new(format!("{proxy_url}/v1"), None).with_credentials(Arc::new(bad))).unwrap();
    let err = bad_model.chat(ChatRequest::new("allowed-model", vec![Message::user("hi")])).await.err().unwrap();
    assert!(matches!(err, LlmError::Auth(_)), "{err:?}");

    // Calling the proxy without a token, or with a forged one, is rejected.
    let forged =
        OpenAiModel::new(OpenAiConfig::new(format!("{proxy_url}/v1"), Some("forged.jwt.token".into()))).unwrap();
    let err = forged.chat(ChatRequest::new("allowed-model", vec![Message::user("hi")])).await.err().unwrap();
    assert!(matches!(err, LlmError::Auth(_)), "{err:?}");
    assert_eq!(seen.lock().unwrap().len(), 1, "rejected calls never reach the provider");
}

const ANTHROPIC_SSE: &str = concat!(
    "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":1}}}\n\n",
    "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
    "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"via proxy\"}}\n\n",
    "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":2}}\n\n",
    "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
);

#[tokio::test]
async fn anthropic_clients_work_through_the_proxy() {
    use graphrt_llm::anthropic::{AnthropicConfig, AnthropicModel};
    let seen: Arc<Mutex<Vec<String>>> = Arc::default();
    let seen2 = seen.clone();
    let upstream = Router::new().route(
        "/v1/messages",
        post(move |headers: HeaderMap| {
            let seen = seen2.clone();
            async move {
                let key = headers.get("x-api-key").and_then(|v| v.to_str().ok()).unwrap_or_default().to_owned();
                seen.lock().unwrap().push(key);
                ([("content-type", "text/event-stream")], ANTHROPIC_SSE)
            }
        }),
    );
    let upstream_url = spawn(upstream).await;

    // The proxy's public origin must be known to hand out base URLs; bind first.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_url = format!("http://{}", listener.local_addr().unwrap());
    let app = router(Config {
        jwt_secret: b"0123456789abcdef0123456789abcdef".to_vec(),
        upstream_base_url: format!("{upstream_url}/v1"),
        upstream_key: "sk-ant-server-side".into(),
        upstream_auth: UpstreamAuth::XApiKey,
        allowed_models: vec![],
        allowed_paths: vec!["/messages".into()],
        token_ttl: Duration::from_secs(600),
        demo_user_token: Some("user-session".into()),
        public_base_url: Some(proxy_url.clone()),
    })
    .unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    // The app's Anthropic client takes base_url from the broker (the bare origin)
    // and sends the token as `x-api-key`.
    let broker = TokenBroker::cached(
        TokenBrokerConfig::new(format!("{proxy_url}/token"))
            .auth(BrokerAuth::Bearer(Arc::new(StaticCredential::new("user-session")))),
    )
    .unwrap();
    let model =
        AnthropicModel::new(AnthropicConfig::default().with_credentials(Arc::new(broker)).credential_name("anthropic"))
            .unwrap();
    let stream = model.chat(ChatRequest::new("claude", vec![Message::user("hi")])).await.unwrap();
    assert_eq!(collect(stream, |_| {}).await.unwrap().text, "via proxy");
    assert_eq!(*seen.lock().unwrap(), vec!["sk-ant-server-side".to_owned()]);
}
