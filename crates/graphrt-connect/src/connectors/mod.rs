//! The built-in connectors and the registry that builds a connector from a
//! source config. Other crates add connectors with
//! [`ConnectorRegistry::register`], without touching this crate.

#[cfg(feature = "api")]
pub mod api;
#[cfg(feature = "google-drive")]
pub mod google_drive;
#[cfg(feature = "notion")]
pub mod notion;
#[cfg(feature = "sharepoint")]
pub mod sharepoint;
#[cfg(feature = "web")]
pub mod web;

use std::sync::Arc;

use indexmap::IndexMap;

use crate::auth::{Auth, OnRotate, SecretLookup};
use crate::config::SourceConfig;
use crate::error::{ConnectError, Result};
use crate::http::{Limits, SourceHttp, egress_policy};
use crate::types::Connector;

/// File extensions the knowledge pipeline can extract text from (graphrt-doc).
pub const INDEXABLE_EXTENSIONS: &[&str] = &[
    "pdf", "docx", "pptx", "xlsx", "ods", "csv", "tsv", "md", "markdown", "txt", "text", "html", "htm", "json", "yaml",
    "yml", "xml", "epub", "eml", "srt", "vtt", "rtf",
];

/// Whether a file can be indexed, by its extension or its content type.
pub fn is_indexable(name: &str, mime_type: Option<&str>) -> bool {
    let by_extension = name
        .rsplit_once('.')
        .is_some_and(|(_, extension)| INDEXABLE_EXTENSIONS.contains(&extension.to_ascii_lowercase().as_str()));
    let by_mime = mime_type.is_some_and(|mime| {
        mime.starts_with("text/")
            || matches!(
                mime,
                "application/pdf"
                    | "application/json"
                    | "application/epub+zip"
                    | "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
                    | "application/vnd.openxmlformats-officedocument.presentationml.presentation"
                    | "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
                    | "application/vnd.oasis.opendocument.spreadsheet"
            )
    });
    by_extension || by_mime
}

/// What a connector factory gets besides the source config.
#[derive(Clone)]
pub struct BuildContext<'a> {
    /// Resolves the secret names of `auth`.
    pub secrets: SecretLookup<'a>,
    pub limits: Limits,
    /// Stores refresh tokens the token endpoint rotates (OAuth sources).
    pub on_rotate: Option<OnRotate>,
    /// Let a Drive, Notion or SharePoint source whose `base_url` points
    /// elsewhere reach private addresses (a mock server in tests, a proxy on
    /// the local network). Off by default.
    pub private_service_endpoints: bool,
}

impl std::fmt::Debug for BuildContext<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BuildContext").field("limits", &self.limits).finish_non_exhaustive()
    }
}

impl<'a> BuildContext<'a> {
    pub fn new(secrets: SecretLookup<'a>) -> Self {
        Self { secrets, limits: Limits::default(), on_rotate: None, private_service_endpoints: false }
    }

    /// The source's credential, resolved.
    ///
    /// @throws `ConnectError::Auth` when a named secret is missing.
    pub fn auth(&self, source: &SourceConfig) -> Result<Auth> {
        let auth = source.auth.resolve(self.secrets)?;
        Ok(match (auth, &self.on_rotate) {
            (Auth::Oauth(tokens), Some(on_rotate)) => match Arc::try_unwrap(tokens) {
                Ok(tokens) => Auth::Oauth(Arc::new(tokens.on_rotate(on_rotate.clone()))),
                Err(shared) => Auth::Oauth(shared),
            },
            (auth, _) => auth,
        })
    }

    /// An HTTP client for the source, private addresses refused unless `allow_private`.
    ///
    /// @throws `ConnectError::Config` when the HTTP client cannot be built.
    pub fn http(&self, source: &SourceConfig, allow_private: bool, service: &'static str) -> Result<SourceHttp> {
        let client = graphrt_http::HttpClient::new(egress_policy(allow_private, &self.limits))
            .map_err(|e| ConnectError::Config(e.to_string()))?;
        Ok(SourceHttp::new(client, self.auth(source)?, self.limits.clone(), service))
    }
}

