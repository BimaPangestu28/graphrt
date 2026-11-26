//! The `graphrt deploy --target fly --direct --json` contract the desktop app
//! relies on, end to end: the real binary against the fake Fly Machines API.
//! Every stdout line must be one JSON object valid against
//! `docs/schemas/deploy-direct.schema.json`, steps come before exactly one
//! final line, exit codes match, and secret values never reach stdout/stderr.
#![cfg(feature = "deploy-fly")]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]

use std::path::PathBuf;
use std::process::Output;

use graphrt_provider_fly::fake::{FakeFly, FakeResponse, Fault, TOKEN};
use serde_json::Value;

const OPENAI_VALUE: &str = "sk-cli-contract-secret-0123456789";

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn validator() -> jsonschema::Validator {
    let text = std::fs::read_to_string(repo_root().join("docs/schemas/deploy-direct.schema.json")).unwrap();
    jsonschema::validator_for(&serde_json::from_str(&text).unwrap()).expect("schema compiles")
}

/// Runs `graphrt deploy <args>` against `fake` without blocking the fake's runtime.
async fn graphrt(fake: &FakeFly, args: &[&str]) -> Output {
    let base_url = fake.server.base_url();
    let args: Vec<String> = args.iter().map(|arg| (*arg).to_owned()).collect();
    tokio::task::spawn_blocking(move || {
        std::process::Command::new(env!("CARGO_BIN_EXE_graphrt"))
            .arg("deploy")
            .args(&args)
            .env("FLY_API_TOKEN", TOKEN)
            .env("GRAPHRT_FLY_API_URL", base_url)
            .env("OPENAI_API_KEY", OPENAI_VALUE)
            .output()
            .expect("run graphrt")
    })
    .await
    .unwrap()
}

/// Parses stdout as JSON Lines, checks every line against the schema and the
/// order (steps, then one final line), and returns the lines.
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
        assert_eq!(lines[lines.len() - 2]["type"], "api_key", "right before the error: {stdout}");
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

fn deploy_args<'a>(version: &'a str, workflow: &'a str) -> Vec<&'a str> {
    vec![
        "--target",
        "fly",
        "--direct",
        "--deployment",
        "Support Bot",
        "--version",
        version,
        "--region",
        "ams",
        "--mode",
        "always-on",
        "--secret",
        "OPENAI_API_KEY",
        "--org",
        "personal",
        "--json",
        workflow,
    ]
}

fn hello() -> String {
    repo_root().join("examples/hello.yaml").display().to_string()
}

#[tokio::test]
async fn deploy_prints_schema_valid_json_lines_and_the_key_once() {
    let fake = FakeFly::start_empty().await;
    let workflow = hello();

    let output = graphrt(&fake, &deploy_args("v1", &workflow)).await;

    assert_eq!(output.status.code(), Some(0), "stderr: {}", String::from_utf8_lossy(&output.stderr));
    let lines = json_lines(&output);
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
    assert_eq!(result["url"], "https://support-bot.fly.dev");
    assert_eq!(result["app"], "support-bot");
    assert_eq!(result["version"], "v1");
    assert_eq!(result["workflow"], "hello");
    let api_key = result["api_key"].as_str().expect("first deploy shows the key").to_owned();
    assert!(api_key.starts_with("grk_") && api_key.len() == 52);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(stdout.matches(&api_key).count(), 1, "the key appears once, in the result");
    assert!(!String::from_utf8_lossy(&output.stderr).contains(&api_key));
    assert_no_secret(&output);
    let state = fake.state();
    assert_eq!(state.secrets.get("GRAPHRT_API_KEY"), Some(&api_key));
    assert_eq!(state.secrets.get("OPENAI_API_KEY").map(String::as_str), Some(OPENAI_VALUE));
    assert_eq!(state.machines[0].config["image"], format!("ghcr.io/graphrt/graphrt:{}", env!("CARGO_PKG_VERSION")));
}

#[tokio::test]
async fn redeploy_does_not_show_the_key_again() {
    let fake = FakeFly::start_empty().await;
    let workflow = hello();
    graphrt(&fake, &deploy_args("v1", &workflow)).await;

    let output = graphrt(&fake, &deploy_args("v2", &workflow)).await;

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(final_line(&output)["api_key"], Value::Null);
}

