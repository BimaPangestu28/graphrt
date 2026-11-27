//! Which catalog indexes `graphrt plugin install <name>` and `plugin search` trust.
//!
//! An index is used only after its minisign signature (`<index>.minisig`, next
//! to the index) verifies against a trusted public key. The check runs on the
//! downloaded bytes before they are parsed (see
//! [`graphrt_plugin::catalog_signature`], the code shared with other hosts).
//!
//! | Index | Trusted when |
//! | --- | --- |
//! | the default, `--index` or `$GRAPHRT_PLUGIN_INDEX` | signed with the built-in graphrt-plugins key (a mirror serves the signature too) |
//! | with `--index-key` or `$GRAPHRT_PLUGIN_INDEX_KEY` | signed with that key, and only that key (a note names it on stderr) |
//! | a local file with `--allow-unsigned-index` | always, with a warning on stderr. Never for an https:// index |
//!
//! A missing, unreadable or wrong signature is an error. Nothing falls back to
//! an unverified index.

use graphrt_plugin::catalog_signature::{CatalogKeys, MAX_SIGNATURE_BYTES, VerifiedCatalog};

use super::fetch::{FetchLimits, Location, fetch_bytes};
use super::index::printable;
use crate::error::CliError;

/// Environment variable naming the catalog public key (`--index-key` wins over it).
pub const INDEX_KEY_ENV: &str = "GRAPHRT_PLUGIN_INDEX_KEY";

/// The flags that change what is trusted.
#[derive(Clone, Copy, Debug, Default)]
pub struct TrustOptions<'a> {
    /// `--index-key`: a base64 minisign public key or the path of a `.pub` file.
    pub index_key: Option<&'a str>,
    /// `--allow-unsigned-index`.
    pub allow_unsigned: bool,
}

/// How an index is checked before it is parsed.
#[derive(Debug)]
pub enum IndexTrust {
    /// Signed with a key of the graphrt-plugins catalog.
    BuiltIn(CatalogKeys),
    /// Signed with the key the user named with `named_by`.
    Custom { keys: CatalogKeys, named_by: &'static str },
    /// A local index used without a signature (`--allow-unsigned-index`).
    Unsigned,
}

impl IndexTrust {
    /// Decide how the index at `index_location` is checked.
    ///
    /// # Errors
    /// Usage errors: `--allow-unsigned-index` for an https:// index or together
    /// with an index key, and an index key that is not a minisign public key.
    pub fn resolve(options: TrustOptions<'_>, index_location: &Location) -> Result<Self, CliError> {
        let named_key = named_index_key(options.index_key);
        if options.allow_unsigned {
            if let Some((_, named_by)) = named_key {
                return Err(CliError::usage(format!(
                    "`--allow-unsigned-index` cannot be combined with {named_by}: either the index is checked \
                     against that key or it is not checked"
                )));
            }
            if !index_location.is_local() {
                return Err(CliError::usage(format!(
                    "`--allow-unsigned-index` only applies to a local index file, and {index_location} is not one: \
                     a catalog served over https:// must be signed (name its publisher's key with `--index-key`)"
                )));
            }
            return Ok(Self::Unsigned);
        }
        match named_key {
            Some((key_text, named_by)) => {
                let keys = read_index_key(&key_text, named_by)?;
                Ok(Self::Custom { keys, named_by })
            }
            None => CatalogKeys::built_in()
                .map(Self::BuiltIn)
                .map_err(|error| CliError::failed(format!("the built-in catalog keys are unusable: {error}"))),
        }
    }

    /// Check `index_bytes`, read from `index_location`, before they are parsed.
    /// Returns what the signature proves, or `None` for an unsigned local index.
    ///
    /// # Errors
    /// A failure naming the index when its signature cannot be read or does
    /// not verify. The caller must not use the bytes then.
    pub fn check(&self, index_location: &Location, index_bytes: &[u8]) -> Result<Option<VerifiedCatalog>, CliError> {
        let keys = match self {
            Self::Unsigned => {
                eprintln!(
                    "warning: {index_location} is used without a signature check (--allow-unsigned-index): nothing \
                     proves who wrote it"
                );
                return Ok(None);
            }
            Self::BuiltIn(keys) => keys,
            Self::Custom { keys, named_by } => {
                let shown_keys = keys.public_keys().collect::<Vec<_>>().join(", ");
                eprintln!(
                    "note: {index_location} is checked against the key from {named_by} ({shown_keys}), not the \
                     graphrt-plugins catalog key"
                );
                keys
            }
        };
        let refused = |reason: String| CliError::failed(format!("{index_location}: {reason}. {}", self.refusal_hint()));
        let signature_location = index_location.signature();
        let limits = FetchLimits { max_bytes: MAX_SIGNATURE_BYTES, timeout: super::SIGNATURE_TIMEOUT };
        let signature_bytes = fetch_bytes(&signature_location, limits)
            .map_err(|error| refused(format!("the plugin catalog signature is missing ({error})")))?;
        let signature_text = String::from_utf8(signature_bytes).map_err(|_| {
            refused(format!("the plugin catalog signature is not valid: {signature_location} is not text"))
        })?;
        keys.verify(index_bytes, &signature_text).map(Some).map_err(|error| refused(error.to_string()))
    }

