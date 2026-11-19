//! Selectors and the variable pool.
//!
//! A [`Selector`] addresses a value as `namespace.name[.path...]`, where the
//! namespace is a node id or one of the reserved namespaces ([`SYS`], [`ENV`],
//! [`CONVERSATION`]). The [`VariablePool`] stores one map per namespace behind an
//! `Arc`, and the namespace table itself is shared too: cloning a pool (for a
//! node context, an iteration item or a loop round) is O(1), and writes copy
//! only the touched namespace (plus the table, when it is shared).

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use indexmap::IndexMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::value::{Value, ValueType};

/// System variables (`sys.query`, `sys.files`, `sys.run_id`, ...). Written by the host only.
pub const SYS: &str = "sys";
/// Environment variables / secrets. Supplied by the host, never persisted in snapshots.
pub const ENV: &str = "env";
/// Conversation-scoped variables. Persisted by the host between runs.
pub const CONVERSATION: &str = "conversation";

/// Address of a value: `[namespace, name, path...]`.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct Selector {
    parts: Arc<[Arc<str>]>,
}

/// Error for malformed selectors.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid selector `{0}`: needs at least a namespace and a name")]
pub struct InvalidSelector(pub String);

impl Selector {
    /// Build from parts. Requires at least two non-empty parts.
    pub fn new<I, S>(parts: I) -> Result<Self, InvalidSelector>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let parts: Vec<Arc<str>> = parts.into_iter().map(|p| Arc::from(p.as_ref())).collect();
        if parts.len() < 2 || parts.iter().any(|p| p.is_empty()) {
            return Err(InvalidSelector(parts.join(".")));
        }
        Ok(Self { parts: parts.into() })
    }

    /// Parse a dotted selector such as `llm.text` or `start.user.name`.
    pub fn parse(dotted: &str) -> Result<Self, InvalidSelector> {
        Self::new(dotted.split('.'))
    }

    /// Two-part selector `namespace.name`.
    pub fn of(namespace: &str, name: &str) -> Self {
        Self { parts: vec![Arc::from(namespace), Arc::from(name)].into() }
    }

    pub fn namespace(&self) -> &str {
        self.parts.first().map(AsRef::as_ref).unwrap_or_default()
    }

    pub fn name(&self) -> &str {
        self.parts.get(1).map(AsRef::as_ref).unwrap_or_default()
    }

    /// Path below the variable (`a.b.c` -> `[c]`).
    pub fn path(&self) -> &[Arc<str>] {
        self.parts.get(2..).unwrap_or_default()
    }

    pub fn parts(&self) -> &[Arc<str>] {
        &self.parts
    }

    /// The `namespace.name` prefix of this selector.
    pub fn root(&self) -> Selector {
        Selector::of(self.namespace(), self.name())
    }

    /// Append path parts.
    pub fn child(&self, part: &str) -> Selector {
        let mut parts: Vec<Arc<str>> = self.parts.to_vec();
        parts.push(Arc::from(part));
        Selector { parts: parts.into() }
    }
}

impl fmt::Display for Selector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.parts.join("."))
    }
}

impl fmt::Debug for Selector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Selector({self})")
    }
}

impl Serialize for Selector {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.parts.iter().map(AsRef::as_ref).collect::<Vec<&str>>().serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Selector {
    /// Accepts `["node", "var"]` or `"node.var"`.
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Repr {
            List(Vec<String>),
            Dotted(String),
        }
        let sel = match Repr::deserialize(deserializer)? {
            Repr::List(parts) => Selector::new(parts),
            Repr::Dotted(s) => Selector::parse(&s),
        };
        sel.map_err(serde::de::Error::custom)
    }
}

type Namespace = Arc<IndexMap<Arc<str>, Value>>;
type ShardMap = HashMap<Arc<str>, Namespace>;
/// `None` until a namespace hashing to the shard is written.
type Shard = Option<Arc<ShardMap>>;

/// Number of shards in the namespace table. Every node owns a namespace, so a
/// large graph has many; sharding keeps a copy-on-write update proportional to
/// one shard instead of the whole table while snapshots are shared with
/// running nodes.
const SHARDS: usize = 32;

