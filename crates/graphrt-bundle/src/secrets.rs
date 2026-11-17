//! Guard against secrets travelling inside bundles.

use crate::error::BundleError;

/// Keys whose literal string values are credentials.
const SECRET_KEYS: &[&str] = &[
    "api_key",
    "apikey",
    "api-key",
    "x-api-key",
    "secret",
    "secret_key",
    "client_secret",
    "token",
    "access_token",
    "bearer_token",
    "session_token",
    "password",
    "secret_access_key",
    "authorization",
    "private_key",
];

/// Fail when `value` contains an inline credential: a non-empty string under a
/// secret-looking key (`api_key`, `token`, `password`, an `Authorization` header...)
/// that is not an environment reference (`${VAR}`). Keys ending in `_env` name a
/// variable and are allowed. `what` names the file in the error message.
pub fn check_no_inline_secrets(value: &serde_json::Value, what: &str) -> Result<(), BundleError> {
    let mut path = Vec::new();
    walk(value, &mut path, what)
}

fn walk(value: &serde_json::Value, path: &mut Vec<String>, what: &str) -> Result<(), BundleError> {
    match value {
        serde_json::Value::Object(map) => {
            for (key, child) in map {
                if is_secret_key(key) {
                    if let serde_json::Value::String(s) = child {
                        if !s.trim().is_empty() && !is_env_reference(s) {
                            path.push(key.clone());
                            let at = path.join(".");
                            return Err(BundleError::Secret(format!(
                                "{what}: `{at}` holds an inline secret; bundles never carry credentials — \
                                 reference an environment variable (`{key}_env` or `${{VAR}}`) instead"
                            )));
                        }
                    }
                }
                path.push(key.clone());
                walk(child, path, what)?;
                path.pop();
            }
            Ok(())
        }
        serde_json::Value::Array(items) => {
            for (i, child) in items.iter().enumerate() {
                path.push(i.to_string());
                walk(child, path, what)?;
                path.pop();
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

fn is_secret_key(key: &str) -> bool {
    let k = key.to_ascii_lowercase();
    if k.ends_with("_env") || k.ends_with("_var") {
        return false;
    }
    SECRET_KEYS.contains(&k.as_str()) || k.ends_with("_secret") || k.ends_with("_password") || k.ends_with("_token")
}

/// `${VAR}` (optionally with surrounding text such as `Bearer ${TOKEN}`).
fn is_env_reference(value: &str) -> bool {
    value.contains("${") && value.contains('}')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_inline_secrets() {
        let ok = serde_json::json!({"providers": [{"name": "openai", "api_key_env": "OPENAI_API_KEY",
            "headers": {"Authorization": "Bearer ${TOKEN}"}}]});
        assert!(check_no_inline_secrets(&ok, "providers.yaml").is_ok());

        let bad = serde_json::json!({"providers": [{"name": "openai", "api_key": "sk-123"}]});
        let err = check_no_inline_secrets(&bad, "providers.yaml").unwrap_err().to_string();
        assert!(err.contains("providers.0.api_key"), "{err}");
        assert!(!err.contains("sk-123"));

        let header = serde_json::json!({"headers": {"Authorization": "Bearer abc"}});
        assert!(check_no_inline_secrets(&header, "x").is_err());
        let custom = serde_json::json!({"crm_token": "t"});
        assert!(check_no_inline_secrets(&custom, "x").is_err());
    }
}
