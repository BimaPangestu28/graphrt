//! The `graphrt deploy --target cloud-run --direct --json` contract the desktop
//! app relies on, end to end: the real binary against the fake Google APIs.
//! Every stdout line must be one JSON object valid against
//! `docs/schemas/deploy-direct-cloud-run.schema.json`, steps come before
//! exactly one final line, exit codes match, and secret values never reach
//! stdout or stderr.
#![cfg(feature = "deploy-cloud-run")]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]

use std::path::PathBuf;
use std::process::Output;

use graphrt_gcp::direct::fake::{FakeGoogle, PROJECT, TOKEN};
use serde_json::Value;

const OPENAI_VALUE: &str = "sk-cloud-run-contract-secret-0123456789";

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn validator() -> jsonschema::Validator {
    let text = std::fs::read_to_string(repo_root().join("docs/schemas/deploy-direct-cloud-run.schema.json")).unwrap();
    jsonschema::validator_for(&serde_json::from_str(&text).unwrap()).expect("schema compiles")
}

/// Runs `graphrt deploy <args>` against `fake` with an access token handed over
/// the way the desktop app does, and nothing else of the machine's Google setup.
async fn graphrt(fake: &FakeGoogle, args: &[&str], token: Option<&str>) -> Output {
    let base_url = fake.url();
    let args: Vec<String> = args.iter().map(|arg| (*arg).to_owned()).collect();
    let token = token.map(str::to_owned);
    let empty_config = std::env::temp_dir().join("graphrt-cloud-run-contract-no-gcloud");
    tokio::task::spawn_blocking(move || {
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_graphrt"));
        command
            .arg("deploy")
            .args(&args)
            .env("GRAPHRT_GCP_API_URL", base_url)
            .env("OPENAI_API_KEY", OPENAI_VALUE)
            .env("CLOUDSDK_CONFIG", empty_config)
            .env_remove("GOOGLE_APPLICATION_CREDENTIALS")
            .env_remove("GOOGLE_CLOUD_PROJECT")
            .env_remove("GOOGLE_OAUTH_ACCESS_TOKEN");
        if let Some(token) = token {
            command.env("GOOGLE_OAUTH_ACCESS_TOKEN", token);
        }
        command.output().expect("run graphrt")
    })
    .await
    .unwrap()
}

fn json_lines(output: &Output) -> Vec<Value> {
    let stdout = String::from_utf8(output.stdout.clone()).unwrap();
    let validator = validator();
    let lines: Vec<Value> = stdout
        .lines()
        .map(|line| serde_json::from_str(line).unwrap_or_else(|error| panic!("not JSON ({error}): {line}")))
        .collect();
    for line in &lines {
        let errors: Vec<String> = validator.iter_errors(line).map(|error| error.to_string()).collect();
        assert!(errors.is_empty(), "{line} does not match the schema: {errors:?}");
    }
    let (last, steps) = lines.split_last().expect("at least the final line");
    let (api_key_lines, steps): (Vec<&Value>, Vec<&Value>) = steps.iter().partition(|line| line["type"] == "api_key");
    assert!(steps.iter().all(|line| line["type"] == "step"), "only steps before the final line: {stdout}");
    if !api_key_lines.is_empty() {
        assert_eq!(api_key_lines.len(), 1, "at most one api_key line: {stdout}");
        assert_eq!(last["type"], "error", "an api_key line only precedes an error: {stdout}");
    }
    assert!(last["type"] == "result" || last["type"] == "error", "{last}");
    lines
}

fn final_line(output: &Output) -> Value {
    json_lines(output).pop().unwrap()
}

fn assert_no_secret(output: &Output) {
    for stream in [&output.stdout, &output.stderr] {
        let text = String::from_utf8_lossy(stream);
        assert!(!text.contains(OPENAI_VALUE), "a secret value leaked: {text}");
    }
}

fn chatbot() -> String {
    repo_root().join("examples/chatbot.yaml").display().to_string()
}

fn deploy_args<'a>(version: &'a str, workflow: &'a str, extra: &[&'a str]) -> Vec<&'a str> {
    let mut args = vec![
        "--target",
        "cloud-run",
        "--direct",
        "--project",
        PROJECT,
        "--region",
        "us-central1",
        "--deployment",
        "Support Bot",
        "--version",
        version,
        "--secret",
        "OPENAI_API_KEY",
        "--label",
        "graphrt-test=1",
        "--json",
    ];
    args.extend_from_slice(extra);
    args.push(workflow);
    args
}

