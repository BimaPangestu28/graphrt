//! [`Variables`]: the small ordered map of named values a node produces and a
//! pool namespace holds.
//!
//! A node's outputs become its namespace in the variable pool and travel in its
//! [`Event::NodeSucceeded`](crate::event::Event::NodeSucceeded) unchanged:
//! `Variables` is a reference-counted handle, so storing the outputs, emitting
//! them and taking pool snapshots share one allocation instead of copying the
//! map. Writes copy the entries only while they are shared (copy-on-write).
//!
//! Nodes produce a handful of variables, so entries are kept in insertion order
//! in a flat vector (the first few stored inline in the shared allocation) and
//! looked up by a linear scan, which beats hashing at these sizes. Names are
//! [`VarName`]s, which hold a `&'static str` without allocating.

use std::borrow::Borrow;
use std::collections::HashMap;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::ops::{Deref, Index};
use std::sync::Arc;

use indexmap::IndexMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use smallvec::SmallVec;

use crate::value::Value;

/// Name of a variable in a [`Variables`] map.
///
/// Built from a string literal it costs no allocation (`"text".into()`); other
/// names are stored as a shared `Arc<str>`. Compares, hashes and orders as the
/// string it holds.
#[derive(Clone)]
pub struct VarName(NameRepr);

#[derive(Clone)]
enum NameRepr {
    Static(&'static str),
    Shared(Arc<str>),
}

impl VarName {
    /// A name from a string literal, usable in `const` contexts.
    pub const fn from_static(name: &'static str) -> Self {
        Self(NameRepr::Static(name))
    }

    pub fn as_str(&self) -> &str {
        match &self.0 {
            NameRepr::Static(name) => name,
            NameRepr::Shared(name) => name,
        }
    }
}

impl Deref for VarName {
    type Target = str;

    fn deref(&self) -> &str {
        self.as_str()
    }
}

impl AsRef<str> for VarName {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl Borrow<str> for VarName {
    fn borrow(&self) -> &str {
        self.as_str()
    }
}

impl PartialEq for VarName {
    fn eq(&self, other: &Self) -> bool {
        self.as_str() == other.as_str()
    }
}

impl Eq for VarName {}

impl PartialEq<str> for VarName {
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}

impl PartialEq<&str> for VarName {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

impl Hash for VarName {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.as_str().hash(state);
    }
}

impl PartialOrd for VarName {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for VarName {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.as_str().cmp(other.as_str())
    }
}

impl fmt::Display for VarName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Debug for VarName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.as_str(), f)
    }
}

impl From<&'static str> for VarName {
    fn from(name: &'static str) -> Self {
        Self::from_static(name)
    }
}

impl From<String> for VarName {
    fn from(name: String) -> Self {
        Self(NameRepr::Shared(Arc::from(name)))
    }
}

impl From<&String> for VarName {
    fn from(name: &String) -> Self {
        Self(NameRepr::Shared(Arc::from(name.as_str())))
    }
}

impl From<Box<str>> for VarName {
    fn from(name: Box<str>) -> Self {
        Self(NameRepr::Shared(Arc::from(name)))
    }
}

impl From<Arc<str>> for VarName {
    fn from(name: Arc<str>) -> Self {
        Self(NameRepr::Shared(name))
    }
}

impl From<&Arc<str>> for VarName {
    fn from(name: &Arc<str>) -> Self {
        Self(NameRepr::Shared(Arc::clone(name)))
    }
}

impl From<VarName> for String {
    fn from(name: VarName) -> Self {
        name.as_str().to_owned()
    }
}

impl From<VarName> for Arc<str> {
    fn from(name: VarName) -> Self {
        match name.0 {
            NameRepr::Static(name) => Arc::from(name),
            NameRepr::Shared(name) => name,
        }
    }
}

impl Serialize for VarName {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for VarName {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer).map(Self::from)
    }
}

/// Entries stored inline in the shared allocation before spilling to the heap.
/// Most nodes produce at most this many outputs.
const INLINE_ENTRIES: usize = 4;

/// Above this many entries, bulk inserts deduplicate names through a hash
/// index instead of a scan per entry.
const LINEAR_DEDUP_LIMIT: usize = 32;

type Entries = SmallVec<[(VarName, Value); INLINE_ENTRIES]>;

