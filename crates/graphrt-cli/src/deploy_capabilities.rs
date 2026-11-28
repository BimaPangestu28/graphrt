//! `graphrt deploy capabilities [--json]`: what each direct deploy target
//! supports in this build, so an app (the desktop app) reads one stable JSON
//! object instead of looking for words in `--help`. The contract is
//! `docs/deploy-capabilities.md` and `docs/schemas/deploy-capabilities.schema.json`.
//!
//! The object only grows: fields are added, never renamed or removed, and
//! `schema_version` changes only when a field changes meaning.

use std::io::Write as _;

use serde_json::{Value, json};

use crate::error::{CliError, EXIT_OK};

/// Version of the object's shape.
pub const SCHEMA_VERSION: u64 = 1;

/// The `graphrt serve` features a direct deploy can turn on (the flag names without `--`).
const SERVE_FEATURES: [&str; 9] =
    ["chat", "cors-origin", "conversations", "monitor", "devices", "approvals", "expire-approvals", "push", "channel"];

/// The capabilities of every direct target.
pub fn capabilities() -> Value {
    let fly = json!({
        "target": "fly",
        "available": cfg!(feature = "deploy-fly"),
        "docs": "docs/deploy-direct.md",
        "schema": "docs/schemas/deploy-direct.schema.json",
        "credentials": { "env": ["FLY_API_TOKEN"], "flags": [] },
        "flags": ["--region", "--mode", "--org", "--app", "--image", "--secret", "--rotate-api-key", "--drop-store"],
        "modes": ["scale-to-zero", "always-on"],
        "default_mode": "scale-to-zero",
        "max_request_secs": Value::Null,
        "serve": SERVE_FEATURES,
        "store": { "kind": "volume", "features": ["conversations", "monitor", "devices", "push", "channel"], "drop_store": true },
        "channels": ["telegram", "whatsapp", "slack", "teams"],
        "channels_always_on": ["telegram", "slack-socket-mode"],
        "idle_cost": "stopped machine rootfs, plus the store volume while it exists",
    });
    let cloud_run = json!({
        "target": "cloud-run",
        "available": cfg!(feature = "deploy-cloud-run"),
        "docs": "docs/deploy-cloud-run.md",
        "schema": "docs/schemas/deploy-direct.schema.json",
        "credentials": {
            "env": ["GOOGLE_OAUTH_ACCESS_TOKEN", "GOOGLE_APPLICATION_CREDENTIALS", "GOOGLE_CLOUD_PROJECT"],
            "flags": ["--project"],
        },
        "flags": ["--project", "--region", "--label", "--mode", "--app", "--image", "--secret", "--rotate-api-key", "--drop-store"],
        "modes": ["scale-to-zero", "always-on"],
        "default_mode": "scale-to-zero",
        "max_request_secs": 3600,
        "serve": SERVE_FEATURES,
        "store": { "kind": "gcs", "features": ["conversations", "monitor", "devices", "push", "channel"], "drop_store": true },
        "channels": ["telegram", "whatsapp", "slack", "teams"],
        "channels_always_on": ["telegram", "slack-socket-mode"],
        "idle_cost": "none for the service, storage of the image and the store bucket",
    });
    let lambda = json!({
        "target": "lambda",
        "available": cfg!(feature = "deploy-aws"),
        "docs": "docs/deploy-direct-aws.md",
        "schema": "docs/schemas/deploy-direct-aws.schema.json",
        "credentials": {
            "env": ["AWS_ACCESS_KEY_ID", "AWS_SECRET_ACCESS_KEY", "AWS_SESSION_TOKEN", "AWS_PROFILE", "AWS_REGION", "AWS_SHARED_CREDENTIALS_FILE"],
            "flags": ["--profile", "--region"],
        },
        "flags": ["--region", "--profile", "--app", "--image", "--secret", "--rotate-api-key", "--set role_arn=ARN", "--drop-store"],
        "modes": ["scale-to-zero"],
        "default_mode": "scale-to-zero",
        "max_request_secs": 900,
        "serve": SERVE_FEATURES,
        "store": {
            "kind": "s3",
            "features": ["conversations", "monitor", "devices", "push", "channel"],
            "drop_store": true,
            "requires_feature": "store-s3",
            "available": cfg!(feature = "store-s3"),
        },
        "channels": ["whatsapp", "slack", "teams"],
        "channels_always_on": [],
        "channels_refused": ["telegram", "slack-socket-mode"],
        "idle_cost": "none for the function, storage of the image and the store bucket",
    });
    json!({
        "type": "result",
        "action": "capabilities",
        "schema_version": SCHEMA_VERSION,
        "graphrt": env!("CARGO_PKG_VERSION"),
        "targets": [fly, cloud_run, lambda],
    })
}

/// `graphrt deploy capabilities`.
///
/// # Errors
/// Never: a closed stdout is ignored, as for the other JSON lines.
pub fn print(json: bool) -> Result<u8, CliError> {
    let value = capabilities();
    let text = if json {
        value.to_string()
    } else {
        let mut lines = Vec::new();
        let field = |target: &Value, name: &str| target.get(name).cloned().unwrap_or(Value::Null);
        for target in value.get("targets").and_then(Value::as_array).into_iter().flatten() {
            let list = |name: &str| {
                field(target, name)
                    .as_array()
                    .map(|items| items.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", "))
                    .unwrap_or_default()
            };
            let store = field(target, "store");
            lines.push(format!(
                "{} ({}): modes {}, store {}, channels {}",
                field(target, "target").as_str().unwrap_or_default(),
                if field(target, "available") == true { "in this build" } else { "not in this build" },
                list("modes"),
                store.get("kind").and_then(Value::as_str).unwrap_or("none"),
                list("channels"),
            ));
        }
        lines.join("\n")
    };
    let _ignored = writeln!(std::io::stdout().lock(), "{text}");
    Ok(EXIT_OK)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_target_names_the_same_fields() {
        let value = capabilities();
        let targets = value["targets"].as_array().cloned().unwrap_or_default();
        assert_eq!(targets.len(), 3);
        for target in &targets {
            for field in
                ["target", "available", "docs", "schema", "credentials", "flags", "modes", "serve", "store", "channels"]
            {
                assert!(target.get(field).is_some(), "{} has no {field}", target["target"]);
            }
        }
        let lambda = targets.iter().find(|target| target["target"] == "lambda").cloned().unwrap_or_default();
        assert_eq!(lambda["store"]["kind"], "s3");
        assert!(lambda["serve"].as_array().is_some_and(|serve| serve.contains(&Value::from("monitor"))));
    }
}
