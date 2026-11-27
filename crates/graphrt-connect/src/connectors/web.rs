//! Web pages: one page, a crawl from start pages (bounded by depth, page count
//! and domain), or every page of a sitemap.xml. robots.txt is honoured unless a
//! source turns it off explicitly, private addresses are refused unless the
//! source allows them, and every page is size and time limited.

use std::collections::{HashMap, HashSet, VecDeque};

use futures::StreamExt;
use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
use url::Url;

use crate::error::{ConnectError, Result};
use crate::http::SourceHttp;
use crate::types::{Connector, ItemContent, Listing, RemoteItem};

/// Most pages one crawl may index.
pub const MAX_PAGES_LIMIT: usize = 2_000;
/// Deepest crawl allowed.
pub const MAX_DEPTH_LIMIT: u32 = 5;
const MAX_SITEMAPS: usize = 50;
const MAX_ROBOTS_BYTES: usize = 512 * 1024;
const CRAWL_CONCURRENCY: usize = 4;

type PageResult = (Url, u32, Result<(ItemContent, String, Vec<Url>)>);

/// How a web source finds its pages.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WebMode {
    /// Exactly the given pages.
    #[default]
    Page,
    /// The given pages and the pages they link to, up to `max_depth` links away.
    Crawl,
    /// Every page listed by the given sitemap.xml files (sitemap indexes too).
    Sitemap,
}

/// Config of a `web` source.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebConfig {
    /// Start pages, or sitemap addresses in `sitemap` mode.
    pub urls: Vec<String>,
    #[serde(default)]
    pub mode: WebMode,
    /// Links followed from a start page (`crawl` mode).
    #[serde(default = "default_depth")]
    pub max_depth: u32,
    /// Pages indexed at most.
    #[serde(default = "default_max_pages")]
    pub max_pages: usize,
    /// Only follow links on the hosts of the start pages (default `true`).
    #[serde(default = "yes")]
    pub same_domain: bool,
    /// Only pages whose address starts with one of these (after the domain check).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub include: Vec<String>,
    /// Never pages whose address starts with one of these.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude: Vec<String>,
    /// Honour robots.txt (default `true`).
    #[serde(default = "yes")]
    pub respect_robots: bool,
    /// Allow private and local network addresses (an intranet). Off by default.
    #[serde(default)]
    pub allow_private: bool,
}

fn default_depth() -> u32 {
    2
}

fn default_max_pages() -> usize {
    100
}

fn yes() -> bool {
    true
}

impl WebConfig {
    /// Check the config and normalize the start addresses.
    ///
    /// @throws `ConnectError::Config` for no or invalid addresses and out of range limits.
    pub fn validate(&self) -> Result<Vec<Url>> {
        if self.urls.is_empty() {
            return Err(ConnectError::Config("add at least one web address".into()));
        }
        if self.max_pages == 0 || self.max_pages > MAX_PAGES_LIMIT {
            return Err(ConnectError::Config(format!("pages per sync must be between 1 and {MAX_PAGES_LIMIT}")));
        }
        if self.max_depth > MAX_DEPTH_LIMIT {
            return Err(ConnectError::Config(format!("link depth must be at most {MAX_DEPTH_LIMIT}")));
        }
        self.urls
            .iter()
            .map(|raw| {
                let url = Url::parse(raw.trim())
                    .map_err(|_| ConnectError::Config(format!("`{raw}` is not a web address (https://...)")))?;
                if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
                    return Err(ConnectError::Config(format!("`{raw}` must start with https:// or http://")));
                }
                Ok(normalize(url))
            })
            .collect()
    }
}

/// The address without its fragment, which never changes the page.
fn normalize(mut url: Url) -> Url {
    url.set_fragment(None);
    url
}

/// The `web` connector.
#[derive(Debug)]
pub struct WebConnector {
    config: WebConfig,
    starts: Vec<Url>,
    http: SourceHttp,
    robots: tokio::sync::Mutex<HashMap<String, Robots>>,
}

impl WebConnector {
    /// @throws `ConnectError::Config` for an invalid config.
    pub fn new(config: WebConfig, http: SourceHttp) -> Result<Self> {
        let starts = config.validate()?;
        Ok(Self { config, starts, http, robots: tokio::sync::Mutex::new(HashMap::new()) })
    }

