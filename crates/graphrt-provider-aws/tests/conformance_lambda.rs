//! Conformance suite (plan §7) for `provider-lambda` against the fake Lambda API.
#![cfg(all(feature = "provider-ecs", feature = "provider-lambda"))]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::print_stdout)]

mod support;

use std::collections::HashMap;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures::StreamExt;
use graphrt_provider_aws::api::{Addressing, DeploymentRef, Provider, ProviderErrorKind, ReleaseMode};
use graphrt_provider_aws::lambda::{
    AgentRequest, InvokeError, LambdaConfig, LambdaFunction, LambdaProvider, StreamMode,
};
use support::{FakeAws, LambdaBehavior, fast_retry, quantile};

fn provider_with(fake: &FakeAws, max_run_secs: u64, buffering_threshold: Duration) -> LambdaProvider {
    let config = LambdaConfig {
        endpoint: fake.endpoint("lambda"),
        limits: fast_retry(),
        functions: HashMap::from([(
            "chat".to_owned(),
            LambdaFunction { function_name: "arn:aws:lambda:eu-west-1:123:function:graphrt-chat".into() },
        )]),
        max_run_secs,
        buffering_threshold,
    };
    LambdaProvider::new(fake.chain(), config).unwrap()
}

fn provider(fake: &FakeAws) -> LambdaProvider {
    provider_with(fake, 840, Duration::from_millis(300))
}

fn deployment() -> DeploymentRef {
    DeploymentRef::new("chat", "7", "")
}

fn request() -> AgentRequest {
    AgentRequest {
        method: "POST".into(),
        path_and_query: "/internal/v1/runs".into(),
        headers: vec![
            ("graphrt-sig".into(), "v1;kid=k1;ts=1;mac=00".into()),
            ("content-type".into(), "application/json".into()),
        ],
        body: br#"{"inputs":{}}"#.to_vec(),
        request_id: "run-1".into(),
    }
}

fn set(fake: &FakeAws, behavior: LambdaBehavior) {
    fake.state.lock().unwrap().lambda = Some(behavior);
}

async fn collect(response: graphrt_provider_aws::lambda::LambdaResponse) -> (Vec<u8>, Option<InvokeError>) {
    let mut body = Vec::new();
    let mut stream = response.body;
    while let Some(item) = stream.next().await {
        match item {
            Ok(chunk) => body.extend_from_slice(&chunk),
            Err(e) => return (body, Some(e)),
        }
    }
    (body, None)
}

#[tokio::test]
async fn invoke_sends_a_function_url_event_with_qualifier() {
    let fake = support::start().await;
    let provider = provider(&fake);
    let response = provider.invoke_agent(&deployment(), &request()).await.unwrap();
    assert_eq!(response.head.status, 200);
    let (body, error) = collect(response).await;
    assert!(error.is_none(), "{error:?}");
    assert_eq!(String::from_utf8_lossy(&body).matches("event: token").count(), 3);
    let state = fake.state.lock().unwrap();
    assert_eq!(state.lambda_queries[0], "arn:aws:lambda:eu-west-1:123:function:graphrt-chat?Qualifier=7");
    let event = &state.lambda_requests[0];
    assert_eq!(event["version"], "2.0");
    assert_eq!(event["rawPath"], "/internal/v1/runs");
    assert_eq!(event["headers"]["graphrt-sig"], "v1;kid=k1;ts=1;mac=00");
    assert_eq!(state.signature_failures, 0);
}

#[tokio::test]
async fn c1_c4_wake_release_list_are_url_addressed_no_ops() {
    let fake = support::start().await;
    let provider = provider(&fake);
    assert_eq!(provider.capabilities().addressing, Addressing::Url);
    assert_eq!(provider.capabilities().max_grace_secs, 0, "no preemption window");
    let endpoints = provider.wake(&deployment(), 20).await.unwrap();
    assert_eq!(endpoints.len(), 1);
    assert_eq!(support::url_of(&endpoints[0].address), "lambda://arn:aws:lambda:eu-west-1:123:function:graphrt-chat:7");
    provider.release(&endpoints[0].replica, ReleaseMode::ScaleIn).await.unwrap();
    assert_eq!(provider.list(&deployment()).await.unwrap().len(), 1);
    assert_eq!(fake.state.lock().unwrap().lambda_requests.len(), 0, "no invoke on wake/release/list");
}

