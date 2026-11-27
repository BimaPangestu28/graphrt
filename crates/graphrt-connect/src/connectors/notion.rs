//! Notion: chosen pages, the rows of chosen databases, or every page the
//! integration was given access to. Blocks become Markdown (headings, lists,
//! to-dos, quotes, code, tables), nested blocks included.
//!
//! Works with an internal integration token (`auth: { type: bearer }`, pages
//! shared with the integration) or a public integration's OAuth token.

use futures::future::BoxFuture;
use reqwest::Method;
use reqwest::header::{HeaderName, HeaderValue};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::error::{ConnectError, Result};
use crate::http::SourceHttp;
use crate::types::{Connector, ItemContent, Listing, RemoteItem};

pub const DEFAULT_BASE_URL: &str = "https://api.notion.com/v1";
/// The Notion API version the connector speaks.
pub const NOTION_VERSION: &str = "2022-06-28";
/// Nested block levels read below a page.
const MAX_BLOCK_DEPTH: usize = 4;
/// Blocks read per page at most.
const MAX_BLOCKS: usize = 5_000;

fn default_base_url() -> String {
    DEFAULT_BASE_URL.into()
}

fn default_max_pages() -> usize {
    2_000
}

/// Config of a `notion` source. Without `page_ids` and `database_ids`, every
/// page shared with the integration is indexed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NotionConfig {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub page_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub database_ids: Vec<String>,
    #[serde(default = "default_max_pages")]
    pub max_pages: usize,
    /// Notion API address (tests point it at a mock server).
    #[serde(default = "default_base_url")]
    pub base_url: String,
}

/// The `notion` connector.
#[derive(Debug)]
pub struct NotionConnector {
    config: NotionConfig,
    http: SourceHttp,
    headers: Vec<(HeaderName, HeaderValue)>,
}

/// A Notion id from an id or a page link (`https://www.notion.so/Title-<32 hex>`).
pub fn notion_id(text: &str) -> Option<String> {
    let text = text.trim();
    let tail = text.split(['?', '#']).next().unwrap_or(text);
    let last = tail.rsplit('/').next().unwrap_or(tail);
    let hex: String = last.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    let candidate = hex.get(hex.len().checked_sub(32)?..)?;
    let compact_ok = last.replace('-', "").ends_with(candidate);
    compact_ok.then(|| candidate.to_ascii_lowercase())
}

/// Plain text of a `rich_text` array.
fn rich_text(value: Option<&Value>) -> String {
    value
        .and_then(Value::as_array)
        .map(|parts| parts.iter().filter_map(|part| part.get("plain_text").and_then(Value::as_str)).collect::<String>())
        .unwrap_or_default()
}

/// The title of a page: its `title` property.
fn page_title(page: &Value) -> String {
    page.get("properties")
        .and_then(Value::as_object)
        .and_then(|props| props.values().find(|p| p.get("type").and_then(Value::as_str) == Some("title")))
        .map(|p| rich_text(p.get("title")))
        .filter(|t| !t.trim().is_empty())
        .unwrap_or_else(|| "Untitled".to_owned())
}

/// The other properties of a database row as `Name: value` lines.
fn property_lines(page: &Value) -> Vec<String> {
    let Some(props) = page.get("properties").and_then(Value::as_object) else { return Vec::new() };
    let mut lines = Vec::new();
    for (name, prop) in props {
        let kind = prop.get("type").and_then(Value::as_str).unwrap_or_default();
        let value = prop.get(kind);
        let text = match kind {
            "title" => continue,
            "rich_text" => rich_text(value),
            "number" => value.filter(|v| !v.is_null()).map(Value::to_string).unwrap_or_default(),
            "select" | "status" => {
                value.and_then(|v| v.get("name")).and_then(Value::as_str).unwrap_or_default().to_owned()
            }
            "multi_select" => value
                .and_then(Value::as_array)
                .map(|o| o.iter().filter_map(|x| x.get("name").and_then(Value::as_str)).collect::<Vec<_>>().join(", "))
                .unwrap_or_default(),
            "date" => value.and_then(|v| v.get("start")).and_then(Value::as_str).unwrap_or_default().to_owned(),
            "checkbox" => {
                value.and_then(Value::as_bool).map(|b| if b { "yes" } else { "no" }.to_owned()).unwrap_or_default()
            }
            "url" | "email" | "phone_number" => value.and_then(Value::as_str).unwrap_or_default().to_owned(),
            _ => String::new(),
        };
        if !text.trim().is_empty() {
            lines.push(format!("{name}: {text}"));
        }
    }
    lines
}

