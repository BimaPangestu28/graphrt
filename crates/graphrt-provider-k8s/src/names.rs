//! Kubernetes object names and label selectors of deployment versions.
//!
//! Every version is its own Deployment + Service named
//! `<deployment>-<version>` (decision 6: versions are immutable and old ones
//! stay wakeable for 7 days), cut to a DNS-1123 label of at most 63 bytes with a
//! stable hash suffix when it is too long.

use crate::api::DeploymentRef;
use crate::resources::{DEPLOYMENT_LABEL, VERSION_LABEL};

const MAX_NAME: usize = 63;

/// Name of the Deployment and Service of one deployment version.
pub fn object_name(deployment: &DeploymentRef) -> String {
    let raw = format!("{}-{}", deployment.deployment, deployment.version);
    let mut name: String = raw
        .chars()
        .map(|char| if char.is_ascii_alphanumeric() { char.to_ascii_lowercase() } else { '-' })
        .collect();
    let changed = name != raw || name.len() > MAX_NAME;
    if changed {
        // Sanitising can map different inputs to one name: a hash of the raw input keeps them apart.
        let suffix = format!("-{:08x}", fnv1a(raw.as_bytes()));
        name.truncate(MAX_NAME - suffix.len());
        name = name.trim_end_matches('-').to_owned();
        name.push_str(&suffix);
    }
    let trimmed = name.trim_start_matches('-');
    if trimmed.is_empty() { format!("g{:08x}", fnv1a(raw.as_bytes())) } else { trimmed.to_owned() }
}

/// Whether `value` is a valid label value (deployment and version names must be).
pub fn is_label_value(value: &str) -> bool {
    let bytes = value.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= MAX_NAME
        && bytes.first().is_some_and(u8::is_ascii_alphanumeric)
        && bytes.last().is_some_and(u8::is_ascii_alphanumeric)
        && bytes.iter().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

/// `labelSelector` query value matching the pods of one version (URL-encoded).
pub fn pod_selector(deployment: &DeploymentRef) -> String {
    encode_query(&format!(
        "{DEPLOYMENT_LABEL}={},{VERSION_LABEL}={}",
        deployment.deployment, deployment.version
    ))
}

/// Percent-encode a query value (label selectors contain `/`, `=` and `,`).
pub fn encode_query(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len() * 3);
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(char::from(byte));
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

/// 32-bit FNV-1a: stable across builds and platforms (unlike `DefaultHasher`).
fn fnv1a(bytes: &[u8]) -> u32 {
    bytes.iter().fold(0x811c_9dc5_u32, |hash, byte| (hash ^ u32::from(*byte)).wrapping_mul(0x0100_0193))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simple_names_pass_through() {
        assert_eq!(object_name(&DeploymentRef::new("support-bot", "v3")), "support-bot-v3");
    }

    #[test]
    fn unsafe_or_long_names_are_sanitised_with_a_stable_hash() {
        let dotted = object_name(&DeploymentRef::new("Support.Bot", "1.2"));
        assert!(dotted.starts_with("support-bot-1-2-"), "{dotted}");
        assert_ne!(dotted, object_name(&DeploymentRef::new("support-bot", "1-2")));
        assert_eq!(dotted, object_name(&DeploymentRef::new("Support.Bot", "1.2")));
        let long = object_name(&DeploymentRef::new("a".repeat(80), "v1"));
        assert!(long.len() <= MAX_NAME, "{long}");
        let odd = object_name(&DeploymentRef::new("_", "_"));
        assert!(odd.chars().next().is_some_and(|char| char.is_ascii_alphanumeric()), "{odd}");
    }

    #[test]
    fn label_values_and_selectors() {
        assert!(is_label_value("support-bot") && is_label_value("v1.2_3"));
        assert!(!is_label_value("") && !is_label_value("-x") && !is_label_value("a/b") && !is_label_value(&"a".repeat(64)));
        assert_eq!(
            pod_selector(&DeploymentRef::new("bot", "v1")),
            "graphrt.dev%2Fdeployment%3Dbot%2Cgraphrt.dev%2Fversion%3Dv1"
        );
    }
}
