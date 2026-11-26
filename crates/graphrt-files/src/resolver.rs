//! [`FileResolver`] over a workspace, so every node that takes files
//! (document extractor, vision, HTTP uploads) can read what the agent stored,
//! and nodes that produce files have a place to put them.

use futures::future::BoxFuture;
use graphrt_core::file::{FileBytes, FileError, FileResolver};
use graphrt_core::value::File;

use crate::path::WorkspacePath;
use crate::store::StoreError;
use crate::workspace::{FilesError, REFERENCE_SCHEME, Workspace};

/// Directory [`WorkspaceResolver::store`] puts files in.
const OUTPUT_DIR: &str = "outputs";
/// How many numbered names `store` tries when a file name is taken.
const MAX_NAME_ATTEMPTS: usize = 100;

/// Reads `workspace:` file references and stores new files under `outputs/`.
///
/// Put it in a [`graphrt_core::file::ChainResolver`]: files with other
/// references are `Unsupported` here and fall through to the next resolver.
#[derive(Clone, Debug)]
pub struct WorkspaceResolver {
    workspace: Workspace,
    across_scopes: bool,
}

impl WorkspaceResolver {
    /// A resolver for the files `workspace` can see. A scoped view only
    /// resolves references of its own scope.
    pub fn new(workspace: Workspace) -> Self {
        Self { workspace, across_scopes: false }
    }

    /// A resolver for a workspace that is split into per-user scopes
    /// ([`crate::FileTools::scoped_by_context`]): it reads the file a
    /// reference names in whichever scope it lies, so the nodes after an
    /// agent can use the files that agent produced. It stores nothing, because
    /// a stored file would have no scope to belong to.
    pub fn across_scopes(workspace: Workspace) -> Self {
        Self { workspace, across_scopes: true }
    }
}

impl FileResolver for WorkspaceResolver {
    fn read<'a>(&'a self, file: &'a File, max_bytes: u64) -> BoxFuture<'a, Result<FileBytes, FileError>> {
        Box::pin(async move {
            let Some(reference) = file.reference.as_deref().filter(|r| r.starts_with(REFERENCE_SCHEME)) else {
                return Err(FileError::Unsupported(file.name.clone()));
            };
            let name = file.name.clone();
            let located = if self.across_scopes {
                self.workspace.locate_reference(reference)
            } else {
                self.workspace.path_of_reference(reference).map(|path| (self.workspace.clone(), path))
            };
            let Some((workspace, path)) = located else {
                return Err(FileError::Denied { name, reason: "the file is outside this workspace".into() });
            };
            let limit = max_bytes.min(workspace.limits().max_file_bytes);
            let bytes = workspace.read_all(&path, limit).await.map_err(|e| file_error(&name, e, max_bytes))?;
            // The type is what the bytes are, whatever the reference claims.
            let media = crate::sniff(&bytes, path.extension());
            Ok(FileBytes { bytes, mime_type: Some(media.mime.to_owned()) })
        })
    }

    fn store<'a>(
        &'a self,
        name: &'a str,
        _mime_type: Option<&'a str>,
        bytes: Vec<u8>,
    ) -> BoxFuture<'a, Result<File, FileError>> {
        Box::pin(async move {
            if self.across_scopes {
                return Err(FileError::Unsupported(name.to_owned()));
            }
            let invalid = |message: String| FileError::Invalid { name: name.to_owned(), message };
            let file_name = safe_file_name(name);
            let (stem, extension) = match file_name.rsplit_once('.') {
                Some((stem, extension)) if !stem.is_empty() => (stem, format!(".{extension}")),
                _ => (file_name.as_str(), String::new()),
            };
            // `report.csv`, then `report-2.csv`, `report-3.csv`... The bytes are
            // only copied when a name turns out to be taken.
            let mut bytes = Some(bytes);
            for attempt in 1..=MAX_NAME_ATTEMPTS {
                let candidate = if attempt == 1 {
                    format!("{OUTPUT_DIR}/{stem}{extension}")
                } else {
                    format!("{OUTPUT_DIR}/{stem}-{attempt}{extension}")
                };
                let path = WorkspacePath::parse(&candidate).map_err(|e| invalid(e.to_string()))?;
                if self.workspace.stat(&path).await.map_err(|e| file_error(name, e, 0))?.is_some() {
                    continue;
                }
                let Some(content) = bytes.take() else { break };
                match self.workspace.write(&path, content.clone(), false).await {
                    Ok(stored) => return Ok(self.workspace.file_value(&stored)),
                    // Another writer took the name between the check and the write.
                    Err(FilesError::Store(StoreError::AlreadyExists(_))) => bytes = Some(content),
                    Err(e) => return Err(file_error(name, e, 0)),
                }
            }
            Err(invalid(format!("no free file name after {MAX_NAME_ATTEMPTS} attempts")))
        })
    }
}

