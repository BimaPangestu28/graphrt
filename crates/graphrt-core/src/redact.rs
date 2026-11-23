//! Scrubbing plaintext secrets out of free text.
//!
//! Typed values never leak: [`Value::Secret`] serializes and displays as
//! [`SECRET_MASK`]. Free text is different. A node that needs a secret's plaintext
//! (an HTTP call, a code sandbox, an LLM provider) can get it echoed back inside an
//! error message, for example a provider error body that repeats the API key. These
//! helpers replace every occurrence of known secret values with [`SECRET_MASK`].
//!
//! The engine keeps a [`SecretSet`] per run (environment secrets, re-supplied resume
//! secrets, secret-typed inputs and node outputs) and scrubs every node error message
//! with it before the error is emitted, retried, stored or turned into outputs.

use std::borrow::Cow;
use std::sync::{Arc, Mutex};

use crate::value::{SECRET_MASK, Value};

/// Secrets shorter than this are not scrubbed from text: masking every occurrence
/// of a one- to three-character value would mangle messages without protecting
/// anything an attacker could not guess.
pub const MIN_SCRUB_LEN: usize = 4;

/// Replace every occurrence of each of `secrets` in `text` with [`SECRET_MASK`].
///
/// Longer secrets are replaced first, so a secret that contains another one is
/// masked as a whole. Values shorter than [`MIN_SCRUB_LEN`] are ignored.
///
/// ```
/// use graphrt_core::redact::redact_secrets;
/// let message = redact_secrets("invalid key sk-live-1234 for org", &["sk-live-1234"]);
/// assert_eq!(message, "invalid key ****** for org");
/// ```
pub fn redact_secrets<'a, S: AsRef<str>>(text: &'a str, secrets: &[S]) -> Cow<'a, str> {
    let mut candidates: Vec<&str> =
        secrets.iter().map(AsRef::as_ref).filter(|s| s.len() >= MIN_SCRUB_LEN && text.contains(*s)).collect();
    if candidates.is_empty() {
        return Cow::Borrowed(text);
    }
    candidates.sort_unstable_by_key(|s| std::cmp::Reverse(s.len()));
    let mut scrubbed = text.to_owned();
    for secret in candidates {
        if scrubbed.contains(secret) {
            scrubbed = scrubbed.replace(secret, SECRET_MASK);
        }
    }
    Cow::Owned(scrubbed)
}

/// The plaintext of every secret a run has seen, for scrubbing free text.
///
/// Its `Debug` output shows only the number of values.
#[derive(Default)]
pub struct SecretSet {
    values: Mutex<Vec<Arc<str>>>,
}

impl std::fmt::Debug for SecretSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let count = self.values.lock().map(|v| v.len()).unwrap_or_default();
        f.debug_struct("SecretSet").field("values", &count).finish()
    }
}

impl SecretSet {
    /// An empty set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Remember the plaintext of `value` and of any secret nested in it.
    pub fn observe(&self, value: &Value) {
        match value {
            Value::Secret(plaintext) => self.insert(plaintext),
            Value::Array(items) => items.iter().for_each(|item| self.observe(item)),
            Value::Object(map) => map.values().for_each(|item| self.observe(item)),
            _ => {}
        }
    }

    /// Remember a plaintext secret that is not held in a typed [`Value::Secret`]
    /// (e.g. a value a node rendered for a tool call context).
    pub fn observe_plaintext(&self, plaintext: &str) {
        if plaintext.len() < MIN_SCRUB_LEN {
            return;
        }
        if let Ok(mut values) = self.values.lock()
            && !values.iter().any(|known| known.as_ref() == plaintext)
        {
            values.push(Arc::from(plaintext));
        }
    }

    fn insert(&self, plaintext: &Arc<str>) {
        if plaintext.len() < MIN_SCRUB_LEN {
            return;
        }
        if let Ok(mut values) = self.values.lock()
            && !values.iter().any(|known| known == plaintext)
        {
            values.push(Arc::clone(plaintext));
        }
    }

    /// Whether no secret has been observed.
    pub fn is_empty(&self) -> bool {
        self.values.lock().map(|v| v.is_empty()).unwrap_or(true)
    }

    /// `text` with every observed secret replaced by [`SECRET_MASK`].
    pub fn scrub<'a>(&self, text: &'a str) -> Cow<'a, str> {
        match self.values.lock() {
            Ok(values) => redact_secrets(text, &values),
            // A poisoned lock means a panic while inserting; mask conservatively.
            Err(_) => Cow::Borrowed(SECRET_MASK),
        }
    }

    /// Scrub `message` in place.
    pub fn scrub_in_place(&self, message: &mut String) {
        if let Cow::Owned(scrubbed) = self.scrub(message) {
            *message = scrubbed;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replaces_every_occurrence_longest_first() {
        let text = "key=sk-abcdef, again sk-abcdef, prefix sk-abc";
        assert_eq!(redact_secrets(text, &["sk-abc", "sk-abcdef"]), "key=******, again ******, prefix ******");
    }

    #[test]
    fn short_values_and_misses_borrow() {
        assert!(matches!(redact_secrets("abc and xyz", &["abc"]), Cow::Borrowed(_)));
        assert!(matches!(redact_secrets("nothing here", &["sk-live"]), Cow::Borrowed(_)));
    }

    #[test]
    fn set_observes_nested_secrets_and_hides_them_in_debug() {
        let set = SecretSet::new();
        set.observe(&Value::from_json(serde_json::json!({"a": [1, 2]})));
        assert!(set.is_empty());
        set.observe(&Value::array([Value::Secret(Arc::from("tok-1234")), Value::Secret(Arc::from("no"))]));
        set.observe(&Value::Secret(Arc::from("tok-1234")));
        assert_eq!(set.scrub("Bearer tok-1234 rejected; no"), "Bearer ****** rejected; no");
        assert!(!format!("{set:?}").contains("tok-1234"));
        set.observe_plaintext("eyJ-user-jwt");
        set.observe_plaintext("eyJ-user-jwt");
        set.observe_plaintext("ab");
        assert_eq!(set.scrub("jwt eyJ-user-jwt; ab"), "jwt ******; ab");
    }
}
