//! Where `--store` keeps things: a directory, or (feature `store-s3`) shared
//! objects in an S3 bucket.

#[cfg(feature = "store-s3")]
use std::sync::Arc;

#[cfg(feature = "store-s3")]
use super::objects::{Objects, S3Objects};

/// The location of `--store`: a directory, or objects (`s3://bucket/prefix`).
#[derive(Clone, Debug)]
pub enum StoreLocation {
    Dir(std::path::PathBuf),
    #[cfg(feature = "store-s3")]
    Objects(Arc<dyn Objects>),
}

/// Scheme of an S3 store location.
pub const S3_SCHEME: &str = "s3://";

/// The location named by `text` (`--store` or `$GRAPHRT_STORE`).
///
/// @throws String - An `s3://` location without the `store-s3` feature, or one that cannot be used.
pub fn parse_location(text: &str, lookup: impl Fn(&str) -> Option<String>) -> Result<StoreLocation, String> {
    let Some(location) = text.strip_prefix(S3_SCHEME) else {
        return Ok(StoreLocation::Dir(std::path::PathBuf::from(text)));
    };
    #[cfg(feature = "store-s3")]
    {
        // One client per location: the run store, conversations, devices and
        // push share its connections (a cold start opens one TLS connection).
        use std::collections::BTreeMap;
        use std::sync::{Mutex, PoisonError};
        static OPENED: Mutex<BTreeMap<String, Arc<dyn Objects>>> = Mutex::new(BTreeMap::new());
        let mut opened = OPENED.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(objects) = opened.get(location) {
            return Ok(StoreLocation::Objects(Arc::clone(objects)));
        }
        let objects: Arc<dyn Objects> = Arc::new(S3Objects::open(location, lookup)?);
        opened.insert(location.to_owned(), Arc::clone(&objects));
        Ok(StoreLocation::Objects(objects))
    }
    #[cfg(not(feature = "store-s3"))]
    {
        let _ = (location, lookup);
        Err("`--store s3://...` needs a build with the `store-s3` feature".to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_path_is_a_directory() {
        assert!(matches!(parse_location("/data", |_| None), Ok(StoreLocation::Dir(_))));
    }
}