/// Ordered map of variable names to values, shared on clone.
///
/// Iteration follows insertion order; inserting an existing name replaces its
/// value in place. Cloning is a reference-count bump and an empty map holds no
/// allocation. Lookups scan the entries, so the map suits the handful of
/// variables a node produces, not large dictionaries (use a
/// [`Value::Object`] for those).
///
/// ```
/// use graphrt_core::variables::Variables;
/// use graphrt_core::Value;
///
/// let mut outputs = Variables::new();
/// outputs.insert("text", Value::from("hi"));
/// let shared = outputs.clone(); // no copy
/// assert_eq!(shared.get("text"), Some(&Value::from("hi")));
/// assert_eq!(outputs["missing"], Value::Null);
/// ```
#[derive(Clone, Default)]
pub struct Variables {
    entries: Option<Arc<Entries>>,
}

/// Returned by indexing a [`Variables`] with a missing name.
static NULL: Value = Value::Null;

impl Variables {
    pub const fn new() -> Self {
        Self { entries: None }
    }

    fn entries(&self) -> &[(VarName, Value)] {
        self.entries.as_deref().map_or(&[], |entries| entries.as_slice())
    }

    /// Entries for writing: allocated on first use, copied if shared.
    fn entries_mut(&mut self) -> &mut Entries {
        Arc::make_mut(self.entries.get_or_insert_with(Default::default))
    }

    fn position(&self, name: &str) -> Option<usize> {
        self.entries().iter().position(|(key, _)| key.as_str() == name)
    }

    pub fn len(&self) -> usize {
        self.entries().len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries().is_empty()
    }

    pub fn get(&self, name: &str) -> Option<&Value> {
        self.entries().iter().find(|(key, _)| key.as_str() == name).map(|(_, value)| value)
    }

    pub fn get_key_value(&self, name: &str) -> Option<(&VarName, &Value)> {
        self.entries().iter().find(|(key, _)| key.as_str() == name).map(|(key, value)| (key, value))
    }

    /// Mutable access to a value (copies the entries first if they are shared).
    pub fn get_mut(&mut self, name: &str) -> Option<&mut Value> {
        let position = self.position(name)?;
        self.entries_mut().get_mut(position).map(|(_, value)| value)
    }

    pub fn contains_key(&self, name: &str) -> bool {
        self.position(name).is_some()
    }

    /// Insert or replace. A replaced value keeps its position and is returned.
    pub fn insert(&mut self, name: impl Into<VarName>, value: Value) -> Option<Value> {
        let name = name.into();
        if let Some(slot) = self.get_mut(&name) {
            return Some(std::mem::replace(slot, value));
        }
        self.entries_mut().push((name, value));
        None
    }

    /// [`Variables::insert`] for a borrowed name: the name is only copied when
    /// it is new, so overwriting an existing variable allocates nothing.
    pub fn insert_str(&mut self, name: &str, value: Value) -> Option<Value> {
        if let Some(slot) = self.get_mut(name) {
            return Some(std::mem::replace(slot, value));
        }
        self.entries_mut().push((VarName::from(Arc::<str>::from(name)), value));
        None
    }

    /// Remove a variable, keeping the order of the others.
    pub fn remove(&mut self, name: &str) -> Option<Value> {
        let position = self.position(name)?;
        let entries = self.entries_mut();
        (position < entries.len()).then(|| entries.remove(position).1)
    }

    /// Reserve room for `additional` more entries.
    pub fn reserve(&mut self, additional: usize) {
        if additional > 0 {
            self.entries_mut().reserve(additional);
        }
    }

    /// `(name, value)` pairs in insertion order.
    pub fn iter(&self) -> Iter<'_> {
        Iter { entries: self.entries().iter() }
    }

    pub fn keys(&self) -> impl DoubleEndedIterator<Item = &str> + ExactSizeIterator + '_ {
        self.entries().iter().map(|(key, _)| key.as_str())
    }

    /// Entries with their [`VarName`]s (cloning a name never copies its text).
    pub(crate) fn named_entries(&self) -> impl Iterator<Item = (&VarName, &Value)> + '_ {
        self.entries().iter().map(|(name, value)| (name, value))
    }

    pub fn values(&self) -> impl DoubleEndedIterator<Item = &Value> + ExactSizeIterator + '_ {
        self.entries().iter().map(|(_, value)| value)
    }

    /// Whether both maps share the same storage (a clone of one another, unchanged since).
    pub fn ptr_eq(&self, other: &Self) -> bool {
        match (&self.entries, &other.entries) {
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            (None, None) => true,
            _ => false,
        }
    }

    /// Copy into an `IndexMap` with owned names.
    pub fn to_index_map(&self) -> IndexMap<String, Value> {
        self.iter().map(|(name, value)| (name.to_owned(), value.clone())).collect()
    }
}

