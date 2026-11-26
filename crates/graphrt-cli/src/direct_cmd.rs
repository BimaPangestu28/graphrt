//! `graphrt deploy --target fly --direct` and its management subcommands
//! (feature `deploy-fly`). The contract (flags, JSON Lines, error codes) is
//! documented in `docs/deploy-direct.md` and pinned by `tests/deploy_direct.rs`.
//!
//! Output: with `--json`, stdout carries JSON Lines only: `step` events, then
//! exactly one `result` or `error` line (an error after a new API key was
//! stored is preceded by one `api_key` line). Without it, progress goes to stderr
//! and a short summary to stdout. Secret values never reach either stream:
//! every line except the API key field of the final line is scrubbed of them.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::Path;

use graphrt_provider_fly::direct::{
    AgentFile, DeployRequest, DirectError, DirectSettings, ErrorCode, FlyDirect, ServiceMode, StepEvent, StepStatus,
    app_name_for,
};
use graphrt_provider_fly::{FLY_API_TOKEN_ENV, FlyToken, TokenOrigin};
use serde_json::{Value, json};

use crate::deploy_cmd::{DeployAction, DeployRunArgs, DirectMode, DirectTargetArgs};
use crate::error::{CliError, EXIT_FAILED, EXIT_INVALID, EXIT_OK};

/// Image repository of the published graphrt image (agent flavour).
const IMAGE_REPOSITORY: &str = "ghcr.io/graphrt/graphrt";
/// Overrides the Machines API URL (tests, or the internal `http://_api.internal:4280`).
const API_URL_ENV: &str = "GRAPHRT_FLY_API_URL";
/// Organization a missing app is created in without `--org`.
const DEFAULT_ORG: &str = "personal";
/// Prefix of generated API keys (same shape as `graphrt gateway key new`).
const API_KEY_PREFIX: &str = "grk_";

/// A failure with the JSON `code` it is reported under.
struct Failure {
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
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self { code, message: message.into(), api_key: None }
    }

    fn invalid(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::InvalidInput.as_str(), message)
    }

    fn exit_code(&self) -> u8 {
        if self.code == ErrorCode::InvalidInput.as_str() { EXIT_INVALID } else { EXIT_FAILED }
    }
}

impl From<DirectError> for Failure {
    fn from(error: DirectError) -> Self {
        Self::new(error.code.as_str(), error.message)
    }
}

/// Writes progress and results, scrubbing secret values.
struct Reporter {
    json: bool,
    secret_values: Vec<String>,
}

impl Reporter {
    fn new(json: bool) -> Self {
        Self { json, secret_values: Vec::new() }
    }

