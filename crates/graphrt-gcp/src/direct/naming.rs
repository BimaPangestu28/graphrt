//! Names of the resources of a direct deployment.

use crate::cloud_run::naming::version_tag;

/// Longest Cloud Run service name (Cloud Run refuses 50 and more).
const MAX_SERVICE_CHARS: usize = 49;
/// Longest Cloud Storage bucket name.
const MAX_BUCKET_CHARS: usize = 63;
/// Longest IAM service account id.
const MAX_ACCOUNT_CHARS: usize = 30;

/// The Cloud Run service of a deployment: lowercased, everything but
/// `a-z0-9` turned into `-`, starting with a letter, at most 49 characters
/// (`Support Bot` -> `support-bot`).
///
/// @throws DirectError(InvalidInput) - nothing usable is left of the name
pub fn service_name_for(deployment: &str) -> Result<String, super::DirectError> {
    let mut name = slug(deployment);
    if name.starts_with(|character: char| character.is_ascii_digit()) {
        name.insert_str(0, "agent-");
    }
    let name = shorten(&name, MAX_SERVICE_CHARS);
    if name.is_empty() {
        return Err(super::DirectError::invalid(format!(
            "--deployment {deployment:?} has no letters or digits to name a Cloud Run service with: pass --app"
        )));
    }
    Ok(name)
}

/// The bucket of a deployment: `<project>-<service>`, or a hashed name when
/// that is too long (bucket names are global, the project id makes them unique).
pub fn bucket_name_for(project: &str, service: &str) -> String {
    let name = format!("{project}-{service}");
    if name.len() <= MAX_BUCKET_CHARS && !name.contains("google") {
        return name;
    }
    format!("graphrt-{:016x}", fnv1a(name.as_bytes()))
}

/// The service account a deployment runs as: `graphrt-<service>`, shortened
/// with a hash when longer than 30 characters.
pub fn service_account_id_for(service: &str) -> String {
    let id = format!("graphrt-{service}");
    if id.len() <= MAX_ACCOUNT_CHARS {
        return id;
    }
    let hash = format!("{:08x}", fnv1a(service.as_bytes()) & 0xffff_ffff);
    let keep = MAX_ACCOUNT_CHARS - hash.len() - 1;
    let prefix: String = id.chars().take(keep).collect();
    format!("{}-{hash}", prefix.trim_end_matches('-'))
}

/// Secret Manager id of `env` for `service` (`OPENAI_API_KEY` -> `support-openai-api-key`).
pub(super) fn secret_id_for(service: &str, env: &str) -> String {
    format!("{service}-{}", env.to_ascii_lowercase().replace('_', "-"))
}

/// The traffic tag of a version label.
pub(super) fn tag_for(version: &str) -> String {
    version_tag(version)
}

/// A label value Google accepts (`[a-z0-9_-]`, at most 63 characters).
pub(super) fn label_value(value: &str) -> String {
    crate::cloud_run::naming::label_value(value)
}

/// Whether Cloud Run accepts `name` as a service name.
pub(super) fn is_service_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_SERVICE_CHARS
        && name.starts_with(|character: char| character.is_ascii_lowercase())
        && !name.ends_with('-')
        && name.bytes().all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

/// A label key: starts with a lowercase letter, then `[a-z0-9_-]`, at most 63.
pub(super) fn is_label_key(key: &str) -> bool {
    key.starts_with(|character: char| character.is_ascii_lowercase()) && is_label_value(key)
}

/// A label value: `[a-z0-9_-]`, at most 63 (may be empty).
pub(super) fn is_label_value(value: &str) -> bool {
    value.len() <= 63
        && value.bytes().all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"_-".contains(&byte))
}

/// Lowercase `[a-z0-9-]`, no leading, trailing or double dashes.
fn slug(text: &str) -> String {
    let mut slug = String::with_capacity(text.len());
    for character in text.chars().flat_map(char::to_lowercase) {
        let mapped = if character.is_ascii_lowercase() || character.is_ascii_digit() { character } else { '-' };
        if mapped == '-' && (slug.is_empty() || slug.ends_with('-')) {
            continue;
        }
        slug.push(mapped);
    }
    slug.trim_end_matches('-').to_owned()
}

fn shorten(value: &str, max: usize) -> String {
    if value.len() <= max {
        return value.to_owned();
    }
    let prefix: String = value.chars().take(max).collect();
    prefix.trim_end_matches('-').to_owned()
}

fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| (hash ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn services_are_dns_labels_of_the_deployment() {
        assert_eq!(service_name_for("Support Bot").unwrap(), "support-bot");
        assert_eq!(service_name_for("9lives").unwrap(), "agent-9lives");
        assert!(service_name_for("!!").is_err());
        let long = service_name_for(&"x".repeat(80)).unwrap();
        assert!(is_service_name(&long) && long.len() == 49, "{long}");
    }

    #[test]
    fn buckets_and_accounts_fit_their_limits() {
        assert_eq!(bucket_name_for("my-project", "support-bot"), "my-project-support-bot");
        let long = bucket_name_for("a-very-long-project-id-here", &"y".repeat(49));
        assert!(long.len() <= 63 && long.starts_with("graphrt-"), "{long}");
        assert!(!bucket_name_for("google-thing", "s").contains("google"));
        assert_eq!(service_account_id_for("support-bot"), "graphrt-support-bot");
        let account = service_account_id_for(&"z".repeat(49));
        assert!(account.len() <= 30 && account.starts_with("graphrt-z"), "{account}");
        assert_ne!(account, service_account_id_for(&"z".repeat(48)));
        assert_eq!(secret_id_for("support", "OPENAI_API_KEY"), "support-openai-api-key");
    }

    #[test]
    fn label_rules() {
        assert!(is_label_key("graphrt-test") && is_label_value(""));
        assert!(!is_label_key("1abc") && !is_label_key("Team"));
    }
}
