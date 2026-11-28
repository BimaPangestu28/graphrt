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

/// What the agent file (a workflow or a bundle) carries to every target: a
/// workflow file alone brings no subagents, a bundle brings them with the
/// budgets of its runs (`docs/bundles.md`). `build`, `file` and `from` are
/// words for people, not for parsing.
#[cfg(any(feature = "deploy-fly", feature = "deploy-cloud-run", feature = "deploy-aws"))]
fn agent_file() -> Value {
    json!({
        "formats": ["yaml", "json", "grt"],
        "subagents": {
            "in": "bundle",
            "manifest_field": "workflows",
            "path": "workflows/<name>.<yaml|yml|json>",
            "build": "graphrt bundle build <entry> [--workflows DIR]",
        },
        "limits": {
            "in": "bundle",
            "manifest_field": "limits",
            "fields": ["max_total_tokens", "max_cost_micros"],
            "build": "graphrt bundle build <entry> --max-tokens-per-run N --max-cost-per-run AMOUNT",
            "combine": "lower",
        },
        "prices": { "in": "bundle", "file": "providers (graphrt bundle build --providers FILE), `prices` per model" },
        "memory": { "from": "long_term_memory of any agent node of the agent file", "needs": "store" },
    })
}

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
    #[allow(unused_mut)]
    let mut value = json!({
        "type": "result",
        "action": "capabilities",
        "schema_version": SCHEMA_VERSION,
        "graphrt": env!("CARGO_PKG_VERSION"),
        "targets": [fly, cloud_run, lambda],
    });
    #[cfg(any(feature = "deploy-fly", feature = "deploy-cloud-run", feature = "deploy-aws"))]
    with_agent_file(&mut value);
    value
}

/// What builds with a direct target add: memory per target, and what the
/// agent file carries (`agent_file`).
#[cfg(any(feature = "deploy-fly", feature = "deploy-cloud-run", feature = "deploy-aws"))]
fn with_agent_file(value: &mut Value) {
    let kept = json!({ "kept": true, "dir": graphrt_provider_api::direct::MEMORY_PATH });
    let refused = json!({
        "kept": false,
        "refused": "Lambda keeps no files between requests, and requests at once would write notes apart",
    });
    for target in value.get_mut("targets").and_then(Value::as_array_mut).into_iter().flatten() {
        let memory = if target.get("target").and_then(Value::as_str) == Some("lambda") { &refused } else { &kept };
        if memory["kept"] == true
            && let Some(features) = target.pointer_mut("/store/features").and_then(Value::as_array_mut)
        {
            features.push(Value::from("memory"));
        }
        if let Some(fields) = target.as_object_mut() {
            fields.insert("memory".to_owned(), memory.clone());
        }
    }
    if let Some(fields) = value.as_object_mut() {
        fields.insert("agent_file".to_owned(), agent_file());
    }
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

    #[cfg(any(feature = "deploy-fly", feature = "deploy-cloud-run", feature = "deploy-aws"))]
    #[test]
    fn a_target_says_whether_it_keeps_memory_and_the_store_lists_it_then() {
        let value = capabilities();
        for target in value["targets"].as_array().cloned().unwrap_or_default() {
            let kept = target["memory"]["kept"] == true;
            let listed = target["store"]["features"].as_array().is_some_and(|f| f.contains(&Value::from("memory")));
            assert_eq!(kept, listed, "{}", target["target"]);
            if kept {
                assert_eq!(target["memory"]["dir"], "/data/memory");
            } else {
                assert!(target["memory"]["refused"].is_string());
            }
        }
        assert_eq!(value["agent_file"]["limits"]["fields"], json!(["max_total_tokens", "max_cost_micros"]));
        assert_eq!(value["agent_file"]["limits"]["combine"], "lower");
    }
}
