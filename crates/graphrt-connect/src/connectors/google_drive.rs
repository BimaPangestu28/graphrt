//! Google Drive: the files of a folder (and its sub folders), or chosen files.
//! Google Docs are exported as Markdown, Sheets as CSV, Slides as text; other
//! files are downloaded when their format can be indexed (PDF, Office, text).
//! Needs the read only scope `https://www.googleapis.com/auth/drive.readonly`.

use std::collections::{HashSet, VecDeque};

use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::is_indexable;
use crate::error::{ConnectError, Result};
use crate::http::SourceHttp;
use crate::types::{Connector, ItemContent, Listing, RemoteItem};

/// The OAuth scope the connector needs (read only).
pub const SCOPE: &str = "https://www.googleapis.com/auth/drive.readonly";
pub const DEFAULT_BASE_URL: &str = "https://www.googleapis.com/drive/v3";
const FOLDER: &str = "application/vnd.google-apps.folder";
const FIELDS: &str = "id,name,mimeType,modifiedTime,md5Checksum,version,webViewLink,size,trashed";
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

/// Config of a `google_drive` source.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoogleDriveConfig {
    /// Folder to index (`root` is My Drive). Either this or `file_ids`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder_id: Option<String>,
    /// Single files to index.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub file_ids: Vec<String>,
    /// Include sub folders (default `true`).
    #[serde(default = "yes")]
    pub recursive: bool,
    #[serde(default = "default_max_files")]
    pub max_files: usize,
    /// Drive API address (tests point it at a mock server).
    #[serde(default = "default_base_url")]
    pub base_url: String,
}

/// The `google_drive` connector.
#[derive(Debug)]
pub struct GoogleDriveConnector {
    config: GoogleDriveConfig,
    http: SourceHttp,
}

/// How a Google file type is exported (`None`: not a Google type).
fn export_format(mime: &str) -> Option<(&'static str, &'static str)> {
    match mime {
        "application/vnd.google-apps.document" => Some(("text/markdown", "md")),
        "application/vnd.google-apps.spreadsheet" => Some(("text/csv", "csv")),
        "application/vnd.google-apps.presentation" => Some(("text/plain", "txt")),
        _ => None,
    }
}

/// Percent-encode a value for a query string.
fn encode(value: &str) -> String {
    url::form_urlencoded::byte_serialize(value.as_bytes()).collect()
}

impl GoogleDriveConnector {
    /// @throws `ConnectError::Config` when neither a folder nor files are given.
    pub fn new(config: GoogleDriveConfig, http: SourceHttp) -> Result<Self> {
        if config.folder_id.as_deref().is_none_or(str::is_empty) && config.file_ids.is_empty() {
            return Err(ConnectError::Config("choose a Google Drive folder or files".into()));
        }
        let ids = config.folder_id.iter().chain(config.file_ids.iter());
        if ids.clone().any(|id| id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'))
        {
            return Err(ConnectError::Config("a Google Drive id has unexpected characters".into()));
        }
        Ok(Self { config, http })
    }

    fn base(&self) -> &str {
        self.config.base_url.trim_end_matches('/')
    }

    fn item(&self, file: &Value) -> Option<RemoteItem> {
        let id = file.get("id")?.as_str()?.to_owned();
        let name = file.get("name").and_then(Value::as_str).unwrap_or(&id).to_owned();
        let mime = file.get("mimeType").and_then(Value::as_str).unwrap_or_default();
        if file.get("trashed").and_then(Value::as_bool) == Some(true) {
            return None;
        }
        if export_format(mime).is_none() && !is_indexable(&name, Some(mime)) {
            return None;
        }
        let version = file
            .get("version")
            .and_then(Value::as_str)
            .map(|v| format!("v:{v}"))
            .or_else(|| file.get("md5Checksum").and_then(Value::as_str).map(|m| format!("md5:{m}")))
            .or_else(|| file.get("modifiedTime").and_then(Value::as_str).map(|m| format!("modified:{m}")));
        Some(RemoteItem {
            id,
            title: name,
            url: file.get("webViewLink").and_then(Value::as_str).map(str::to_owned),
            version,
            mime_type: Some(mime.to_owned()).filter(|m| !m.is_empty()),
            content: None,
        })
    }

    async fn folder_page(&self, folder: &str, page_token: Option<&str>) -> Result<Value> {
        let query = format!("'{folder}' in parents and trashed = false");
        let mut url = format!(
            "{}/files?q={}&fields={}&pageSize=1000&supportsAllDrives=true&includeItemsFromAllDrives=true",
            self.base(),
            encode(&query),
            encode(&format!("nextPageToken,files({FIELDS})")),
        );
        if let Some(token) = page_token {
            url.push_str("&pageToken=");
            url.push_str(&encode(token));
        }
        self.http.get_json(&url).await
    }
}

fn mime_of(item: &RemoteItem) -> &str {
    item.mime_type.as_deref().unwrap_or_default()
}

impl Connector for GoogleDriveConnector {
    fn kind(&self) -> &'static str {
        "google_drive"
    }