/// One block as Markdown (without its children).
fn block_markdown(block: &Value, numbered: &mut usize) -> Option<String> {
    let kind = block.get("type")?.as_str()?;
    let body = block.get(kind);
    let text = || rich_text(body.and_then(|b| b.get("rich_text")));
    if kind != "numbered_list_item" {
        *numbered = 0;
    }
    let line = match kind {
        "paragraph" => text(),
        "heading_1" => format!("# {}", text()),
        "heading_2" => format!("## {}", text()),
        "heading_3" => format!("### {}", text()),
        "bulleted_list_item" => format!("- {}", text()),
        "numbered_list_item" => {
            *numbered += 1;
            format!("{numbered}. {}", text())
        }
        "to_do" => {
            let done = body.and_then(|b| b.get("checked")).and_then(Value::as_bool) == Some(true);
            format!("- [{}] {}", if done { "x" } else { " " }, text())
        }
        "toggle" => text(),
        "quote" | "callout" => format!("> {}", text()),
        "code" => format!("```\n{}\n```", text()),
        "equation" => body.and_then(|b| b.get("expression")).and_then(Value::as_str).unwrap_or_default().to_owned(),
        "table_row" => {
            let cells = body.and_then(|b| b.get("cells")).and_then(Value::as_array)?;
            format!("| {} |", cells.iter().map(|c| rich_text(Some(c))).collect::<Vec<_>>().join(" | "))
        }
        "child_page" | "child_database" => {
            format!("## {}", body.and_then(|b| b.get("title")).and_then(Value::as_str).unwrap_or_default())
        }
        "bookmark" | "embed" | "link_preview" => body.and_then(|b| b.get("url")).and_then(Value::as_str)?.to_owned(),
        "divider" => "---".to_owned(),
        _ => return None,
    };
    (!line.trim().is_empty()).then_some(line)
}

impl NotionConnector {
    /// @throws `ConnectError::Config` for an id or link that is not a Notion id.
    pub fn new(mut config: NotionConfig, http: SourceHttp) -> Result<Self> {
        let normalize = |ids: &mut Vec<String>| -> Result<()> {
            for id in ids.iter_mut() {
                *id = notion_id(id)
                    .ok_or_else(|| ConnectError::Config(format!("`{id}` is not a Notion page or database link")))?;
            }
            Ok(())
        };
        normalize(&mut config.page_ids)?;
        normalize(&mut config.database_ids)?;
        let headers = vec![(HeaderName::from_static("notion-version"), HeaderValue::from_static(NOTION_VERSION))];
        Ok(Self { config, http, headers })
    }

    fn base(&self) -> &str {
        self.config.base_url.trim_end_matches('/')
    }

    fn item(&self, page: &Value) -> Option<RemoteItem> {
        if page.get("archived").and_then(Value::as_bool) == Some(true)
            || page.get("in_trash").and_then(Value::as_bool) == Some(true)
        {
            return None;
        }
        Some(RemoteItem {
            id: page.get("id")?.as_str()?.replace('-', ""),
            title: page_title(page),
            url: page.get("url").and_then(Value::as_str).map(str::to_owned),
            version: page.get("last_edited_time").and_then(Value::as_str).map(|t| format!("edited:{t}")),
            mime_type: None,
            content: None,
        })
    }

    /// Every result of a paginated `POST` (search, database query).
    async fn collect(&self, url: &str, body: Value, listing: &mut Listing) -> Result<()> {
        let mut cursor: Option<String> = None;
        loop {
            let mut request = body.clone();
            if let (Some(cursor), Some(map)) = (&cursor, request.as_object_mut()) {
                map.insert("start_cursor".into(), Value::String(cursor.clone()));
            }
            let page = self.http.json(Method::POST, url, Some(&request), &self.headers).await?;
            for result in page.get("results").and_then(Value::as_array).into_iter().flatten() {
                if result.get("object").and_then(Value::as_str) != Some("page") {
                    continue;
                }
                if let Some(item) = self.item(result) {
                    if listing.items.len() >= self.config.max_pages {
                        listing.complete = false;
                        listing.warnings.push(format!("stopped at {} pages", self.config.max_pages));
                        return Ok(());
                    }
                    if !listing.items.iter().any(|known| known.id == item.id) {
                        listing.items.push(item);
                    }
                }
            }
            cursor = page.get("next_cursor").and_then(Value::as_str).map(str::to_owned);
            if page.get("has_more").and_then(Value::as_bool) != Some(true) || cursor.is_none() {
                return Ok(());
            }
        }
    }

    /// The Markdown of a page's blocks, children included up to [`MAX_BLOCK_DEPTH`].
    async fn blocks(&self, page_id: &str) -> Result<String> {
        let mut budget = MAX_BLOCKS;
        let lines = self.block_tree(page_id.to_owned(), 0, &mut budget).await?;
        Ok(lines.join("\n\n"))
    }