/// A file name from a name a node or a remote server chose: the last path
/// component, with every character a workspace path refuses replaced.
fn safe_file_name(name: &str) -> String {
    let last = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let cleaned: String = last
        .chars()
        .map(|c| if WorkspacePath::parse(&c.to_string()).is_ok() || c == '.' || c == ' ' { c } else { '_' })
        .collect();
    let trimmed = cleaned.trim_matches(|c: char| c == '.' || c == ' ');
    let candidate = if trimmed.is_empty() { "file" } else { trimmed };
    let candidate: String = candidate.chars().take(80).collect();
    // What is left can still be a reserved name (`CON`, `nul.txt`).
    if WorkspacePath::parse(&candidate).is_ok() { candidate } else { format!("file-{candidate}") }
}

fn file_error(name: &str, error: FilesError, max_bytes: u64) -> FileError {
    let name = name.to_owned();
    match error {
        FilesError::Store(StoreError::NotFound(_)) => FileError::NotFound { name },
        FilesError::Store(StoreError::Io { message, .. }) => FileError::Io { name, message },
        FilesError::TooLarge { limit, .. } => {
            FileError::TooLarge { name, limit: if max_bytes == 0 { limit } else { limit.min(max_bytes) } }
        }
        other => FileError::Denied { name, reason: other.to_string() },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::WorkspaceLimits;

    fn path(raw: &str) -> WorkspacePath {
        WorkspacePath::parse(raw).unwrap()
    }

    #[tokio::test]
    async fn reads_workspace_references_and_ignores_others() {
        let workspace = Workspace::in_memory(WorkspaceLimits::default());
        let stored = workspace.write(&path("in/photo.txt"), b"\x89PNG\r\n\x1a\nrest".to_vec(), false).await.unwrap();
        let resolver = WorkspaceResolver::new(workspace.clone());
        let file = workspace.file_value(&stored);
        let read = resolver.read(&file, 1_000).await.unwrap();
        // The type comes from the bytes: the name said `.txt`.
        assert_eq!((read.bytes.len(), read.mime_type.as_deref()), (12, Some("image/png")));
        assert!(matches!(resolver.read(&file, 5).await, Err(FileError::TooLarge { limit: 5, .. })));

        let foreign = File { name: "x".into(), reference: Some("uploads/x".into()), ..File::default() };
        assert!(matches!(resolver.read(&foreign, 10).await, Err(FileError::Unsupported(_))));
        let escape = File { name: "x".into(), reference: Some("workspace:../x".into()), ..File::default() };
        assert!(matches!(resolver.read(&escape, 10).await, Err(FileError::Denied { .. })));
        let missing = File { name: "x".into(), reference: Some("workspace:nope.txt".into()), ..File::default() };
        assert!(matches!(resolver.read(&missing, 10).await, Err(FileError::NotFound { .. })));
    }

    #[tokio::test]
    async fn a_scoped_resolver_cannot_read_another_scope() {
        let root = Workspace::in_memory(WorkspaceLimits::default());
        let alice = root.scoped("alice");
        let stored = alice.write(&path("a.txt"), b"alice".to_vec(), false).await.unwrap();
        let file = alice.file_value(&stored);
        assert_eq!(WorkspaceResolver::new(alice).read(&file, 100).await.unwrap().bytes, b"alice");
        let bob = WorkspaceResolver::new(root.scoped("bob"));
        assert!(matches!(bob.read(&file, 100).await, Err(FileError::Denied { .. })));
        // The unscoped workspace does not look into scopes unless the host asks for it.
        assert!(matches!(WorkspaceResolver::new(root.clone()).read(&file, 100).await, Err(FileError::Denied { .. })));
        let host = WorkspaceResolver::across_scopes(root);
        assert_eq!(host.read(&file, 100).await.unwrap().bytes, b"alice");
        assert!(matches!(host.store("x.txt", None, b"x".to_vec()).await, Err(FileError::Unsupported(_))));
    }

    #[tokio::test]
    async fn stored_files_get_safe_unique_names() {
        let workspace = Workspace::in_memory(WorkspaceLimits::default());
        let resolver = WorkspaceResolver::new(workspace.clone());
        let first = resolver.store("../../etc/report.csv", None, b"a,b\n".to_vec()).await.unwrap();
        assert_eq!(first.reference.as_deref(), Some("workspace:outputs/report.csv"));
        assert_eq!(first.mime_type.as_deref(), Some("text/csv"));
        let second = resolver.store("report.csv", None, b"c,d\n".to_vec()).await.unwrap();
        assert_eq!(second.reference.as_deref(), Some("workspace:outputs/report-2.csv"));
        assert_eq!(resolver.read(&second, 100).await.unwrap().bytes, b"c,d\n");
        // Programs are refused whatever they are called.
        assert!(matches!(resolver.store("x.png", None, b"MZ\x90\0".to_vec()).await, Err(FileError::Denied { .. })));
    }

    #[test]
    fn file_names_from_outside_are_made_safe() {
        assert_eq!(safe_file_name("a/b/c.txt"), "c.txt");
        assert_eq!(safe_file_name("C:\\Users\\x\\evil?.txt"), "evil_.txt");
        assert_eq!(safe_file_name(".env"), "env");
        assert_eq!(safe_file_name("..."), "file");
        assert_eq!(safe_file_name("nul.txt"), "file-nul.txt");
        assert_eq!(safe_file_name("a\u{202e}b.txt"), "a_b.txt");
        assert!(WorkspacePath::parse(&safe_file_name(&"x".repeat(500))).is_ok());
    }
}
