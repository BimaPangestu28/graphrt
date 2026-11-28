//! `graphrt deploy --target lambda --direct --json`: the refusals that happen
//! before any AWS call, and the shape of every line against
//! `docs/schemas/deploy-direct-aws.schema.json`. The deploy itself was run
//! against real AWS (docs/deploy-direct-aws.md, "Tested on real AWS").
#![cfg(feature = "deploy-aws")]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]

use std::path::PathBuf;
use std::process::Output;

use serde_json::Value;

const OPENAI_VALUE: &str = "sk-lambda-contract-secret-0123456789";

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn workflow() -> String {
    repo_root().join("examples/chatbot.yaml").display().to_string()
}

/// Runs `graphrt deploy <args>` with fake keys and every AWS endpoint pointing
/// at a closed port, so a call that should not happen fails fast.
fn graphrt(args: &[&str], profile_dir: &std::path::Path) -> Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_graphrt"))
        .arg("deploy")
        .args(args)
        .env_remove("AWS_PROFILE")
        .env_remove("AWS_REGION")
        .env_remove("AWS_DEFAULT_REGION")
        .env("AWS_ACCESS_KEY_ID", "AKIAFAKEFAKEFAKE")
        .env("AWS_SECRET_ACCESS_KEY", "fake-secret-key-value")
        .env("AWS_SHARED_CREDENTIALS_FILE", profile_dir.join("credentials"))
        .env("AWS_CONFIG_FILE", profile_dir.join("config"))
        .env("GRAPHRT_AWS_ENDPOINT_URL", "http://127.0.0.1:9")
        .env("OPENAI_API_KEY", OPENAI_VALUE)
        .output()
        .expect("run graphrt")
}

fn last_line(output: &Output) -> Value {
    let text = std::fs::read_to_string(repo_root().join("docs/schemas/deploy-direct-aws.schema.json")).unwrap();
    let validator = jsonschema::validator_for(&serde_json::from_str(&text).unwrap()).expect("schema compiles");
    let stdout = String::from_utf8(output.stdout.clone()).unwrap();
    let lines: Vec<Value> = stdout.lines().map(|line| serde_json::from_str(line).unwrap()).collect();
    for line in &lines {
        let errors: Vec<String> = validator.iter_errors(line).map(|error| error.to_string()).collect();
        assert!(errors.is_empty(), "{line} does not match the schema: {errors:?}");
    }
    for stream in [&output.stdout, &output.stderr] {
        assert!(!String::from_utf8_lossy(stream).contains(OPENAI_VALUE), "a secret value leaked");
    }
    lines.last().cloned().unwrap_or_else(|| panic!("no output: {stdout}"))
}

/// A fresh directory for the AWS shared files of one test.
struct Dir(PathBuf);

