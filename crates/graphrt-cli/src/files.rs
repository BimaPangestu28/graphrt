//! Local-directory [`FileResolver`]: serves `local_file` references from one root
//! directory, refusing any path that escapes it.

use std::path::{Component, Path, PathBuf};

use futures::future::BoxFuture;
use graphrt_core::file::{FileBytes, FileError, FileResolver};
use graphrt_core::value::{File, FileTransfer};

/// Resolves `local_file` / `tool_file` references relative to `root`.
#[derive(Clone, Debug)]
pub struct LocalDirResolver {
    root: PathBuf,
}

impl LocalDirResolver {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Join `reference` to the root, rejecting absolute paths and `..` components.
    fn path_for(&self, name: &str, reference: &str) -> Result<PathBuf, FileError> {
        let rel = Path::new(reference);
        let safe = rel.components().all(|c| matches!(c, Component::Normal(_) | Component::CurDir));
        if !safe || reference.is_empty() {
            return Err(FileError::Denied { name: name.to_owned(), reason: "path escapes the files directory".into() });
        }
        Ok(self.root.join(rel))
    }
}

impl FileResolver for LocalDirResolver {
    fn read<'a>(&'a self, file: &'a File, max_bytes: u64) -> BoxFuture<'a, Result<FileBytes, FileError>> {
        Box::pin(async move {
            if !matches!(file.transfer, FileTransfer::LocalFile | FileTransfer::ToolFile) {
                return Err(FileError::Unsupported(file.name.clone()));
            }
            let Some(reference) = file.reference.as_deref() else {
                return Err(FileError::Unsupported(file.name.clone()));
            };
            let name = file.name.clone();
            let path = self.path_for(&name, reference)?;
            let meta = tokio::fs::metadata(&path).await.map_err(|e| match e.kind() {
                std::io::ErrorKind::NotFound => FileError::NotFound { name: name.clone() },
                _ => FileError::Io { name: name.clone(), message: e.to_string() },
            })?;
            if meta.len() > max_bytes {
                return Err(FileError::TooLarge { name, limit: max_bytes });
            }
            let bytes = tokio::fs::read(&path)
                .await
                .map_err(|e| FileError::Io { name: name.clone(), message: e.to_string() })?;
            Ok(FileBytes { bytes, mime_type: file.mime_type.clone() })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn reads_inside_root_and_blocks_traversal() {
        let dir = std::env::temp_dir().join(format!("graphrt-files-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), b"hello").unwrap();
        let resolver = LocalDirResolver::new(&dir);
        let ok = File {
            name: "a".into(),
            transfer: FileTransfer::LocalFile,
            reference: Some("a.txt".into()),
            ..File::default()
        };
        assert_eq!(resolver.read(&ok, 100).await.unwrap().bytes, b"hello");
        let evil = File { reference: Some("../etc/passwd".into()), ..ok.clone() };
        assert!(matches!(resolver.read(&evil, 100).await, Err(FileError::Denied { .. })));
        let abs = File { reference: Some("/etc/passwd".into()), ..ok.clone() };
        assert!(matches!(resolver.read(&abs, 100).await, Err(FileError::Denied { .. })));
        assert!(matches!(resolver.read(&ok, 2).await, Err(FileError::TooLarge { .. })));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
