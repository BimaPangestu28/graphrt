//! SharePoint document libraries and OneDrive folders over Microsoft Graph.
//! Files are listed folder by folder (`cTag` changes only when the content
//! changes, so renamed or re-shared files are not downloaded again) and
//! downloaded through `/content`. Needs the delegated read only scopes
//! `Files.Read.All` and `Sites.Read.All` (plus `offline_access`).

use std::collections::{HashSet, VecDeque};

use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::is_indexable;
use crate::error::{ConnectError, Result};
use crate::http::SourceHttp;
use crate::types::{Connector, ItemContent, Listing, RemoteItem};

pub const DEFAULT_BASE_URL: &str = "https://graph.microsoft.com/v1.0";
/// Delegated scopes a sign-in asks for (read only).
pub const SCOPES: &str = "offline_access Files.Read.All Sites.Read.All";
const SELECT: &str = "id,name,file,folder,eTag,cTag,lastModifiedDateTime,webUrl,size,deleted";
const MAX_FOLDERS: usize = 2_000;

fn default_base_url() -> String {
    DEFAULT_BASE_URL.into()
}

fn yes() -> bool {
    true
}

fn default_max_files() -> usize {
    5_000
}

/// Config of a `sharepoint` source: a site (its default library, or
/// `drive_id`), a drive by id, or the signed in user's OneDrive (`onedrive: true`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SharePointConfig {
    /// `https://contoso.sharepoint.com/sites/Marketing`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub site_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub drive_id: Option<String>,
    /// The signed in user's own OneDrive.
    #[serde(default)]
    pub onedrive: bool,
    /// Folder inside the library (`Policies/2026`); the whole library when empty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder_path: Option<String>,
    #[serde(default = "yes")]
    pub recursive: bool,
    #[serde(default = "default_max_files")]
    pub max_files: usize,
    /// Microsoft Graph address (tests point it at a mock server).
    #[serde(default = "default_base_url")]
    pub base_url: String,
}

/// The `sharepoint` connector.
#[derive(Debug)]
pub struct SharePointConnector {
    config: SharePointConfig,
    http: SourceHttp,
    /// `/drives/{id}`, resolved on the first listing.
    drive: tokio::sync::OnceCell<String>,
}

fn encode_path(path: &str) -> String {
    path.split('/')
        .filter(|segment| !segment.is_empty())
        .map(|segment| url::form_urlencoded::byte_serialize(segment.as_bytes()).collect::<String>().replace('+', "%20"))
        .collect::<Vec<_>>()
        .join("/")
}

impl SharePointConnector {
    /// @throws `ConnectError::Config` when no site, drive or OneDrive is chosen.
    pub fn new(config: SharePointConfig, http: SourceHttp) -> Result<Self> {
        if config.site_url.is_none() && config.drive_id.is_none() && !config.onedrive {
            return Err(ConnectError::Config("choose a SharePoint site, a library or OneDrive".into()));
        }
        if let Some(site) = &config.site_url {
            let url = url::Url::parse(site)
                .map_err(|_| ConnectError::Config(format!("`{site}` is not a SharePoint site address")))?;
            if url.scheme() != "https" || url.host_str().is_none() {
                return Err(ConnectError::Config("SharePoint site addresses start with https://".into()));
            }
        }
        Ok(Self { config, http, drive: tokio::sync::OnceCell::new() })
    }

    fn base(&self) -> &str {
        self.config.base_url.trim_end_matches('/')
    }

