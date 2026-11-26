//! Rollback protection for signed catalog indexes.
//!
//! A signature proves who published an index, not that it is the latest one:
//! whoever can replace the index can serve an older one with its old, valid
//! signature, to bring back a plugin version that was withdrawn. So graphrt
//! remembers, per index location and signing key, the newest `generated_at`
//! it has accepted (`~/.graphrt/plugin-catalog-state.json`) and refuses an
//! index from that location and key that is older, or that no longer says
//! when it was generated.
//!
//! - The first index of a location and key is accepted as is (trust on first
//!   use). Two catalogs of one publisher, or a mirror, have their own history.
//! - Unsigned local indexes (`--allow-unsigned-index`) are not tracked.
//! - Without a home directory there is no place for the state and nothing is
//!   checked.
//! - A state file that cannot be read counts as empty, with a warning: who can
//!   write it can also delete it, or install plugins directly.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::fetch::read_capped;

/// File name of the state, next to the default plugin directory.
const STATE_FILE: &str = "plugin-catalog-state.json";
/// The state holds one short entry per catalog key ever used.
const MAX_STATE_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Default, Deserialize, Serialize)]
struct State {
    /// Newest accepted index per index location, then per base64 public key.
    #[serde(default)]
    catalogs: BTreeMap<String, BTreeMap<String, Seen>>,
}

#[derive(Debug, Deserialize, Serialize)]
struct Seen {
    generated_at: String,
}

/// `~/.graphrt/plugin-catalog-state.json`, if a home directory is known.
pub fn default_state_path() -> Option<PathBuf> {
    Some(graphrt_plugin::default_install_dir()?.parent()?.join(STATE_FILE))
}

/// The index whose history is tracked: where it was read from and the key
/// that signed it.
#[derive(Clone, Copy, Debug)]
pub struct SignedIndex<'a> {
    /// The https:// URL or the absolute path of the index.
    pub location: &'a str,
    /// The base64 public key that signed it.
    pub public_key: &'a str,
}