/// Namespace table split into [`SHARDS`] independently shared maps. Cloning is
/// O(1); an update copies at most the shard array (`SHARDS` pointers) and one shard.
///
/// Shards and the shard array are allocated on first write: an empty table
/// costs no allocation, where eagerly creating every shard cost 33 per pool
/// and a pool is created for every execution frame.
#[derive(Clone, Default)]
struct Table {
    shards: Option<Arc<[Shard; SHARDS]>>,
}

/// Shard index of a namespace (FNV-1a: stable and fast for short ids).
fn shard_of(namespace: &str) -> usize {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in namespace.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    // The remainder is below SHARDS, so the conversion cannot fail.
    usize::try_from(hash % SHARDS as u64).unwrap_or(0)
}

impl Table {
    fn shard(&self, namespace: &str) -> Option<&ShardMap> {
        self.shards.as_ref()?.get(shard_of(namespace))?.as_deref()
    }

    /// Mutable shard holding `namespace`, allocating or copying shared structure as needed.
    #[allow(clippy::indexing_slicing)] // shard_of() < SHARDS by construction
    fn shard_mut(&mut self, namespace: &str) -> &mut ShardMap {
        let shards = Arc::make_mut(self.shards.get_or_insert_with(Default::default));
        Arc::make_mut(shards[shard_of(namespace)].get_or_insert_with(Default::default))
    }

    fn get(&self, namespace: &str) -> Option<&Namespace> {
        self.shard(namespace)?.get(namespace)
    }

    fn entry(&mut self, namespace: &str) -> &mut Namespace {
        let shard = self.shard_mut(namespace);
        // Reuse the existing key (a reference-count bump) instead of allocating
        // a new `Arc<str>` on every write to a namespace that already exists.
        let key = shard.get_key_value(namespace).map_or_else(|| Arc::from(namespace), |(key, _)| Arc::clone(key));
        shard.entry(key).or_default()
    }

    /// Like [`Table::entry`], but a new namespace takes `namespace` as its key
    /// (a reference-count bump) instead of allocating a copy.
    fn entry_shared(&mut self, namespace: &Arc<str>) -> &mut Namespace {
        let shard = self.shard_mut(namespace);
        let key =
            shard.get_key_value(namespace.as_ref()).map_or_else(|| Arc::clone(namespace), |(key, _)| Arc::clone(key));
        shard.entry(key).or_default()
    }

    fn remove(&mut self, namespace: &str) {
        if self.get(namespace).is_some() {
            self.shard_mut(namespace).remove(namespace);
        }
    }

    fn iter(&self) -> impl Iterator<Item = (&Arc<str>, &Namespace)> {
        self.shards.iter().flat_map(|shards| shards.iter().flatten()).flat_map(|shard| shard.iter())
    }
}

fn insert_outputs(namespace: &mut Namespace, outputs: &IndexMap<String, Value>) {
    let ns = Arc::make_mut(namespace);
    ns.reserve(outputs.len());
    for (k, v) in outputs {
        ns.insert(Arc::from(k.as_str()), v.clone());
    }
}

/// Variable storage for one execution frame.
#[derive(Clone, Default)]
pub struct VariablePool {
    namespaces: Table,
}

impl fmt::Debug for VariablePool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut map = f.debug_map();
        for (ns, vars) in self.namespaces.iter() {
            map.entry(ns, vars);
        }
        map.finish()
    }
}

impl VariablePool {
    pub fn new() -> Self {
        Self::default()
    }

    /// Resolve a selector, descending into nested values. Missing values return `None`.
    pub fn get(&self, selector: &Selector) -> Option<Value> {
        let root = self.namespaces.get(selector.namespace())?.get(selector.name())?;
        if selector.path().is_empty() { Some(root.clone()) } else { root.get_path(selector.path()) }
    }

    /// Resolve a selector, returning [`Value::Null`] when absent.
    pub fn get_or_null(&self, selector: &Selector) -> Value {
        self.get(selector).unwrap_or(Value::Null)
    }

