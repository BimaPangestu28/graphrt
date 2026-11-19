//! The local telemetry state file: the opt-in flag, the random install id, the
//! day of the last report, and run counts aggregated per day.
//!
//! `telemetry.json` lives in the user config directory
//! (`$GRAPHRT_CONFIG_DIR`, else `~/Library/Application Support/graphrt` on macOS,
//! `%APPDATA%\graphrt` on Windows, `$XDG_CONFIG_HOME/graphrt` or
//! `~/.config/graphrt` elsewhere). Every read-modify-write holds an OS lock on
//! `telemetry.lock`, so concurrent `graphrt` processes neither lose counts nor
//! send twice on the same day. The file is replaced atomically (temp + rename).

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Overrides the config directory (tests, containers with a read-only home).
pub const CONFIG_DIR_ENV: &str = "GRAPHRT_CONFIG_DIR";
/// Days of counts kept locally while reports cannot be sent.
pub const MAX_DAYS: usize = 31;

/// Counts for one UTC day.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DayCounts {
    #[serde(default)]
    pub runs: u64,
    #[serde(default)]
    pub serve_requests: u64,
}

impl DayCounts {
    fn add(&mut self, other: DayCounts) {
        self.runs = self.runs.saturating_add(other.runs);
        self.serve_requests = self.serve_requests.saturating_add(other.serve_requests);
    }
}

/// Contents of `telemetry.json`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct State {
    /// Set by `graphrt telemetry enable`.
    #[serde(default)]
    pub enabled: bool,
    /// 32 random hex digits, created on first use while enabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub install_id: Option<String>,
    /// UTC day (`YYYY-MM-DD`) of the last report attempt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_sent_day: Option<String>,
    /// Counts not reported yet, keyed by UTC day.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub days: BTreeMap<String, DayCounts>,
}

impl State {
    /// Add counts to `day`, keeping at most [`MAX_DAYS`] days (oldest dropped).
    pub fn record(&mut self, day: &str, counts: DayCounts) {
        self.days.entry(day.to_owned()).or_default().add(counts);
        while self.days.len() > MAX_DAYS {
            self.days.pop_first();
        }
    }

    /// Remove and return the counts of days before `today` (complete days).
    pub fn take_completed(&mut self, today: &str) -> BTreeMap<String, DayCounts> {
        let current = self.days.split_off(today);
        std::mem::replace(&mut self.days, current)
    }

    /// Put back counts whose report failed.
    pub fn restore(&mut self, days: BTreeMap<String, DayCounts>) {
        for (day, counts) in days {
            self.record(&day, counts);
        }
    }

    /// The install id, created (16 random bytes) when missing.
    ///
    /// @throws String - When the OS random source is unavailable.
    pub fn ensure_install_id(&mut self) -> Result<String, String> {
        if let Some(id) = &self.install_id {
            return Ok(id.clone());
        }
        let mut bytes = [0u8; 16];
        getrandom::fill(&mut bytes).map_err(|e| format!("no random source: {e}"))?;
        let id = crate::hex::to_hex(&bytes);
        self.install_id = Some(id.clone());
        Ok(id)
    }
}

/// Where the state lives.
#[derive(Clone, Debug)]
pub struct Paths {
    pub file: PathBuf,
    lock: PathBuf,
}

impl Paths {
    pub fn in_dir(dir: &Path) -> Self {
        Self { file: dir.join("telemetry.json"), lock: dir.join("telemetry.lock") }
    }

    /// The paths in the user config directory; `None` when it cannot be determined.
    pub fn locate(lookup: &dyn Fn(&str) -> Option<String>) -> Option<Self> {
        config_dir(lookup).map(|dir| Self::in_dir(&dir))
    }

    fn dir(&self) -> Option<&Path> {
        self.file.parent()
    }
}

/// `graphrt`'s directory in the user config directory.
pub fn config_dir(lookup: &dyn Fn(&str) -> Option<String>) -> Option<PathBuf> {
    let var = |name: &str| lookup(name).filter(|v| !v.is_empty()).map(PathBuf::from);
    if let Some(dir) = var(CONFIG_DIR_ENV) {
        return Some(dir);
    }
    let base = if cfg!(target_os = "macos") {
        var("HOME").map(|home| home.join("Library/Application Support"))
    } else if cfg!(windows) {
        var("APPDATA")
    } else {
        var("XDG_CONFIG_HOME").or_else(|| var("HOME").map(|home| home.join(".config")))
    };
    base.map(|base| base.join("graphrt"))
}

