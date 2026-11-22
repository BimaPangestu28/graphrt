//! `DeployDriver`s for ECS Fargate and Lambda against the fake control planes,
//! end to end with the run-time providers (deploy → wake / invoke).
#![cfg(all(feature = "deploy-ecs", feature = "deploy-lambda"))]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

mod support;

use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use futures::StreamExt;
use graphrt_provider_aws::api::{DeployDriver, DeployErrorKind, DeploymentSpec, Provider, Resources, SecretRef};
use graphrt_provider_aws::deploy::ecs::{EcsDeployConfig, EcsDeployDriver};
use graphrt_provider_aws::deploy::lambda::{LambdaDeployConfig, LambdaDeployDriver};
use graphrt_provider_aws::ecs::{EcsConfig, EcsProvider, EcsTaskTemplate};
use graphrt_provider_aws::lambda::{AgentRequest, LambdaConfig, LambdaProvider};
use serde_json::{Value, json};
use support::{FakeAws, fast_retry};

const OPENAI_ARN: &str = "arn:aws:secretsmanager:eu-west-1:123456789012:secret:llm-AbCdEf";
const ANTHROPIC_ARN: &str = "arn:aws:ssm:eu-west-1:123456789012:parameter/graphrt/anthropic";
const ECR_IMAGE: &str = "123456789012.dkr.ecr.eu-west-1.amazonaws.com/graphrt:0.5.0";

fn spec(version: &str, settings: Value) -> DeploymentSpec {
    DeploymentSpec {
        name: "chat".into(),
        version: version.into(),
        image: Some(ECR_IMAGE.into()),
        command: vec!["graphrt".into(), "serve".into(), "--trust-gateway".into(), "flow.yaml".into()],
        env: BTreeMap::from([("RUST_LOG".to_owned(), "info".to_owned())]),
        secrets: Vec::new(),
        max_active_runs: 4,
        min_replicas: 0,
        max_replicas: 5,
        grace_secs: 0,
        resources: Resources { cpu_millis: 1000, memory_mib: 2048 },
        workflow_fingerprint: "sha256:abc".into(),
        provider_settings: serde_json::from_value(settings).unwrap(),
    }
}

fn secrets() -> Vec<SecretRef> {
    vec![
        SecretRef { env: "OPENAI_API_KEY".into(), reference: format!("{OPENAI_ARN}:openai::") },
        SecretRef { env: "ANTHROPIC_API_KEY".into(), reference: ANTHROPIC_ARN.into() },
    ]
}

fn ecs_driver(fake: &FakeAws) -> EcsDeployDriver {
    EcsDeployDriver::new(fake.chain(), EcsDeployConfig { endpoint: fake.endpoint("ecs"), limits: fast_retry() })
        .unwrap()
}

fn lambda_driver(fake: &FakeAws) -> LambdaDeployDriver {
    let config = LambdaDeployConfig {
        lambda: fake.endpoint("lambda"),
        secrets_manager: fake.endpoint("secretsmanager"),
        ssm: fake.endpoint("ssm"),
        limits: fast_retry(),
        ready_timeout: Duration::from_secs(5),
        poll_interval: Duration::from_millis(5),
    };
    LambdaDeployDriver::new(fake.chain(), config).unwrap()
}

fn ecs_settings() -> Value {
    json!({
        "execution_role_arn": "arn:aws:iam::123456789012:role/graphrt-agent-execution-role",
        "task_role_arn": "arn:aws:iam::123456789012:role/graphrt-agent-task-role",
        "log_group": "/graphrt/chat",
        "cpu_architecture": "ARM64",
    })
}

#[tokio::test]
async fn ecs_registers_a_task_definition_and_the_provider_starts_it() {
    let fake = support::start().await;
    let mut spec = spec("v7", ecs_settings());
    spec.secrets = secrets();
    let reference = ecs_driver(&fake).apply(&spec).await.unwrap();
    assert_eq!(reference.target, "arn:aws:ecs:eu-west-1:123456789012:task-definition/graphrt-chat:1");
    assert_eq!((reference.name.as_str(), reference.version.as_str()), ("chat", "v7"));

    let request = fake.state.lock().unwrap().task_definitions[0].clone();
    assert_eq!(request["requiresCompatibilities"], json!(["FARGATE"]));
    assert_eq!(request["networkMode"], "awsvpc");
    assert_eq!((request["cpu"].as_str(), request["memory"].as_str()), (Some("1024"), Some("2048")));
    assert_eq!(request["runtimePlatform"]["cpuArchitecture"], "ARM64");
    let container = &request["containerDefinitions"][0];
    assert_eq!(container["stopTimeout"], 120, "default grace = Fargate maximum");
    assert_eq!(container["image"], ECR_IMAGE);
    assert_eq!(container["command"][2], "--trust-gateway");
    assert_eq!(container["environment"], json!([{"name": "RUST_LOG", "value": "info"}]));
    assert_eq!(
        container["secrets"],
        json!([
            {"name": "OPENAI_API_KEY", "valueFrom": format!("{OPENAI_ARN}:openai::")},
            {"name": "ANTHROPIC_API_KEY", "valueFrom": ANTHROPIC_ARN},
        ])
    );
    assert_eq!(container["logConfiguration"]["options"]["awslogs-group"], "/graphrt/chat");
    assert_eq!(request["taskRoleArn"], "arn:aws:iam::123456789012:role/graphrt-agent-task-role");

    // The run-time provider starts exactly the registered revision.
    let template = EcsTaskTemplate {
        cluster: "graphrt".into(),
        task_definition_family: "unused".into(),
        subnets: vec!["subnet-1".into()],
        security_groups: vec!["sg-1".into()],
        assign_public_ip: false,
        container_port: 8080,
        capacity_provider: None,
        platform_version: None,
    };
    let config = EcsConfig {
        endpoint: fake.endpoint("ecs"),
        limits: fast_retry(),
        deployments: HashMap::from([("chat".to_owned(), template)]),
        wake_timeout: Duration::from_secs(5),
        poll_interval: Duration::from_millis(10),
    };
    let provider = EcsProvider::new(fake.chain(), config).unwrap();
    provider.wake(&reference, 1).await.unwrap();
    assert_eq!(fake.state.lock().unwrap().tasks[0].task_definition, reference.target);
}

