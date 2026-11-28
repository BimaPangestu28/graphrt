//! `graphrt deploy --direct` and its management subcommands: one agent
//! served straight by a platform, no gateway. Fly.io (feature `deploy-fly`,
//! [`fly`], `docs/deploy-direct.md`), Google Cloud Run (feature
//! `deploy-cloud-run`, [`cloud_run`], `docs/deploy-cloud-run.md`) and AWS Lambda
//! (feature `deploy-aws`, [`lambda`], `docs/deploy-direct-aws.md`) share the
//! flags, the output and the checks of this module. The contracts are pinned by
//! `tests/deploy_direct.rs`, `tests/deploy_direct_cloud_run.rs` and
//! `tests/deploy_direct_lambda.rs`.
//!
//! Output: with `--json`, stdout carries JSON Lines only: `step` events, then
//! exactly one `result` or `error` line (an error after a new API key was
//! stored is preceded by one `api_key` line). Without it, progress goes to stderr
//! and a short summary to stdout. Secret values never reach either stream:
//! every line except the API key field of the final line is scrubbed of them.

#[cfg(feature = "deploy-cloud-run")]
mod cloud_run;
#[cfg(feature = "deploy-fly")]
mod fly;
#[cfg(feature = "deploy-aws")]
mod lambda;

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::Path;

use graphrt_provider_api::direct::{
    AgentFile, ChatAccess, ChatOptions, DirectChannel, ServeOptions, ServiceMode, StepEvent, StepStatus,
};
use serde_json::{Value, json};

use crate::deploy_cmd::{DeployAction, DeployRunArgs, DeployTarget, DirectChatAccess, DirectMode, DirectServeArgs};
use crate::error::{CliError, EXIT_FAILED, EXIT_INVALID, EXIT_OK};

/// Prefix of generated API keys (same shape as `graphrt gateway key new`).
const API_KEY_PREFIX: &str = "grk_";

/// The `code` of invalid input, the same on every platform.
const INVALID_INPUT: &str = "invalid_input";

/// A failure with the JSON `code` it is reported under.
pub(crate) struct Failure {
    code: &'static str,
    message: String,
    api_key: Option<String>,
}

impl std::fmt::Debug for Failure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Failure")
            .field("code", &self.code)
            .field("message", &self.message)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

impl Failure {
    pub(crate) fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self { code, message: message.into(), api_key: None }
    }

    pub(crate) fn invalid(message: impl Into<String>) -> Self {
        Self::new(INVALID_INPUT, message)
    }

    fn exit_code(&self) -> u8 {
        if self.code == INVALID_INPUT { EXIT_INVALID } else { EXIT_FAILED }
    }
}

/// Writes progress and results, scrubbing secret values.
pub(crate) struct Reporter {
    json: bool,
    secret_values: Vec<String>,
}

impl Reporter {
    pub(crate) fn new(json: bool) -> Self {
        Self { json, secret_values: Vec::new() }
    }

    /// Hides `value` from every later line (values shorter than 4 bytes would
    /// blank out ordinary words and are not hidden).
    pub(crate) fn hide(&mut self, value: &str) {
        if value.len() >= 4 {
            self.secret_values.push(value.to_owned());
        }
    }

    fn scrub(&self, text: &str) -> String {
        self.secret_values
            .iter()
            .fold(text.to_owned(), |scrubbed, value| scrubbed.replace(value.as_str(), "[redacted]"))
    }

    /// `value` with every string scrubbed (before encoding, so escaping cannot hide a value).
    fn scrub_value(&self, value: Value) -> Value {
        match value {
            Value::String(text) => Value::String(self.scrub(&text)),
            Value::Array(items) => Value::Array(items.into_iter().map(|item| self.scrub_value(item)).collect()),
            Value::Object(object) => {
                Value::Object(object.into_iter().map(|(key, item)| (key, self.scrub_value(item))).collect())
            }
            other => other,
        }
    }

    pub(crate) fn step(&self, event: &StepEvent) {
        let message = self.scrub(&event.message);
        if self.json {
            let line = json!({"type": "step", "step": event.step, "status": event.status, "message": message});
            self.stdout_line(&line.to_string());
            return;
        }
        let marker = match event.status {
            StepStatus::Running => "..",
            StepStatus::Done => "ok",
            StepStatus::Failed => "!!",
        };
        let step = serde_json::to_value(event.step).ok().and_then(|value| value.as_str().map(str::to_owned));
        let _ignored = writeln!(std::io::stderr().lock(), "[{marker}] {}: {message}", step.unwrap_or_default());
    }

