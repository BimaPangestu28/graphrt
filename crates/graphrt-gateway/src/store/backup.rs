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

async fn prune(dir: &Path, keep: usize) -> std::io::Result<()> {
    let mut entries = tokio::fs::read_dir(dir).await?;
    let mut backups = Vec::new();
    while let Some(entry) = entries.next_entry().await? {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with("gateway-") && name.ends_with(".db") {
            backups.push(entry.path());
        }
    }
    backups.sort();
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
        let mut names: Vec<String> = std::fs::read_dir(&dir).unwrap().map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned()).collect();
        names.sort();
        assert_eq!(names, vec!["gateway-0000000000002.db", "gateway-0000000000003.db"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