    /// The children of one block as Markdown lines, each followed by its own children.
    fn block_tree<'a>(
        &'a self,
        block_id: String,
        depth: usize,
        budget: &'a mut usize,
    ) -> BoxFuture<'a, Result<Vec<String>>> {
        Box::pin(async move {
            let mut children = Vec::new();
            let mut cursor: Option<String> = None;
            loop {
                let mut url = format!("{}/blocks/{block_id}/children?page_size=100", self.base());
                if let Some(cursor) = &cursor {
                    url.push_str("&start_cursor=");
                    url.push_str(cursor);
                }
                let page = self.http.json(Method::GET, &url, None, &self.headers).await?;
                children.extend(page.get("results").and_then(Value::as_array).into_iter().flatten().cloned());
                cursor = page.get("next_cursor").and_then(Value::as_str).map(str::to_owned);
                if page.get("has_more").and_then(Value::as_bool) != Some(true) || cursor.is_none() {
                    break;
                }
            }
            let mut lines = Vec::new();
            let mut numbered = 0usize;
            for block in children {
                if *budget == 0 {
                    break;
                }
                *budget -= 1;
                if let Some(markdown) = block_markdown(&block, &mut numbered) {
                    lines.push(format!("{}{markdown}", "  ".repeat(depth)));
                }
                let has_children = block.get("has_children").and_then(Value::as_bool) == Some(true);
                let kind = block.get("type").and_then(Value::as_str).unwrap_or_default();
                // Child pages are pages of their own, listed separately.
                if has_children
                    && depth + 1 < MAX_BLOCK_DEPTH
                    && !matches!(kind, "child_page" | "child_database")
                    && let Some(id) = block.get("id").and_then(Value::as_str)
                {
                    lines.extend(self.block_tree(id.to_owned(), depth + 1, budget).await?);
                }
            }
            Ok(lines)
        })
    }
}

impl Connector for NotionConnector {
    fn kind(&self) -> &'static str {
        "notion"
    }

    fn list(&self) -> BoxFuture<'_, Result<Listing>> {
        Box::pin(async move {
            let mut listing = Listing::complete(Vec::new());
            for id in &self.config.page_ids {
                let page =
                    self.http.json(Method::GET, &format!("{}/pages/{id}", self.base()), None, &self.headers).await?;
                listing.items.extend(self.item(&page));
            }
            for id in &self.config.database_ids {
                let url = format!("{}/databases/{id}/query", self.base());
                self.collect(&url, json!({ "page_size": 100 }), &mut listing).await?;
            }
            if self.config.page_ids.is_empty() && self.config.database_ids.is_empty() {
                let url = format!("{}/search", self.base());
                let body = json!({ "page_size": 100, "filter": { "property": "object", "value": "page" } });
                self.collect(&url, body, &mut listing).await?;
            }
            Ok(listing)
        })
    }

    fn fetch<'a>(&'a self, item: &'a RemoteItem) -> BoxFuture<'a, Result<ItemContent>> {
        Box::pin(async move {
            let page =
                self.http.json(Method::GET, &format!("{}/pages/{}", self.base(), item.id), None, &self.headers).await?;
            let mut parts = vec![format!("# {}", page_title(&page))];
            let properties = property_lines(&page);
            if !properties.is_empty() {
                parts.push(properties.join("\n"));
            }
            let body = self.blocks(&item.id).await?;
            if !body.is_empty() {
                parts.push(body);
            }
            Ok(ItemContent::Text(parts.join("\n\n")))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_from_links() {
        let id = "1429989fe8ac4effbc8f57f56486db54";
        assert_eq!(notion_id(id).as_deref(), Some(id));
        assert_eq!(notion_id("14299 89f-e8ac-4eff-bc8f-57f56486db54"), None);
        assert_eq!(notion_id("1429989f-e8ac-4eff-bc8f-57f56486db54").as_deref(), Some(id));
        assert_eq!(notion_id(&format!("https://www.notion.so/acme/Price-list-{id}?pvs=4")).as_deref(), Some(id));
        assert_eq!(notion_id("https://www.notion.so/acme/Price-list"), None);
    }

    #[test]
    fn blocks_become_markdown() {
        let mut numbered = 0;
        let block = |kind: &str, text: &str| json!({ "type": kind, kind: { "rich_text": [{ "plain_text": text }] } });
        assert_eq!(block_markdown(&block("heading_2", "Prices"), &mut numbered).as_deref(), Some("## Prices"));
        assert_eq!(block_markdown(&block("numbered_list_item", "a"), &mut numbered).as_deref(), Some("1. a"));
        assert_eq!(block_markdown(&block("numbered_list_item", "b"), &mut numbered).as_deref(), Some("2. b"));
        assert_eq!(block_markdown(&block("paragraph", ""), &mut numbered), None);
        let todo = json!({ "type": "to_do", "to_do": { "checked": true, "rich_text": [{ "plain_text": "ship" }] } });
        assert_eq!(block_markdown(&todo, &mut numbered).as_deref(), Some("- [x] ship"));
    }

    #[test]
    fn rows_list_their_properties() {
        let row = json!({ "properties": {
            "Name": { "type": "title", "title": [{ "plain_text": "Latte" }] },
            "Price": { "type": "number", "number": 4.5 },
            "Tags": { "type": "multi_select", "multi_select": [{ "name": "hot" }, { "name": "milk" }] }
        }});
        assert_eq!(page_title(&row), "Latte");
        assert_eq!(property_lines(&row), vec!["Price: 4.5", "Tags: hot, milk"]);
    }
}
