//! Typed access to `DeploymentSpec::provider_settings`.

use std::collections::BTreeMap;

use serde_json::Value;

use crate::api::DeployError;

/// Read-only view of the provider settings of one spec.
#[derive(Clone, Copy, Debug)]
pub struct Settings<'a>(pub &'a BTreeMap<String, Value>);

impl<'a> Settings<'a> {
    /// A string setting.
    ///
    /// @throws `DeployError::invalid` when the value is not a string.
    pub fn string(&self, key: &str) -> Result<Option<&'a str>, DeployError> {
        match self.0.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(s)) => Ok(Some(s.as_str())),
            Some(_) => Err(DeployError::invalid(format!("provider setting {key} must be a string"))),
        }
    }

    /// A required string setting.
    ///
    /// @throws `DeployError::invalid` when missing or not a string.
    pub fn required(&self, key: &str) -> Result<&'a str, DeployError> {
        self.string(key)?
            .filter(|s| !s.is_empty())
            .ok_or_else(|| DeployError::invalid(format!("provider setting {key} is required")))
    }

    /// An unsigned integer setting.
    ///
    /// @throws `DeployError::invalid` when the value is not a non-negative integer.
    pub fn u64(&self, key: &str) -> Result<Option<u64>, DeployError> {
        match self.0.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(value) => value
                .as_u64()
                .map(Some)
                .ok_or_else(|| DeployError::invalid(format!("provider setting {key} must be a non-negative integer"))),
        }
    }

    /// A boolean setting.
    ///
    /// @throws `DeployError::invalid` when the value is not a boolean.
    pub fn bool(&self, key: &str) -> Result<Option<bool>, DeployError> {
        match self.0.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::Bool(b)) => Ok(Some(*b)),
            Some(_) => Err(DeployError::invalid(format!("provider setting {key} must be a boolean"))),
        }
    }

    /// A list of strings.
    ///
    /// @throws `DeployError::invalid` when the value is not a list of strings.
    pub fn strings(&self, key: &str) -> Result<Vec<&'a str>, DeployError> {
        match self.0.get(key) {
            None | Some(Value::Null) => Ok(Vec::new()),
            Some(Value::Array(items)) => items
                .iter()
                .map(|v| {
                    v.as_str().ok_or_else(|| DeployError::invalid(format!("provider setting {key} must list strings")))
                })
                .collect(),
            Some(_) => Err(DeployError::invalid(format!("provider setting {key} must be a list of strings"))),
        }
    }
}