impl Dir {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!("graphrt-lambda-direct-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn deploy(extra: &[&str], dir: &std::path::Path) -> (Value, Option<i32>) {
    let agent = workflow();
    let mut args =
        vec!["--target", "lambda", "--direct", "--deployment", "support", "--secret", "OPENAI_API_KEY", "--json"];
    // `--version` and `--region` once each: an `extra` value replaces the default.
    for (flag, default) in [("--version", "v1"), ("--region", "ap-southeast-1")] {
        if !extra.contains(&flag) {
            args.extend([flag, default]);
        }
    }
    args.extend_from_slice(extra);
    args.push(&agent);
    let output = graphrt(&args, dir);
    (last_line(&output), output.status.code())
}

#[test]
fn refusals_come_before_any_aws_call_with_a_fix() {
    let dir = Dir::new("refusals");
    std::fs::write(dir.path().join("credentials"), "[graphrt]\naws_access_key_id = AKIAX\naws_secret_access_key = y\n")
        .unwrap();
    std::fs::write(dir.path().join("config"), "[profile sso]\nsso_session = corp\n").unwrap();

    let (line, code) = deploy(&["--mode", "always-on"], dir.path());
    assert_eq!((line["code"].as_str(), code), (Some("invalid_input"), Some(2)), "{line}");
    assert!(line["message"].as_str().unwrap().contains("--target fly --mode always-on"));

    // The store features are accepted (they get an S3 bucket): the deploy goes
    // on to AWS, which is unreachable here.
    let (line, _) = deploy(&["--monitor", "--conversations", "--push", "mailto:ops@example.com"], dir.path());
    assert_eq!(line["code"], "aws_unavailable", "{line}");

    let (line, _) = deploy(&["--channel", "telegram"], dir.path());
    assert_eq!(line["code"], "invalid_input");
    assert!(line["message"].as_str().unwrap().contains("--target fly --mode always-on"), "{line}");

    let (line, _) = deploy(&["--monitor", "--drop-store"], dir.path());
    assert_eq!(line["code"], "invalid_input", "{line}");

    let (line, code) = deploy(&["--profile", "missing"], dir.path());
    assert_eq!((line["code"].as_str(), code), (Some("missing_credentials"), Some(1)), "{line}");
    assert!(line["message"].as_str().unwrap().contains("aws configure --profile missing"));

    let (line, _) = deploy(&["--profile", "sso"], dir.path());
    assert_eq!(line["code"], "missing_credentials");
    assert!(line["message"].as_str().unwrap().contains("export-credentials"), "{line}");

    let (line, _) = deploy(&["--region", "singapore"], dir.path());
    assert_eq!(line["code"], "region_unavailable", "{line}");

    let (line, _) = deploy(&["--version", "v 1"], dir.path());
    assert_eq!(line["code"], "invalid_input");
    assert!(line["message"].as_str().unwrap().contains("--version"), "{line}");

    let (line, _) = deploy(&["--set", "memory=4096"], dir.path());
    assert!(line["message"].as_str().unwrap().contains("role_arn"), "{line}");
}

#[test]
fn an_unreachable_aws_is_an_error_line_and_not_a_hang() {
    let dir = Dir::new("unreachable");
    let (line, code) = deploy(&["--profile", "graphrt"], dir.path());
    // No profile file in this directory: the named profile is missing.
    assert_eq!((line["code"].as_str(), code), (Some("missing_credentials"), Some(1)), "{line}");

    let (line, code) = deploy(&[], dir.path());
    assert_eq!(line["code"], "aws_unavailable", "{line}");
    assert_eq!(code, Some(1));

    let output = graphrt(
        &[
            "status",
            "--target",
            "lambda",
            "--direct",
            "--deployment",
            "support",
            "--region",
            "ap-southeast-1",
            "--json",
        ],
        dir.path(),
    );
    assert_eq!(last_line(&output)["code"], "aws_unavailable");
}

#[test]
fn capabilities_name_every_target_for_apps() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_graphrt"))
        .args(["deploy", "capabilities", "--json"])
        .output()
        .expect("run graphrt");
    assert_eq!(output.status.code(), Some(0));
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    let text = std::fs::read_to_string(repo_root().join("docs/schemas/deploy-capabilities.schema.json")).unwrap();
    let validator = jsonschema::validator_for(&serde_json::from_str(&text).unwrap()).expect("schema compiles");
    let errors: Vec<String> = validator.iter_errors(&value).map(|error| error.to_string()).collect();
    assert!(errors.is_empty(), "{value} does not match the schema: {errors:?}");
    let lambda = value["targets"].as_array().unwrap().iter().find(|t| t["target"] == "lambda").unwrap().clone();
    assert_eq!((lambda["available"].as_bool(), lambda["store"]["kind"].as_str()), (Some(true), Some("s3")));
}

#[test]
fn deploy_help_lists_the_capabilities_subcommand() {
    // Apps call `deploy capabilities --json` only when `deploy --help` lists it.
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_graphrt")).args(["deploy", "--help"]).output().unwrap();
    let help = String::from_utf8_lossy(&output.stdout);
    let commands = help.split("Commands:").nth(1).unwrap_or_default();
    assert!(commands.lines().any(|line| line.trim_start().starts_with("capabilities ")), "{help}");
}

#[test]
fn fly_and_lambda_flags_do_not_mix() {
    let dir = Dir::new("mix");
    let agent = workflow();
    let output = graphrt(
        &["--target", "fly", "--direct", "--deployment", "a", "--version", "v1", "--profile", "x", "--json", &agent],
        dir.path(),
    );
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stdout).contains("--profile is an AWS option (--target lambda)"));
    let output =
        graphrt(&["--target", "ecs", "--direct", "--deployment", "a", "--version", "v1", "--json", &agent], dir.path());
    assert!(String::from_utf8_lossy(&output.stdout).contains("lambda only, not ecs"));
}