    /// What the user can do about a refused index.
    fn refusal_hint(&self) -> String {
        match self {
            Self::Custom { named_by, .. } => {
                format!("The index was not used: it must be signed with the key from {named_by}")
            }
            Self::BuiltIn(_) | Self::Unsigned => "The index was not used: it must carry a signature of the \
                 graphrt-plugins catalog key. For a catalog of another publisher, name their minisign public key \
                 with `--index-key`"
                .to_owned(),
        }
    }
}

/// The index key text and where it was named: `--index-key`, else the environment.
fn named_index_key(flag: Option<&str>) -> Option<(String, &'static str)> {
    if let Some(flag) = flag {
        return Some((flag.to_owned(), "`--index-key`"));
    }
    match std::env::var(INDEX_KEY_ENV) {
        Ok(value) if !value.trim().is_empty() => Some((value, "$GRAPHRT_PLUGIN_INDEX_KEY")),
        _ => None,
    }
}

/// The key named by `key_text`: the content of that file when it is one,
/// otherwise the base64 key itself.
fn read_index_key(key_text: &str, named_by: &str) -> Result<CatalogKeys, CliError> {
    let key_path = std::path::Path::new(key_text);
    let public_key = if key_path.is_file() {
        let location = Location::File(key_path.to_path_buf());
        let limits = FetchLimits { max_bytes: MAX_SIGNATURE_BYTES, timeout: super::SIGNATURE_TIMEOUT };
        let key_bytes = fetch_bytes(&location, limits).map_err(CliError::usage)?;
        String::from_utf8(key_bytes).map_err(|_| CliError::usage(format!("{location} is not a minisign .pub file")))?
    } else {
        key_text.to_owned()
    };
    CatalogKeys::from_public_keys(&[public_key]).map_err(|error| {
        CliError::usage(printable(&format!(
            "{named_by}: {error}; give the base64 public key or the path of a minisign .pub file"
        )))
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    const KEY: &str = "RWS9wgWfM+4ayykVUw9LH3DOjwa+g/JFaREz3FWd/U8GJaa6q4WrNd9c";

    fn local() -> Location {
        Location::File("catalog/index.json".into())
    }

    fn remote() -> Location {
        Location::Https("https://x.example/index.json".into())
    }

    #[test]
    fn unsigned_indexes_are_local_files_only() {
        let unsigned = TrustOptions { index_key: None, allow_unsigned: true };
        assert!(matches!(IndexTrust::resolve(unsigned, &local()), Ok(IndexTrust::Unsigned)));
        let error = IndexTrust::resolve(unsigned, &remote()).unwrap_err();
        assert_eq!(error.code, crate::error::EXIT_INVALID);
        assert!(error.message.contains("only applies to a local index file"), "{}", error.message);
    }

    #[test]
    fn an_index_key_and_unsigned_exclude_each_other() {
        let both = TrustOptions { index_key: Some(KEY), allow_unsigned: true };
        let error = IndexTrust::resolve(both, &local()).unwrap_err();
        assert!(error.message.contains("cannot be combined"), "{}", error.message);
    }

    #[test]
    fn a_named_key_replaces_the_built_in_keys() {
        let named = TrustOptions { index_key: Some(KEY), allow_unsigned: false };
        let trust = IndexTrust::resolve(named, &remote()).unwrap();
        assert!(matches!(trust, IndexTrust::Custom { named_by: "`--index-key`", .. }), "{trust:?}");
        let bad = TrustOptions { index_key: Some("not-a-key"), allow_unsigned: false };
        let error = IndexTrust::resolve(bad, &remote()).unwrap_err();
        assert_eq!(error.code, crate::error::EXIT_INVALID);
        assert!(error.message.contains("minisign"), "{}", error.message);
    }
}
