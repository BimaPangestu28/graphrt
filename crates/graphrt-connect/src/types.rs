//! What a connector lists and fetches.

use futures::future::BoxFuture;

use crate::error::Result;

/// One document of a source, as listed by its connector.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RemoteItem {
    /// Stable id inside the source (a Drive file id, a page URL, an API record id).
    pub id: String,
    /// Shown with the passages found in it.
    pub title: String,
    /// Where a person can open it, when there is such an address.
    pub url: Option<String>,
    /// Changes whenever the content changes (ETag, modified time, revision).
    /// An item listed with the same version as last sync is not fetched again.
    /// `None` means the connector cannot tell, so the item is fetched and its
    /// content hash decides.
    pub version: Option<String>,
    /// Content type reported by the service, when the listing gives it.
    pub mime_type: Option<String>,
    /// Content already known from the listing (API records, Notion rows), so
    /// the sync does not have to call [`Connector::fetch`].
    pub content: Option<ItemContent>,
}

/// The content of an item.
#[derive(Clone, PartialEq, Eq)]
pub enum ItemContent {
    /// Text ready to index (Markdown or plain).
    Text(String),
    /// A file to extract text from (PDF, Word, HTML, ...).
    File {
        bytes: Vec<u8>,
        /// Content type reported by the service, when known.
        mime_type: Option<String>,
        /// File name used to detect the format (`report.pdf`).
        file_name: String,
    },
}

impl std::fmt::Debug for ItemContent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Text(text) => f.debug_struct("Text").field("chars", &text.chars().count()).finish(),
            Self::File { bytes, mime_type, file_name } => f
                .debug_struct("File")
                .field("bytes", &bytes.len())
                .field("mime_type", mime_type)
                .field("file_name", file_name)
                .finish(),
        }
    }
}

impl ItemContent {
    /// Bytes hashed to detect a change when the service gives no version.
    pub fn hash_input(&self) -> &[u8] {
        match self {
            Self::Text(text) => text.as_bytes(),
            Self::File { bytes, .. } => bytes,
        }
    }
}

/// Everything a connector found in its source.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Listing {
    pub items: Vec<RemoteItem>,
    /// `false` when the listing stopped early (a page or item limit was
    /// reached, or part of the source could not be read). Items missing from
    /// an incomplete listing are kept instead of being removed.
    pub complete: bool,
    /// Problems that did not stop the listing (a sub folder without access,
    /// pages robots.txt disallows), in words a person can act on.
    pub warnings: Vec<String>,
}

impl Listing {
    /// A complete listing of `items`.
    pub fn complete(items: Vec<RemoteItem>) -> Self {
        Self { items, complete: true, warnings: Vec::new() }
    }
}

/// A source of documents: a web site, an API, a Drive folder, a Notion
/// workspace, a SharePoint library.
///
/// Implementations do the service specific part only. Change detection,
/// deletion of vanished documents, locking and scheduling are done once by
/// [`crate::sync_source`] and [`crate::Scheduler`].
pub trait Connector: Send + Sync + std::fmt::Debug {
    /// The connector type, as written in configs (`web`, `google_drive`).
    fn kind(&self) -> &'static str;

    /// Every item of the source, with a version when the service gives one.
    fn list(&self) -> BoxFuture<'_, Result<Listing>>;

    /// The content of one listed item (not called for items listed with content).
    fn fetch<'a>(&'a self, item: &'a RemoteItem) -> BoxFuture<'a, Result<ItemContent>>;
}