#[tokio::test]
async fn management_subcommands_print_schema_valid_results() {
    let fake = FakeFly::start_empty().await;
    let workflow = hello();
    graphrt(&fake, &deploy_args("v1", &workflow)).await;
    graphrt(&fake, &deploy_args("v2", &workflow)).await;
    let target = ["--target", "fly", "--direct", "--deployment", "Support Bot", "--json"];
    let with = |action: &[&'static str]| -> Vec<&'static str> { [action, &target[..]].concat() };

    let status = final_line(&graphrt(&fake, &with(&["status"])).await);
    assert_eq!((status["status"].as_str(), status["current_version"].as_str()), (Some("online"), Some("v2")));
    assert!(status["last_deployed_at"].as_str().unwrap().ends_with('Z'));
    let machine = &status["machines"][0];
    assert!(machine["id"].is_string() && machine["state"].is_string() && machine["region"].is_string());
    assert_eq!(status["mode"], "always-on");
    let versions = final_line(&graphrt(&fake, &with(&["list-versions"])).await);
    assert_eq!(versions["current_version"], "v2");
    assert_eq!(versions["versions"][0]["label"], "v2");
    assert!(versions["versions"][0]["deployed_at"].is_string());
    assert_eq!(versions["versions"][1]["current"], false);
    let rollback = final_line(&graphrt(&fake, &with(&["rollback", "v1"])).await);
    assert_eq!(rollback["version"], "v1");
    let paused = final_line(&graphrt(&fake, &with(&["pause"])).await);
    assert_eq!((paused["status"].as_str(), paused["changed"].as_bool()), (Some("paused"), Some(true)));
    let status = final_line(&graphrt(&fake, &with(&["status"])).await);
    assert_eq!(status["status"], "paused");
    let resumed = final_line(&graphrt(&fake, &with(&["resume"])).await);
    assert_eq!((resumed["version"].as_str(), resumed["changed"].as_bool()), (Some("v1"), Some(true)));
    let removed = final_line(&graphrt(&fake, &with(&["remove"])).await);
    assert_eq!(removed["removed"], true);

    let missing = graphrt(&fake, &with(&["status"])).await;
    assert_eq!(missing.status.code(), Some(1));
    assert_eq!(final_line(&missing)["code"], "not_found");
}

#[tokio::test]
async fn fly_errors_echoing_the_request_never_leak_secret_values() {
    let fake = FakeFly::start_empty().await;
    fake.state().echo_secrets_on_error = true;
    let workflow = hello();

    let output = graphrt(&fake, &deploy_args("v1", &workflow)).await;

    assert_eq!(output.status.code(), Some(1));
    let error = final_line(&output);
    assert_eq!(error["code"], "fly_api");
    assert!(error["message"].as_str().unwrap().contains("[redacted]"), "{error}");
    assert!(error.get("api_key").is_none(), "no key in error lines");
    assert!(!String::from_utf8_lossy(&output.stdout).contains(r#""type":"api_key""#), "no key was stored");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.contains("grk_"), "the unused key candidate is hidden too: {stdout}");
    assert_no_secret(&output);
}

#[tokio::test]
async fn invalid_input_exits_2_with_an_error_line() {
    let fake = FakeFly::start_empty().await;
    let workflow = hello();
    let mut unset_secret = deploy_args("v1", &workflow);
    unset_secret[12] = "GRAPHRT_TEST_UNSET_SECRET_VARIABLE";
    let output = graphrt(&fake, &unset_secret).await;
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(final_line(&output)["code"], "invalid_input");

    let broken = std::env::temp_dir().join(format!("graphrt-direct-broken-{}.yaml", std::process::id()));
    std::fs::write(&broken, "this: is: not: a workflow").unwrap();
    let output = graphrt(&fake, &deploy_args("v1", broken.to_str().unwrap())).await;
    let _cleanup = std::fs::remove_file(&broken);
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(final_line(&output)["code"], "invalid_input");
    assert!(fake.server.requests().is_empty(), "nothing reaches Fly before the input is valid");
}

#[tokio::test]
async fn taken_app_name_is_reported_with_its_code() {
    let fake = FakeFly::start_empty().await;
    fake.state().taken_app_names.push("support-bot".to_owned());
    let workflow = hello();

    let output = graphrt(&fake, &deploy_args("v1", &workflow)).await;

    assert_eq!(output.status.code(), Some(1));
    let lines = json_lines(&output);
    assert_eq!(lines[lines.len() - 2]["status"], "failed");
    assert_eq!(lines.last().unwrap()["code"], "app_name_taken");
}

#[tokio::test]
async fn without_json_the_output_is_for_humans() {
    let fake = FakeFly::start_empty().await;
    let workflow = hello();
    let args: Vec<&str> = deploy_args("v1", &workflow).into_iter().filter(|arg| *arg != "--json").collect();

    let output = graphrt(&fake, &args).await;

    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("deployed v1 to https://support-bot.fly.dev"), "{stdout}");
    assert!(stdout.contains("API key (shown once"), "{stdout}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("[ok] checking"));
    assert_no_secret(&output);
}

#[tokio::test]
async fn failure_after_storing_a_new_key_hands_it_over_in_an_api_key_line_not_the_error() {
    let fake = FakeFly::start_empty().await;
    fake.server.inject(Fault {
        path_prefix: "/v1/apps/support-bot/machines".to_owned(),
        response: FakeResponse::json(400, &serde_json::json!({"error": "invalid machine config"})),
        count: 1,
    });
    let workflow = hello();

    let output = graphrt(&fake, &deploy_args("v1", &workflow)).await;

    assert_eq!(output.status.code(), Some(1));
    let lines = json_lines(&output);
    let error = lines.last().unwrap();
    assert_eq!(error["code"], "fly_api");
    assert!(error.get("api_key").is_none());
    let key_line = &lines[lines.len() - 2];
    assert_eq!(key_line["type"], "api_key");
    let api_key = key_line["api_key"].as_str().unwrap();
    assert_eq!(fake.state().secrets.get("GRAPHRT_API_KEY").map(String::as_str), Some(api_key), "the stored key");
    assert_eq!(String::from_utf8_lossy(&output.stdout).matches(api_key).count(), 1, "printed once");
    assert_no_secret(&output);
}
