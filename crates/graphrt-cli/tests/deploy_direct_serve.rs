//! What a direct deployment serves besides the API (`graphrt deploy --direct
//! --chat --monitor --push ...`), end to end: the real binary against the fake
//! Fly Machines API, and then the command Fly would run in the Machine, run
//! here against a store directory, to check that paired devices and the
//! server's push key outlive a stop and a redeploy.
#![cfg(all(feature = "deploy-fly", feature = "web-monitor", feature = "web-push", feature = "web-chat"))]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Output;

use graphrt_provider_fly::fake::{FakeFly, TOKEN};
use serde_json::{Value, json};

const API_KEY_ENV: &str = "GRAPHRT_API_KEY";

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn hello() -> String {
    repo_root().join("examples/hello.yaml").display().to_string()
}

fn validator() -> jsonschema::Validator {
    let text = std::fs::read_to_string(repo_root().join("docs/schemas/deploy-direct.schema.json")).unwrap();
    jsonschema::validator_for(&serde_json::from_str(&text).unwrap()).expect("schema compiles")
}

async fn graphrt(fake: &FakeFly, args: &[&str]) -> Output {
    graphrt_with_env(fake, args, &[]).await
}

async fn graphrt_with_env(fake: &FakeFly, args: &[&str], env: &[(&str, &str)]) -> Output {
    let base_url = fake.server.base_url();
    let args: Vec<String> = args.iter().map(|arg| (*arg).to_owned()).collect();
    let env: Vec<(String, String)> =
        env.iter().map(|(name, value)| ((*name).to_owned(), (*value).to_owned())).collect();
    tokio::task::spawn_blocking(move || {
        std::process::Command::new(env!("CARGO_BIN_EXE_graphrt"))
            .arg("deploy")
            .args(&args)
            .env("FLY_API_TOKEN", TOKEN)
            .env("GRAPHRT_FLY_API_URL", base_url)
            .envs(env)
            .output()
            .expect("run graphrt")
    })
    .await
    .unwrap()
}

/// stdout as schema-checked JSON Lines.
fn json_lines(output: &Output) -> Vec<Value> {
    let stdout = String::from_utf8(output.stdout.clone()).unwrap();
    let validator = validator();
    stdout
        .lines()
        .map(|line| {
            let value: Value = serde_json::from_str(line).unwrap_or_else(|error| panic!("not JSON ({error}): {line}"));
            let errors: Vec<String> = validator.iter_errors(&value).map(|error| error.to_string()).collect();
            assert!(errors.is_empty(), "{value} does not match the schema: {errors:?}");
            value
        })
        .collect()
}

fn final_line(output: &Output) -> Value {
    json_lines(output).pop().expect("a final line")
}

fn deploy_args<'a>(version: &'a str, workflow: &'a str, extra: &[&'a str]) -> Vec<&'a str> {
    let mut args = vec!["--target", "fly", "--direct", "--deployment", "Support Bot", "--version", version];
    args.extend(["--region", "ams", "--json"]);
    args.extend(extra);
    args.push(workflow);
    args
}

const MONITORING: [&str; 4] = ["--monitor", "--push", "mailto:owner@example.com", "--expire-approvals"];

#[tokio::test]
async fn a_plain_deploy_reports_nothing_extra_and_no_store() {
    let fake = FakeFly::start_empty().await;
    let workflow = hello();

    let output = graphrt(&fake, &deploy_args("v1", &workflow, &[])).await;

    assert_eq!(output.status.code(), Some(0), "stderr: {}", String::from_utf8_lossy(&output.stderr));
    let result = final_line(&output);
    assert_eq!(result["store"], Value::Null);
    assert_eq!(
        result["serve"],
        json!({
            "chat": null, "cors_origins": [], "conversations": false, "monitor": false, "devices": false,
            "approvals": false, "expire_approvals": false, "push": false, "channels": [],
        })
    );
    assert!(fake.state().volumes.is_empty());
}

