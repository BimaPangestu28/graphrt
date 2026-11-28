//! Keyed objects with conditional writes: what the shared stores of
//! `graphrt serve --store s3://bucket/prefix` are built on ([`super::shared`]).
//!
//! The only atomic operations are the ones every object store offers: create
//! if absent (`If-None-Match: *`) and replace while unchanged (`If-Match` on
//! the ETag). That is enough for several servers (Lambda instances, Cloud Run
//! revisions) to share one store without a lock or a database, and it maps onto
//! S3 (and R2 or MinIO) today and onto GCS (`ifGenerationMatch`) or Azure Blob
//! (`If-Match`) the same way.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Mutex, PoisonError};

use futures::future::BoxFuture;

/// How a write may replace what is stored.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Condition {
    /// Whatever is there.
    Always,
    /// Only when the key is free.
    Absent,
    /// Only while the object has this ETag.
    Matches(String),
}

/// An object and the ETag a conditional write names it by.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Object {
    pub bytes: Vec<u8>,
    pub etag: String,
}

/// Why an object operation failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ObjectError {
    /// The write's condition did not hold: another writer changed the object.
    Precondition,
    /// The store cannot be reached or refused (never names credentials).
    Io(String),
}

impl fmt::Display for ObjectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Precondition => f.write_str("the object changed since it was read"),
            Self::Io(message) => write!(f, "object store: {message}"),
        }
    }
}

impl std::error::Error for ObjectError {}

/// A read for a caller that may already hold the current version.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Read {
    Object(Object),
    /// Still the ETag the caller named.
    Unchanged,
    Missing,
}

/// Objects under string keys (`a/b/c`, letters, digits and `._-:=`).
pub trait Objects: Send + Sync + fmt::Debug {
    /// The object `key`, or [`Read::Unchanged`] while its ETag is `known`.
    fn read<'a>(&'a self, key: &'a str, known: Option<&'a str>) -> BoxFuture<'a, Result<Read, ObjectError>>;
    /// Write `bytes` when `condition` holds; returns the new ETag.
    fn write<'a>(
        &'a self,
        key: &'a str,
        bytes: Vec<u8>,
        condition: Condition,
    ) -> BoxFuture<'a, Result<String, ObjectError>>;
    /// Delete `key` (a missing key is not an error).
    fn remove<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<(), ObjectError>>;
    /// The keys starting with `prefix` and their ETags, in key order.
    fn keys<'a>(&'a self, prefix: &'a str) -> BoxFuture<'a, Result<Vec<(String, String)>, ObjectError>>;
    /// Where the objects are, for the startup log (`s3://bucket/prefix`).
    fn describe(&self) -> String;
}

/// The object `key`, or `None` when it does not exist.
pub async fn get(objects: &dyn Objects, key: &str) -> Result<Option<Object>, ObjectError> {
    match objects.read(key, None).await? {
        Read::Object(object) => Ok(Some(object)),
        Read::Unchanged | Read::Missing => Ok(None),
    }
}

/// Objects in memory, for tests and as the reference of the semantics.
#[derive(Debug, Default)]
pub struct MemoryObjects {
    objects: Mutex<BTreeMap<String, Object>>,
    next_etag: Mutex<u64>,
}

impl MemoryObjects {
    fn etag(&self) -> String {
        let mut next = self.next_etag.lock().unwrap_or_else(PoisonError::into_inner);
        *next += 1;
        format!("\"m{next}\"")
    }
}

impl Objects for MemoryObjects {
    fn read<'a>(&'a self, key: &'a str, known: Option<&'a str>) -> BoxFuture<'a, Result<Read, ObjectError>> {
        let objects = self.objects.lock().unwrap_or_else(PoisonError::into_inner);
        let read = match objects.get(key) {
            Some(object) if Some(object.etag.as_str()) == known => Read::Unchanged,
            Some(object) => Read::Object(object.clone()),
            None => Read::Missing,
        };
        Box::pin(std::future::ready(Ok(read)))
    }

    fn write<'a>(
        &'a self,
        key: &'a str,
        bytes: Vec<u8>,
        condition: Condition,
    ) -> BoxFuture<'a, Result<String, ObjectError>> {
        let etag = self.etag();
        let mut objects = self.objects.lock().unwrap_or_else(PoisonError::into_inner);
        let allowed = match (&condition, objects.get(key)) {
            (Condition::Always, _) | (Condition::Absent, None) => true,
            (Condition::Matches(expected), Some(object)) => &object.etag == expected,
            (Condition::Absent, Some(_)) | (Condition::Matches(_), None) => false,
        };
        let result = if allowed {
            objects.insert(key.to_owned(), Object { bytes, etag: etag.clone() });
            Ok(etag)
        } else {
            Err(ObjectError::Precondition)
        };
        Box::pin(std::future::ready(result))
    }

    fn remove<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<(), ObjectError>> {
        self.objects.lock().unwrap_or_else(PoisonError::into_inner).remove(key);
        Box::pin(std::future::ready(Ok(())))
    }

    fn keys<'a>(&'a self, prefix: &'a str) -> BoxFuture<'a, Result<Vec<(String, String)>, ObjectError>> {
        let objects = self.objects.lock().unwrap_or_else(PoisonError::into_inner);
        let keys = objects
            .range(prefix.to_owned()..)
            .take_while(|(key, _)| key.starts_with(prefix))
            .map(|(key, object)| (key.clone(), object.etag.clone()))
            .collect();
        Box::pin(std::future::ready(Ok(keys)))
    }

    fn describe(&self) -> String {
        "memory objects".to_owned()
    }
}

/// Objects in an S3 bucket (or an S3 compatible store), below a prefix.
#[derive(Debug, Clone)]
pub struct S3Objects {
    store: graphrt_files::S3Store,
    shown: String,
}