    fn in_scope(&self, url: &Url) -> bool {
        if !matches!(url.scheme(), "http" | "https") || !crawlable(url) {
            return false;
        }
        if self.config.same_domain && !self.starts.iter().any(|start| start.host_str() == url.host_str()) {
            return false;
        }
        let text = url.as_str();
        if self.config.exclude.iter().any(|prefix| text.starts_with(prefix.as_str())) {
            return false;
        }
        self.config.include.is_empty() || self.config.include.iter().any(|prefix| text.starts_with(prefix.as_str()))
    }

    /// Whether robots.txt lets graphrt read `url` (cached per origin).
    async fn allowed(&self, url: &Url) -> bool {
        if !self.config.respect_robots {
            return true;
        }
        let origin = url.origin().ascii_serialization();
        let mut cache = self.robots.lock().await;
        if let Some(robots) = cache.get(&origin) {
            return robots.allows(url);
        }
        let robots = self.load_robots(&origin).await;
        let allowed = robots.allows(url);
        cache.insert(origin, robots);
        allowed
    }

    async fn load_robots(&self, origin: &str) -> Robots {
        match self.http.get_bytes(&format!("{origin}/robots.txt"), &[]).await {
            Ok(body) if body.bytes.len() <= MAX_ROBOTS_BYTES => Robots::parse(&String::from_utf8_lossy(&body.bytes)),
            Ok(_) => Robots::allow_all(),
            // No robots.txt (or it is gone): everything is allowed (RFC 9309, 2.3.1.3).
            Err(ConnectError::NotFound(_) | ConnectError::Forbidden(_) | ConnectError::Http { .. }) => {
                Robots::allow_all()
            }
            // Unreachable robots.txt: assume a full disallow (RFC 9309, 2.3.1.4).
            Err(_) => Robots::disallow_all(),
        }
    }

    async fn page(&self, url: &Url) -> Result<(ItemContent, String, Vec<Url>)> {
        let body = self.http.get_bytes(url.as_str(), &[]).await?;
        let is_html = body.mime_type.as_deref().is_none_or(|m| m.contains("html"));
        let text = if is_html { String::from_utf8_lossy(&body.bytes).into_owned() } else { String::new() };
        let title = if is_html { html_title(&text) } else { None }.unwrap_or_else(|| display_name(url));
        let links = if is_html { html_links(&text, url) } else { Vec::new() };
        let content = ItemContent::File { bytes: body.bytes, mime_type: body.mime_type, file_name: file_name(url) };
        Ok((content, title, links))
    }

