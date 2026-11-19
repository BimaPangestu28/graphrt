//! Reading the catalog index and plugin tarballs: over https, or from local
//! files, always with a size cap (and a timeout over the network).

use std::io::Read;
use std::path::PathBuf;
use std::time::Duration;

/// Where a catalog file lives.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Location {
    /// An `https://` URL.
    Https(String),
    /// A local file (a plain path or a `file://` URL).
    File(PathBuf),
}

impl Location {
    /// Whether this is a local file. Tarballs may come from local files only
    /// when the index itself is local (a mirror or a test catalog).
    pub fn is_local(&self) -> bool {
        matches!(self, Self::File(_))
    }
}

impl std::fmt::Display for Location {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Https(url) => formatter.write_str(url),
            Self::File(path) => write!(formatter, "{}", path.display()),
        }
    }
}

/// Parse the index location given by `--index` or `$GRAPHRT_PLUGIN_INDEX`:
/// an `https://` URL, a `file://` URL or a local path.
///
/// # Errors
/// A message for any other URL scheme (plain `http://` included: the index
/// carries the tarball checksums, so it must not be tampered with in transit).
pub fn parse_index_location(text: &str) -> Result<Location, String> {
    if let Some(location) = parse_url(text)? {
        return Ok(location);
    }
    Ok(Location::File(PathBuf::from(text)))
}

/// Parse a catalog entry's `tarball_url`: `https://`, or `file://` when the
/// index was read from a local file.
///
/// # Errors
/// A message for every other URL, including plain paths and `http://`.
pub fn parse_tarball_location(url: &str, index_location: &Location) -> Result<Location, String> {
    match parse_url(url)? {
        Some(Location::Https(url)) => Ok(Location::Https(url)),
        Some(Location::File(path)) if index_location.is_local() => Ok(Location::File(path)),
        Some(Location::File(_)) => Err(format!("tarball URL `{url}` must use https:// (the index is not local)")),
        None => Err(format!("tarball URL `{url}` must be an https:// URL")),
    }
}

/// `Some` for an `https://` or `file://` URL, `None` for text without a scheme.
fn parse_url(text: &str) -> Result<Option<Location>, String> {
    let Some((scheme, rest)) = text.split_once("://") else {
        return Ok(None);
    };
    match scheme.to_ascii_lowercase().as_str() {
        "https" => Ok(Some(Location::Https(text.to_owned()))),
        "file" => {
            let path = PathBuf::from(rest);
            if !path.is_absolute() {
                return Err(format!("`{text}` must be an absolute file:// URL (file:///path/to/file)"));
            }
            Ok(Some(Location::File(path)))
        }
        _ => Err(format!("`{text}` must be an https:// URL, a file:// URL or a local path")),
    }
}

/// How much may be read, and for how long.
#[derive(Clone, Copy, Debug)]
pub struct FetchLimits {
    pub max_bytes: u64,
    pub timeout: Duration,
}

/// Read the whole file at `location`, refusing more than `limits.max_bytes`.
///
/// # Errors
/// A message naming the location for I/O, network, HTTP status and size failures.
pub fn fetch_bytes(location: &Location, limits: FetchLimits) -> Result<Vec<u8>, String> {
    let result = match location {
        Location::File(path) => read_file_capped(path, limits.max_bytes),
        Location::Https(url) => download_capped(url, limits),
    };
    result.map_err(|error| format!("cannot read {location}: {error}"))
}

fn read_file_capped(path: &std::path::Path, max_bytes: u64) -> Result<Vec<u8>, String> {
    let file = std::fs::File::open(path).map_err(|error| error.to_string())?;
    read_capped(file, max_bytes)
}

/// Read everything from `reader`, failing as soon as more than `max_bytes` arrive.
pub fn read_capped(reader: impl Read, max_bytes: u64) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    reader.take(max_bytes.saturating_add(1)).read_to_end(&mut bytes).map_err(|error| error.to_string())?;
    if bytes.len() as u64 > max_bytes {
        return Err(format!("larger than the {max_bytes}-byte limit"));
    }
    Ok(bytes)
}

fn download_capped(url: &str, limits: FetchLimits) -> Result<Vec<u8>, String> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| format!("cannot start runtime: {error}"))?;
    runtime.block_on(download_capped_async(url, limits))
}

/// Redirects are followed (GitHub release downloads redirect to a CDN) only
/// to https URLs, at most five times.
fn https_only_redirects() -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(|attempt| {
        if attempt.url().scheme() != "https" {
            let message = format!("refusing a redirect to non-https URL {}", attempt.url());
            return attempt.error(message);
        }
        if attempt.previous().len() >= 5 {
            return attempt.error("too many redirects");
        }
        attempt.follow()
    })
}

async fn download_capped_async(url: &str, limits: FetchLimits) -> Result<Vec<u8>, String> {
    let _already_installed = rustls::crypto::ring::default_provider().install_default();
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(limits.timeout)
        .redirect(https_only_redirects())
        .user_agent(concat!("graphrt/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|error| format!("cannot build HTTP client: {}", error.without_url()))?;
    let mut response = client.get(url).send().await.map_err(|error| error.without_url().to_string())?;
    if !response.status().is_success() {
        return Err(format!("HTTP {}", response.status()));
    }
    if response.content_length().is_some_and(|length| length > limits.max_bytes) {
        return Err(format!("larger than the {}-byte limit", limits.max_bytes));
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|error| error.without_url().to_string())? {
        bytes.extend_from_slice(&chunk);
        if bytes.len() as u64 > limits.max_bytes {
            return Err(format!("larger than the {}-byte limit", limits.max_bytes));
        }
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_locations_are_https_file_urls_or_paths() {
        assert_eq!(
            parse_index_location("https://x.example/index.json"),
            Ok(Location::Https("https://x.example/index.json".into()))
        );
        assert_eq!(parse_index_location("file:///tmp/index.json"), Ok(Location::File("/tmp/index.json".into())));
        assert_eq!(parse_index_location("catalog/index.json"), Ok(Location::File("catalog/index.json".into())));
        assert!(parse_index_location("http://x.example/index.json").unwrap_err().contains("https://"));
        assert!(parse_index_location("ftp://x.example/index.json").is_err());
        assert!(parse_index_location("file://relative/index.json").unwrap_err().contains("absolute"));
    }

    #[test]
    fn tarballs_need_https_unless_the_index_is_local() {
        let remote_index = Location::Https("https://x.example/index.json".into());
        let local_index = Location::File("/tmp/index.json".into());
        let https = "https://x.example/a-1.0.0.tar.gz";
        assert_eq!(parse_tarball_location(https, &remote_index), Ok(Location::Https(https.into())));
        assert_eq!(
            parse_tarball_location("file:///tmp/a.tar.gz", &local_index),
            Ok(Location::File("/tmp/a.tar.gz".into()))
        );
        assert!(parse_tarball_location("file:///etc/passwd", &remote_index).unwrap_err().contains("not local"));
        assert!(parse_tarball_location("http://x.example/a.tar.gz", &local_index).is_err());
        assert!(parse_tarball_location("/tmp/a.tar.gz", &local_index).is_err());
    }

    #[test]
    fn reads_are_capped() {
        assert_eq!(read_capped(&b"12345"[..], 5).unwrap(), b"12345");
        assert!(read_capped(&b"123456"[..], 5).unwrap_err().contains("5-byte limit"));
    }
}
