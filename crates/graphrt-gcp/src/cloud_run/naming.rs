//! Names Cloud Run accepts: service ids, revision names, traffic tags, labels.
//!
//! Service ids and tags are DNS labels (`[a-z][a-z0-9-]*`, no trailing `-`). A
//! revision is `{service}-{tag}` and must stay ≤ 63 characters, so services are
//! capped at 40 and tags at 20; longer names keep a prefix plus an FNV-1a hash.

/// Longest service id graphrt creates.
const MAX_SERVICE_ID_CHARS: usize = 40;
/// Longest traffic tag graphrt creates.
const MAX_TAG_CHARS: usize = 20;
/// Shortest traffic tag Cloud Run accepts ("length must be between 3 and 47").
const MIN_TAG_CHARS: usize = 3;
/// Longest label value Cloud Run accepts.
#[cfg(feature = "deploy-cloud-run")]
const MAX_LABEL_CHARS: usize = 63;

/// Service id of a deployment: `{prefix}{deployment}` as a DNS label.
pub fn service_id(prefix: &str, deployment: &str) -> String {
    dns_label(&format!("{prefix}{deployment}"), MAX_SERVICE_ID_CHARS, 's')
}

/// Traffic tag of a version (`v12` → `v12`, `2026.11.22` → `v2026-11-22`).
///
/// Cloud Run refuses tags shorter than 3 characters, so a short one gets the
/// prefix `ver-` (`v7` → `ver-v7`, `7` → `ver-v7`).
pub fn version_tag(version: &str) -> String {
    let tag = dns_label(version, MAX_TAG_CHARS, 'v');
    if tag.len() < MIN_TAG_CHARS { format!("ver-{tag}") } else { tag }
}

#[cfg(feature = "deploy-cloud-run")]
/// Revision name of a version: `{service}-{tag}`.
pub fn revision_name(service_id: &str, tag: &str) -> String {
    format!("{service_id}-{tag}")
}

#[cfg(feature = "deploy-cloud-run")]
/// A label value (`[a-z0-9_-]`, ≤ 63 characters).
pub fn label_value(value: &str) -> String {
    let cleaned: String = value
        .chars()
        .map(|character| character.to_ascii_lowercase())
        .map(|character| if character.is_ascii_alphanumeric() || character == '_' { character } else { '-' })
        .collect();
    shorten(&cleaned, MAX_LABEL_CHARS)
}

/// Lowercase, invalid characters to `-`, collapsed, starting with a letter
/// (`lead` is prepended otherwise), at most `max` characters.
fn dns_label(raw: &str, max: usize, lead: char) -> String {
    let mut label = String::with_capacity(raw.len() + 1);
    for character in raw.chars().map(|character| character.to_ascii_lowercase()) {
        let mapped = if character.is_ascii_alphanumeric() { character } else { '-' };
        if !(mapped == '-' && (label.is_empty() || label.ends_with('-'))) {
            label.push(mapped);
        }
    }
    let mut label = label.trim_end_matches('-').to_owned();
    if !label.starts_with(|character: char| character.is_ascii_lowercase()) {
        label.insert(0, lead);
    }
    shorten(&label, max).trim_end_matches('-').to_owned()
}

/// Keeps `value` when it fits, else a prefix, `-` and 8 hex digits of its hash.
fn shorten(value: &str, max: usize) -> String {
    if value.len() <= max {
        return value.to_owned();
    }
    let hash = format!("{:08x}", fnv1a(value.as_bytes()) & 0xffff_ffff);
    let keep = max.saturating_sub(hash.len() + 1);
    let prefix: String = value.chars().take(keep).collect();
    format!("{}-{hash}", prefix.trim_end_matches('-'))
}

fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| (hash ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_ids_are_dns_labels() {
        assert_eq!(service_id("graphrt-", "chat"), "graphrt-chat");
        assert_eq!(service_id("graphrt-", "Support_Bot!!"), "graphrt-support-bot");
        assert_eq!(service_id("", "9lives"), "s9lives");
        let long = service_id("graphrt-", &"x".repeat(80));
        assert!(long.len() <= MAX_SERVICE_ID_CHARS && long.starts_with("graphrt-x"), "{long}");
        assert_ne!(long, service_id("graphrt-", &"x".repeat(81)));
    }

    #[cfg(feature = "deploy-cloud-run")]
    #[test]
    fn tags_and_revisions_fit_63_chars() {
        assert_eq!(version_tag("v12"), "v12");
        assert_eq!(version_tag("v7"), "ver-v7", "Cloud Run needs 3 characters or more");
        assert_eq!(version_tag("7"), "ver-v7");
        assert_eq!(version_tag("2026.11.22"), "v2026-11-22");
        let tag = version_tag("sha256:0123456789abcdef0123456789abcdef");
        assert!(tag.len() <= MAX_TAG_CHARS, "{tag}");
        let revision = revision_name(&service_id("graphrt-", &"y".repeat(90)), &tag);
        assert!(revision.len() <= 63, "{revision}");
    }

    #[cfg(feature = "deploy-cloud-run")]
    #[test]
    fn label_values_are_clean() {
        assert_eq!(label_value("Chat Bot/v1"), "chat-bot-v1");
        assert!(label_value(&"z".repeat(100)).len() <= 63);
    }
}