    /// `{base}/drives/{id}` of the configured library.
    async fn drive(&self) -> Result<&str> {
        self.drive
            .get_or_try_init(|| async {
                if let Some(id) = &self.config.drive_id {
                    return Ok(format!("{}/drives/{}", self.base(), encode_path(id)));
                }
                let url = if self.config.onedrive {
                    format!("{}/me/drive?$select=id", self.base())
                } else {
                    let site = self.config.site_url.as_deref().unwrap_or_default();
                    let parsed = url::Url::parse(site)
                        .map_err(|_| ConnectError::Config(format!("`{site}` is not a SharePoint site address")))?;
                    let host = parsed.host_str().unwrap_or_default();
                    let path = parsed.path().trim_matches('/');
                    let site_ref =
                        if path.is_empty() { host.to_owned() } else { format!("{host}:/{}:", encode_path(path)) };
                    format!("{}/sites/{site_ref}/drive?$select=id", self.base())
                };
                let drive = self.http.get_json(&url).await?;
                let id = drive
                    .get("id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| ConnectError::Decode("Microsoft Graph returned a library without an id".into()))?;
                Ok(format!("{}/drives/{}", self.base(), encode_path(id)))
            })
            .await
            .map(String::as_str)
    }

    fn item(&self, entry: &Value) -> Option<RemoteItem> {
        if entry.get("deleted").is_some() || entry.get("file").is_none() {
            return None;
        }
        let name = entry.get("name")?.as_str()?.to_owned();
        let mime = entry.pointer("/file/mimeType").and_then(Value::as_str).map(str::to_owned);
        if !is_indexable(&name, mime.as_deref()) {
            return None;
        }
        let version = entry
            .get("cTag")
            .or_else(|| entry.get("eTag"))
            .and_then(Value::as_str)
            .map(|tag| format!("tag:{tag}"))
            .or_else(|| entry.get("lastModifiedDateTime").and_then(Value::as_str).map(|m| format!("modified:{m}")));
        Some(RemoteItem {
            id: entry.get("id")?.as_str()?.to_owned(),
            title: name,
            url: entry.get("webUrl").and_then(Value::as_str).map(str::to_owned),
            version,
            mime_type: mime,
            content: None,
        })
    }
}

impl Connector for SharePointConnector {
    fn kind(&self) -> &'static str {
        "sharepoint"
    }

    fn list(&self) -> BoxFuture<'_, Result<Listing>> {
        Box::pin(async move {
            let drive = self.drive().await?.to_owned();
            let root =
                match self.config.folder_path.as_deref().map(str::trim).filter(|p| !p.trim_matches('/').is_empty()) {
                    Some(path) => format!("{drive}/root:/{}:", encode_path(path)),
                    None => format!("{drive}/root"),
                };
            let mut listing = Listing::complete(Vec::new());
            let mut folders = VecDeque::from([root]);
            let mut visited = HashSet::new();
            while let Some(folder) = folders.pop_front() {
                if !visited.insert(folder.clone()) {
                    continue;
                }
                if visited.len() > MAX_FOLDERS {
                    listing.complete = false;
                    listing.warnings.push(format!("read the first {MAX_FOLDERS} folders only"));
                    break;
                }
                let mut next = Some(format!("{folder}/children?$select={SELECT}&$top=200"));
                while let Some(url) = next.take() {
                    let page = match self.http.get_json(&url).await {
                        Ok(page) => page,
                        Err(error) if visited.len() == 1 => return Err(error),
                        Err(error) => {
                            listing.complete = false;
                            listing.warnings.push(format!("a sub folder could not be read: {error}"));
                            break;
                        }
                    };
                    for entry in page.get("value").and_then(Value::as_array).into_iter().flatten() {
                        if entry.get("folder").is_some() {
                            if self.config.recursive
                                && let Some(id) = entry.get("id").and_then(Value::as_str)
                            {
                                folders.push_back(format!("{drive}/items/{}", encode_path(id)));
                            }
                            continue;
                        }
                        if let Some(item) = self.item(entry) {
                            if listing.items.len() >= self.config.max_files {
                                listing.complete = false;
                                listing.warnings.push(format!("stopped at {} files", self.config.max_files));
                                return Ok(listing);
                            }
                            listing.items.push(item);
                        }
                    }
                    // `@odata.nextLink` is a full Graph address; it is checked like any other.
                    next = page.get("@odata.nextLink").and_then(Value::as_str).map(str::to_owned);
                }
            }
            Ok(listing)
        })
    }

    fn fetch<'a>(&'a self, item: &'a RemoteItem) -> BoxFuture<'a, Result<ItemContent>> {
        Box::pin(async move {
            let drive = self.drive().await?;
            let body = self.http.get_bytes(&format!("{drive}/items/{}/content", encode_path(&item.id)), &[]).await?;
            Ok(ItemContent::File {
                bytes: body.bytes,
                mime_type: item.mime_type.clone().or(body.mime_type),
                file_name: item.title.clone(),
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_are_encoded_per_segment() {
        assert_eq!(encode_path("/Shared Documents/Q&A/"), "Shared%20Documents/Q%26A");
    }
}