impl fmt::Debug for Variables {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map().entries(self.iter()).finish()
    }
}

/// Map equality: the same names with equal values, in any order.
impl PartialEq for Variables {
    fn eq(&self, other: &Self) -> bool {
        self.len() == other.len() && self.iter().all(|(name, value)| other.get(name) == Some(value))
    }
}

/// Indexing a missing name yields [`Value::Null`] instead of panicking.
impl Index<&str> for Variables {
    type Output = Value;

    fn index(&self, name: &str) -> &Value {
        self.get(name).unwrap_or(&NULL)
    }
}

impl<K: Into<VarName>> Extend<(K, Value)> for Variables {
    fn extend<I: IntoIterator<Item = (K, Value)>>(&mut self, iter: I) {
        let iter = iter.into_iter();
        if self.len().saturating_add(iter.size_hint().0) <= LINEAR_DEDUP_LIMIT {
            for (name, value) in iter {
                self.insert(name, value);
            }
            return;
        }
        let entries = self.entries_mut();
        let mut index: HashMap<VarName, usize> =
            entries.iter().enumerate().map(|(position, (name, _))| (name.clone(), position)).collect();
        for (name, value) in iter {
            let name = name.into();
            match index.get(name.as_str()).and_then(|&position| entries.get_mut(position)) {
                Some(slot) => slot.1 = value,
                None => {
                    index.insert(name.clone(), entries.len());
                    entries.push((name, value));
                }
            }
        }
    }
}

impl<K: Into<VarName>> FromIterator<(K, Value)> for Variables {
    fn from_iter<I: IntoIterator<Item = (K, Value)>>(iter: I) -> Self {
        let mut variables = Self::new();
        variables.extend(iter);
        variables
    }
}

impl<K: Into<VarName>, const N: usize> From<[(K, Value); N]> for Variables {
    fn from(entries: [(K, Value); N]) -> Self {
        entries.into_iter().collect()
    }
}

impl From<IndexMap<String, Value>> for Variables {
    fn from(map: IndexMap<String, Value>) -> Self {
        // Keys of an `IndexMap` are unique: no deduplication needed.
        let entries: Entries = map.into_iter().map(|(name, value)| (VarName::from(name), value)).collect();
        if entries.is_empty() { Self::new() } else { Self { entries: Some(Arc::new(entries)) } }
    }
}

impl From<Variables> for IndexMap<String, Value> {
    fn from(variables: Variables) -> Self {
        variables.into_iter().map(|(name, value)| (String::from(name), value)).collect()
    }
}

/// Borrowing iterator over a [`Variables`] map.
#[derive(Clone, Debug)]
pub struct Iter<'a> {
    entries: std::slice::Iter<'a, (VarName, Value)>,
}

impl<'a> Iterator for Iter<'a> {
    type Item = (&'a str, &'a Value);

    fn next(&mut self) -> Option<Self::Item> {
        self.entries.next().map(|(name, value)| (name.as_str(), value))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.entries.size_hint()
    }
}

impl DoubleEndedIterator for Iter<'_> {
    fn next_back(&mut self) -> Option<Self::Item> {
        self.entries.next_back().map(|(name, value)| (name.as_str(), value))
    }
}

impl ExactSizeIterator for Iter<'_> {}

impl<'a> IntoIterator for &'a Variables {
    type Item = (&'a str, &'a Value);
    type IntoIter = Iter<'a>;

    fn into_iter(self) -> Iter<'a> {
        self.iter()
    }
}

/// Owning iterator over a [`Variables`] map (copies the entries if shared).
pub struct IntoIter {
    entries: smallvec::IntoIter<[(VarName, Value); INLINE_ENTRIES]>,
}

impl fmt::Debug for IntoIter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IntoIter").field("remaining", &self.entries.len()).finish()
    }
}