#[tokio::test]
async fn chat_for_guests_from_a_site_needs_no_volume() {
    let fake = FakeFly::start_empty().await;
    let workflow = hello();
    let extra =
        ["--chat", "--chat-access", "guests", "--chat-title", "Ask us", "--cors-origin", "https://www.example.com"];

    let output = graphrt(&fake, &deploy_args("v1", &workflow, &extra)).await;

    assert_eq!(output.status.code(), Some(0), "stderr: {}", String::from_utf8_lossy(&output.stderr));
    let result = final_line(&output);
    assert_eq!(result["serve"]["chat"], "guests");
    assert_eq!(result["serve"]["cors_origins"], json!(["https://www.example.com"]));
    assert_eq!(result["store"], Value::Null);
    let state = fake.state();
    assert!(state.volumes.is_empty(), "no volume, so no extra cost");
    let command = state.machines[0].config["init"]["cmd"].to_string();
    assert!(command.contains(r#""--chat","hello","--chat-access","guests","--chat-title","Ask us""#), "{command}");
}

#[tokio::test]
async fn monitoring_reports_its_store_volume_and_status_shows_it() {
    let fake = FakeFly::start_empty().await;
    let workflow = hello();

    let output = graphrt(&fake, &deploy_args("v1", &workflow, &MONITORING)).await;

    assert_eq!(output.status.code(), Some(0), "stderr: {}", String::from_utf8_lossy(&output.stderr));
    let result = final_line(&output);
    assert_eq!(result["serve"]["monitor"], true);
    assert_eq!(result["serve"]["devices"], true);
    assert_eq!(result["serve"]["approvals"], true);
    assert_eq!(result["serve"]["push"], true);
    assert_eq!(result["store"]["size_gb"], 1);
    assert_eq!(result["store"]["region"], "ams");
    assert_eq!(result["store"]["in_use"], true);
    assert!(!result.to_string().contains("owner@example.com"), "the contact address is not echoed");

    let status =
        graphrt(&fake, &["status", "--target", "fly", "--direct", "--deployment", "Support Bot", "--json"]).await;
    let status = final_line(&status);
    assert_eq!(status["store"]["volume"], result["store"]["volume"]);

    // Turning monitoring off keeps the data and says how to delete it; --drop-store deletes it.
    let kept = final_line(&graphrt(&fake, &deploy_args("v2", &workflow, &[])).await);
    assert_eq!(kept["store"]["in_use"], false);
    let dropped = final_line(&graphrt(&fake, &deploy_args("v3", &workflow, &["--drop-store"])).await);
    assert_eq!(dropped["store"], Value::Null);
    assert!(fake.state().volumes.is_empty());
}

#[tokio::test]
async fn a_channel_is_served_with_its_credentials_as_fly_secrets_only() {
    let fake = FakeFly::start_empty().await;
    let workflow = hello();
    let app_secret = "whatsapp-app-secret-0123456789";
    let secrets = [
        ("WHATSAPP_ACCESS_TOKEN", "whatsapp-access-token-0123456789"),
        ("WHATSAPP_PHONE_NUMBER_ID", "106540352242922"),
        ("WHATSAPP_APP_SECRET", app_secret),
        ("WHATSAPP_VERIFY_TOKEN", "verify-0123456789"),
    ];
    let mut extra = vec!["--channel", "whatsapp"];
    for (name, _) in &secrets {
        extra.extend(["--secret", name]);
    }

    let output = graphrt_with_env(&fake, &deploy_args("v1", &workflow, &extra), &secrets).await;

    assert_eq!(output.status.code(), Some(0), "stderr: {}", String::from_utf8_lossy(&output.stderr));
    let result = final_line(&output);
    assert_eq!(result["serve"]["channels"], json!(["whatsapp"]));
    assert_eq!(result["store"]["in_use"], true, "channel conversations outlive a restart");
    let state = fake.state();
    assert_eq!(state.secrets.get("WHATSAPP_APP_SECRET").map(String::as_str), Some(app_secret));
    let config = state.machines[0].config.to_string();
    assert!(config.contains(r#""--channel","whatsapp=hello""#), "{config}");
    for (_, value) in &secrets {
        assert!(!config.contains(value), "a secret reached the Machine config: {config}");
        assert!(!String::from_utf8_lossy(&output.stdout).contains(value));
        assert!(!String::from_utf8_lossy(&output.stderr).contains(value));
    }
}

#[tokio::test]
async fn telegram_polling_is_refused_on_a_machine_that_stops_when_idle() {
    let fake = FakeFly::start_empty().await;
    let workflow = hello();
    let extra = ["--mode", "scale-to-zero", "--channel", "telegram"];

    let output = graphrt(&fake, &deploy_args("v1", &workflow, &extra)).await;

    assert_eq!(output.status.code(), Some(2));
    let error = final_line(&output);
    assert_eq!(error["code"], "invalid_input", "{error}");
    assert!(error["message"].as_str().unwrap().contains("--mode always-on"), "{error}");
    assert!(fake.state().apps.is_empty(), "nothing was created on Fly");
}

#[tokio::test]
async fn without_json_the_pages_and_the_volume_are_named_for_a_person() {
    let fake = FakeFly::start_empty().await;
    let workflow = hello();
    let mut args = deploy_args("v1", &workflow, &["--chat", "--monitor"]);
    args.retain(|arg| *arg != "--json");

    let output = graphrt(&fake, &args).await;

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("chat page: https://support-bot.fly.dev/chat/"), "{stdout}");
    assert!(stdout.contains("monitoring page: https://support-bot.fly.dev/monitor/"), "{stdout}");
    assert!(stdout.contains("store: Fly volume") && stdout.contains("billed by Fly"), "{stdout}");
}

#[tokio::test]
async fn options_that_cannot_work_are_refused_before_any_call_to_fly() {
    let fake = FakeFly::start_empty().await;
    let workflow = hello();
    let cases: [(&[&str], &str); 4] = [
        (&["--chat-access", "guests"], "--chat-access needs --chat"),
        (&["--cors-origin", "www.example.com"], "--cors-origin"),
        (&["--push", "owner@example.com"], "mailto:"),
        (&["--monitor", "--drop-store"], "--drop-store"),
    ];
    for (extra, expected) in cases {
        let output = graphrt(&fake, &deploy_args("v1", &workflow, extra)).await;
        assert_eq!(output.status.code(), Some(2), "{extra:?}");
        let error = final_line(&output);
        assert_eq!(error["code"], "invalid_input", "{error}");
        assert!(error["message"].as_str().unwrap().contains(expected), "{error}");
    }
    assert!(fake.state().apps.is_empty(), "nothing was created on Fly");

    let gateway = graphrt(&fake, &["--target", "local", "--deployment", "d", "--monitor", &workflow]).await;
    assert_eq!(gateway.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&gateway.stderr).contains("--monitor needs --direct"));
}

/// One HTTP request to the server on `port`: the status and the JSON body.
fn call(port: u16, method: &str, path: &str, token: &str, body: Option<&Value>) -> (u16, Value) {
    let Ok(mut stream) = std::net::TcpStream::connect(("127.0.0.1", port)) else { return (0, Value::Null) };
    let body = body.map(Value::to_string).unwrap_or_default();
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {token}\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    if stream.write_all(request.as_bytes()).is_err() {
        return (0, Value::Null);
    }
    let mut response = String::new();
    let _ = stream.read_to_string(&mut response);
    let status = response.split(' ').nth(1).and_then(|status| status.parse().ok()).unwrap_or(0);
    let text = response.split_once("\r\n\r\n").map_or("", |(_, text)| text);
    // The body may be chunked: take the JSON object out of it.
    let json = text.find('{').zip(text.rfind('}')).and_then(|(from, to)| serde_json::from_str(&text[from..=to]).ok());
    (status, json.unwrap_or(Value::Null))
}

/// A running `graphrt serve`, killed when dropped.
struct Server {
    child: std::process::Child,
    port: u16,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The command of the deployment's serving Machine.
fn machine_command(fake: &FakeFly) -> Vec<String> {
    let state = fake.state();
    let machine = state.machines.iter().find(|machine| machine.metadata("graphrt_role") == Some("current")).unwrap();
    let command = machine.config.pointer("/processes/0/cmd").or_else(|| machine.config.pointer("/init/cmd"));
    command.unwrap().as_array().unwrap().iter().map(|part| part.as_str().unwrap().to_owned()).collect()
}

/// Starts a Machine's command on this computer: the workflow directory, the
/// store mount and the address are replaced by local ones, every other
/// argument is what Fly would run.
fn start_machine_command(command: &[String], workflows: &Path, store: &Path, api_key: &str) -> Server {
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let local: Vec<String> = command
        .iter()
        .map(|argument| match argument.as_str() {
            "/workflows" => workflows.display().to_string(),
            "/data" => store.display().to_string(),
            "0.0.0.0:8080" => format!("127.0.0.1:{port}"),
            other => other.to_owned(),
        })
        .collect();
    let child = std::process::Command::new(env!("CARGO_BIN_EXE_graphrt"))
        .args(&local)
        .arg("--no-default-plugins")
        .env(API_KEY_ENV, api_key)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn graphrt serve");
    let server = Server { child, port };
    for _ in 0..1000 {
        if call(port, "GET", "/readyz", "", None).0 == 200 {
            return server;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    panic!("graphrt {local:?} did not get ready");
}

/// The requirement behind the store volume: a device paired once keeps working,
/// and the server keeps its push key, when the Machine stops and starts (scale
/// to zero) and when a new version is deployed.
#[tokio::test]
async fn paired_devices_and_the_push_key_outlive_a_stop_and_a_redeploy() {
    let fake = FakeFly::start_empty().await;
    let root = graphrt_core::private_fs::create_fresh_temp_dir("graphrt-direct-serve-store").unwrap();
    let workflows = root.join("workflows");
    std::fs::create_dir_all(&workflows).unwrap();
    std::fs::copy(hello(), workflows.join("hello.yaml")).unwrap();
    // Stands in for the Fly volume mounted at /data.
    let store = root.join("volume");
    let workflow = hello();

    let deployed = final_line(&graphrt(&fake, &deploy_args("v1", &workflow, &MONITORING)).await);
    let api_key = deployed["api_key"].as_str().expect("the first deploy shows the key").to_owned();
    let url = deployed["url"].as_str().unwrap().to_owned();

    let first_command = machine_command(&fake);
    assert!(first_command.contains(&"--monitor".to_owned()) && first_command.contains(&"--store".to_owned()));
    let (device_token, push_key) = tokio::task::spawn_blocking({
        let (workflows, store, api_key) = (workflows.clone(), store.clone(), api_key.clone());
        move || {
            let server = start_machine_command(&first_command, &workflows, &store, &api_key);
            let (status, paired) =
                call(server.port, "POST", "/v1/devices", &api_key, Some(&json!({"name": "Ana's phone"})));
            assert_eq!(status, 201, "{paired}");
            let token = paired["token"].as_str().unwrap().to_owned();
            assert_eq!(paired["pairing"]["url"], url.as_str(), "the pairing payload names the public address");
            assert!(paired["link"].as_str().unwrap().starts_with(&format!("{url}/monitor/#pair=")));
            assert_eq!(call(server.port, "GET", "/v1/devices/me", &token, None).0, 200);
            assert_eq!(call(server.port, "GET", "/v1/approvals", &token, None).0, 200);
            assert_eq!(call(server.port, "GET", "/monitor/", "", None).0, 200);
            let (status, push) = call(server.port, "GET", "/v1/push", &token, None);
            assert_eq!(status, 200, "{push}");
            (token, push["public_key"].as_str().unwrap().to_owned())
        }
    })
    .await
    .unwrap();

    // A new version: the same Machine and volume, so the same store.
    let redeployed = final_line(&graphrt(&fake, &deploy_args("v2", &workflow, &MONITORING)).await);
    assert_eq!(redeployed["store"], deployed["store"]);
    assert_eq!(redeployed["api_key"], Value::Null);

    let second_command = machine_command(&fake);
    tokio::task::spawn_blocking(move || {
        {
            // Twice: the redeployed version's first boot, then a start after an idle stop.
            for boot in ["after the redeploy", "after a stop"] {
                let server = start_machine_command(&second_command, &workflows, &store, &api_key);
                let (status, device) = call(server.port, "GET", "/v1/devices/me", &device_token, None);
                assert_eq!(status, 200, "{boot}: the paired device still signs in ({device})");
                assert_eq!(device["name"], "Ana's phone");
                let (_, push) = call(server.port, "GET", "/v1/push", &device_token, None);
                assert_eq!(push["public_key"], push_key.as_str(), "{boot}: subscriptions stay valid");
                // Revoking ends the token at once.
                if boot == "after a stop" {
                    let id = device["id"].as_str().unwrap();
                    assert_eq!(call(server.port, "DELETE", &format!("/v1/devices/{id}"), &api_key, None).0, 204);
                    assert_eq!(call(server.port, "GET", "/v1/devices/me", &device_token, None).0, 401);
                }
            }
        }
    })
    .await
    .unwrap();
    let _ = std::fs::remove_dir_all(&root);
}