#[tokio::test]
async fn ecs_rejects_specs_fargate_cannot_run_before_calling_aws() {
    let fake = support::start().await;
    let driver = ecs_driver(&fake);
    let mut plaintext = spec("v7", ecs_settings());
    plaintext.secrets = vec![SecretRef { env: "OPENAI_API_KEY".into(), reference: "sk-live-plaintext".into() }];
    let error = driver.apply(&plaintext).await.unwrap_err();
    assert_eq!(error.kind, DeployErrorKind::Invalid);
    assert!(!error.message.contains("sk-live-plaintext"));

    let mut long_grace = spec("v7", ecs_settings());
    long_grace.grace_secs = 121;
    assert_eq!(driver.apply(&long_grace).await.unwrap_err().kind, DeployErrorKind::Invalid);

    let no_role = spec("v7", json!({}));
    assert!(driver.apply(&no_role).await.unwrap_err().message.contains("execution_role_arn"));

    let mut grace_30 = spec("v8", ecs_settings());
    grace_30.grace_secs = 30;
    driver.apply(&grace_30).await.unwrap();
    let state = fake.state.lock().unwrap();
    assert_eq!(state.task_definitions.len(), 1, "invalid specs made no API call");
    assert_eq!(state.task_definitions[0]["containerDefinitions"][0]["stopTimeout"], 30);
}

fn lambda_settings() -> Value {
    json!({ "role_arn": "arn:aws:iam::123456789012:role/graphrt-chat-lambda", "architecture": "arm64" })
}

#[tokio::test]
async fn lambda_creates_publishes_aliases_and_streams_through_the_provider() {
    let fake = support::start().await;
    fake.state.lock().unwrap().update_polls = 2;
    let reference = lambda_driver(&fake).apply(&spec("v7", lambda_settings())).await.unwrap();
    assert_eq!(reference.target, "arn:aws:lambda:eu-west-1:123456789012:function:graphrt-chat");
    assert_eq!(reference.version, "v7");
    {
        let state = fake.state.lock().unwrap();
        let function = &state.functions["graphrt-chat"];
        let configuration = &function.configuration;
        assert_eq!(configuration["Timeout"], 870, "max_run_secs 840 + 30");
        assert_eq!(configuration["MemorySize"], 2048);
        assert_eq!(configuration["Architectures"], json!(["arm64"]));
        assert_eq!(configuration["ImageConfig"]["Command"][1], "serve");
        let variables = &configuration["Environment"]["Variables"];
        assert_eq!(variables["AWS_LWA_INVOKE_MODE"], "response_stream");
        assert_eq!(variables["AWS_LWA_PORT"], "8080");
        assert_eq!(variables["AWS_LWA_READINESS_CHECK_PATH"], "/healthz");
        assert_eq!(variables["RUST_LOG"], "info");
        assert_eq!(function.aliases["v7"], "1");
        let url = function.url.clone().unwrap();
        assert_eq!((url["AuthType"].as_str(), url["InvokeMode"].as_str()), (Some("AWS_IAM"), Some("RESPONSE_STREAM")));
        assert_eq!(url["Qualifier"], "v7");
        assert_eq!(state.signature_failures, 0);
    }

    let config =
        LambdaConfig { endpoint: fake.endpoint("lambda"), limits: fast_retry(), ..LambdaConfig::new("eu-west-1") };
    let provider = LambdaProvider::new(fake.chain(), config).unwrap();
    let request =
        AgentRequest { method: "POST".into(), path_and_query: "/internal/v1/runs".into(), ..AgentRequest::default() };
    let response = provider.invoke(&reference, &request).await.unwrap();
    let chunks: Vec<_> = response.body.collect().await;
    assert!(chunks.iter().all(Result::is_ok));
    assert_eq!(
        fake.state.lock().unwrap().lambda_queries[0],
        "arn:aws:lambda:eu-west-1:123456789012:function:graphrt-chat?Qualifier=v7"
    );
    assert!(provider.wake(&reference, 1).await.is_ok());
}