    fn list(&self) -> BoxFuture<'_, Result<Listing>> {
        Box::pin(async move {
            let mut listing = Listing::complete(Vec::new());
            for id in &self.config.file_ids {
                let url = format!("{}/files/{id}?fields={}&supportsAllDrives=true", self.base(), encode(FIELDS));
                let file = self.http.get_json(&url).await?;
                match self.item(&file) {
                    Some(item) => listing.items.push(item),
                    None => listing.warnings.push(format!(
                        "{} is a format that cannot be indexed",
                        file.get("name").and_then(Value::as_str).unwrap_or(id)
                    )),
                }
            }
            let Some(root) = self.config.folder_id.clone() else { return Ok(listing) };
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
                let mut page_token: Option<String> = None;
                loop {
                    let page = match self.folder_page(&folder, page_token.as_deref()).await {
                        Ok(page) => page,
                        // The top folder must be readable; a sub folder without access is a warning.
                        Err(error) if visited.len() == 1 => return Err(error),
                        Err(error) => {
                            listing.complete = false;
                            listing.warnings.push(format!("a sub folder could not be read: {error}"));
                            break;
                        }
                    };
                    for file in page.get("files").and_then(Value::as_array).into_iter().flatten() {
                        let mime = file.get("mimeType").and_then(Value::as_str).unwrap_or_default();
                        if mime == FOLDER {
                            if self.config.recursive
                                && let Some(id) = file.get("id").and_then(Value::as_str)
                            {
                                folders.push_back(id.to_owned());
                            }
                            continue;
                        }
                        if let Some(item) = self.item(file) {
                            if listing.items.len() >= self.config.max_files {
                                listing.complete = false;
                                listing.warnings.push(format!("stopped at {} files", self.config.max_files));
                                return Ok(listing);
                            }
                            listing.items.push(item);
                        }
                    }
                    page_token = page.get("nextPageToken").and_then(Value::as_str).map(str::to_owned);
                    if page_token.is_none() {
                        break;
                    }
                }
            }
            Ok(listing)
        })
    }

    fn fetch<'a>(&'a self, item: &'a RemoteItem) -> BoxFuture<'a, Result<ItemContent>> {
        Box::pin(async move {
            let mime = mime_of(item);
            if let Some((export, extension)) = export_format(mime) {
                let url = format!("{}/files/{}/export?mimeType={}", self.base(), item.id, encode(export));
                let body = match self.http.get_bytes(&url, &[]).await {
                    // Older Drive deployments cannot export Markdown: fall back to plain text.
                    Err(ConnectError::Http { status: 400, .. }) if export == "text/markdown" => {
                        let url = format!("{}/files/{}/export?mimeType=text%2Fplain", self.base(), item.id);
                        self.http.get_bytes(&url, &[]).await?
                    }
                    other => other?,
                };
                return Ok(ItemContent::File {
                    bytes: body.bytes,
                    mime_type: Some(export.to_owned()),
                    file_name: format!("{}.{extension}", item.title),
                });
            }
            let url = format!("{}/files/{}?alt=media&supportsAllDrives=true", self.base(), item.id);
            let body = self.http.get_bytes(&url, &[]).await?;
            Ok(ItemContent::File {
                bytes: body.bytes,
                mime_type: Some(mime.to_owned()).filter(|m| !m.is_empty()).or(body.mime_type),
                file_name: item.title.clone(),
            })
        })
    }
}