fn s3_error(error: impl fmt::Display) -> ObjectError {
    ObjectError::Io(error.to_string())
}

impl S3Objects {
    /// The bucket and prefix of `location` (`bucket/prefix`, without `s3://`),
    /// with the region and credentials of the usual AWS variables read through
    /// `lookup` (`AWS_REGION`, `AWS_ACCESS_KEY_ID`..., the instance or container role).
    ///
    /// @throws String - A bucket or prefix that cannot be used, or no region.
    pub fn open(location: &str, lookup: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        let (bucket, prefix) = location.split_once('/').unwrap_or((location, ""));
        let region = ["GRAPHRT_S3_REGION", "AWS_REGION", "AWS_DEFAULT_REGION"]
            .into_iter()
            .find_map(&lookup)
            .ok_or_else(|| "set AWS_REGION to the bucket's region".to_owned())?;
        let mut config = graphrt_files::S3Config::new(bucket, region);
        config.prefix = Some(prefix.trim_matches('/').to_owned()).filter(|prefix| !prefix.is_empty());
        config.endpoint = lookup("GRAPHRT_S3_ENDPOINT");
        config.allow_http = lookup("GRAPHRT_S3_ALLOW_HTTP").is_some_and(|value| value == "1" || value == "true");
        config.request_timeout = std::time::Duration::from_secs(20);
        let store = graphrt_files::S3Store::from_lookup(&config, lookup).map_err(|e| e.to_string())?;
        Ok(Self { store, shown: format!("s3://{location}") })
    }
}

impl Objects for S3Objects {
    fn read<'a>(&'a self, key: &'a str, known: Option<&'a str>) -> BoxFuture<'a, Result<Read, ObjectError>> {
        Box::pin(async move {
            Ok(match self.store.get_object(key, known).await.map_err(s3_error)? {
                graphrt_files::Fetched::Object(object) => {
                    Read::Object(Object { bytes: object.bytes, etag: object.etag })
                }
                graphrt_files::Fetched::Unchanged => Read::Unchanged,
                graphrt_files::Fetched::Missing => Read::Missing,
            })
        })
    }

    fn write<'a>(
        &'a self,
        key: &'a str,
        bytes: Vec<u8>,
        condition: Condition,
    ) -> BoxFuture<'a, Result<String, ObjectError>> {
        Box::pin(async move {
            let condition = match condition {
                Condition::Always => graphrt_files::PutCondition::Always,
                Condition::Absent => graphrt_files::PutCondition::Absent,
                Condition::Matches(etag) => graphrt_files::PutCondition::Matches(etag),
            };
            self.store.put_object(key, bytes, &condition).await.map_err(|error| match error {
                graphrt_files::ObjectError::Precondition => ObjectError::Precondition,
                graphrt_files::ObjectError::Store(error) => s3_error(error),
            })
        })
    }

    fn remove<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<(), ObjectError>> {
        Box::pin(async move { self.store.delete_object(key).await.map_err(s3_error) })
    }

    fn keys<'a>(&'a self, prefix: &'a str) -> BoxFuture<'a, Result<Vec<(String, String)>, ObjectError>> {
        Box::pin(async move { self.store.list_objects(prefix).await.map_err(s3_error) })
    }

    fn describe(&self) -> String {
        self.shown.clone()
    }
}

/// Runs `future` to completion from synchronous code (the devices and push
/// files are read and written in synchronous calls). Inside the server's
/// multi thread runtime the worker steps aside while it waits.
pub fn block_on<F: std::future::Future + Send>(future: F) -> F::Output
where
    F::Output: Send,
{
    match tokio::runtime::Handle::try_current() {
        Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(|| handle.block_on(future))
        }
        // A current thread runtime (tests) or none: wait on a thread of its own.
        _ => std::thread::scope(|scope| {
            scope
                .spawn(|| match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                    Ok(runtime) => Some(runtime.block_on(future)),
                    Err(_) => None,
                })
                .join()
                .ok()
                .flatten()
        })
        .unwrap_or_else(|| unreachable_runtime()),
    }
}

#[allow(clippy::panic)]
fn unreachable_runtime() -> ! {
    // Building a current thread runtime only fails when the OS refuses a thread
    // or a timer: nothing else in the server could work then either.
    panic!("cannot start an async runtime for the object store")
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn conditional_writes_let_one_writer_win() {
        let objects = MemoryObjects::default();
        let first = objects.write("a/b", b"1".to_vec(), Condition::Absent).await.unwrap();
        assert_eq!(objects.write("a/b", b"2".to_vec(), Condition::Absent).await, Err(ObjectError::Precondition));
        let second = objects.write("a/b", b"2".to_vec(), Condition::Matches(first.clone())).await.unwrap();
        assert_eq!(
            objects.write("a/b", b"3".to_vec(), Condition::Matches(first)).await,
            Err(ObjectError::Precondition)
        );
        assert_eq!(objects.read("a/b", Some(&second)).await.unwrap(), Read::Unchanged);
        objects.write("a/c", b"x".to_vec(), Condition::Always).await.unwrap();
        objects.write("b", b"y".to_vec(), Condition::Always).await.unwrap();
        let keys: Vec<String> = objects.keys("a/").await.unwrap().into_iter().map(|(key, _)| key).collect();
        assert_eq!(keys, ["a/b", "a/c"]);
        objects.remove("a/b").await.unwrap();
        assert_eq!(get(&objects, "a/b").await.unwrap(), None);
    }

    #[test]
    fn block_on_works_without_a_runtime_and_inside_one() {
        assert_eq!(block_on(async { 1 + 1 }), 2);
        let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
        assert_eq!(runtime.block_on(async { block_on(async { 3 }) }), 3);
    }
}