    /// Set `namespace.name`. Any path in the selector is ignored; use [`VariablePool::set_path`]
    /// for nested writes.
    pub fn set(&mut self, selector: &Selector, value: Value) {
        self.insert(selector.namespace(), selector.name(), value);
    }

    /// Set a variable by namespace and name.
    pub fn insert(&mut self, namespace: &str, name: &str, value: Value) {
        Arc::make_mut(self.namespaces.entry(namespace)).insert(Arc::from(name), value);
    }

    /// Write into a nested path, creating intermediate objects as needed.
    /// Array elements are addressed by index; writing past the end of an array
    /// is an error (nothing is written).
    pub fn set_path(&mut self, selector: &Selector, value: Value) -> Result<(), PathError> {
        if selector.path().is_empty() {
            self.set(selector, value);
            return Ok(());
        }
        let current = self.get(&selector.root()).unwrap_or(Value::Null);
        let updated = write_path(current, selector.path(), value)
            .map_err(|index| PathError { selector: selector.to_string(), index })?;
        self.set(selector, updated);
        Ok(())
    }

    /// Remove one variable.
    pub fn remove(&mut self, selector: &Selector) -> Option<Value> {
        if !self.namespaces.get(selector.namespace())?.contains_key(selector.name()) {
            return None;
        }
        Arc::make_mut(self.namespaces.entry(selector.namespace())).shift_remove(selector.name())
    }

    /// Drop a whole namespace (e.g. a node's outputs before a loop round re-runs it).
    pub fn remove_namespace(&mut self, namespace: &str) {
        self.namespaces.remove(namespace);
    }

    /// Bulk-insert a node's outputs.
    pub fn set_outputs(&mut self, namespace: &str, outputs: &IndexMap<String, Value>) {
        insert_outputs(self.namespaces.entry(namespace), outputs);
    }

    /// [`VariablePool::set_outputs`] for a namespace the caller already holds as
    /// an `Arc<str>` (the engine's node ids), which avoids copying the name.
    pub(crate) fn set_outputs_shared(&mut self, namespace: &Arc<str>, outputs: &IndexMap<String, Value>) {
        insert_outputs(self.namespaces.entry_shared(namespace), outputs);
    }

    /// All variables in a namespace.
    pub fn namespace(&self, namespace: &str) -> Option<&IndexMap<Arc<str>, Value>> {
        self.namespaces.get(namespace).map(AsRef::as_ref)
    }

    pub fn contains(&self, selector: &Selector) -> bool {
        self.get(selector).is_some()
    }

    /// Iterate `(namespace, name, value)`.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str, &Value)> {
        self.namespaces.iter().flat_map(|(ns, vars)| vars.iter().map(move |(k, v)| (ns.as_ref(), k.as_ref(), v)))
    }

    /// Serializable form for snapshots. The [`ENV`] namespace is excluded: the host
    /// must supply environment variables again on resume. Secrets in other
    /// namespaces are stored as `{"$secret": "redacted"}` markers and restored as
    /// redacted secrets (see [`Value::redacted_secret`]).
    pub fn to_snapshot(&self) -> PoolSnapshot {
        let mut out: IndexMap<String, IndexMap<String, Value>> = IndexMap::new();
        let mut entries: Vec<(&Arc<str>, &Namespace)> = self.namespaces.iter().collect();
        entries.sort_by(|a, b| a.0.cmp(b.0));
        for (ns, vars) in entries {
            if ns.as_ref() == ENV {
                continue;
            }
            out.insert(ns.to_string(), vars.iter().map(|(k, v)| (k.to_string(), v.to_snapshot_value())).collect());
        }
        PoolSnapshot(out)
    }

    /// Restore from a snapshot (env variables must be re-applied by the caller).
    pub fn from_snapshot(snapshot: PoolSnapshot) -> Self {
        let mut pool = Self::new();
        for (ns, vars) in snapshot.0 {
            for (k, v) in vars {
                pool.insert(&ns, &k, v.from_snapshot_value());
            }
        }
        pool
    }

    /// Insert a typed declared variable, validating it against its declared type.
    pub fn insert_typed(
        &mut self,
        namespace: &str,
        name: &str,
        ty: ValueType,
        value: Value,
    ) -> Result<(), crate::value::TypeMismatch> {
        let v = ty.coerce(value)?;
        self.insert(namespace, name, v);
        Ok(())
    }
}