/// Builds a connector from a source config.
pub type ConnectorFactory = Arc<dyn Fn(&SourceConfig, &BuildContext<'_>) -> Result<Box<dyn Connector>> + Send + Sync>;

/// Connector types by name.
#[derive(Clone, Default)]
pub struct ConnectorRegistry {
    factories: IndexMap<String, ConnectorFactory>,
}

impl std::fmt::Debug for ConnectorRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConnectorRegistry").field("types", &self.factories.keys().collect::<Vec<_>>()).finish()
    }
}

/// Parse the connector specific part of a source config.
#[cfg(any(feature = "web", feature = "api", feature = "google-drive", feature = "notion", feature = "sharepoint"))]
fn options<T: serde::de::DeserializeOwned>(source: &SourceConfig) -> Result<T> {
    serde_json::from_value(serde_json::Value::Object(source.options.clone()))
        .map_err(|e| ConnectError::Config(format!("source `{}` ({}): {e}", source.id, source.kind)))
}

impl ConnectorRegistry {
    /// A registry without connectors.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Every connector compiled in (by cargo feature).
    pub fn with_builtins() -> Self {
        #[allow(unused_mut)]
        let mut registry = Self::default();
        #[cfg(feature = "web")]
        registry.register("web", |source, ctx| {
            let config: web::WebConfig = options(source)?;
            let http = ctx.http(source, config.allow_private, "the web site")?;
            Ok(Box::new(web::WebConnector::new(config, http)?))
        });
        #[cfg(feature = "api")]
        registry.register("api", |source, ctx| {
            let config: api::ApiConfig = options(source)?;
            let http = ctx.http(source, config.allow_private, "the API")?;
            Ok(Box::new(api::ApiConnector::new(config, http)?))
        });
        #[cfg(feature = "google-drive")]
        registry.register("google_drive", |source, ctx| {
            let config: google_drive::GoogleDriveConfig = options(source)?;
            let allow_private = config.base_url != google_drive::DEFAULT_BASE_URL && ctx.private_service_endpoints;
            let http = ctx.http(source, allow_private, "Google Drive")?;
            Ok(Box::new(google_drive::GoogleDriveConnector::new(config, http)?))
        });
        #[cfg(feature = "notion")]
        registry.register("notion", |source, ctx| {
            let config: notion::NotionConfig = options(source)?;
            let allow_private = config.base_url != notion::DEFAULT_BASE_URL && ctx.private_service_endpoints;
            let http = ctx.http(source, allow_private, "Notion")?;
            Ok(Box::new(notion::NotionConnector::new(config, http)?))
        });
        #[cfg(feature = "sharepoint")]
        registry.register("sharepoint", |source, ctx| {
            let config: sharepoint::SharePointConfig = options(source)?;
            let allow_private = config.base_url != sharepoint::DEFAULT_BASE_URL && ctx.private_service_endpoints;
            let http = ctx.http(source, allow_private, "SharePoint")?;
            Ok(Box::new(sharepoint::SharePointConnector::new(config, http)?))
        });
        registry
    }

    /// Add (or replace) a connector type.
    pub fn register<F>(&mut self, kind: impl Into<String>, factory: F) -> &mut Self
    where
        F: Fn(&SourceConfig, &BuildContext<'_>) -> Result<Box<dyn Connector>> + Send + Sync + 'static,
    {
        self.factories.insert(kind.into(), Arc::new(factory));
        self
    }

    /// Connector types this registry can build.
    pub fn kinds(&self) -> Vec<&str> {
        self.factories.keys().map(String::as_str).collect()
    }

    /// Build the connector of a source.
    ///
    /// @throws `ConnectError::Config` for an unknown type or an invalid config,
    /// `ConnectError::Auth` for a missing secret.
    pub fn build(&self, source: &SourceConfig, ctx: &BuildContext<'_>) -> Result<Box<dyn Connector>> {
        let factory = self.factories.get(&source.kind).ok_or_else(|| {
            ConnectError::Config(format!(
                "source `{}`: unknown type `{}` (available: {})",
                source.id,
                source.kind,
                self.kinds().join(", ")
            ))
        })?;
        factory(source, ctx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indexable_files() {
        assert!(is_indexable("Price list.PDF", None));
        assert!(is_indexable("notes", Some("text/plain")));
        assert!(!is_indexable("photo.jpg", Some("image/jpeg")));
        assert!(!is_indexable("archive.zip", None));
    }
}
