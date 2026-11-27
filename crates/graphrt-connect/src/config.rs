//! One synced source, as written in the `sources:` list of a knowledge config.
//!
//! ```yaml
//! sources:
//!   - id: site
//!     name: Company web site
//!     dataset: handbook
//!     type: web
//!     mode: crawl
//!     urls: [https://example.com/]
//!     max_depth: 2
//!     schedule: { every: 6h }
//!   - id: notion
//!     dataset: handbook
//!     type: notion
//!     auth: { type: bearer, token_env: NOTION_TOKEN }
//!     schedule: { daily: "06:00", timezone: Asia/Jakarta }
//! ```

use serde::{Deserialize, Serialize};

use crate::auth::AuthConfig;
use crate::error::{ConnectError, Result};
use crate::schedule::SyncSchedule;

/// A source: which connector, with which settings and credential, into which
/// dataset, on which schedule. Fields other than the common ones below are
/// the connector's own settings (see the `*Config` types of each connector).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SourceConfig {
    /// Unique among the sources of one knowledge base: letters, digits, `-`, `_`.
    pub id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    /// The dataset synced documents go into.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub dataset: String,
    /// Connector type: `web`, `api`, `google_drive`, `notion`, `sharepoint`, or
    /// one registered by the host.
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub schedule: SyncSchedule,
    #[serde(default)]
    pub auth: AuthConfig,
    /// The connector's own settings.
    #[serde(flatten)]
    pub options: serde_json::Map<String, serde_json::Value>,
}

impl SourceConfig {
    /// A source of `kind` with the connector settings `options` (a JSON object).
    pub fn new(id: impl Into<String>, kind: impl Into<String>, options: serde_json::Value) -> Self {
        Self {
            id: id.into(),
            name: String::new(),
            dataset: String::new(),
            kind: kind.into(),
            schedule: SyncSchedule::Manual,
            auth: AuthConfig::None,
            options: match options {
                serde_json::Value::Object(map) => map,
                _ => serde_json::Map::new(),
            },
        }
    }

    pub fn dataset(mut self, dataset: impl Into<String>) -> Self {
        self.dataset = dataset.into();
        self
    }

    pub fn schedule(mut self, schedule: SyncSchedule) -> Self {
        self.schedule = schedule;
        self
    }

    pub fn auth(mut self, auth: AuthConfig) -> Self {
        self.auth = auth;
        self
    }

    /// The name shown to people (the id when no name is set).
    pub fn display_name(&self) -> &str {
        if self.name.is_empty() { &self.id } else { &self.name }
    }

    /// Check the id and the schedule (connector settings are checked when the
    /// connector is built).
    ///
    /// @throws `ConnectError::Config` naming the problem.
    pub fn validate(&self) -> Result<()> {
        let valid_id = !self.id.is_empty()
            && self.id.len() <= 64
            && self.id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        if !valid_id {
            return Err(ConnectError::Config(format!(
                "source id `{}`: use 1 to 64 letters, digits, `-` or `_`",
                self.id
            )));
        }
        self.schedule.validate()
    }

    /// Parse a list of sources (`sources:` of a knowledge config, as JSON).
    ///
    /// @throws `ConnectError::Config` for invalid entries or duplicate ids.
    pub fn parse_list(values: &[serde_json::Value]) -> Result<Vec<Self>> {
        let mut sources: Vec<Self> = Vec::with_capacity(values.len());
        for value in values {
            let source: Self = serde_json::from_value(value.clone())
                .map_err(|e| ConnectError::Config(format!("invalid source: {e}")))?;
            source.validate()?;
            if sources.iter().any(|known| known.id == source.id) {
                return Err(ConnectError::Config(format!("two sources have the id `{}`", source.id)));
            }
            sources.push(source);
        }
        Ok(sources)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_with_connector_options() {
        let sources = SourceConfig::parse_list(&[serde_json::json!({
            "id": "site", "dataset": "handbook", "type": "web", "urls": ["https://example.com/"],
            "mode": "crawl", "schedule": { "every": "6h" }
        })])
        .unwrap();
        let site = &sources[0];
        assert_eq!(site.kind, "web");
        assert_eq!(site.schedule, SyncSchedule::every_hours(6));
        assert_eq!(site.options.get("mode"), Some(&serde_json::json!("crawl")));
        assert!(!site.options.contains_key("id"), "common fields are not connector options");
    }

    #[test]
    fn rejects_bad_ids_and_duplicates() {
        let source = |id: &str| serde_json::json!({ "id": id, "type": "web", "urls": [] });
        assert!(SourceConfig::parse_list(&[source("bad id")]).is_err());
        assert!(SourceConfig::parse_list(&[source("a"), source("a")]).is_err());
    }
}
