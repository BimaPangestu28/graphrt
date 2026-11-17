//! [`FileResolver`] for files reachable by URL (`remote_url` transfer), using the
//! egress-policy-enforcing [`HttpClient`], so file URLs supplied by users cannot
//! reach private networks.

use futures::future::BoxFuture;
use graphrt_core::file::{FileBytes, FileError, FileResolver};
use graphrt_core::value::{File, FileTransfer};

use crate::client::HttpClient;

/// Downloads `http(s)` file URLs with a size cap.
#[derive(Clone, Debug)]
pub struct HttpFileResolver {
    client: HttpClient,
}

impl HttpFileResolver {
    pub fn new(client: HttpClient) -> Self {
        Self { client }
    }
}

impl FileResolver for HttpFileResolver {
    fn read<'a>(&'a self, file: &'a File, max_bytes: u64) -> BoxFuture<'a, Result<FileBytes, FileError>> {
        Box::pin(async move {
            let Some(raw) = file.url.as_deref().filter(|u| u.starts_with("http://") || u.starts_with("https://"))
            else {
                return Err(FileError::Unsupported(file.name.clone()));
            };
            if file.transfer != FileTransfer::RemoteUrl && file.transfer != FileTransfer::ToolFile {
                return Err(FileError::Unsupported(file.name.clone()));
            }
            let name = file.name.clone();
            let url = reqwest::Url::parse(raw)
                .map_err(|e| FileError::Invalid { name: name.clone(), message: format!("invalid URL: {e}") })?;
            self.client.check_url(&url).map_err(|e| FileError::Denied { name: name.clone(), reason: e.to_string() })?;
            let mut response = self
                .client
                .client(true)
                .get(url)
                .send()
                .await
                .map_err(|e| FileError::Io { name: name.clone(), message: e.without_url().to_string() })?;
            let status = response.status();
            if status == reqwest::StatusCode::NOT_FOUND {
                return Err(FileError::NotFound { name });
            }
            if !status.is_success() {
                return Err(FileError::Io { name, message: format!("HTTP status {}", status.as_u16()) });
            }
            if response.content_length().is_some_and(|len| len > max_bytes) {
                return Err(FileError::TooLarge { name, limit: max_bytes });
            }
            let mime = response
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .map(|v| v.split(';').next().unwrap_or(v).trim().to_owned())
                .filter(|v| !v.is_empty())
                .or_else(|| file.mime_type.clone());
            let mut bytes = Vec::new();
            loop {
                match response.chunk().await {
                    Ok(Some(chunk)) => {
                        let total = u64::try_from(bytes.len().saturating_add(chunk.len())).unwrap_or(u64::MAX);
                        if total > max_bytes {
                            return Err(FileError::TooLarge { name, limit: max_bytes });
                        }
                        bytes.extend_from_slice(&chunk);
                    }
                    Ok(None) => break,
                    Err(e) => return Err(FileError::Io { name, message: e.without_url().to_string() }),
                }
            }
            Ok(FileBytes { bytes, mime_type: mime })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::EgressPolicy;

    #[tokio::test]
    async fn rejects_private_addresses_and_non_http() {
        let resolver = HttpFileResolver::new(HttpClient::new(EgressPolicy::default()).unwrap());
        let private = File { name: "x".into(), url: Some("http://127.0.0.1/x".into()), ..File::default() };
        assert!(matches!(resolver.read(&private, 10).await, Err(FileError::Denied { .. })));
        let data = File { name: "y".into(), url: Some("data:,hi".into()), ..File::default() };
        assert!(matches!(resolver.read(&data, 10).await, Err(FileError::Unsupported(_))));
    }
}