    /// The final `result` line. `api_key` (deploy results only) is added after
    /// scrubbing: it is the one secret this command is meant to print.
    pub(crate) fn result(&self, fields: Value, api_key: Option<&str>, human: &str) {
        if !self.json {
            self.stdout_line(&self.scrub(human));
            if let Some(api_key) = api_key {
                self.stdout_line(&format!("API key (shown once, store it now): {api_key}"));
            }
            return;
        }
        let mut line = serde_json::Map::new();
        line.insert("type".to_owned(), Value::from("result"));
        if let Value::Object(fields) = self.scrub_value(fields) {
            line.extend(fields);
        }
        if line.get("action").and_then(Value::as_str) == Some("deploy") {
            line.insert("api_key".to_owned(), api_key.map_or(Value::Null, Value::from));
        }
        self.stdout_line(&Value::Object(line).to_string());
    }

    pub(crate) fn error(&self, failure: &Failure) -> CliError {
        let message = self.scrub(&failure.message);
        if self.json {
            // A key stored before the failure gets its own line, never the error line.
            if let Some(api_key) = &failure.api_key {
                self.stdout_line(&json!({"type": "api_key", "api_key": api_key}).to_string());
            }
            let line = json!({"type": "error", "code": failure.code, "message": message});
            self.stdout_line(&line.to_string());
        } else if let Some(api_key) = &failure.api_key {
            self.stdout_line(&format!("API key (stored before the failure, shown once): {api_key}"));
        }
        CliError { code: failure.exit_code(), message: format!("{}: {message}", failure.code), preformatted: false }
    }

    fn stdout_line(&self, line: &str) {
        let _ignored = writeln!(std::io::stdout().lock(), "{line}");
    }
}

/// `graphrt deploy --direct ... <agent>` on the platform of `--target`.
///
/// # Errors
/// Any failure, after its `error` line (with `--json`).
pub fn deploy(args: &DeployRunArgs) -> Result<u8, CliError> {
    let mut reporter = Reporter::new(args.json);
    let outcome = match args.target {
        #[cfg(feature = "deploy-fly")]
        DeployTarget::Fly => fly::run_deploy(args, &mut reporter),
        #[cfg(feature = "deploy-cloud-run")]
        DeployTarget::CloudRun => cloud_run::run_deploy(args, &mut reporter),
        #[cfg(feature = "deploy-aws")]
        DeployTarget::Lambda => lambda::run_deploy(args, &mut reporter),
        other => Err(Failure::invalid(format!("--direct does not support --target {}", other.kind()))),
    };
    match outcome {
        Ok(()) => Ok(EXIT_OK),
        Err(failure) => Err(reporter.error(&failure)),
    }
}

/// `graphrt deploy status|list-versions|rollback|pause|resume|remove --direct`.
///
/// # Errors
/// Any failure, after its `error` line (with `--json`).
pub fn manage(action: &DeployAction) -> Result<u8, CliError> {
    let reporter = Reporter::new(action.target().json);
    let outcome = match action.target().target {
        #[cfg(feature = "deploy-fly")]
        DeployTarget::Fly => fly::run_action(action, &reporter),
        #[cfg(feature = "deploy-cloud-run")]
        DeployTarget::CloudRun => cloud_run::run_action(action, &reporter),
        #[cfg(feature = "deploy-aws")]
        DeployTarget::Lambda => lambda::run_action(action, &reporter),
        other => Err(Failure::invalid(format!("--direct does not support --target {}", other.kind()))),
    };
    match outcome {
        Ok(()) => Ok(EXIT_OK),
        Err(failure) => Err(reporter.error(&failure)),
    }
}

/// The deploy fields every platform reads the same way.
pub(crate) struct CommonDeploy {
    pub(crate) deployment: String,
    pub(crate) version: String,
    pub(crate) serve: ServeOptions,
    pub(crate) secrets: BTreeMap<String, String>,
    pub(crate) agent: AgentFile,
    pub(crate) api_key_candidate: String,
    pub(crate) mode: ServiceMode,
}