impl Iterator for IntoIter {
    type Item = (VarName, Value);

    fn next(&mut self) -> Option<Self::Item> {
        self.entries.next()
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.entries.size_hint()
    }
}

impl DoubleEndedIterator for IntoIter {
    fn next_back(&mut self) -> Option<Self::Item> {
        self.entries.next_back()
    }
}

impl ExactSizeIterator for IntoIter {}

impl IntoIterator for Variables {
    type Item = (VarName, Value);
    type IntoIter = IntoIter;

    fn into_iter(self) -> IntoIter {
        let entries = self.entries.map(Arc::unwrap_or_clone).unwrap_or_default();
        IntoIter { entries: entries.into_iter() }
    }
}

impl Serialize for Variables {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_map(self.iter())
    }
}

impl<'de> Deserialize<'de> for Variables {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // Later duplicates replace earlier values, as with `IndexMap`.
        IndexMap::<String, Value>::deserialize(deserializer).map(Self::from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_replaces_in_place_and_keeps_order() {
        let mut vars = Variables::new();
        assert!(vars.is_empty());
        assert_eq!(vars.insert("a", Value::Int(1)), None);
        assert_eq!(vars.insert(String::from("b"), Value::Int(2)), None);
        assert_eq!(vars.insert("a", Value::Int(3)), Some(Value::Int(1)));
        assert_eq!(vars.keys().collect::<Vec<_>>(), ["a", "b"]);
        assert_eq!(vars["a"], Value::Int(3));
        assert_eq!(vars["missing"], Value::Null);
        assert_eq!(vars.insert_str("b", Value::Int(4)), Some(Value::Int(2)));
        assert_eq!(vars.insert_str("c", Value::Int(5)), None);
        assert_eq!(vars.remove("a"), Some(Value::Int(3)));
        assert_eq!(vars.remove("a"), None);
        assert_eq!(vars.keys().collect::<Vec<_>>(), ["b", "c"]);
    }

    #[test]
    fn clones_share_until_written() {
        let mut a = Variables::from([("x", Value::Int(1))]);
        let b = a.clone();
        assert!(a.ptr_eq(&b));
        a.insert("x", Value::Int(2));
        assert!(!a.ptr_eq(&b));
        assert_eq!(b["x"], Value::Int(1));
        assert_eq!(a["x"], Value::Int(2));
    }

    #[test]
    fn bulk_inserts_deduplicate_names() {
        let small: Variables = [("k", Value::Int(1)), ("k", Value::Int(2))].into_iter().collect();
        assert_eq!(small.len(), 1);
        assert_eq!(small["k"], Value::Int(2));
        let large: Variables = (0..100).chain(0..100).map(|i| (format!("k{i}"), Value::Int(i))).collect();
        assert_eq!(large.len(), 100);
        assert_eq!(large.keys().nth(42), Some("k42"));
    }

    #[test]
    fn equality_ignores_order() {
        let a = Variables::from([("x", Value::Int(1)), ("y", Value::Int(2))]);
        let b = Variables::from([("y", Value::Int(2)), ("x", Value::Int(1))]);
        assert_eq!(a, b);
        assert_ne!(a, Variables::from([("x", Value::Int(1))]));
    }

    #[test]
    fn serializes_as_an_ordered_map() {
        let vars = Variables::from([("z", Value::Int(1)), ("a", Value::from("s"))]);
        let json = serde_json::to_string(&vars).unwrap_or_default();
        assert_eq!(json, r#"{"z":1,"a":"s"}"#);
        let back: Variables = serde_json::from_str(&json).unwrap_or_default();
        assert_eq!(back, vars);
        assert_eq!(IndexMap::from(back.clone()).keys().collect::<Vec<_>>(), ["z", "a"]);
        assert_eq!(Variables::from(vars.to_index_map()), vars);
    }

    #[test]
    fn names_compare_as_strings() {
        let literal = VarName::from("name");
        let shared = VarName::from(String::from("name"));
        assert_eq!(literal, shared);
        assert_eq!(literal, "name");
        assert_eq!(String::from(shared.clone()), "name");
        assert_eq!(Arc::<str>::from(literal).as_ref(), "name");
        assert_eq!(format!("{shared} {shared:?}"), "name \"name\"");
    }
}