fn manage_args<'a>(action: &'a str, extra: &[&'a str]) -> Vec<&'a str> {
    let mut args = vec![action];
    args.extend_from_slice(extra);
    args.extend(["--target", "cloud-run", "--direct", "--project", PROJECT, "--deployment", "Support Bot", "--json"]);
    args
}

#[tokio::test]
async fn a_deployment_lives_through_redeploy_rollback_pause_and_removal() {
    let fake = FakeGoogle::start().await;
    let workflow = chatbot();

    let first = graphrt(&fake, &deploy_args("v1", &workflow, &["--chat", "--conversations"]), Some(TOKEN)).await;
    assert_eq!(first.status.code(), Some(0), "stderr: {}", String::from_utf8_lossy(&first.stderr));
    assert_no_secret(&first);
    let lines = json_lines(&first);
    let steps: Vec<String> = lines
        .iter()
        .filter(|line| line["type"] == "step")
        .map(|line| format!("{}:{}", line["step"].as_str().unwrap(), line["status"].as_str().unwrap()))
        .collect();
    assert_eq!(
        steps,
        [
            "preparing:running",
            "preparing:done",
            "secrets:running",
            "secrets:done",
            "starting:running",
            "starting:done",
            "checking:running",
            "checking:done"
        ]
    );
    let result = lines.last().unwrap();
    assert_eq!(result["app"], "support-bot");
    assert_eq!(result["workflow"], "chatbot");
    assert_eq!(result["serve"]["chat"], "key");
    assert_eq!(result["store"]["bucket"], format!("{PROJECT}-support-bot"));
    assert!(
        result["image"]
            .as_str()
            .unwrap()
            .starts_with("us-central1-docker.pkg.dev/fake-project/graphrt-ghcr/graphrt/graphrt:")
    );
    let api_key = result["api_key"].as_str().expect("the first deploy shows the key").to_owned();
    assert!(api_key.starts_with("grk_") && api_key.len() == 52);
    {
        let state = fake.state();
        assert!(state.service_accounts.contains("graphrt-support-bot@fake-project.iam.gserviceaccount.com"));
        assert!(state.repositories.contains("graphrt-ghcr"));
        assert_eq!(state.buckets["fake-project-support-bot"]["labels"]["graphrt-test"], "1");
        let service = &state.services["support-bot"];
        assert_eq!(service["invokerIamDisabled"], true);
        let args = service["template"]["containers"][0]["args"].to_string();
        assert!(args.contains("--conversations") && args.contains("--store") && !args.contains(OPENAI_VALUE), "{args}");
        let policy = state.secret_policies["support-bot-openai-api-key"].to_string();
        assert!(policy.contains("secretmanager.secretAccessor") && policy.contains("graphrt-support-bot@"), "{policy}");
    }
    // The access token is a person's: calls bill the deployment's project.
    let calls = fake.requests_to("GET", "/v2/");
    assert!(calls.iter().all(|call| call.header("x-goog-user-project") == Some(PROJECT)));

    let second = graphrt(&fake, &deploy_args("v2", &workflow, &["--chat", "--conversations"]), Some(TOKEN)).await;
    assert_eq!(second.status.code(), Some(0), "stderr: {}", String::from_utf8_lossy(&second.stderr));
    let result = final_line(&second);
    assert_eq!(result["api_key"], Value::Null, "the key stays and is not shown again");
    assert_eq!(fake.state().secret_versions["support-bot-graphrt-api-key"].len(), 1);

    let versions = final_line(&graphrt(&fake, &manage_args("list-versions", &[]), Some(TOKEN)).await);
    let labels: Vec<&str> =
        versions["versions"].as_array().unwrap().iter().map(|v| v["label"].as_str().unwrap()).collect();
    assert_eq!(labels, ["v2", "v1"]);
    assert_eq!(versions["current_version"], "v2");

    let rollback = graphrt(&fake, &manage_args("rollback", &["v1"]), Some(TOKEN)).await;
    assert_eq!(rollback.status.code(), Some(0), "stderr: {}", String::from_utf8_lossy(&rollback.stderr));
    assert_eq!(final_line(&rollback)["version"], "v1");
    let status = final_line(&graphrt(&fake, &manage_args("status", &[]), Some(TOKEN)).await);
    assert_eq!((status["status"].as_str(), status["current_version"].as_str()), (Some("online"), Some("v1")));
    assert_eq!(status["store"]["in_use"], true);
    assert_eq!(status["mode"], "scale-to-zero");

    let pause = final_line(&graphrt(&fake, &manage_args("pause", &[]), Some(TOKEN)).await);
    assert_eq!(pause["changed"], true);
    assert_eq!(fake.state().services["support-bot"]["ingress"], "INGRESS_TRAFFIC_INTERNAL_ONLY");
    let status = final_line(&graphrt(&fake, &manage_args("status", &[]), Some(TOKEN)).await);
    assert_eq!(status["status"], "paused");
    let again = final_line(&graphrt(&fake, &manage_args("pause", &[]), Some(TOKEN)).await);
    assert_eq!(again["changed"], false);
    let resume = final_line(&graphrt(&fake, &manage_args("resume", &[]), Some(TOKEN)).await);
    assert_eq!((resume["changed"].as_bool(), resume["version"].as_str()), (Some(true), Some("v1")));

    let remove = graphrt(&fake, &manage_args("remove", &[]), Some(TOKEN)).await;
    assert_eq!(remove.status.code(), Some(0), "stderr: {}", String::from_utf8_lossy(&remove.stderr));
    let removed = final_line(&remove);
    assert_eq!(removed["removed"], true);
    assert_eq!(removed["bucket_deleted"], true);
    assert_eq!(removed["service_account_deleted"], true);
    assert_eq!(removed["secrets_deleted"], 2);
    {
        let state = fake.state();
        assert!(state.services.is_empty() && state.buckets.is_empty() && state.secrets.is_empty());
        assert!(state.service_accounts.is_empty());
    }
    let gone = graphrt(&fake, &manage_args("status", &[]), Some(TOKEN)).await;
    assert_eq!(gone.status.code(), Some(1));
    assert_eq!(final_line(&gone)["code"], "not_found");
}

#[tokio::test]
async fn a_revision_that_never_gets_ready_leaves_the_previous_version_serving() {
    let fake = FakeGoogle::start().await;
    let workflow = chatbot();
    let first = graphrt(&fake, &deploy_args("v1", &workflow, &[]), Some(TOKEN)).await;
    assert_eq!(first.status.code(), Some(0), "stderr: {}", String::from_utf8_lossy(&first.stderr));

    fake.state().fail_next_revision = true;
    let failed = graphrt(&fake, &deploy_args("v2", &workflow, &["--rotate-api-key"]), Some(TOKEN)).await;
    assert_eq!(failed.status.code(), Some(1));
    assert_no_secret(&failed);
    let lines = json_lines(&failed);
    let error = lines.last().unwrap();
    assert_eq!(error["code"], "health_check_failed");
    assert!(error["message"].as_str().unwrap().contains("previous version"), "{error}");
    let key_line = &lines[lines.len() - 2];
    assert_eq!(key_line["type"], "api_key", "the rotated key is stored already: shown once");
    assert!(lines.iter().any(|line| line["step"] == "starting" && line["status"] == "failed"));

    let status = final_line(&graphrt(&fake, &manage_args("status", &[]), Some(TOKEN)).await);
    assert_eq!(status["current_version"], "v1");
    let traffic = status["traffic"].as_array().unwrap();
    assert!(traffic.iter().all(|target| target["version"] != "v2"), "the failed version got no tag: {traffic:?}");
}

#[tokio::test]
async fn refusals_say_what_to_fix() {
    let fake = FakeGoogle::start().await;
    let workflow = chatbot();

    let no_project: Vec<&str> =
        deploy_args("v1", &workflow, &[]).into_iter().filter(|arg| *arg != "--project" && *arg != PROJECT).collect();
    let output = graphrt(&fake, &no_project, Some(TOKEN)).await;
    assert_eq!(output.status.code(), Some(2));
    let error = final_line(&output);
    assert_eq!(error["code"], "invalid_input");
    assert!(error["message"].as_str().unwrap().contains("--project"));

    let output = graphrt(&fake, &deploy_args("v1", &workflow, &[]), None).await;
    assert_eq!(output.status.code(), Some(1));
    let error = final_line(&output);
    assert_eq!(error["code"], "missing_credentials");
    assert!(error["message"].as_str().unwrap().contains("gcloud auth application-default login"));

    let output = graphrt(&fake, &deploy_args("v1", &workflow, &["--org", "personal"]), Some(TOKEN)).await;
    assert_eq!(final_line(&output)["code"], "invalid_input");

    let output = graphrt(&fake, &deploy_args("v1", &workflow, &[]), Some("ya29.expired")).await;
    assert_eq!(final_line(&output)["code"], "unauthorized");

    fake.state().stuck_disabled_api = Some("secretmanager.googleapis.com".to_owned());
    let output = graphrt(&fake, &deploy_args("v1", &workflow, &[]), Some(TOKEN)).await;
    let error = final_line(&output);
    assert_eq!(error["code"], "api_disabled");
    assert!(
        error["message"]
            .as_str()
            .unwrap()
            .contains("gcloud services enable secretmanager.googleapis.com --project fake-project"),
        "{error}"
    );
    fake.state().stuck_disabled_api = None;

    fake.state().billing_disabled = true;
    let output = graphrt(&fake, &deploy_args("v1", &workflow, &[]), Some(TOKEN)).await;
    let error = final_line(&output);
    assert_eq!(error["code"], "billing_disabled");
    assert!(error["message"].as_str().unwrap().contains("billing/linkedaccount?project=fake-project"));
}

#[tokio::test]
async fn apis_that_are_off_are_turned_on() {
    let fake = FakeGoogle::start().await;
    fake.state().enabled_apis.retain(|api| api == "storage.googleapis.com");
    let output = graphrt(&fake, &deploy_args("v1", &chatbot(), &[]), Some(TOKEN)).await;
    assert_eq!(output.status.code(), Some(0), "stderr: {}", String::from_utf8_lossy(&output.stderr));
    let enable = fake.requests_to("POST", "/v1/projects/fake-project/services:batchEnable");
    assert_eq!(enable.len(), 1);
    assert!(enable[0].body_text().contains("run.googleapis.com"));
}