/// Read the state; a missing or unreadable file reads as the default (disabled).
pub fn read(paths: &Paths) -> State {
    std::fs::read(&paths.file).ok().and_then(|bytes| serde_json::from_slice(&bytes).ok()).unwrap_or_default()
}

/// Read, change and write the state under the lock.
///
/// @param paths - State location; the directory is created when missing.
/// @param change - Edits the state; its result is returned. The file is only
///   written when the state changed.
/// @throws String - When the directory, lock or file cannot be written.
pub fn update<T>(paths: &Paths, change: impl FnOnce(&mut State) -> T) -> Result<T, String> {
    let dir = paths.dir().ok_or("telemetry state has no directory")?;
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create `{}`: {e}", dir.display()))?;
    let lock = lock(&paths.lock)?;
    let before = read(paths);
    let mut state = before.clone();
    let result = change(&mut state);
    if state != before {
        write(paths, &state)?;
    }
    drop(lock);
    Ok(result)
}

fn lock(path: &Path) -> Result<File, String> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .map_err(|e| format!("cannot open `{}`: {e}", path.display()))?;
    file.lock().map_err(|e| format!("cannot lock `{}`: {e}", path.display()))?;
    Ok(file)
}

fn write(paths: &Paths, state: &State) -> Result<(), String> {
    let text = serde_json::to_vec_pretty(state).map_err(|e| e.to_string())?;
    let temp = paths.file.with_extension(format!("json.{}.tmp", std::process::id()));
    std::fs::write(&temp, text).map_err(|e| format!("cannot write `{}`: {e}", temp.display()))?;
    std::fs::rename(&temp, &paths.file).map_err(|e| {
        let _cleanup = std::fs::remove_file(&temp);
        format!("cannot write `{}`: {e}", paths.file.display())
    })
}

/// Today's UTC date as `YYYY-MM-DD`.
pub fn today() -> String {
    let secs =
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or_default();
    date_of_day(i64::try_from(secs / 86_400).unwrap_or_default())
}

/// The civil date of a day number since 1970-01-01 (proleptic Gregorian).
pub fn date_of_day(days: i64) -> String {
    // Howard Hinnant's `civil_from_days`.
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era = (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 { month_index + 3 } else { month_index - 9 };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates() {
        assert_eq!(date_of_day(0), "1970-01-01");
        assert_eq!(date_of_day(11_016), "2000-02-29");
        assert_eq!(date_of_day(19_723), "2024-01-01");
        assert_eq!(date_of_day(-1), "1969-12-31");
    }

    #[test]
    fn completed_days_are_taken_and_restored() {
        let mut state = State::default();
        state.record("2026-11-17", DayCounts { runs: 1, serve_requests: 0 });
        state.record("2026-11-18", DayCounts { runs: 2, serve_requests: 5 });
        state.record("2026-11-19", DayCounts { runs: 3, serve_requests: 0 });
        let taken = state.take_completed("2026-11-19");
        assert_eq!(taken.keys().collect::<Vec<_>>(), ["2026-11-17", "2026-11-18"]);
        assert_eq!(state.days.keys().collect::<Vec<_>>(), ["2026-11-19"]);
        state.restore(taken);
        assert_eq!(state.days.len(), 3);
    }

    #[test]
    fn old_days_are_pruned() {
        let mut state = State::default();
        for day in 0..40 {
            state.record(&date_of_day(20_000 + day), DayCounts { runs: 1, serve_requests: 0 });
        }
        assert_eq!(state.days.len(), MAX_DAYS);
        assert_eq!(state.days.keys().next().map(String::as_str), Some(date_of_day(20_009).as_str()));
    }

    #[test]
    fn install_ids_are_random_hex() {
        let (mut a, mut b) = (State::default(), State::default());
        let id = a.ensure_install_id().unwrap();
        assert_eq!(id.len(), 32);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(a.ensure_install_id().unwrap(), id, "stable once created");
        assert_ne!(b.ensure_install_id().unwrap(), id);
    }

    #[test]
    fn config_dir_override() {
        let lookup = |name: &str| (name == CONFIG_DIR_ENV).then(|| "/tmp/x".to_owned());
        assert_eq!(config_dir(&lookup), Some(PathBuf::from("/tmp/x")));
        assert_eq!(config_dir(&|_| None), None);
    }
}
