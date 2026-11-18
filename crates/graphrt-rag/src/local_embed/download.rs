//! Fetch model files from the Hugging Face Hub into a local cache.
//!
//! Files land in `<cache>/<owner>--<name>/<revision>/` and are reused on every
//! later run, so after the first download embeddings work fully offline. Each
//! file is written to a unique temporary name and renamed into place, so an
//! interrupted download never leaves a truncated model behind.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use super::MODEL_FILES;
use crate::error::RagError;

/// Largest single file accepted (base-size encoders are well below this).
const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024 * 1024;

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Directory holding `repo` at `revision` inside `cache_root` (not created).
pub(super) fn model_dir(cache_root: &Path, repo: &str, revision: &str) -> PathBuf {
    cache_root.join(repo.replace('/', "--")).join(revision)
}

/// Make sure every model file of `repo@revision` is in the cache; download the missing ones.
pub(super) async fn ensure_model(
    endpoint: &str,
    repo: &str,
    revision: &str,
    cache_root: &Path,
) -> Result<PathBuf, RagError> {
    let dir = model_dir(cache_root, repo, revision);
    let missing: Vec<&str> = MODEL_FILES.iter().copied().filter(|f| !dir.join(f).is_file()).collect();
    if missing.is_empty() {
        return Ok(dir);
    }
    std::fs::create_dir_all(&dir).map_err(|e| RagError::Io(format!("{}: {e}", dir.display())))?;
    let client = download_client()?;
    for file in missing {
        let url = format!("{}/{repo}/resolve/{revision}/{file}", endpoint.trim_end_matches('/'));
        download(&client, &url, &dir.join(file)).await?;
    }
    Ok(dir)
}

/// Hub downloads redirect to a CDN, so this client follows up to five redirects,
/// each of which must stay on `https`. It never carries credentials.
fn download_client() -> Result<reqwest::Client, RagError> {
    let _already_installed = rustls::crypto::ring::default_provider().install_default();
    let policy = reqwest::redirect::Policy::custom(|attempt| {
        let secure = attempt.url().scheme() == "https";
        if attempt.previous().len() >= 5 || !secure { attempt.stop() } else { attempt.follow() }
    });
    reqwest::Client::builder()
        .redirect(policy)
        .connect_timeout(Duration::from_secs(15))
        .read_timeout(Duration::from_secs(60))
        .user_agent(concat!("graphrt-rag/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| RagError::Config(format!("cannot build HTTP client: {}", e.without_url())))
}

async fn download(client: &reqwest::Client, url: &str, target: &Path) -> Result<(), RagError> {
    let network = |e: reqwest::Error| RagError::Network(format!("{url}: {}", e.without_url()));
    let mut response = client.get(url).send().await.map_err(network)?;
    let status = response.status();
    if !status.is_success() {
        let hint = match status.as_u16() {
            401 | 403 => " (gated or private model: download it yourself and set `path`)",
            404 => " (check the model name, the revision, and that it ships model.safetensors)",
            _ => "",
        };
        return Err(RagError::Http { status: status.as_u16(), message: format!("{url}{hint}") });
    }
    if response.content_length().is_some_and(|n| n > MAX_FILE_BYTES) {
        return Err(RagError::Model(format!("{url} is larger than {MAX_FILE_BYTES} bytes")));
    }

    let name = target.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let n = TEMP_COUNTER.fetch_add(1, Ordering::SeqCst);
    let temp = target.with_file_name(format!(".{name}.{}.{n}.part", std::process::id()));
    let io = |p: &Path, e: std::io::Error| RagError::Io(format!("{}: {e}", p.display()));
    let result = async {
        let mut file = std::fs::File::create(&temp).map_err(|e| io(&temp, e))?;
        let mut written: u64 = 0;
        while let Some(chunk) = response.chunk().await.map_err(network)? {
            written += chunk.len() as u64;
            if written > MAX_FILE_BYTES {
                return Err(RagError::Model(format!("{url} is larger than {MAX_FILE_BYTES} bytes")));
            }
            file.write_all(&chunk).map_err(|e| io(&temp, e))?;
        }
        file.sync_all().map_err(|e| io(&temp, e))?;
        std::fs::rename(&temp, target).map_err(|e| io(target, e))
    }
    .await;
    if result.is_err() {
        let _cleanup = std::fs::remove_file(&temp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_layout_is_flat_per_repo_and_revision() {
        let dir = model_dir(Path::new("/cache"), "BAAI/bge-small-en-v1.5", "main");
        assert_eq!(dir, Path::new("/cache/BAAI--bge-small-en-v1.5/main"));
    }
}
