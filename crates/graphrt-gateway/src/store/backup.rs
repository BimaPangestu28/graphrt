//! In-process disaster-recovery backups of the T1 store (plan §4): every
//! `interval`, `VACUUM INTO` a timestamped file in `dir`, keeping the newest
//! `keep`. Uploading the file to S3 / GCS / Azure Blob arrives with the
//! `auth-*` features (F2); until then point `dir` at a second volume or a
//! synced mount.

use std::path::{Path, PathBuf};
use std::time::Duration;

use graphrt_core::store::StoreError;

use super::SqliteStore;

/// Backup settings.
#[derive(Debug, Clone)]
pub struct BackupPolicy {
    /// Directory receiving `gateway-<unix_ms>.db` files.
    pub dir: PathBuf,
    /// Time between backups (plan: 5 minutes, the T1 disk-loss RPO).
    pub interval: Duration,
    /// Backups kept.
    pub keep: usize,
}

/// Writes one backup now and prunes old ones. Returns its path.
///
/// # Errors
/// When the directory cannot be created or `VACUUM INTO` fails.
pub async fn backup_once(store: &SqliteStore, policy: &BackupPolicy, now_ms: u64) -> Result<PathBuf, StoreError> {
    tokio::fs::create_dir_all(&policy.dir).await.map_err(|error| StoreError::Io(error.to_string()))?;
    let target = policy.dir.join(format!("gateway-{now_ms:013}.db"));
    store.backup_to(target.clone()).await?;
    prune(&policy.dir, policy.keep).await.map_err(|error| StoreError::Io(error.to_string()))?;
    Ok(target)
}

/// The newest `gateway-*.db` backup in `dir`.
pub fn newest_backup(dir: &Path) -> Option<PathBuf> {
    let mut backups: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            name.starts_with("gateway-") && name.ends_with(".db")
        })
        .map(|entry| entry.path())
        .collect();
    backups.sort_unstable();
    backups.pop()
}

/// What [`prepare_store`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Prepared {
    /// The store passed `quick_check` (SQLite replayed its WAL on open).
    Verified,
    /// No store yet and no backup: a fresh one is created.
    Fresh,
    /// Restored from this backup (the failed store, if any, kept as `.corrupt-<ms>`).
    Restored(PathBuf),
    /// Verification disabled.
    Unchecked,
}

fn quick_check(path: &Path) -> Result<(), String> {
    let connection = rusqlite::Connection::open(path).map_err(|error| error.to_string())?;
    connection.busy_timeout(Duration::from_secs(5)).map_err(|error| error.to_string())?;
    let verdict: String =
        connection.query_row("PRAGMA quick_check", [], |row| row.get(0)).map_err(|error| error.to_string())?;
    if verdict == "ok" { Ok(()) } else { Err(verdict) }
}

fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

