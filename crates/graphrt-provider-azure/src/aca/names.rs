//! Naming rules shared by the run-time provider and the deploy driver:
//! container app names, version labels (revision suffixes) and grace bounds.

/// Largest `terminationGracePeriodSeconds` Container Apps accepts.
pub const MAX_GRACE_SECS: u64 = 600;
/// Default grace (plan §5: set to 120 s by `graphrt deploy`).
pub const DEFAULT_GRACE_SECS: u64 = 120;
/// Longest version label (revision names are `<app>--<label>`).
pub(crate) const MAX_LABEL_LEN: usize = 30;

/// The revision suffix / traffic label of a version: lowercase letters,
/// digits and `-`, starting with a letter (`7` → `v7`, `v7` → `v7`), at most
/// 30 characters. `None` when the version cannot be one.
pub fn version_label(version: &str) -> Option<String> {
    let lowered = version.to_ascii_lowercase();
    let label = if lowered.starts_with(|c: char| c.is_ascii_digit()) { format!("v{lowered}") } else { lowered };
    let valid = (1..=MAX_LABEL_LEN).contains(&label.len())
        && label.starts_with(|c: char| c.is_ascii_lowercase())
        && !label.ends_with('-')
        && !label.contains("--")
        && label.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    valid.then_some(label)
}

pub fn revision_name(app_name: &str, label: &str) -> String {
    format!("{app_name}--{label}")
}

pub(crate) fn is_app_name(name: &str) -> bool {
    (2..=32).contains(&name.len())
        && name.starts_with(|c: char| c.is_ascii_lowercase())
        && !name.ends_with('-')
        && !name.contains("--")
        && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}