    /// Hides `value` from every later line (values shorter than 4 bytes would
    /// blank out ordinary words and are not hidden).
    fn hide(&mut self, value: &str) {
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

    fn step(&self, event: &StepEvent) {
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
    fn result(&self, fields: Value, api_key: Option<&str>, human: &str) {
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

    fn error(&self, failure: &Failure) -> CliError {
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

/// `graphrt deploy --target fly --direct ... <agent>`.
///
/// # Errors
/// Any failure, after its `error` line (with `--json`).
pub fn deploy(args: &DeployRunArgs) -> Result<u8, CliError> {
    let mut reporter = Reporter::new(args.json);
    match run_deploy(args, &mut reporter) {
        Ok(()) => Ok(EXIT_OK),
        Err(failure) => Err(reporter.error(&failure)),
    }
}

fn run_deploy(args: &DeployRunArgs, reporter: &mut Reporter) -> Result<(), Failure> {
    let deployment = args.deployment.clone().ok_or_else(|| Failure::invalid("--deployment is required"))?;
    let version = args.version.clone().ok_or_else(|| Failure::invalid("--direct needs --version <label>"))?;
    let workflow_path =
        args.workflow.clone().ok_or_else(|| Failure::invalid("a workflow or bundle file is required"))?;
    let secrets = read_secrets(&args.secrets)?;
    for value in secrets.values() {
        reporter.hide(value);
    }
    let agent = read_agent(&workflow_path)?;
    let direct = direct_for(&deployment, args.app.as_deref())?;
    let api_key_candidate = generate_api_key()?;
    // Hidden everywhere except the field that hands it over.
    reporter.hide(&api_key_candidate);
    let request = DeployRequest {
        deployment,
        version,
        region: args.region.clone().unwrap_or_default(),
        mode: match args.mode {
            Some(DirectMode::AlwaysOn) => ServiceMode::AlwaysOn,
            Some(DirectMode::ScaleToZero) | None => ServiceMode::ScaleToZero,
        },
        org_slug: args.org.clone().unwrap_or_else(|| DEFAULT_ORG.to_owned()),
        image: args.image.clone().unwrap_or_else(default_image),
        secrets,
        api_key_candidate,
        rotate_api_key: args.rotate_api_key,
        agent,
        now_ms: graphrt_core::store::now_ms(),
    };
    let outcome = runtime()?
        .block_on(direct.deploy(&request, &mut |event| reporter.step(&event)))
        .map_err(|failure| Failure { api_key: failure.api_key, ..Failure::from(failure.error) })?;
    let fields = json!({
        "action": "deploy", "url": outcome.url, "version": outcome.version, "app": outcome.app,
        "workflow": outcome.workflow,
    });
    let human = format!(
        "deployed {} to {} (POST {}/v1/workflows/{}/runs)",
        outcome.version, outcome.url, outcome.url, outcome.workflow
    );
    reporter.result(fields, outcome.api_key.as_deref(), &human);
    Ok(())
}

/// `graphrt deploy status|list-versions|rollback|pause|resume|remove --direct`.
///
/// # Errors
/// Any failure, after its `error` line (with `--json`).
pub fn manage(action: &DeployAction) -> Result<u8, CliError> {
    let reporter = Reporter::new(action.target().json);
    match run_action(action, &reporter) {
        Ok(()) => Ok(EXIT_OK),
        Err(failure) => Err(reporter.error(&failure)),
    }
}

fn run_action(action: &DeployAction, reporter: &Reporter) -> Result<(), Failure> {
    let target: &DirectTargetArgs = action.target();
    let direct = direct_for(&target.deployment, target.app.as_deref())?;
    let runtime = runtime()?;
    let mut progress = |event: StepEvent| reporter.step(&event);
    let (fields, human) = match action {
        DeployAction::Status(_) => {
            let status = runtime.block_on(direct.status(graphrt_core::store::now_ms()))?;
            let human = format!(
                "{}: {} ({}), version {}, deployed {}",
                status.app,
                serde_json::to_value(status.status).unwrap_or(Value::Null).as_str().unwrap_or("unknown"),
                status.url,
                status.current_version.as_deref().unwrap_or("-"),
                status.last_deployed_at.as_deref().unwrap_or("-")
            );
            (serde_json::to_value(&status).unwrap_or(Value::Null), human)
        }
        DeployAction::ListVersions(_) => {
            let versions = runtime.block_on(direct.list_versions())?;
            let human = versions
                .iter()
                .map(|entry| {
                    let marker = if entry.current { "*" } else { " " };
                    format!(
                        "{marker} {}  {}  {}",
                        entry.label,
                        entry.deployed_at.as_deref().unwrap_or("-"),
                        entry.state
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            let current_version = versions.iter().find(|entry| entry.current).map(|entry| entry.label.clone());
            let fields = json!({
                "app": direct.app_name(), "url": direct.url(), "current_version": current_version, "versions": versions,
            });
            (fields, human)
        }
        DeployAction::Rollback { version, .. } => {
            let serving = runtime.block_on(direct.rollback(version, &mut progress))?;
            let human = format!("{} now serves {}", serving.url, serving.version);
            (serde_json::to_value(&serving).unwrap_or(Value::Null), human)
        }
        DeployAction::Pause(_) => {
            let changed = runtime.block_on(direct.pause())?;
            let human = format!("{} paused{}", direct.app_name(), if changed { "" } else { " (already)" });
            (json!({"app": direct.app_name(), "url": direct.url(), "status": "paused", "changed": changed}), human)
        }
        DeployAction::Resume(_) => {
            let serving = runtime.block_on(direct.resume(&mut progress))?;
            let version = serving.as_ref().map(|serving| serving.version.clone());
            let human = format!("{} online{}", direct.url(), if serving.is_some() { "" } else { " (was not paused)" });
            let fields = json!({
                "app": direct.app_name(), "url": direct.url(), "status": "online",
                "version": version, "changed": serving.is_some(),
            });
            (fields, human)
        }
        DeployAction::Remove(_) => {
            let removed = runtime.block_on(direct.remove())?;
            let human = format!("{} {}", direct.app_name(), if removed { "removed" } else { "did not exist" });
            (json!({"app": direct.app_name(), "removed": removed}), human)
        }
    };
    let mut fields = fields;
    if let Some(object) = fields.as_object_mut() {
        object.insert("action".to_owned(), Value::from(action.name()));
    }
    reporter.result(fields, None, &human);
    Ok(())
}

/// The deployer of a deployment's app, with the token from `FLY_API_TOKEN`.
fn direct_for(deployment: &str, app: Option<&str>) -> Result<FlyDirect, Failure> {
    if FlyToken::from_env().is_none() {
        return Err(Failure::new(
            "missing_token",
            format!(
                "set {FLY_API_TOKEN_ENV} to a Fly token that may create apps and set secrets (docs/deploy-direct.md)"
            ),
        ));
    }
    let app_name = match app {
        Some(app) => app.to_owned(),
        None => app_name_for(deployment)?,
    };
    let mut settings = DirectSettings::new(TokenOrigin::Env(FLY_API_TOKEN_ENV.to_owned()));
    if let Some(api_url) = std::env::var(API_URL_ENV).ok().filter(|url| !url.is_empty()) {
        settings.api_base_url = api_url;
    }
    let _already_installed = rustls::crypto::ring::default_provider().install_default();
    Ok(FlyDirect::new(settings, app_name)?)
}

/// Values of `--secret NAME` from the environment variables of the same name.
fn read_secrets(names: &[String]) -> Result<BTreeMap<String, String>, Failure> {
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
fn read_agent(path: &Path) -> Result<AgentFile, Failure> {
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
        "grt" => check_bundle(path, &contents)?,
        _ => {
            return Err(Failure::invalid(format!(
                "{}: expected a .grt bundle or a .yaml/.json workflow",
                path.display()
            )));
        }
    }
    Ok(AgentFile { file_name, contents })
}

#[cfg(feature = "bundle")]
fn check_bundle(path: &Path, contents: &[u8]) -> Result<(), Failure> {
    graphrt_bundle::Bundle::open(contents, &graphrt_bundle::OpenOptions::default())
        .map(drop)
        .map_err(|error| Failure::invalid(format!("{}: {error}", path.display())))
}

#[cfg(not(feature = "bundle"))]
fn check_bundle(_path: &Path, _contents: &[u8]) -> Result<(), Failure> {
    Ok(())
}

/// The published image of this graphrt version.
fn default_image() -> String {
    format!("{IMAGE_REPOSITORY}:{}", env!("CARGO_PKG_VERSION"))
}

/// `grk_` + 48 random hex digits (24 bytes from the OS).
fn generate_api_key() -> Result<String, Failure> {
    let mut bytes = [0u8; 24];
    getrandom::fill(&mut bytes).map_err(|error| Failure::new("internal", format!("no random source: {error}")))?;
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    Ok(format!("{API_KEY_PREFIX}{hex}"))
}

fn runtime() -> Result<tokio::runtime::Runtime, Failure> {
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
    fn default_image_is_the_published_one_of_this_version() {
        assert_eq!(default_image(), format!("ghcr.io/graphrt/graphrt:{}", env!("CARGO_PKG_VERSION")));
    }
}