/// Run by a gateway taking over (active/passive): opens the store once so
/// SQLite replays its WAL, checks it with `PRAGMA quick_check`, and restores
/// the newest backup of `backup_dir` when the check fails or the file is
/// missing. RPO of a restore is the backup interval (plan §4).
///
/// # Errors
/// When a failed store cannot be moved aside or the backup cannot be copied.
pub fn prepare_store(store: &Path, backup_dir: Option<&Path>, verify: bool) -> Result<Prepared, String> {
    if !verify {
        return Ok(Prepared::Unchecked);
    }
    let exists = store.exists();
    let failure = if exists { quick_check(store).err() } else { None };
    if exists && failure.is_none() {
        return Ok(Prepared::Verified);
    }
    let Some(backup) = backup_dir.and_then(newest_backup) else {
        return match failure {
            Some(failure) => {
                Err(format!("store {} failed quick_check ({failure}) and no backup exists", store.display()))
            }
            None => Ok(Prepared::Fresh),
        };
    };
    if let Some(failure) = failure {
        let aside = sibling(store, &format!(".corrupt-{}", graphrt_core::store::now_ms()));
        eprintln!("gateway: ha: store failed quick_check ({failure}); moving it to {}", aside.display());
        std::fs::rename(store, &aside).map_err(|error| format!("moving the failed store aside: {error}"))?;
        for suffix in ["-wal", "-shm"] {
            let _moved = std::fs::rename(sibling(store, suffix), sibling(&aside, suffix));
        }
    }
    if let Some(parent) = store.parent().filter(|parent| !parent.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let temporary = sibling(store, ".restoring");
    std::fs::copy(&backup, &temporary).map_err(|error| format!("copying {}: {error}", backup.display()))?;
    std::fs::rename(&temporary, store).map_err(|error| error.to_string())?;
    Ok(Prepared::Restored(backup))
}

async fn prune(dir: &Path, keep: usize) -> std::io::Result<()> {
    let mut entries = tokio::fs::read_dir(dir).await?;
    let mut backups = Vec::new();
    while let Some(entry) = entries.next_entry().await? {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with("gateway-") && name.ends_with(".db") {
            backups.push(entry.path());
        }
    }
    backups.sort_unstable();
    let excess = backups.len().saturating_sub(keep.max(1));
    for path in backups.into_iter().take(excess) {
        tokio::fs::remove_file(path).await?;
    }
    Ok(())
}

/// Runs [`backup_once`] every `policy.interval` until the task is dropped.
pub async fn run_backups(store: SqliteStore, policy: BackupPolicy) {
    let mut ticker = tokio::time::interval(policy.interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    ticker.tick().await;
    loop {
        ticker.tick().await;
        if let Err(error) = backup_once(&store, &policy, graphrt_core::store::now_ms()).await {
            eprintln!("gateway: store backup failed: {error}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn keeps_the_newest_backups() {
        let dir = std::env::temp_dir().join(format!("graphrt-gw-backup-{}", std::process::id()));
        let _cleanup = std::fs::remove_dir_all(&dir);
        let store = SqliteStore::in_memory().unwrap();
        let policy = BackupPolicy { dir: dir.clone(), interval: Duration::from_secs(300), keep: 2 };
        for now in [1, 2, 3] {
            backup_once(&store, &policy, now).await.unwrap();
        }
        let mut names: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort_unstable();
        assert_eq!(names, vec!["gateway-0000000000002.db", "gateway-0000000000003.db"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn takeover_restores_the_newest_backup_of_a_missing_or_corrupt_store() {
        use graphrt_core::store::{RunRecord, RunStore};
        let dir = std::env::temp_dir().join(format!("graphrt-gw-restore-{}", std::process::id()));
        let _cleanup = std::fs::remove_dir_all(&dir);
        let store_path = dir.join("gateway.db");
        let backups = dir.join("backups");
        assert_eq!(prepare_store(&store_path, Some(&backups), true).unwrap(), Prepared::Fresh);
        {
            let store = SqliteStore::open(&store_path).unwrap();
            store.create(RunRecord::queued("r1", "wf", "fp", Some("shop".into()), 1)).await.unwrap();
            let policy = BackupPolicy { dir: backups.clone(), interval: Duration::from_secs(300), keep: 2 };
            backup_once(&store, &policy, 1).await.unwrap();
        }
        assert_eq!(prepare_store(&store_path, Some(&backups), true).unwrap(), Prepared::Verified);
        std::fs::write(&store_path, b"definitely not sqlite, long enough to be read as a header ......").unwrap();
        let _wal = std::fs::remove_file(sibling(&store_path, "-wal"));
        let _shm = std::fs::remove_file(sibling(&store_path, "-shm"));
        assert!(matches!(prepare_store(&store_path, Some(&backups), true).unwrap(), Prepared::Restored(_)));
        let restored = SqliteStore::open(&store_path).unwrap();
        assert!(restored.get("r1").await.unwrap().is_some());
        std::fs::remove_file(&store_path).unwrap();
        assert!(matches!(prepare_store(&store_path, Some(&backups), true).unwrap(), Prepared::Restored(_)));
        assert!(prepare_store(&dir.join("other.db"), None, false).unwrap() == Prepared::Unchecked);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
