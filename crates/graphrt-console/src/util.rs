//! Small helpers: random tokens, hashes, time, dates and a JSON file that
//! several servers on one directory share.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ring::rand::{SecureRandom, SystemRandom};
use serde::Serialize;
use serde::de::DeserializeOwned;
use sha2::{Digest, Sha256};

use crate::error::{ConsoleError, Result};

pub(crate) const MS_PER_MINUTE: u64 = 60 * 1000;
pub(crate) const MS_PER_DAY: u64 = 24 * 60 * MS_PER_MINUTE;

/// Current Unix time in milliseconds.
pub(crate) fn now_ms() -> u64 {
    graphrt_core::store::now_ms()
}

/// `n` random bytes from the operating system.
///
/// @throws ConsoleError::Crypto - When the system has no randomness to give.
pub(crate) fn random_bytes<const N: usize>() -> Result<[u8; N]> {
    let mut bytes = [0u8; N];
    SystemRandom::new().fill(&mut bytes).map_err(|_| ConsoleError::Crypto("no system randomness".into()))?;
    Ok(bytes)
}

/// 256 random bits as base64url (43 characters).
///
/// @throws ConsoleError::Crypto - When the system has no randomness to give.
pub(crate) fn random_token() -> Result<String> {
    Ok(URL_SAFE_NO_PAD.encode(random_bytes::<32>()?))
}

/// A short random id (96 bits, base64url).
///
/// @throws ConsoleError::Crypto - When the system has no randomness to give.
pub(crate) fn random_id() -> Result<String> {
    Ok(URL_SAFE_NO_PAD.encode(random_bytes::<12>()?))
}

pub(crate) fn b64url(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Hex SHA-256 of `bytes`.
pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _infallible = write!(out, "{byte:02x}");
    }
    out
}

/// Compare without early exit, so timing does not reveal a prefix.
pub(crate) fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let mut diff = a.len() ^ b.len();
    for i in 0..a.len().max(b.len()) {
        let x = a.get(i).copied().unwrap_or(0);
        let y = b.get(i).copied().unwrap_or(0);
        diff |= usize::from(x ^ y);
    }
    diff == 0
}

/// The UTC date of `ms` as `YYYY-MM-DD`.
pub(crate) fn utc_date(ms: u64) -> String {
    let (year, month, day) = civil_from_days((ms / MS_PER_DAY) as i64);
    format!("{year:04}-{month:02}-{day:02}")
}