/// Refuse an index older than the newest one already accepted from the same
/// location and key, and otherwise remember this one.
///
/// # Errors
/// A message when the index is a rollback. A state file that cannot be
/// written is a warning on stderr, not an error.
pub fn check_and_record(state_path: &Path, index: SignedIndex<'_>, generated_at: Option<&str>) -> Result<(), String> {
    let mut state = read_state(state_path);
    let seen = state
        .catalogs
        .get(index.location)
        .and_then(|keys| keys.get(index.public_key))
        .map(|seen| seen.generated_at.clone());
    let seen_time = seen.as_deref().and_then(Timestamp::parse);
    let current_time = generated_at.and_then(Timestamp::parse);
    let state_shown = state_path.display();
    match (seen_time, current_time) {
        (Some(seen_time), Some(current_time)) if current_time < seen_time => Err(format!(
            "the index was generated at {}, before the one this computer already used ({}): refusing a possible \
             rollback to withdrawn plugin versions. Try again later, or remove {state_shown} if the publisher really \
             went back to an older catalog",
            generated_at.unwrap_or_default(),
            seen.unwrap_or_default()
        )),
        (Some(_), None) => Err(format!(
            "the index has no usable `generated_at`, unlike the one this computer already used ({}): refusing a \
             possible rollback. Remove {state_shown} if the publisher really dropped it",
            seen.unwrap_or_default()
        )),
        (seen_time, Some(current_time)) if seen_time.as_ref() != Some(&current_time) => {
            let generated_at = generated_at.unwrap_or_default().to_owned();
            let keys = state.catalogs.entry(index.location.to_owned()).or_default();
            keys.insert(index.public_key.to_owned(), Seen { generated_at });
            if let Err(error) = write_state(state_path, &state) {
                eprintln!("warning: cannot remember the catalog version in {state_shown}: {error}");
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

fn read_state(state_path: &Path) -> State {
    let file = match std::fs::File::open(state_path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return State::default(),
        Err(error) => return unreadable_state(state_path, &error.to_string()),
    };
    let parsed = read_capped(file, MAX_STATE_BYTES)
        .and_then(|bytes| serde_json::from_slice::<State>(&bytes).map_err(|error| error.to_string()));
    match parsed {
        Ok(state) => state,
        Err(error) => unreadable_state(state_path, &error),
    }
}

fn unreadable_state(state_path: &Path, error: &str) -> State {
    eprintln!("warning: ignoring {}: {error}", state_path.display());
    State::default()
}

/// Write a new file, then rename it over the state, so a concurrent reader
/// never sees half of it.
fn write_state(state_path: &Path, state: &State) -> Result<(), String> {
    let text = serde_json::to_string_pretty(state).map_err(|error| error.to_string())?;
    if let Some(parent) = state_path.parent() {
        graphrt_core::private_fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let mut temporary = state_path.as_os_str().to_owned();
    temporary.push(format!(".tmp-{}", std::process::id()));
    let temporary = PathBuf::from(temporary);
    let written = graphrt_core::private_fs::write(&temporary, text)
        .and_then(|()| std::fs::rename(&temporary, state_path))
        .map_err(|error| error.to_string());
    if written.is_err() {
        let _already_gone = std::fs::remove_file(&temporary);
    }
    written
}

/// A UTC time as catalogs write it: `2026-11-19T08:14:13Z`, with an optional
/// fraction of a second. Ordered by time.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Timestamp {
    /// `YYYY-MM-DDTHH:MM:SS`: fixed width, so text order is time order.
    second: String,
    nanos: u32,
}

impl Timestamp {
    /// `None` for any other form (offsets other than `Z` included): such a
    /// value cannot be compared.
    fn parse(text: &str) -> Option<Self> {
        const SHAPE: &[u8] = b"dddd-dd-ddTdd:dd:dd";
        let rest = text.trim().strip_suffix('Z')?;
        let (second, fraction) = match rest.split_once('.') {
            Some((second, fraction)) => (second, fraction),
            None => (rest, ""),
        };
        let shaped = second.len() == SHAPE.len()
            && second.bytes().zip(SHAPE).all(|(byte, shape)| match shape {
                b'd' => byte.is_ascii_digit(),
                literal => byte == *literal,
            });
        let fraction_ok = rest.contains('.') != fraction.is_empty()
            && fraction.len() <= 9
            && fraction.bytes().all(|byte| byte.is_ascii_digit());
        if !shaped || !fraction_ok {
            return None;
        }
        let nanos = format!("{fraction:0<9}").parse().ok()?;
        Some(Self { second: second.to_owned(), nanos })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    const KEY: SignedIndex<'static> = SignedIndex { location: "https://x.example/index.json", public_key: "key-a" };

    fn state_file(test_name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("graphrt-catalog-state-{}-{test_name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("nested").join(STATE_FILE)
    }

    #[test]
    fn timestamps_order_by_time() {
        let parse = |text| Timestamp::parse(text).unwrap();
        assert!(parse("2026-11-19T08:14:13Z") < parse("2026-11-19T08:14:14Z"));
        assert!(parse("2026-11-19T08:14:13Z") < parse("2026-11-19T08:14:13.5Z"));
        assert!(parse("2026-11-19T08:14:13.25Z") < parse("2026-11-19T08:14:13.5Z"));
        assert_eq!(parse("2026-11-19T08:14:13.500Z"), parse(" 2026-11-19T08:14:13.5Z "));
        assert!(parse("2026-12-01T00:00:00Z") > parse("2026-11-30T23:59:59.999Z"));
        for bad in ["", "2026-11-19", "2026-11-19T08:14:13", "2026-11-19T08:14:13+00:00", "2026-11-19 08:14:13Z"] {
            assert_eq!(Timestamp::parse(bad), None, "{bad:?}");
        }
        for bad in ["2026-11-19T08:14:13.Z", "2026-11-19T08:14:13.1234567890Z", "2026-11-19T08:14:1xZ"] {
            assert_eq!(Timestamp::parse(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn an_older_index_of_the_same_key_is_refused() {
        let path = state_file("older");
        check_and_record(&path, KEY, Some("2026-11-20T00:00:00Z")).unwrap();
        let error = check_and_record(&path, KEY, Some("2026-11-19T23:59:59Z")).unwrap_err();
        assert!(error.contains("rollback") && error.contains("2026-11-20T00:00:00Z"), "{error}");
        check_and_record(&path, KEY, Some("2026-11-20T00:00:00Z")).unwrap();
        check_and_record(&path, KEY, Some("2026-11-21T00:00:00Z")).unwrap();
        assert!(check_and_record(&path, KEY, Some("2026-11-20T00:00:00Z")).is_err(), "the newest one counts");
    }

    #[test]
    fn an_index_that_dropped_its_time_is_refused() {
        let path = state_file("dropped");
        check_and_record(&path, KEY, None).unwrap();
        check_and_record(&path, KEY, Some("not a time")).unwrap();
        check_and_record(&path, KEY, Some("2026-11-20T00:00:00Z")).unwrap();
        for missing in [None, Some("not a time")] {
            let error = check_and_record(&path, KEY, missing).unwrap_err();
            assert!(error.contains("no usable `generated_at`"), "{error}");
        }
    }

    #[test]
    fn every_location_and_key_has_its_own_history() {
        let path = state_file("keys");
        let other_key = SignedIndex { public_key: "key-b", ..KEY };
        let other_location = SignedIndex { location: "https://x.example/index-v2.json", ..KEY };
        check_and_record(&path, KEY, Some("2026-11-20T00:00:00Z")).unwrap();
        check_and_record(&path, other_key, Some("2020-01-01T00:00:00Z")).unwrap();
        check_and_record(&path, other_location, Some("2021-01-01T00:00:00Z")).unwrap();
        assert!(check_and_record(&path, KEY, Some("2020-01-01T00:00:00Z")).is_err());
        assert!(check_and_record(&path, other_key, Some("2019-01-01T00:00:00Z")).is_err());
        assert!(check_and_record(&path, other_location, Some("2020-01-01T00:00:00Z")).is_err());
    }

    #[test]
    fn an_unreadable_state_counts_as_empty() {
        let path = state_file("corrupt");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{ not json").unwrap();
        check_and_record(&path, KEY, Some("2026-11-20T00:00:00Z")).unwrap();
        assert!(check_and_record(&path, KEY, Some("2026-11-19T00:00:00Z")).is_err(), "rewritten and used again");
    }
}