/// Reads and checks the flags and files every platform needs, hiding secret
/// values and the API key candidate from every later line.
pub(crate) fn common_deploy(args: &DeployRunArgs, reporter: &mut Reporter) -> Result<CommonDeploy, Failure> {
    let deployment = args.deployment.clone().ok_or_else(|| Failure::invalid("--deployment is required"))?;
    let version = args.version.clone().ok_or_else(|| Failure::invalid("--direct needs --version <label>"))?;
    let workflow_path =
        args.workflow.clone().ok_or_else(|| Failure::invalid("a workflow or bundle file is required"))?;
    let serve = serve_options(&args.serve)?;
    let secrets = read_secrets(&args.secrets)?;
    for value in secrets.values() {
        reporter.hide(value);
    }
    let agent = read_agent(&workflow_path)?;
    let api_key_candidate = generate_api_key()?;
    // Hidden everywhere except the field that hands it over.
    reporter.hide(&api_key_candidate);
    let mode = match args.mode {
        Some(DirectMode::AlwaysOn) => ServiceMode::AlwaysOn,
        Some(DirectMode::ScaleToZero) | None => ServiceMode::ScaleToZero,
    };
    Ok(CommonDeploy { deployment, version, serve, secrets, agent, api_key_candidate, mode })
}

/// The `graphrt serve` features of a deploy, from its flags.
///
/// @throws Failure(invalid_input) - a chat option without `--chat`
pub(crate) fn serve_options(args: &DirectServeArgs) -> Result<ServeOptions, Failure> {
    if let Some(flag) = args.chat_option_without_chat() {
        return Err(Failure::invalid(format!("{flag} needs --chat")));
    }
    let chat = args.chat.then(|| ChatOptions {
        access: match args.chat_access {
            Some(DirectChatAccess::Guests) => ChatAccess::Guests,
            Some(DirectChatAccess::Key) | None => ChatAccess::Key,
        },
        title: args.chat_title.clone(),
        guest_rate_limit: args.chat_guest_rate_limit,
        guest_budget: args.chat_guest_budget,
    });
    Ok(ServeOptions {
        chat,
        cors_origins: args.cors_origin.clone(),
        conversations: args.conversations,
        monitor: args.monitor,
        devices: args.devices,
        approvals: args.approvals,
        expire_approvals: args.expire_approvals,
        push: args.push.clone(),
        channels: args
            .channel
            .iter()
            .map(|kind| {
                DirectChannel::parse(kind)
                    .ok_or_else(|| Failure::invalid("--channel takes telegram, whatsapp, slack or teams"))
            })
            .collect::<Result<_, _>>()?,
    })
}

/// Values of `--secret NAME` from the environment variables of the same name.
pub(crate) fn read_secrets(names: &[String]) -> Result<BTreeMap<String, String>, Failure> {
    let mut secrets = BTreeMap::new();
    for name in names {
        if name.contains('=') {
            return Err(Failure::invalid(format!(
                "--secret takes a NAME with --direct (the value comes from ${{NAME}}), got `{}`",
                name.split('=').next().unwrap_or_default()
            )));
        }
        let value = std::env::var(name).ok().filter(|value| !value.is_empty()).ok_or_else(|| {
            Failure::invalid(format!("--secret {name}: environment variable {name} is unset or empty"))
        })?;
        secrets.insert(name.clone(), value);
    }
    Ok(secrets)
}

/// Reads and checks the agent file locally, so a broken agent never reaches Fly.
pub(crate) fn read_agent(path: &Path) -> Result<AgentFile, Failure> {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| Failure::invalid(format!("{}: not a file name", path.display())))?
        .to_owned();
    let extension = path.extension().and_then(|extension| extension.to_str()).unwrap_or_default().to_ascii_lowercase();
    let contents = std::fs::read(path).map_err(|error| Failure::invalid(format!("{}: {error}", path.display())))?;
    match extension.as_str() {
        "yaml" | "yml" | "json" => {
            let text = std::str::from_utf8(&contents)
                .map_err(|_| Failure::invalid(format!("{}: not UTF-8 text", path.display())))?;
            graphrt_core::Workflow::parse(text)
                .map_err(|error| Failure::invalid(format!("{}: {error}", path.display())))?;
        }
        "grt" => {
            let unpacked = check_bundle(path, &contents)?;
            return Ok(AgentFile::new(file_name, contents).unpacked(unpacked));
        }
        _ => {
            return Err(Failure::invalid(format!(
                "{}: expected a .grt bundle or a .yaml/.json workflow",
                path.display()
            )));
        }
    }
    Ok(AgentFile::new(file_name, contents))
}

