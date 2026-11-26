//! `DirStore` on a real directory: what it stores, and what it refuses to
//! follow out of its root.

#![allow(clippy::unwrap_used)]

mod support;

use graphrt_files::{DirStore, FileStore, StoreError, Workspace, WorkspaceLimits, hex};
use support::{TempDir, path};

fn store(dir: &TempDir) -> DirStore {
    DirStore::open(dir.path().join("ws")).unwrap()
}

#[tokio::test]
async fn round_trip_ranges_listing_and_delete() {
    let dir = TempDir::new("roundtrip");
    let store = store(&dir);
    assert!(store.stat(&path("a.txt")).await.unwrap().is_none());
    let entry = store.write(&path("docs/2026/a.txt"), b"hello world".to_vec(), false).await.unwrap();
    assert_eq!(entry.size, 11);
    assert!(entry.modified_ms.is_some());
    store.write(&path("b.txt"), b"b".to_vec(), false).await.unwrap();

    assert_eq!(store.read(&path("docs/2026/a.txt"), 0, 100).await.unwrap(), b"hello world");
    assert_eq!(store.read(&path("docs/2026/a.txt"), 6, 3).await.unwrap(), b"wor");
    assert!(store.read(&path("docs/2026/a.txt"), 500, 3).await.unwrap().is_empty());
    assert!(matches!(store.read(&path("nope.txt"), 0, 1).await, Err(StoreError::NotFound(_))));
    assert!(matches!(store.read(&path("docs"), 0, 1).await, Err(StoreError::NotAFile(_))));

    let all = store.list(None, 100).await.unwrap();
    assert_eq!(all.entries.iter().map(|e| e.path.as_str()).collect::<Vec<_>>(), ["b.txt", "docs/2026/a.txt"]);
    let below = store.list(Some(&path("docs")), 100).await.unwrap();
    assert_eq!(below.entries.len(), 1);
    let capped = store.list(None, 1).await.unwrap();
    assert_eq!((capped.entries.len(), capped.truncated), (1, true));
    assert!(matches!(store.list(Some(&path("missing")), 10).await, Err(StoreError::NotFound(_))));

    assert!(matches!(store.write(&path("b.txt"), b"x".to_vec(), false).await, Err(StoreError::AlreadyExists(_))));
    assert_eq!(store.write(&path("b.txt"), b"xyz".to_vec(), true).await.unwrap().size, 3);
    // A directory is never replaced by a file.
    assert!(matches!(store.write(&path("docs"), b"x".to_vec(), true).await, Err(StoreError::NotAFile(_))));
    // A file is never used as a directory.
    assert!(matches!(store.write(&path("b.txt/c.txt"), b"x".to_vec(), true).await, Err(StoreError::NotAFile(_))));

    store.delete(&path("b.txt")).await.unwrap();
    assert!(matches!(store.delete(&path("b.txt")).await, Err(StoreError::NotFound(_))));
    assert!(matches!(store.delete(&path("docs")).await, Err(StoreError::NotAFile(_))));
}

#[tokio::test]
async fn writes_leave_no_temporary_files_and_hashes_stream() {
    let dir = TempDir::new("atomic");
    let store = store(&dir);
    let big = vec![7u8; 3 * 1024 * 1024 + 17];
    store.write(&path("big.bin"), big.clone(), false).await.unwrap();
    store.write(&path("big.bin"), big.clone(), true).await.unwrap();
    let names: Vec<String> =
        std::fs::read_dir(store.root()).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
    assert_eq!(names, ["big.bin"]);

    use sha2::Digest;
    let expected: [u8; 32] = sha2::Sha256::digest(&big).into();
    assert_eq!(store.sha256(&path("big.bin")).await.unwrap(), expected);
    // The default (chunked) hash of the trait agrees with the streaming one.
    let workspace = Workspace::in_memory(WorkspaceLimits::default());
    workspace.write(&path("big.bin"), big, false).await.unwrap();
    assert_eq!(workspace.sha256(&path("big.bin")).await.unwrap(), hex(&expected));
}

#[cfg(unix)]
#[tokio::test]
async fn files_and_directories_are_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let dir = TempDir::new("modes");
    let store = store(&dir);
    store.write(&path("sub/a.txt"), b"x".to_vec(), false).await.unwrap();
    let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(store.root()), 0o700);
    assert_eq!(mode(&store.root().join("sub")), 0o700);
    assert_eq!(mode(&store.root().join("sub/a.txt")), 0o600);
}