    async fn crawl(&self) -> Result<Listing> {
        let mut listing = Listing { complete: true, ..Listing::default() };
        let mut seen: HashSet<String> = HashSet::new();
        let mut queue: VecDeque<(Url, u32)> = VecDeque::new();
        for start in &self.starts {
            if !self.allowed(start).await {
                return Err(ConnectError::Denied(format!(
                    "robots.txt of {} does not allow reading {}",
                    start.host_str().unwrap_or_default(),
                    display_name(start)
                )));
            }
            if seen.insert(start.as_str().to_owned()) {
                queue.push_back((start.clone(), 0));
            }
        }
        let max_depth = if self.config.mode == WebMode::Crawl { self.config.max_depth } else { 0 };
        let mut failures = 0usize;
        while !queue.is_empty() {
            // One batch of pages in parallel, at most a few at a time per site.
            let batch: Vec<(Url, u32)> = queue.drain(..queue.len().min(CRAWL_CONCURRENCY * 4)).collect();
            let tasks: Vec<BoxFuture<'_, PageResult>> = batch
                .into_iter()
                .map(|(url, depth)| -> BoxFuture<'_, PageResult> {
                    Box::pin(async move {
                        let result = self.page(&url).await;
                        (url, depth, result)
                    })
                })
                .collect();
            let mut results = futures::stream::iter(tasks).buffered(CRAWL_CONCURRENCY);
            while let Some((url, depth, result)) = results.next().await {
                match result {
                    Ok((content, title, links)) => {
                        if listing.items.len() >= self.config.max_pages {
                            listing.complete = false;
                            continue;
                        }
                        listing.items.push(RemoteItem {
                            id: url.as_str().to_owned(),
                            title,
                            url: Some(url.as_str().to_owned()),
                            version: None,
                            mime_type: None,
                            content: Some(content),
                        });
                        if depth < max_depth {
                            for link in links {
                                // Pages robots.txt disallows are left out quietly, like out of scope ones.
                                if self.in_scope(&link)
                                    && !seen.contains(link.as_str())
                                    && self.allowed(&link).await
                                    && seen.insert(link.as_str().to_owned())
                                {
                                    queue.push_back((link, depth + 1));
                                }
                            }
                        }
                    }
                    Err(error) => {
                        failures += 1;
                        // A start page that fails stops the sync, so nothing is removed by mistake.
                        if depth == 0 && self.starts.len() == 1 {
                            return Err(error);
                        }
                        // A page that cannot be read now may still exist: keep what was indexed.
                        listing.complete = false;
                        if listing.warnings.len() < 20 {
                            listing.warnings.push(format!("{}: {error}", display_name(&url)));
                        }
                    }
                }
            }
            if listing.items.len() >= self.config.max_pages {
                if !queue.is_empty() {
                    listing.complete = false;
                    listing.warnings.push(format!(
                        "stopped at {} pages; raise the page limit to index more",
                        self.config.max_pages
                    ));
                }
                break;
            }
        }
        if failures > 20 {
            listing.warnings.push(format!("{failures} pages could not be read"));
        }
        Ok(listing)
    }

    async fn sitemap(&self) -> Result<Listing> {
        let mut listing = Listing { complete: true, ..Listing::default() };
        let mut pending: VecDeque<Url> = self.starts.iter().cloned().collect();
        let mut visited = 0usize;
        let mut seen = HashSet::new();
        while let Some(sitemap_url) = pending.pop_front() {
            visited += 1;
            if visited > MAX_SITEMAPS {
                listing.complete = false;
                listing.warnings.push(format!("read the first {MAX_SITEMAPS} sitemaps only"));
                break;
            }
            let body = match self.http.get_bytes(sitemap_url.as_str(), &[]).await {
                Ok(body) => body,
                Err(error) if visited == 1 => return Err(error),
                Err(error) => {
                    listing.complete = false;
                    listing.warnings.push(format!("{}: {error}", display_name(&sitemap_url)));
                    continue;
                }
            };
            let parsed = parse_sitemap(&String::from_utf8_lossy(&body.bytes));
            for child in parsed.sitemaps {
                if let Ok(url) = sitemap_url.join(&child) {
                    pending.push_back(url);
                }
            }
            for entry in parsed.urls {
                let Ok(url) = sitemap_url.join(&entry.loc).map(normalize) else { continue };
                if !self.in_scope(&url) || !seen.insert(url.as_str().to_owned()) {
                    continue;
                }
                if listing.items.len() >= self.config.max_pages {
                    listing.complete = false;
                    break;
                }
                if !self.allowed(&url).await {
                    continue;
                }
                listing.items.push(RemoteItem {
                    id: url.as_str().to_owned(),
                    title: display_name(&url),
                    url: Some(url.as_str().to_owned()),
                    // `lastmod` lets unchanged pages be skipped without downloading them.
                    version: entry.lastmod.map(|lastmod| format!("lastmod:{lastmod}")),
                    mime_type: None,
                    content: None,
                });
            }
        }
        if !listing.complete && listing.items.len() >= self.config.max_pages {
            listing
                .warnings
                .push(format!("stopped at {} pages; raise the page limit to index more", self.config.max_pages));
        }
        Ok(listing)
    }
}

impl Connector for WebConnector {
    fn kind(&self) -> &'static str {
        "web"
    }

    fn list(&self) -> BoxFuture<'_, Result<Listing>> {
        Box::pin(async move {
            match self.config.mode {
                WebMode::Sitemap => self.sitemap().await,
                WebMode::Page | WebMode::Crawl => self.crawl().await,
            }
        })
    }

    fn fetch<'a>(&'a self, item: &'a RemoteItem) -> BoxFuture<'a, Result<ItemContent>> {
        Box::pin(async move {
            let url =
                Url::parse(&item.id).map_err(|_| ConnectError::Config(format!("`{}` is not an address", item.id)))?;
            let (content, _, _) = self.page(&url).await?;
            Ok(content)
        })
    }
}

/// Whether a link may lead to a page or a document worth indexing: no
/// extension, a web page extension, or a format the knowledge pipeline reads.
/// Videos, images, archives, scripts and styles are not followed.
fn crawlable(url: &Url) -> bool {
    let last = url.path_segments().and_then(|mut segments| segments.next_back()).unwrap_or_default();
    let Some((_, extension)) = last.rsplit_once('.') else { return true };
    let extension = extension.to_ascii_lowercase();
    matches!(extension.as_str(), "html" | "htm" | "xhtml" | "shtml" | "php" | "asp" | "aspx" | "jsp")
        || super::INDEXABLE_EXTENSIONS.contains(&extension.as_str())
}