#[tokio::test]
async fn c6_streaming_adds_under_50ms_per_event_and_never_buffers() {
    let fake = support::start().await;
    let provider = provider(&fake);
    set(&fake, LambdaBehavior::Stream { chunks: 60, interval: Duration::from_millis(20) });
    let response = provider.invoke_agent(&deployment(), &request()).await.unwrap();
    let diagnostics = response.diagnostics.clone();
    let mut stream = response.body;
    let mut delays = Vec::new();
    let mut buffer = String::new();
    while let Some(chunk) = stream.next().await {
        let received_us = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_micros();
        buffer.push_str(&String::from_utf8_lossy(&chunk.unwrap()));
        while let Some(end) = buffer.find("\n\n") {
            let event: String = buffer.drain(..end + 2).collect();
            let sent: u128 = event.split("\"sent_us\":").nth(1).unwrap().split(',').next().unwrap().parse().unwrap();
            delays.push(Duration::from_micros(u64::try_from(received_us.saturating_sub(sent)).unwrap()));
        }
    }
    assert_eq!(delays.len(), 60);
    let p95 = quantile(delays.clone(), 0.95);
    println!("C6 lambda fake: per-event added latency p50 {:?}, p95 {p95:?}", quantile(delays, 0.5));
    assert!(p95 <= Duration::from_millis(50), "p95 {p95:?}");
    assert_eq!(diagnostics.mode(), StreamMode::Streaming);
    assert!(diagnostics.body_chunks() >= 60);
}

#[tokio::test]
async fn c6_buffered_adapter_reply_is_detected() {
    let fake = support::start().await;
    let provider = provider(&fake);
    set(&fake, LambdaBehavior::Buffered { chunks: 5, delay: Duration::from_millis(50) });
    let response = provider.invoke_agent(&deployment(), &request()).await.unwrap();
    assert_eq!(response.diagnostics.mode(), StreamMode::Buffered);
    let (body, error) = collect(response).await;
    assert!(error.is_none());
    assert_eq!(String::from_utf8_lossy(&body).matches("event: token").count(), 5);
}

#[tokio::test]
async fn c6_unflushed_single_chunk_after_threshold_is_reported_as_buffered() {
    let fake = support::start().await;
    let provider = provider(&fake);
    set(&fake, LambdaBehavior::SingleChunk { chunks: 5, delay: Duration::from_millis(400) });
    let response = provider.invoke_agent(&deployment(), &request()).await.unwrap();
    let diagnostics = response.diagnostics.clone();
    let (_, error) = collect(response).await;
    assert!(error.is_none());
    assert_eq!(diagnostics.mode(), StreamMode::Buffered);
    assert!(diagnostics.first_body_after().unwrap() >= Duration::from_millis(400));
}

#[tokio::test]
async fn c7_platform_timeout_is_an_explicit_failure() {
    let fake = support::start().await;
    let provider = provider(&fake);
    set(&fake, LambdaBehavior::Timeout { chunks: 2 });
    let response = provider.invoke_agent(&deployment(), &request()).await.unwrap();
    let (body, error) = collect(response).await;
    assert_eq!(
        String::from_utf8_lossy(&body).matches("event: token").count(),
        2,
        "events before the cut are delivered"
    );
    let error = error.expect("the run must not end silently");
    assert!(matches!(error, InvokeError::PlatformTimeout { .. }), "{error:?}");
    assert_eq!(error.error_type(), "platform_timeout");
}

#[tokio::test]
async fn c7_gateway_cap_ends_a_stalled_run_with_platform_timeout() {
    let fake = support::start().await;
    let provider = provider_with(&fake, 1, Duration::from_secs(20));
    set(&fake, LambdaBehavior::Stall { stall: Duration::from_secs(5) });
    let started = Instant::now();
    let response = provider.invoke_agent(&deployment(), &request()).await.unwrap();
    let (_, error) = collect(response).await;
    assert!(matches!(error, Some(InvokeError::PlatformTimeout { .. })), "{error:?}");
    assert!(started.elapsed() < Duration::from_millis(2500), "{:?}", started.elapsed());
    assert!(
        LambdaProvider::new(fake.chain(), LambdaConfig { max_run_secs: 841, ..LambdaConfig::new("eu-west-1") })
            .is_err()
    );
}