/// Everything an attacker with a foothold in the directory could plant.
#[cfg(unix)]
#[tokio::test]
async fn links_never_lead_out_of_the_root() {
    use std::os::unix::fs::symlink;
    let dir = TempDir::new("links");
    let outside = dir.path().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("secret.txt"), b"TOP SECRET").unwrap();
    let store = store(&dir);
    store.write(&path("real.txt"), b"real".to_vec(), false).await.unwrap();

    // A link to a file outside, a link to a directory outside, a link to a file inside.
    symlink(outside.join("secret.txt"), store.root().join("leak.txt")).unwrap();
    symlink(&outside, store.root().join("dir")).unwrap();
    symlink(store.root().join("real.txt"), store.root().join("alias.txt")).unwrap();
    symlink("/etc/passwd", store.root().join("passwd")).unwrap();

    for target in ["leak.txt", "dir/secret.txt", "alias.txt", "passwd"] {
        let read = store.read(&path(target), 0, 100).await;
        assert!(matches!(read, Err(StoreError::Denied { .. })), "read {target}: {read:?}");
        let stat = store.stat(&path(target)).await;
        assert!(matches!(stat, Err(StoreError::Denied { .. })), "stat {target}: {stat:?}");
        let hash = store.sha256(&path(target)).await;
        assert!(matches!(hash, Err(StoreError::Denied { .. })), "hash {target}: {hash:?}");
        let delete = store.delete(&path(target)).await;
        assert!(matches!(delete, Err(StoreError::Denied { .. })), "delete {target}: {delete:?}");
    }
    // Writing through a link would create or replace a file outside.
    for target in ["leak.txt", "dir/new.txt", "dir/secret.txt"] {
        let write = store.write(&path(target), b"owned".to_vec(), true).await;
        assert!(matches!(write, Err(StoreError::Denied { .. })), "write {target}: {write:?}");
    }
    assert_eq!(std::fs::read(outside.join("secret.txt")).unwrap(), b"TOP SECRET");
    assert!(!outside.join("new.txt").exists());

    // Listings show real files only, and never walk into a linked directory.
    let listed = store.list(None, 100).await.unwrap();
    assert_eq!(listed.entries.iter().map(|e| e.path.as_str()).collect::<Vec<_>>(), ["real.txt"]);
    assert!(matches!(store.list(Some(&path("dir")), 100).await, Err(StoreError::Denied { .. })));

    // The same holds one level up, through the workspace.
    let workspace = Workspace::new(std::sync::Arc::new(store), WorkspaceLimits::default());
    let through = workspace.read_all(&path("leak.txt"), 100).await;
    assert!(matches!(through, Err(graphrt_files::FilesError::Store(StoreError::Denied { .. }))), "{through:?}");
}

/// A FIFO would block a reader forever; a device would stream without end.
#[cfg(unix)]
#[tokio::test]
async fn special_files_are_not_opened() {
    let dir = TempDir::new("special");
    let store = store(&dir);
    let fifo = store.root().join("pipe.txt");
    let made = std::process::Command::new("mkfifo").arg(&fifo).status();
    if !made.is_ok_and(|status| status.success()) {
        return; // no mkfifo on this system
    }
    let read = tokio::time::timeout(std::time::Duration::from_secs(5), store.read(&path("pipe.txt"), 0, 10)).await;
    assert!(matches!(read, Ok(Err(StoreError::NotAFile(_)))), "{read:?}");
    assert!(store.list(None, 10).await.unwrap().entries.is_empty());
}

#[tokio::test]
async fn hidden_and_unaddressable_names_are_not_listed() {
    let dir = TempDir::new("hidden");
    let store = store(&dir);
    std::fs::write(store.root().join(".env"), b"KEY=1").unwrap();
    std::fs::create_dir_all(store.root().join(".git")).unwrap();
    std::fs::write(store.root().join(".git/config"), b"x").unwrap();
    std::fs::write(store.root().join("ok.txt"), b"x").unwrap();
    let listed = store.list(None, 100).await.unwrap();
    assert_eq!(listed.entries.iter().map(|e| e.path.as_str()).collect::<Vec<_>>(), ["ok.txt"]);
}

#[tokio::test]
async fn the_root_may_itself_be_reached_through_a_link() {
    // Temp directories often are (macOS: /var -> /private/var).
    let dir = TempDir::new("rootlink");
    let real = dir.path().join("real");
    std::fs::create_dir_all(&real).unwrap();
    #[cfg(unix)]
    {
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let store = DirStore::open(&link).unwrap();
        store.write(&path("a.txt"), b"x".to_vec(), false).await.unwrap();
        assert_eq!(std::fs::read(real.join("a.txt")).unwrap(), b"x");
    }
    assert!(DirStore::open(&real).is_ok());
    let file = dir.path().join("file");
    std::fs::write(&file, b"x").unwrap();
    assert!(DirStore::open(&file).is_err());
}

#[tokio::test]
async fn concurrent_writers_never_tear_a_file() {
    let dir = TempDir::new("concurrent");
    let store = store(&dir);
    let writes = (0..16u8).map(|i| {
        let store = store.clone();
        async move { store.write(&path("same.bin"), vec![i; 64 * 1024], true).await }
    });
    for result in futures::future::join_all(writes).await {
        result.unwrap();
    }
    let bytes = store.read(&path("same.bin"), 0, u64::MAX).await.unwrap();
    assert_eq!(bytes.len(), 64 * 1024);
    assert!(bytes.iter().all(|b| *b == bytes[0]), "the file mixes two writes");
}

/// Hashing and reading a large file must not hold the async worker: on a
/// single-threaded runtime a timer keeps firing while the file is processed.
#[tokio::test(flavor = "current_thread")]
async fn heavy_file_work_does_not_block_the_async_worker() {
    let dir = TempDir::new("nonblocking");
    let store = store(&dir);
    store.write(&path("big.bin"), vec![3u8; 48 * 1024 * 1024], false).await.unwrap();

    let ticks = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = std::sync::Arc::clone(&ticks);
    let ticker = tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    });
    let started = std::time::Instant::now();
    // Keep the store busy for at least 300 ms, whatever the speed of the machine.
    while started.elapsed() < std::time::Duration::from_millis(300) {
        store.sha256(&path("big.bin")).await.unwrap();
        store.read(&path("big.bin"), 0, u64::MAX).await.unwrap();
    }
    let busy_ms = usize::try_from(started.elapsed().as_millis()).unwrap();
    ticker.abort();
    let fired = ticks.load(std::sync::atomic::Ordering::Relaxed);
    // A free worker fires the 1 ms timer all the time (every 16 ms on Windows,
    // whose timers are that coarse). A worker that hashes and reads 48 MiB itself
    // would only get to the timer between calls: a few times per second.
    assert!(fired >= busy_ms / 50, "the timer fired {fired} times while the store was busy for {busy_ms} ms");
}