/// Days since 1970-01-01 to a proleptic Gregorian date (H. Hinnant's algorithm).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// Write `bytes` to `path` atomically and owner-only: a temporary file next
/// to it, synced, then renamed over it.
///
/// @throws ConsoleError::Io - When the file cannot be written.
pub(crate) fn replace_file(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write as _;
    let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
    let result = (|| -> std::io::Result<()> {
        let mut file = graphrt_core::private_fs::create_file(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    if let Err(error) = result {
        let _cleanup = std::fs::remove_file(&tmp);
        return Err(ConsoleError::Io(format!("cannot write {}: {error}", path.display())));
    }
    Ok(())
}

/// Create `dir` (owner-only) when it is missing.
///
/// @throws ConsoleError::Io - When it cannot be created.
pub(crate) fn ensure_dir(dir: &Path) -> Result<()> {
    graphrt_core::private_fs::create_dir_all(dir)
        .map_err(|error| ConsoleError::Io(format!("cannot create {}: {error}", dir.display())))
}

#[derive(Debug)]
struct Loaded<T> {
    value: T,
    modified: Option<SystemTime>,
}

/// A value kept in a JSON file, read again when another process changed it.
/// Every change is written atomically before it becomes the value.
#[derive(Debug)]
pub(crate) struct JsonFile<T> {
    path: PathBuf,
    loaded: Mutex<Loaded<T>>,
}

impl<T: Default + Clone + Serialize + DeserializeOwned> JsonFile<T> {
    /// The value at `path`, the default when the file does not exist yet.
    ///
    /// @throws ConsoleError::Io - When the file cannot be read or holds something else.
    pub(crate) fn open(path: PathBuf) -> Result<Self> {
        let file = Self { path, loaded: Mutex::new(Loaded { value: T::default(), modified: None }) };
        {
            let mut loaded = file.lock();
            file.reload(&mut loaded)?;
        }
        Ok(file)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Loaded<T>> {
        // Every change is made on a copy, so a panic cannot leave a half change.
        self.loaded.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn reload(&self, loaded: &mut Loaded<T>) -> Result<()> {
        let modified = match std::fs::metadata(&self.path) {
            Ok(metadata) => metadata.modified().ok(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                loaded.value = T::default();
                loaded.modified = None;
                return Ok(());
            }
            Err(error) => return Err(ConsoleError::Io(format!("cannot read {}: {error}", self.path.display()))),
        };
        // A time that moved, or one too recent to trust (coarse file system clocks).
        let recent = modified.and_then(|time| time.elapsed().ok()).is_none_or(|age| age.as_secs() < 2);
        if modified == loaded.modified && !recent && loaded.modified.is_some() {
            return Ok(());
        }
        let bytes = std::fs::read(&self.path)
            .map_err(|error| ConsoleError::Io(format!("cannot read {}: {error}", self.path.display())))?;
        loaded.value = serde_json::from_slice(&bytes)
            .map_err(|error| ConsoleError::Io(format!("{} cannot be read: {error}", self.path.display())))?;
        loaded.modified = modified;
        Ok(())
    }

    /// Look at the current value.
    ///
    /// @throws ConsoleError::Io - When the file changed and cannot be read.
    pub(crate) fn read<R>(&self, look: impl FnOnce(&T) -> R) -> Result<R> {
        let mut loaded = self.lock();
        self.reload(&mut loaded)?;
        Ok(look(&loaded.value))
    }

    /// Change a copy of the value, write it, then keep it. A refused change or
    /// a failed write changes nothing.
    ///
    /// @throws ConsoleError - What `change` refused with, or the write error.
    pub(crate) fn update<R>(&self, change: impl FnOnce(&mut T) -> Result<R>) -> Result<R> {
        let mut loaded = self.lock();
        self.reload(&mut loaded)?;
        let mut next = loaded.value.clone();
        let result = change(&mut next)?;
        let bytes = serde_json::to_vec_pretty(&next).map_err(|error| ConsoleError::Io(error.to_string()))?;
        replace_file(&self.path, &bytes)?;
        loaded.modified = std::fs::metadata(&self.path).and_then(|metadata| metadata.modified()).ok();
        loaded.value = next;
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_are_utc_calendar_days() {
        assert_eq!(utc_date(0), "1970-01-01");
        assert_eq!(utc_date(951_782_400_000), "2000-02-29");
        assert_eq!(utc_date(1_798_761_600_000 - 1), "2026-12-31");
        assert_eq!(utc_date(1_798_761_600_000), "2027-01-01");
    }

    #[test]
    fn tokens_are_random_and_url_safe() -> Result<()> {
        let (a, b) = (random_token()?, random_token()?);
        assert_ne!(a, b);
        assert_eq!(a.len(), 43);
        assert!(a.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
        Ok(())
    }

    #[test]
    fn json_file_changes_survive_a_reopen() -> Result<()> {
        let dir = graphrt_core::private_fs::create_fresh_temp_dir("graphrt-console-json")
            .map_err(|e| ConsoleError::Io(e.to_string()))?;
        let path = dir.join("value.json");
        let file: JsonFile<Vec<String>> = JsonFile::open(path.clone())?;
        file.update(|value| {
            value.push("a".into());
            Ok(())
        })?;
        let refused: Result<()> = file.update(|value| {
            value.push("b".into());
            Err(ConsoleError::Invalid("no".into()))
        });
        assert!(refused.is_err());
        let again: JsonFile<Vec<String>> = JsonFile::open(path)?;
        assert_eq!(again.read(Clone::clone)?, vec!["a".to_owned()]);
        let _cleanup = std::fs::remove_dir_all(dir);
        Ok(())
    }
}