#[tokio::test]
async fn function_errors_are_surfaced() {
    let fake = support::start().await;
    let provider = provider(&fake);
    set(&fake, LambdaBehavior::FunctionError);
    let response = provider.invoke_agent(&deployment(), &request()).await.unwrap();
    let (_, error) = collect(response).await;
    assert!(matches!(error, Some(InvokeError::Function { ref code, .. }) if code == "Runtime.ExitError"), "{error:?}");
}

#[tokio::test]
async fn c8_c9_invoke_refreshes_credentials_and_retries_throttling_only() {
    let fake = support::start().await;
    let provider = provider(&fake);
    fake.state.lock().unwrap().expired_token_next = 1;
    fake.state.lock().unwrap().throttle_next = 1;
    let response = provider.invoke_agent(&deployment(), &request()).await.unwrap();
    assert!(collect(response).await.1.is_none());
    assert_eq!(fake.state.lock().unwrap().lambda_requests.len(), 1);

    // A 5xx may mean the function ran: the invoke is not retried.
    fake.state.lock().unwrap().server_error_next = 1;
    let error = provider.invoke_agent(&deployment(), &request()).await.unwrap_err();
    assert!(matches!(&error, InvokeError::Provider(e) if e.kind == ProviderErrorKind::Transient), "{error:?}");
    assert_eq!(fake.state.lock().unwrap().server_error_next, 0);
    assert_eq!(fake.state.lock().unwrap().lambda_requests.len(), 1);
}

#[tokio::test]
async fn provider_invoke_carries_gateway_requests_over_lambda() {
    use graphrt_provider_aws::api::{Endpoint, EndpointAddress, InvokeRequest, ReplicaRef};

    let fake = support::start().await;
    let provider = provider(&fake);
    let endpoints = provider.wake(&deployment(), 1).await.unwrap();
    assert!(!provider.capabilities().can_preempt());
    let request = InvokeRequest {
        method: "POST".into(),
        path: "/internal/v1/runs".into(),
        headers: vec![("graphrt-run-id".into(), "run-42".into()), ("graphrt-sig".into(), "v1;kid=k".into())],
        body: b"{}".to_vec(),
    };
    let response = provider.invoke(&endpoints[0], request.clone()).expect("lambda:// is invoked").await.unwrap();
    assert_eq!(response.status, 200);
    let body: Vec<u8> = response.body.map(|chunk| chunk.unwrap()).concat().await;
    assert_eq!(String::from_utf8_lossy(&body).matches("event: token").count(), 3);
    {
        let state = fake.state.lock().unwrap();
        assert_eq!(state.lambda_requests[0]["requestContext"]["requestId"], "run-42");
        assert_eq!(state.lambda_requests[0]["headers"]["graphrt-sig"], "v1;kid=k");
    }

    // A platform timeout surfaces as a Fatal error tagged platform_timeout.
    set(&fake, LambdaBehavior::Timeout { chunks: 1 });
    let response = provider.invoke(&endpoints[0], request.clone()).unwrap().await.unwrap();
    let items: Vec<_> = response.body.collect().await;
    let error = items.into_iter().find_map(Result::err).expect("timeout error");
    assert_eq!(error.kind, ProviderErrorKind::Fatal);
    assert!(error.message.starts_with(graphrt_provider_aws::lambda::PLATFORM_TIMEOUT_PREFIX), "{error}");

    // HTTP endpoints are not Lambda's: the gateway uses plain HTTP for them.
    let http = Endpoint {
        replica: ReplicaRef { deployment: deployment(), id: "x".into() },
        address: EndpointAddress::Http("http://10.0.0.1:8080".into()),
    };
    assert!(provider.invoke(&http, request).is_none());
}