#[tokio::test]
async fn lambda_updates_waits_out_in_progress_and_keeps_versions_immutable() {
    let fake = support::start().await;
    fake.state.lock().unwrap().update_polls = 2;
    let driver = lambda_driver(&fake);
    driver.apply(&spec("v7", lambda_settings())).await.unwrap();

    let mut v8 = spec("v8", lambda_settings());
    v8.image = Some(format!("{ECR_IMAGE}-patch"));
    driver.apply(&v8).await.unwrap();
    // Re-applying the same version is a no-op (same published version, same alias).
    driver.apply(&v8).await.unwrap();
    {
        let state = fake.state.lock().unwrap();
        let function = &state.functions["graphrt-chat"];
        assert_eq!(function.versions.len(), 2);
        assert_eq!(function.aliases["v8"], "2");
        assert!(state.lambda_calls.iter().any(|c| c.ends_with("/graphrt-chat/code")));
    }

    // Reusing a version name for different code is refused: versions are immutable.
    let mut v7_changed = spec("v7", lambda_settings());
    v7_changed.image = Some(format!("{ECR_IMAGE}-other"));
    let error = driver.apply(&v7_changed).await.unwrap_err();
    assert_eq!(error.kind, DeployErrorKind::Invalid, "{error}");
    assert_eq!(fake.state.lock().unwrap().functions["graphrt-chat"].aliases["v7"], "1");
}

#[tokio::test]
async fn lambda_create_race_falls_back_to_update() {
    let fake = support::start().await;
    fake.state.lock().unwrap().create_conflict_next = true;
    let reference = lambda_driver(&fake).apply(&spec("v1a", lambda_settings())).await.unwrap();
    assert_eq!(reference.version, "v1a");
    let state = fake.state.lock().unwrap();
    assert_eq!(state.functions["graphrt-chat"].image, ECR_IMAGE, "code set by the update path");
}

#[tokio::test]
async fn lambda_secrets_are_refused_by_default_and_resolved_only_on_opt_in() {
    let fake = support::start().await;
    let driver = lambda_driver(&fake);
    let mut with_secrets = spec("v7", lambda_settings());
    with_secrets.secrets = secrets();
    let error = driver.apply(&with_secrets).await.unwrap_err();
    assert_eq!(error.kind, DeployErrorKind::Unsupported);
    assert!(fake.state.lock().unwrap().lambda_calls.is_empty());

    {
        let mut state = fake.state.lock().unwrap();
        state.secret_values.insert(OPENAI_ARN.into(), r#"{"openai":"sk-openai-test-value"}"#.into());
        state.secret_values.insert(ANTHROPIC_ARN.into(), "sk-ant-test-value".into());
    }
    let mut settings = lambda_settings();
    settings["secrets_mode"] = json!("resolve-to-env");
    settings["kms_key_arn"] = json!("arn:aws:kms:eu-west-1:123456789012:key/abc");
    with_secrets.provider_settings = serde_json::from_value(settings).unwrap();
    driver.apply(&with_secrets).await.unwrap();
    let state = fake.state.lock().unwrap();
    let configuration = &state.functions["graphrt-chat"].configuration;
    assert_eq!(configuration["Environment"]["Variables"]["OPENAI_API_KEY"], "sk-openai-test-value");
    assert_eq!(configuration["Environment"]["Variables"]["ANTHROPIC_API_KEY"], "sk-ant-test-value");
    assert_eq!(configuration["KMSKeyArn"], "arn:aws:kms:eu-west-1:123456789012:key/abc");
    assert!(!format!("{driver:?}").contains("sk-openai-test-value"));
}

#[tokio::test]
async fn lambda_rejects_what_it_cannot_run_without_calling_aws() {
    let fake = support::start().await;
    let driver = lambda_driver(&fake);
    let mut cron = lambda_settings();
    cron["triggers"] = json!(["sync", "cron"]);
    let cases = [
        spec("v7", cron),
        spec("7", lambda_settings()),
        DeploymentSpec { image: Some("ghcr.io/x/graphrt:0.5".into()), ..spec("v7", lambda_settings()) },
        spec("v7", json!({ "role_arn": "r", "max_run_secs": 900 })),
        DeploymentSpec {
            env: BTreeMap::from([("AWS_LWA_INVOKE_MODE".to_owned(), "buffered".to_owned())]),
            ..spec("v7", lambda_settings())
        },
        DeploymentSpec {
            env: BTreeMap::from([("AWS_REGION".to_owned(), "x".to_owned())]),
            ..spec("v7", lambda_settings())
        },
    ];
    for case in &cases {
        let error = driver.apply(case).await.unwrap_err();
        assert_eq!(error.kind, DeployErrorKind::Invalid, "{error}");
    }
    assert!(fake.state.lock().unwrap().lambda_calls.is_empty());
}