/// `https://example.com/menu/` -> `example.com/menu`.
pub fn display_name(url: &Url) -> String {
    let host = url.host_str().unwrap_or_default();
    let path = url.path().trim_end_matches('/');
    format!("{host}{path}")
}

/// File name for format detection: the last path segment when it has an
/// extension (`price-list.pdf`), otherwise `page.html`.
fn file_name(url: &Url) -> String {
    let last = url.path_segments().and_then(|mut segments| segments.next_back()).unwrap_or_default();
    if last.contains('.') { last.to_owned() } else { "page.html".to_owned() }
}

/// The text of the first `<title>` element.
pub fn html_title(html: &str) -> Option<String> {
    let lower = html.to_ascii_lowercase();
    let start = lower.find("<title")?;
    let open_end = start + lower.get(start..)?.find('>')? + 1;
    let close = open_end + lower.get(open_end..)?.find("</title")?;
    let title = decode_entities(html.get(open_end..close)?.trim());
    let title = title.split_whitespace().collect::<Vec<_>>().join(" ");
    (!title.is_empty()).then(|| title.chars().take(200).collect())
}

fn decode_entities(text: &str) -> String {
    text.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
}

/// Absolute `http(s)` addresses of the `<a href>` links of a page, resolved
/// against `<base href>` or the page address, without fragments.
pub fn html_links(html: &str, page: &Url) -> Vec<Url> {
    let lower = html.to_ascii_lowercase();
    let base = tag_attribute(&lower, html, "<base", "href").and_then(|href| page.join(&href).ok());
    let base = base.as_ref().unwrap_or(page);
    let mut links = Vec::new();
    let mut seen = HashSet::new();
    let mut cursor = 0;
    while let Some(offset) = lower.get(cursor..).and_then(|rest| rest.find("<a")) {
        let start = cursor + offset;
        let Some(end) = lower.get(start..).and_then(|rest| rest.find('>')).map(|e| start + e) else { break };
        cursor = end;
        // `<abbr>`, `<area>`: only `<a` followed by whitespace is a link.
        if !lower.get(start + 2..).is_some_and(|rest| rest.starts_with(char::is_whitespace)) {
            continue;
        }
        let (Some(tag_lower), Some(tag)) = (lower.get(start..end), html.get(start..end)) else { continue };
        let Some(href) = attribute(tag_lower, tag, "href") else { continue };
        let href = decode_entities(href.trim());
        if href.is_empty() || href.starts_with('#') || href.starts_with("javascript:") || href.starts_with("mailto:") {
            continue;
        }
        let Ok(url) = base.join(&href) else { continue };
        let url = normalize(url);
        if matches!(url.scheme(), "http" | "https") && seen.insert(url.as_str().to_owned()) {
            links.push(url);
        }
    }
    links
}

fn tag_attribute(lower: &str, html: &str, tag: &str, name: &str) -> Option<String> {
    let start = lower.find(tag)?;
    let end = start + lower.get(start..)?.find('>')?;
    attribute(lower.get(start..end)?, html.get(start..end)?, name)
}

/// The value of attribute `name` in one tag (`tag_lower` is `tag` lowercased).
fn attribute(tag_lower: &str, tag: &str, name: &str) -> Option<String> {
    let mut search = 0;
    while let Some(offset) = tag_lower.get(search..)?.find(name) {
        let at = search + offset;
        search = at + name.len();
        let before_ok = tag_lower.get(..at).is_some_and(|b| b.ends_with(char::is_whitespace));
        let rest = tag_lower.get(search..)?.trim_start();
        if !before_ok || !rest.starts_with('=') {
            continue;
        }
        let value_start = tag.len() - rest.len() + 1;
        let value = tag.get(value_start..)?.trim_start();
        return Some(match value.chars().next()? {
            quote @ ('"' | '\'') => value.get(1..)?.split(quote).next()?.to_owned(),
            _ => value.split(|c: char| c.is_whitespace() || c == '>').next()?.to_owned(),
        });
    }
    None
}

/// What a sitemap file lists.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Sitemap {
    pub urls: Vec<SitemapEntry>,
    /// Child sitemaps of a sitemap index.
    pub sitemaps: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SitemapEntry {
    pub loc: String,
    pub lastmod: Option<String>,
}