/// Opens the bundle (hashes, limits) and returns the bytes of its files once unpacked.
#[cfg(feature = "bundle")]
fn check_bundle(path: &Path, contents: &[u8]) -> Result<u64, Failure> {
    let bundle = graphrt_bundle::Bundle::open(contents, &graphrt_bundle::OpenOptions::default())
        .map_err(|error| Failure::invalid(format!("{}: {error}", path.display())))?;
    let unpacked: usize = bundle.paths().filter_map(|file| bundle.file(file)).map(<[u8]>::len).sum();
    Ok(u64::try_from(unpacked).unwrap_or(u64::MAX))
}

#[cfg(not(feature = "bundle"))]
fn check_bundle(_path: &Path, contents: &[u8]) -> Result<u64, Failure> {
    Ok(u64::try_from(contents.len()).unwrap_or(u64::MAX))
}

/// `grk_` + 48 random hex digits (24 bytes from the OS).
pub(crate) fn generate_api_key() -> Result<String, Failure> {
    let mut bytes = [0u8; 24];
    getrandom::fill(&mut bytes).map_err(|error| Failure::new("internal", format!("no random source: {error}")))?;
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    Ok(format!("{API_KEY_PREFIX}{hex}"))
}

pub(crate) fn runtime() -> Result<tokio::runtime::Runtime, Failure> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| Failure::new("internal", format!("cannot start the async runtime: {error}")))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn api_keys_are_long_random_and_prefixed() {
        let first = generate_api_key().unwrap();
        assert!(first.starts_with("grk_") && first.len() == 52, "{first}");
        assert_ne!(first, generate_api_key().unwrap());
    }

    #[test]
    fn scrub_hides_secret_values_but_not_short_words() {
        let mut reporter = Reporter::new(true);
        reporter.hide("sk-abcdef");
        reporter.hide("on");
        assert_eq!(reporter.scrub("value sk-abcdef is on"), "value [redacted] is on");
    }

    #[test]
    fn secret_flags_take_names_only() {
        assert!(read_secrets(&["OPENAI_API_KEY=sk-x".to_owned()]).is_err());
        assert!(read_secrets(&["GRAPHRT_TEST_SURELY_UNSET_VARIABLE".to_owned()]).is_err());
    }

    #[test]
    fn serve_flags_map_to_serve_options() {
        let none = serve_options(&DirectServeArgs::default()).unwrap();
        assert_eq!(none, ServeOptions::default());
        let args = DirectServeArgs {
            chat: true,
            chat_access: Some(DirectChatAccess::Guests),
            chat_guest_budget: Some(30),
            cors_origin: vec!["https://www.example.com".to_owned()],
            monitor: true,
            push: Some("mailto:owner@example.com".to_owned()),
            ..DirectServeArgs::default()
        };
        let options = serve_options(&args).unwrap();
        let chat = options.chat.clone().unwrap();
        assert_eq!((chat.access, chat.guest_budget, chat.guest_rate_limit), (ChatAccess::Guests, Some(30), None));
        assert!(options.monitor && options.needs_store());
        assert_eq!(options.cors_origins, ["https://www.example.com"]);
        let orphan = DirectServeArgs { chat_access: Some(DirectChatAccess::Guests), ..DirectServeArgs::default() };
        assert!(serve_options(&orphan).unwrap_err().message.contains("--chat-access needs --chat"));
        let channels = DirectServeArgs {
            channel: vec!["whatsapp".to_owned(), "telegram".to_owned()],
            ..DirectServeArgs::default()
        };
        let options = serve_options(&channels).unwrap();
        assert_eq!(options.channels, [DirectChannel::WhatsApp, DirectChannel::Telegram]);
        assert!(options.needs_store());
        let unknown = DirectServeArgs { channel: vec!["sms".to_owned()], ..DirectServeArgs::default() };
        assert!(serve_options(&unknown).unwrap_err().message.contains("slack or teams"));
    }
}