/// Persistable variable pool contents (`namespace -> name -> value`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PoolSnapshot(pub IndexMap<String, IndexMap<String, Value>>);

/// Error writing a nested path.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("cannot write `{selector}`: array index {index} is out of range")]
pub struct PathError {
    pub selector: String,
    pub index: usize,
}

/// Returns the out-of-range index on failure.
fn write_path(current: Value, path: &[Arc<str>], value: Value) -> Result<Value, usize> {
    let Some((head, rest)) = path.split_first() else {
        return Ok(value);
    };
    match current {
        Value::Array(items) if head.parse::<usize>().is_ok() => {
            let idx = head.parse::<usize>().unwrap_or(0);
            let mut items = Arc::unwrap_or_clone(items);
            let slot = items.get_mut(idx).ok_or(idx)?;
            let old = std::mem::take(slot);
            *slot = write_path(old, rest, value)?;
            Ok(Value::Array(Arc::new(items)))
        }
        other => {
            let mut map = match other {
                Value::Object(m) => Arc::unwrap_or_clone(m),
                _ => IndexMap::new(),
            };
            // Take the old value in place so the key keeps its position.
            let old = map.get_mut(head.as_ref()).map(std::mem::take).unwrap_or(Value::Null);
            map.insert(head.to_string(), write_path(old, rest, value)?);
            Ok(Value::Object(Arc::new(map)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sel(s: &str) -> Selector {
        Selector::parse(s).unwrap_or_else(|_| Selector::of("bad", "bad"))
    }

    #[test]
    fn selector_parsing() {
        assert!(Selector::parse("a").is_err());
        assert!(Selector::parse("a..b").is_err());
        let s = sel("n.v.x.y");
        assert_eq!(s.namespace(), "n");
        assert_eq!(s.name(), "v");
        assert_eq!(s.path().len(), 2);
        let from_list: Result<Selector, _> = serde_json::from_str("[\"n\",\"v\"]");
        let from_str: Result<Selector, _> = serde_json::from_str("\"n.v\"");
        assert_eq!(from_list.ok(), from_str.ok());
    }

    #[test]
    fn clone_is_copy_on_write() {
        let mut a = VariablePool::new();
        a.insert("n", "x", Value::Int(1));
        let mut b = a.clone();
        b.insert("n", "x", Value::Int(2));
        assert_eq!(a.get(&sel("n.x")), Some(Value::Int(1)));
        assert_eq!(b.get(&sel("n.x")), Some(Value::Int(2)));
    }

    #[test]
    fn nested_get_and_set() {
        let mut p = VariablePool::new();
        p.set_path(&sel("n.obj.a.b"), Value::from("deep")).unwrap();
        p.insert("n", "list", Value::from_json(serde_json::json!([1, 2])));
        p.set_path(&sel("n.list.1"), Value::Int(9)).unwrap();
        assert_eq!(p.get(&sel("n.list")), Some(Value::from_json(serde_json::json!([1, 9]))));
        assert!(p.set_path(&sel("n.list.5"), Value::Int(9)).is_err());
        assert_eq!(p.get(&sel("n.obj.a.b")), Some(Value::from("deep")));
        assert_eq!(p.get(&sel("n.obj.missing")), None);
    }

    #[test]
    fn snapshot_excludes_env() {
        let mut p = VariablePool::new();
        p.insert(ENV, "KEY", Value::Secret(Arc::from("s3cret")));
        p.insert("n", "x", Value::Int(1));
        let snap = p.to_snapshot();
        let json = serde_json::to_string(&snap).unwrap_or_default();
        assert!(!json.contains("s3cret"));
        assert!(!json.contains("env"));
        let restored = VariablePool::from_snapshot(snap);
        assert_eq!(restored.get(&sel("n.x")), Some(Value::Int(1)));
    }
}