/// Parse `<urlset>` and `<sitemapindex>` documents.
pub fn parse_sitemap(xml: &str) -> Sitemap {
    let mut sitemap = Sitemap::default();
    for (tag, is_index) in [("url", false), ("sitemap", true)] {
        let open = format!("<{tag}>");
        let close = format!("</{tag}>");
        let mut cursor = 0;
        while let Some(offset) = xml.get(cursor..).and_then(|rest| rest.find(&open)) {
            let start = cursor + offset + open.len();
            let Some(end) = xml.get(start..).and_then(|rest| rest.find(&close)).map(|e| start + e) else { break };
            cursor = end + close.len();
            let Some(block) = xml.get(start..end) else { continue };
            let Some(loc) = element(block, "loc") else { continue };
            if is_index {
                sitemap.sitemaps.push(loc);
            } else {
                sitemap.urls.push(SitemapEntry { loc, lastmod: element(block, "lastmod") });
            }
        }
    }
    sitemap
}

fn element(block: &str, name: &str) -> Option<String> {
    let open = format!("<{name}>");
    let start = block.find(&open)? + open.len();
    let end = start + block.get(start..)?.find(&format!("</{name}>"))?;
    let value = block.get(start..end)?.trim();
    let value = value.strip_prefix("<![CDATA[").and_then(|v| v.strip_suffix("]]>")).unwrap_or(value);
    (!value.is_empty()).then(|| decode_entities(value.trim()))
}

/// The rules of one robots.txt for graphrt (its own group, else `*`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Robots {
    /// `(allow, pattern)`; the longest matching pattern wins, `allow` on a tie.
    rules: Vec<(bool, String)>,
}

impl Robots {
    pub fn allow_all() -> Self {
        Self::default()
    }

    pub fn disallow_all() -> Self {
        Self { rules: vec![(false, "/".to_owned())] }
    }

    /// Parse a robots.txt (RFC 9309) for the `graphrt` user agent.
    pub fn parse(text: &str) -> Self {
        let mut ours: Option<Vec<(bool, String)>> = None;
        let mut star: Option<Vec<(bool, String)>> = None;
        let mut agents: Vec<String> = Vec::new();
        let mut rules: Vec<(bool, String)> = Vec::new();
        let mut in_rules = false;
        let mut flush = |agents: &mut Vec<String>, rules: &mut Vec<(bool, String)>| {
            for agent in agents.iter() {
                if agent == "graphrt" {
                    ours.get_or_insert_with(Vec::new).extend(rules.iter().cloned());
                } else if agent == "*" {
                    star.get_or_insert_with(Vec::new).extend(rules.iter().cloned());
                }
            }
            agents.clear();
            rules.clear();
        };
        for line in text.lines() {
            let line = line.split('#').next().unwrap_or_default().trim();
            let Some((key, value)) = line.split_once(':') else { continue };
            let key = key.trim().to_ascii_lowercase();
            let value = value.trim();
            match key.as_str() {
                "user-agent" => {
                    if in_rules {
                        flush(&mut agents, &mut rules);
                        in_rules = false;
                    }
                    agents.push(value.to_ascii_lowercase());
                }
                "allow" | "disallow" => {
                    in_rules = true;
                    if !value.is_empty() {
                        rules.push((key == "allow", value.to_owned()));
                    }
                }
                _ => {}
            }
        }
        flush(&mut agents, &mut rules);
        Self { rules: ours.or(star).unwrap_or_default() }
    }

    /// Whether the path (and query) of `url` may be read.
    pub fn allows(&self, url: &Url) -> bool {
        let mut target = url.path().to_owned();
        if let Some(query) = url.query() {
            target.push('?');
            target.push_str(query);
        }
        if target == "/robots.txt" {
            return true;
        }
        let mut best: Option<(usize, bool)> = None;
        for (allow, pattern) in &self.rules {
            if pattern_matches(pattern, &target) {
                let length = pattern.len();
                let better = match best {
                    None => true,
                    Some((best_length, best_allow)) => {
                        length > best_length || (length == best_length && *allow && !best_allow)
                    }
                };
                if better {
                    best = Some((length, *allow));
                }
            }
        }
        best.is_none_or(|(_, allow)| allow)
    }
}

/// robots.txt pattern: a path prefix with `*` wildcards and an optional `$` end anchor.
fn pattern_matches(pattern: &str, path: &str) -> bool {
    let (pattern, anchored) = match pattern.strip_suffix('$') {
        Some(stripped) => (stripped, true),
        None => (pattern, false),
    };
    let parts: Vec<&str> = pattern.split('*').collect();
    let mut position = 0;
    for (index, part) in parts.iter().enumerate() {
        if index == 0 {
            if !path.starts_with(part) {
                return false;
            }
            position = part.len();
            continue;
        }
        match path.get(position..).and_then(|rest| rest.find(part)) {
            Some(found) => position += found + part.len(),
            None => return false,
        }
    }
    !anchored || position == path.len() || parts.last().is_some_and(|last| last.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(text: &str) -> Url {
        Url::parse(text).unwrap()
    }

    #[test]
    fn links_are_resolved_and_filtered() {
        let page = url("https://shop.example/docs/intro");
        let html = r##"<html><head><title> Intro &amp; more </title></head><body>
            <a href="/pricing">Pricing</a> <A HREF='faq#top'>FAQ</A> <a class=x href=https://other.example/>x</a>
            <a href="#local">skip</a> <a href="mailto:a@b.c">mail</a> <abbr href="/no">no</abbr>
            <a href="/pricing">dup</a></body></html>"##;
        let links: Vec<String> = html_links(html, &page).into_iter().map(String::from).collect();
        assert_eq!(
            links,
            vec!["https://shop.example/pricing", "https://shop.example/docs/faq", "https://other.example/"]
        );
        assert_eq!(html_title(html).as_deref(), Some("Intro & more"));
    }

    #[test]
    fn only_pages_and_documents_are_crawled() {
        for (link, expected) in [
            ("https://a.example/docs/", true),
            ("https://a.example/price-list.pdf", true),
            ("https://a.example/index.php?id=1", true),
            ("https://a.example/v1.2/guide", true),
            ("https://a.example/demo.mp4", false),
            ("https://a.example/logo.PNG", false),
            ("https://a.example/app.js", false),
        ] {
            assert_eq!(crawlable(&url(link)), expected, "{link}");
        }
    }

    #[test]
    fn base_href_changes_resolution() {
        let page = url("https://a.example/x/y");
        let links = html_links(r#"<base href="https://a.example/root/"><a href="page">p</a>"#, &page);
        assert_eq!(links, vec![url("https://a.example/root/page")]);
    }

    #[test]
    fn robots_rules() {
        let robots = Robots::parse(
            "User-agent: *\nDisallow: /private\nAllow: /private/open\nDisallow: /*.pdf$\n\nUser-agent: other\nDisallow: /\n",
        );
        assert!(robots.allows(&url("https://a.example/")));
        assert!(!robots.allows(&url("https://a.example/private/x")));
        assert!(robots.allows(&url("https://a.example/private/open/x")));
        assert!(!robots.allows(&url("https://a.example/files/a.pdf")));
        assert!(robots.allows(&url("https://a.example/files/a.pdf?x=1")));
        let ours = Robots::parse("User-agent: *\nDisallow: /\nUser-agent: graphrt\nAllow: /\n");
        assert!(ours.allows(&url("https://a.example/anything")), "the graphrt group wins over *");
        assert!(!Robots::disallow_all().allows(&url("https://a.example/a")));
    }

    #[test]
    fn sitemaps_and_indexes() {
        let urlset = "<urlset><url><loc>https://a.example/a</loc><lastmod>2026-11-01</lastmod></url>\
                      <url><loc><![CDATA[https://a.example/b?x=1&amp;y=2]]></loc></url></urlset>";
        let parsed = parse_sitemap(urlset);
        assert_eq!(parsed.urls.len(), 2);
        assert_eq!(parsed.urls[0].lastmod.as_deref(), Some("2026-11-01"));
        assert_eq!(parsed.urls[1].loc, "https://a.example/b?x=1&y=2");
        let index =
            parse_sitemap("<sitemapindex><sitemap><loc>https://a.example/s1.xml</loc></sitemap></sitemapindex>");
        assert_eq!(index.sitemaps, vec!["https://a.example/s1.xml"]);
    }

    #[test]
    fn config_is_checked() {
        let config = |urls: Vec<&str>| WebConfig {
            urls: urls.into_iter().map(String::from).collect(),
            mode: WebMode::Crawl,
            max_depth: 2,
            max_pages: 10,
            same_domain: true,
            include: Vec::new(),
            exclude: Vec::new(),
            respect_robots: true,
            allow_private: false,
        };
        assert!(config(vec![]).validate().is_err());
        assert!(config(vec!["ftp://a.example"]).validate().is_err());
        assert!(config(vec!["graphrt.com"]).validate().is_err());
        assert_eq!(config(vec!["https://graphrt.com/#x"]).validate().unwrap()[0].as_str(), "https://graphrt.com/");
    }
}
