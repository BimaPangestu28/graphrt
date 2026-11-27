//! The shared document of one agent project.
//!
//! A project is a JSON object of named sections (for example `name`, `flow`,
//! `layout`, `notes`), kept in a [Yjs](https://docs.yjs.dev) compatible CRDT
//! (the `yrs` crate) under the root map [`ROOT`]. Editors never write CRDT
//! operations by hand: they hand the new JSON to [`ProjectDoc::apply_json`],
//! which compares it with the document and writes the smallest set of
//! operations that turns one into the other:
//!
//! - an object becomes a map: concurrent edits of different keys both stay,
//!   the same key edited twice keeps one of the values (the same one on every
//!   replica);
//! - a list of objects that all carry a distinct `id` (else `name`), such as
//!   the nodes and edges of a workflow, is matched item by item: adding,
//!   removing and editing different items at the same time all merge;
//! - other lists are matched by value (longest common subsequence);
//! - prose (prompts, instructions, descriptions, any text with a line break)
//!   becomes shared text: two people typing in the same prompt merge
//!   character by character;
//! - other values are written whole.
//!
//! Every replica that has seen the same updates holds the same JSON, whatever
//! the order the updates came in, which is what makes offline edits merge on
//! reconnect.

use std::cmp::Ordering;

use serde_json::{Map as JsonMap, Value};
use yrs::types::ToJson as _;
use yrs::updates::decoder::Decode as _;
use yrs::updates::encoder::Encode as _;
use yrs::{
    Any, Array as _, ArrayPrelim, ArrayRef, ClientID, Doc, GetString as _, Map as _, MapPrelim, MapRef, OffsetKind,
    Options, Out, ReadTxn as _, StateVector, Text as _, TextPrelim, TextRef, Transact as _, TransactionMut, Update,
};

use crate::error::{CollabError, Result};

/// The root map of a project document.
pub const ROOT: &str = "project";
/// The largest integer a JavaScript number holds exactly (2^53 - 1).
const MAX_SAFE_INTEGER: u64 = (1 << 53) - 1;
/// The deepest nesting of objects and lists a document may hold.
pub const MAX_DEPTH: usize = 48;
/// Above this many cells (`old len * new len`) lists are matched by their
/// common prefix and suffix only.
const MAX_LCS_CELLS: usize = 250_000;

/// Keys whose string values are prose that people edit together.
const TEXT_KEYS: &[&str] = &[
    "prompt",
    "system",
    "system_prompt",
    "instruction",
    "instructions",
    "template",
    "text",
    "query",
    "content",
    "description",
    "desc",
    "notes",
    "note",
    "body",
    "message",
    "answer",
];

/// Keys written first when a section is turned back into JSON, in this order
/// (the rest follow alphabetically), so a workflow reads as people wrote it.
const LEADING_KEYS: &[&str] = &[
    "version",
    "id",
    "name",
    "type",
    "kind",
    "title",
    "description",
    "env",
    "conversation",
    "graph",
    "nodes",
    "edges",
    "source",
    "target",
    "handle",
];

/// The shared document of one project.
#[derive(Debug, Clone)]
pub struct ProjectDoc {
    doc: Doc,
    root: MapRef,
}

impl Default for ProjectDoc {
    fn default() -> Self {
        Self::new()
    }
}

impl ProjectDoc {
    /// An empty document with a random client id.
    pub fn new() -> Self {
        Self::with_client(ClientID::random())
    }

    /// An empty document whose edits carry `client_id` (53 bits at most, like Yjs).
    pub fn with_client_id(client_id: u64) -> Self {
        Self::with_client(ClientID::new(client_id & ((1u64 << 53) - 1)))
    }

    fn with_client(client_id: ClientID) -> Self {
        let mut options = Options::with_client_id(client_id);
        // Text offsets in UTF-16 code units, as Yjs counts them, so web editors can join.
        options.offset_kind = OffsetKind::Utf16;
        let doc = Doc::with_options(options);
        let root = doc.get_or_insert_map(ROOT);
        Self { doc, root }
    }

    /// A document holding the state `update` encodes (from [`Self::encode_state`]).
    ///
    /// @throws CollabError::Update - When `update` is not a valid update.
    pub fn from_state(update: &[u8]) -> Result<Self> {
        let doc = Self::new();
        doc.apply_update(update)?;
        Ok(doc)
    }

    /// The underlying CRDT document.
    pub fn doc(&self) -> &Doc {
        &self.doc
    }

    /// The client id of this replica's edits.
    pub fn client_id(&self) -> u64 {
        self.doc.client_id().get()
    }

    /// The whole project as a JSON object of sections.
    pub fn to_json(&self) -> Value {
        let txn = self.doc.transact();
        map_to_json(&txn, &self.root, 0)
    }

    /// One section, `None` when the project has none of that name.
    pub fn section(&self, name: &str) -> Option<Value> {
        let txn = self.doc.transact();
        self.root.get(&txn, name).map(|out| out_to_json(&txn, out, 0))
    }

    /// Make the project equal `project` (sections it lacks are removed).
    ///
    /// @returns The update that carries the change to other replicas, `None` when nothing changed.
    /// @throws CollabError::Invalid - When `project` nests deeper than [`MAX_DEPTH`].
    pub fn apply_json(&self, project: &JsonMap<String, Value>) -> Result<Option<Vec<u8>>> {
        check_depth_map(project, 0)?;
        let mut txn = self.doc.transact_mut();
        let changed = reconcile_map(&mut txn, &self.root, project, 0);
        Ok(changed.then(|| txn.encode_update_v1()))
    }

    /// Set one section to `value`, leaving the others alone.
    ///
    /// @returns The update, `None` when nothing changed.
    /// @throws CollabError::Invalid - When `value` nests deeper than [`MAX_DEPTH`].
    pub fn set_section(&self, name: &str, value: &Value) -> Result<Option<Vec<u8>>> {
        check_depth(value, 1)?;
        let mut txn = self.doc.transact_mut();
        let changed = reconcile_key(&mut txn, &self.root, name, value, 1);
        Ok(changed.then(|| txn.encode_update_v1()))
    }

    /// Remove one section.
    ///
    /// @returns The update, `None` when there was no such section.
    pub fn remove_section(&self, name: &str) -> Option<Vec<u8>> {
        let mut txn = self.doc.transact_mut();
        self.root.remove(&mut txn, name).map(|_| txn.encode_update_v1())
    }

    /// The state vector (what this replica has seen), encoded.
    pub fn state_vector(&self) -> Vec<u8> {
        self.doc.transact().state_vector().encode_v1()
    }

    /// The whole document as one update (to store it, or to send it to a new replica).
    pub fn encode_state(&self) -> Vec<u8> {
        self.doc.transact().encode_state_as_update_v1(&StateVector::default())
    }

    /// What this replica has that the one with `state_vector` lacks.
    ///
    /// @throws CollabError::Update - When `state_vector` cannot be decoded.
    pub fn diff(&self, state_vector: &[u8]) -> Result<Vec<u8>> {
        let sv = StateVector::decode_v1(state_vector).map_err(|error| CollabError::Update(error.to_string()))?;
        Ok(self.doc.transact().encode_diff_v1(&sv))
    }

    /// Apply an update of another replica.
    ///
    /// @throws CollabError::Update - When `update` cannot be decoded or applied.
    pub fn apply_update(&self, update: &[u8]) -> Result<()> {
        let update = Update::decode_v1(update).map_err(|error| CollabError::Update(error.to_string()))?;
        self.doc.transact_mut().apply_update(update).map_err(|error| CollabError::Update(error.to_string()))
    }

    /// Whether `update` holds insertions this replica does not have yet.
    /// (Deletions are not counted: every diff repeats the deletions it knows.)
    ///
    /// @throws CollabError::Update - When `update` cannot be decoded.
    pub fn is_new(&self, update: &[u8]) -> Result<bool> {
        let update = Update::decode_v1(update).map_err(|error| CollabError::Update(error.to_string()))?;
        let sv = self.doc.transact().state_vector();
        Ok(!update.is_empty() && update.extends(&sv))
    }

    /// Whether this replica has updates it cannot integrate yet (their causes are missing).
    pub fn has_pending(&self) -> bool {
        let txn = self.doc.transact();
        txn.has_missing_updates()
    }
}

/// Fail when `value` nests deeper than [`MAX_DEPTH`].
fn check_depth(value: &Value, depth: usize) -> Result<()> {
    if depth > MAX_DEPTH {
        return Err(CollabError::Invalid(format!("the project nests deeper than {MAX_DEPTH} levels")));
    }
    match value {
        Value::Object(map) => check_depth_map(map, depth),
        Value::Array(items) => items.iter().try_for_each(|item| check_depth(item, depth + 1)),
        _ => Ok(()),
    }
}

fn check_depth_map(map: &JsonMap<String, Value>, depth: usize) -> Result<()> {
    map.values().try_for_each(|value| check_depth(value, depth + 1))
}

/// Whether a string under `key` is prose that should merge character by character.
fn is_text(key: Option<&str>, value: &str) -> bool {
    value.contains('\n') || key.is_some_and(|key| TEXT_KEYS.contains(&key))
}

/// Make `map` equal `new`.
fn reconcile_map(txn: &mut TransactionMut<'_>, map: &MapRef, new: &JsonMap<String, Value>, depth: usize) -> bool {
    let stale: Vec<String> = map.keys(txn).filter(|key| !new.contains_key(*key)).map(str::to_owned).collect();
    let mut changed = !stale.is_empty();
    for key in stale {
        map.remove(txn, &key);
    }
    for (key, value) in new {
        changed |= reconcile_key(txn, map, key, value, depth + 1);
    }
    changed
}

/// Make `map[key]` equal `value`, editing what is there when it has the same shape.
fn reconcile_key(txn: &mut TransactionMut<'_>, map: &MapRef, key: &str, value: &Value, depth: usize) -> bool {
    match (map.get(txn, key), value) {
        (Some(Out::YMap(inner)), Value::Object(object)) => reconcile_map(txn, &inner, object, depth),
        (Some(Out::YArray(inner)), Value::Array(items)) => reconcile_array(txn, &inner, items, depth),
        (Some(Out::YText(inner)), Value::String(text)) if is_text(Some(key), text) => reconcile_text(txn, &inner, text),
        (Some(Out::Any(any)), _) if any_equals(&any, value) => false,
        _ => {
            insert_in_map(txn, map, key, value);
            true
        }
    }
}

fn insert_in_map(txn: &mut TransactionMut<'_>, map: &MapRef, key: &str, value: &Value) {
    match value {
        Value::Object(object) => {
            let inner = map.insert(txn, key, MapPrelim::default());
            fill_map(txn, &inner, object);
        }
        Value::Array(items) => {
            let inner = map.insert(txn, key, ArrayPrelim::default());
            fill_array(txn, &inner, items);
        }
        Value::String(text) if is_text(Some(key), text) => {
            map.insert(txn, key, TextPrelim::new(text.as_str()));
        }
        scalar => {
            map.insert(txn, key, to_any(scalar));
        }
    }
}

fn fill_map(txn: &mut TransactionMut<'_>, map: &MapRef, object: &JsonMap<String, Value>) {
    for (key, value) in object {
        insert_in_map(txn, map, key, value);
    }
}

fn insert_in_array(txn: &mut TransactionMut<'_>, array: &ArrayRef, index: u32, value: &Value) {
    match value {
        Value::Object(object) => {
            let inner = array.insert(txn, index, MapPrelim::default());
            fill_map(txn, &inner, object);
        }
        Value::Array(items) => {
            let inner = array.insert(txn, index, ArrayPrelim::default());
            fill_array(txn, &inner, items);
        }
        Value::String(text) if is_text(None, text) => {
            array.insert(txn, index, TextPrelim::new(text.as_str()));
        }
        scalar => {
            array.insert(txn, index, to_any(scalar));
        }
    }
}

fn fill_array(txn: &mut TransactionMut<'_>, array: &ArrayRef, items: &[Value]) {
    for (index, item) in items.iter().enumerate() {
        insert_in_array(txn, array, u32::try_from(index).unwrap_or(u32::MAX), item);
    }
}

/// The key that identifies the items of a list of objects: `id`, else `name`,
/// when every item of both lists has a distinct string value for it.
fn identity_key(old: &[Value], new: &[Value]) -> Option<&'static str> {
    ["id", "name"].into_iter().find(|key| distinct_strings(old, key) && distinct_strings(new, key))
}

fn distinct_strings(items: &[Value], key: &str) -> bool {
    let mut seen = std::collections::HashSet::new();
    items.iter().all(|item| item.get(key).and_then(Value::as_str).is_some_and(|value| seen.insert(value)))
}

/// Make `array` equal `new`.
fn reconcile_array(txn: &mut TransactionMut<'_>, array: &ArrayRef, new: &[Value], depth: usize) -> bool {
    let old: Vec<Value> = array.iter(txn).map(|out| out_to_json(txn, out, depth)).collect();
    let identity = if old.is_empty() || new.is_empty() { None } else { identity_key(&old, new) };
    let same = |a: &Value, b: &Value| match identity {
        Some(key) => a.get(key) == b.get(key),
        None => a == b,
    };
    let pairs = matching(&old, new, same);
    let mut changed = false;
    // Remove what is not kept, from the end so earlier indexes stay valid.
    let kept_old: std::collections::HashSet<usize> = pairs.iter().map(|(o, _)| *o).collect();
    for index in (0..old.len()).rev() {
        if !kept_old.contains(&index) {
            array.remove(txn, u32::try_from(index).unwrap_or(u32::MAX));
            changed = true;
        }
    }
    // Now the array holds the kept items in order: insert the new ones between them,
    // and edit the kept ones in place (they share only their identity).
    let kept_new: std::collections::HashMap<usize, usize> = pairs.iter().map(|(o, n)| (*n, *o)).collect();
    for (index, value) in new.iter().enumerate() {
        let at = u32::try_from(index).unwrap_or(u32::MAX);
        match kept_new.get(&index) {
            Some(&old_index) => {
                let unchanged = old.get(old_index).is_some_and(|previous| previous == value);
                if !unchanged {
                    changed |= reconcile_index(txn, array, at, value, depth + 1);
                }
            }
            None => {
                insert_in_array(txn, array, at, value);
                changed = true;
            }
        }
    }
    changed
}

/// Make `array[index]` equal `value`, editing it in place when it has the same shape.
fn reconcile_index(txn: &mut TransactionMut<'_>, array: &ArrayRef, index: u32, value: &Value, depth: usize) -> bool {
    match (array.get(txn, index), value) {
        (Some(Out::YMap(inner)), Value::Object(object)) => reconcile_map(txn, &inner, object, depth),
        (Some(Out::YArray(inner)), Value::Array(items)) => reconcile_array(txn, &inner, items, depth),
        (Some(Out::YText(inner)), Value::String(text)) if is_text(None, text) => reconcile_text(txn, &inner, text),
        (Some(Out::Any(any)), _) if any_equals(&any, value) => false,
        _ => {
            array.remove(txn, index);
            insert_in_array(txn, array, index, value);
            true
        }
    }
}

/// Pairs `(old index, new index)` of the items kept, in order: the longest
/// common subsequence under `same`, or the common prefix and suffix of very long lists.
fn matching(old: &[Value], new: &[Value], same: impl Fn(&Value, &Value) -> bool) -> Vec<(usize, usize)> {
    let prefix = old.iter().zip(new).take_while(|(a, b)| same(a, b)).count();
    let suffix = old
        .get(prefix..)
        .unwrap_or_default()
        .iter()
        .rev()
        .zip(new.get(prefix..).unwrap_or_default().iter().rev())
        .take_while(|(a, b)| same(a, b))
        .count();
    let mut pairs: Vec<(usize, usize)> = (0..prefix).map(|i| (i, i)).collect();
    let old_mid = old.get(prefix..old.len() - suffix).unwrap_or_default();
    let new_mid = new.get(prefix..new.len() - suffix).unwrap_or_default();
    if !old_mid.is_empty() && !new_mid.is_empty() && old_mid.len().saturating_mul(new_mid.len()) <= MAX_LCS_CELLS {
        pairs.extend(lcs(old_mid, new_mid, &same).into_iter().map(|(o, n)| (o + prefix, n + prefix)));
    }
    pairs.extend((0..suffix).map(|i| (old.len() - suffix + i, new.len() - suffix + i)));
    pairs
}

/// The longest common subsequence of `a` and `b` as index pairs.
fn lcs(a: &[Value], b: &[Value], same: &impl Fn(&Value, &Value) -> bool) -> Vec<(usize, usize)> {
    let (n, m) = (a.len(), b.len());
    // lengths[i][j]: LCS length of a[i..] and b[j..], flattened.
    let mut lengths = vec![0u32; (n + 1) * (m + 1)];
    let at = |i: usize, j: usize| i * (m + 1) + j;
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            let value = match (a.get(i), b.get(j)) {
                (Some(x), Some(y)) if same(x, y) => lengths.get(at(i + 1, j + 1)).copied().unwrap_or(0) + 1,
                _ => {
                    let down = lengths.get(at(i + 1, j)).copied().unwrap_or(0);
                    let right = lengths.get(at(i, j + 1)).copied().unwrap_or(0);
                    down.max(right)
                }
            };
            if let Some(cell) = lengths.get_mut(at(i, j)) {
                *cell = value;
            }
        }
    }
    let (mut i, mut j) = (0, 0);
    let mut pairs = Vec::new();
    while i < n && j < m {
        match (a.get(i), b.get(j)) {
            (Some(x), Some(y)) if same(x, y) => {
                pairs.push((i, j));
                i += 1;
                j += 1;
            }
            _ => {
                let down = lengths.get(at(i + 1, j)).copied().unwrap_or(0);
                let right = lengths.get(at(i, j + 1)).copied().unwrap_or(0);
                if down >= right {
                    i += 1;
                } else {
                    j += 1;
                }
            }
        }
    }
    pairs
}

/// Make `text` equal `new` by deleting and inserting only the part that differs.
fn reconcile_text(txn: &mut TransactionMut<'_>, text: &TextRef, new: &str) -> bool {
    let old = text.get_string(txn);
    if old == new {
        return false;
    }
    let old_chars: Vec<char> = old.chars().collect();
    let new_chars: Vec<char> = new.chars().collect();
    let prefix = old_chars.iter().zip(&new_chars).take_while(|(a, b)| a == b).count();
    let max_suffix = old_chars.len().min(new_chars.len()) - prefix;
    let suffix =
        old_chars.iter().rev().zip(new_chars.iter().rev()).take(max_suffix).take_while(|(a, b)| a == b).count();
    let utf16 = |chars: &[char]| chars.iter().map(|c| c.len_utf16()).sum::<usize>();
    let start = utf16(old_chars.get(..prefix).unwrap_or_default());
    let removed = utf16(old_chars.get(prefix..old_chars.len() - suffix).unwrap_or_default());
    let inserted: String = new_chars.get(prefix..new_chars.len() - suffix).unwrap_or_default().iter().collect();
    let start = u32::try_from(start).unwrap_or(u32::MAX);
    if removed > 0 {
        text.remove_range(txn, start, u32::try_from(removed).unwrap_or(u32::MAX));
    }
    if !inserted.is_empty() {
        text.insert(txn, start, &inserted);
    }
    true
}

fn to_any(value: &Value) -> Any {
    match value {
        Value::Null => Any::Null,
        Value::Bool(flag) => Any::Bool(*flag),
        // Integers JavaScript holds exactly are numbers (as Yjs writes them), larger ones big integers.
        Value::Number(number) => match number.as_i64() {
            Some(int) if int.unsigned_abs() <= MAX_SAFE_INTEGER => Any::Number(int as f64),
            Some(int) => Any::BigInt(int),
            None => Any::Number(number.as_f64().unwrap_or(0.0)),
        },
        Value::String(text) => Any::String(text.as_str().into()),
        Value::Array(items) => Any::Array(items.iter().map(to_any).collect()),
        Value::Object(map) => Any::Map(std::sync::Arc::new(map.iter().map(|(k, v)| (k.clone(), to_any(v))).collect())),
    }
}

fn any_to_json(any: &Any) -> Value {
    match any {
        Any::Null | Any::Undefined => Value::Null,
        Any::Bool(flag) => Value::Bool(*flag),
        Any::BigInt(int) => Value::from(*int),
        Any::Number(float) => {
            if float.fract() == 0.0 && float.abs() <= MAX_SAFE_INTEGER as f64 {
                Value::from(*float as i64)
            } else {
                serde_json::Number::from_f64(*float).map_or(Value::Null, Value::Number)
            }
        }
        Any::String(text) => Value::String(text.to_string()),
        // Binary content never comes from this crate's writers.
        Any::Buffer(_) => Value::Null,
        Any::Array(items) => Value::Array(items.iter().map(any_to_json).collect()),
        Any::Map(map) => {
            let mut object: Vec<(&String, &Any)> = map.iter().collect();
            object.sort_by(|(a, _), (b, _)| key_order(a, b));
            Value::Object(object.into_iter().map(|(k, v)| (k.clone(), any_to_json(v))).collect())
        }
    }
}

fn any_equals(any: &Any, value: &Value) -> bool {
    !value.is_object() && !value.is_array() && any_to_json(any) == *value
}

fn key_order(a: &str, b: &str) -> Ordering {
    let rank = |key: &str| LEADING_KEYS.iter().position(|leading| *leading == key).unwrap_or(LEADING_KEYS.len());
    rank(a).cmp(&rank(b)).then_with(|| a.cmp(b))
}

fn map_to_json<T: yrs::ReadTxn>(txn: &T, map: &MapRef, depth: usize) -> Value {
    let mut entries: Vec<(String, Out)> = map.iter(txn).map(|(key, out)| (key.to_owned(), out)).collect();
    entries.sort_by(|(a, _), (b, _)| key_order(a, b));
    Value::Object(entries.into_iter().map(|(key, out)| (key, out_to_json(txn, out, depth + 1))).collect())
}

fn out_to_json<T: yrs::ReadTxn>(txn: &T, out: Out, depth: usize) -> Value {
    if depth > MAX_DEPTH * 2 {
        return Value::Null;
    }
    match out {
        Out::Any(any) => any_to_json(&any),
        Out::YText(text) => Value::String(text.get_string(txn)),
        Out::YMap(map) => map_to_json(txn, &map, depth),
        Out::YArray(array) => Value::Array(array.iter(txn).map(|item| out_to_json(txn, item, depth + 1)).collect()),
        // Other shared types (XML, sub documents) are not part of a project: read them as JSON.
        other => any_to_json(&other.to_json(txn)),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn object(value: Value) -> JsonMap<String, Value> {
        match value {
            Value::Object(map) => map,
            _ => panic!("not an object"),
        }
    }

    /// Two replicas with the same starting point.
    fn pair(start: Value) -> (ProjectDoc, ProjectDoc) {
        let a = ProjectDoc::with_client_id(1);
        a.apply_json(&object(start)).unwrap();
        let b = ProjectDoc::with_client_id(2);
        b.apply_update(&a.encode_state()).unwrap();
        (a, b)
    }

    fn exchange(a: &ProjectDoc, b: &ProjectDoc) {
        let to_b = a.diff(&b.state_vector()).unwrap();
        let to_a = b.diff(&a.state_vector()).unwrap();
        b.apply_update(&to_b).unwrap();
        a.apply_update(&to_a).unwrap();
    }

    fn flow() -> Value {
        json!({
            "name": "Support bot",
            "flow": {
                "version": "1",
                "graph": {
                    "nodes": [
                        {"id": "start", "type": "start"},
                        {"id": "llm", "type": "llm", "prompt": "You answer support questions."},
                        {"id": "end", "type": "end"}
                    ],
                    "edges": [
                        {"source": "start", "target": "llm"},
                        {"source": "llm", "target": "end"}
                    ]
                }
            },
            "layout": {"start": {"x": 0, "y": 0}, "llm": {"x": 200, "y": 0}}
        })
    }

    #[test]
    fn round_trips_json() {
        let doc = ProjectDoc::new();
        let value = flow();
        assert!(doc.apply_json(&object(value.clone())).unwrap().is_some());
        assert_eq!(doc.to_json(), value);
        // The same JSON again changes nothing.
        assert!(doc.apply_json(&object(value)).unwrap().is_none());
    }

    #[test]
    fn keeps_numbers_bools_and_nulls() {
        let doc = ProjectDoc::new();
        let value = json!({"a": {"int": 3, "neg": -7, "float": 1.5, "t": true, "n": null, "list": [1, "x", false]}});
        doc.apply_json(&object(value.clone())).unwrap();
        assert_eq!(doc.to_json(), value);
        let copy = ProjectDoc::from_state(&doc.encode_state()).unwrap();
        assert_eq!(copy.to_json(), value);
    }

    #[test]
    fn concurrent_edits_of_different_nodes_merge() {
        let (a, b) = pair(flow());
        let mut on_a = flow();
        on_a["flow"]["graph"]["nodes"][1]["prompt"] = json!("You answer billing questions.");
        let mut on_b = flow();
        on_b["flow"]["graph"]["nodes"].as_array_mut().unwrap().insert(2, json!({"id": "tool", "type": "http"}));
        on_b["layout"]["tool"] = json!({"x": 400, "y": 0});
        a.apply_json(&object(on_a)).unwrap();
        b.apply_json(&object(on_b)).unwrap();
        exchange(&a, &b);
        assert_eq!(a.to_json(), b.to_json());
        let merged = a.to_json();
        assert_eq!(merged["flow"]["graph"]["nodes"][1]["prompt"], "You answer billing questions.");
        assert_eq!(merged["flow"]["graph"]["nodes"][2]["id"], "tool");
        assert_eq!(merged["flow"]["graph"]["nodes"].as_array().unwrap().len(), 4);
        assert_eq!(merged["layout"]["tool"]["x"], 400);
    }

    #[test]
    fn concurrent_typing_in_one_prompt_merges_characters() {
        let (a, b) = pair(flow());
        let mut on_a = flow();
        on_a["flow"]["graph"]["nodes"][1]["prompt"] = json!("Politely, you answer support questions.");
        let mut on_b = flow();
        on_b["flow"]["graph"]["nodes"][1]["prompt"] = json!("You answer support questions in Indonesian.");
        a.apply_json(&object(on_a)).unwrap();
        b.apply_json(&object(on_b)).unwrap();
        exchange(&a, &b);
        assert_eq!(a.to_json(), b.to_json());
        assert_eq!(
            a.to_json()["flow"]["graph"]["nodes"][1]["prompt"],
            "Politely, you answer support questions in Indonesian."
        );
    }

    #[test]
    fn same_key_edited_twice_converges() {
        let (a, b) = pair(json!({"name": "One"}));
        a.apply_json(&object(json!({"name": "Two"}))).unwrap();
        b.apply_json(&object(json!({"name": "Three"}))).unwrap();
        exchange(&a, &b);
        assert_eq!(a.to_json(), b.to_json());
        let name = a.to_json()["name"].clone();
        assert!(name == "Two" || name == "Three");
    }

    #[test]
    fn delete_and_add_nodes_concurrently() {
        let (a, b) = pair(flow());
        let mut on_a = flow();
        on_a["flow"]["graph"]["nodes"].as_array_mut().unwrap().remove(1);
        on_a["flow"]["graph"]["edges"] = json!([{"source": "start", "target": "end"}]);
        let mut on_b = flow();
        on_b["flow"]["graph"]["nodes"].as_array_mut().unwrap().push(json!({"id": "notify", "type": "http"}));
        a.apply_json(&object(on_a)).unwrap();
        b.apply_json(&object(on_b)).unwrap();
        exchange(&a, &b);
        let merged = a.to_json();
        assert_eq!(merged, b.to_json());
        let ids: Vec<&str> =
            merged["flow"]["graph"]["nodes"].as_array().unwrap().iter().filter_map(|n| n["id"].as_str()).collect();
        assert_eq!(ids, ["start", "end", "notify"]);
    }

    #[test]
    fn offline_edits_merge_in_any_order() {
        let (a, b) = pair(flow());
        let c = ProjectDoc::with_client_id(3);
        c.apply_update(&a.encode_state()).unwrap();
        let mut one = flow();
        one["name"] = json!("Renamed");
        let mut two = flow();
        two["layout"]["llm"]["y"] = json!(120);
        let u1 = a.apply_json(&object(one)).unwrap().unwrap();
        let u2 = b.apply_json(&object(two)).unwrap().unwrap();
        // c gets them in the opposite order.
        c.apply_update(&u2).unwrap();
        c.apply_update(&u1).unwrap();
        exchange(&a, &b);
        assert_eq!(a.to_json(), c.to_json());
        assert_eq!(c.to_json()["name"], "Renamed");
        assert_eq!(c.to_json()["layout"]["llm"]["y"], 120);
    }

    #[test]
    fn text_edits_count_utf16_units() {
        let (a, b) = pair(json!({"notes": "héllo 👋 wörld"}));
        a.apply_json(&object(json!({"notes": "héllo 👋 dear wörld"}))).unwrap();
        b.apply_json(&object(json!({"notes": "héllo 👋 wörld!"}))).unwrap();
        exchange(&a, &b);
        assert_eq!(a.to_json()["notes"], "héllo 👋 dear wörld!");
        assert_eq!(b.to_json()["notes"], "héllo 👋 dear wörld!");
    }

    #[test]
    fn lists_without_ids_match_by_value() {
        let doc = ProjectDoc::new();
        doc.apply_json(&object(json!({"tags": ["a", "b", "c"]}))).unwrap();
        doc.apply_json(&object(json!({"tags": ["a", "x", "c", "d"]}))).unwrap();
        assert_eq!(doc.to_json()["tags"], json!(["a", "x", "c", "d"]));
    }

    #[test]
    fn shape_changes_replace_the_value() {
        let doc = ProjectDoc::new();
        doc.apply_json(&object(json!({"x": {"a": 1}, "y": [1], "z": "s"}))).unwrap();
        doc.apply_json(&object(json!({"x": [1, 2], "y": {"b": 2}, "z": 5}))).unwrap();
        assert_eq!(doc.to_json(), json!({"x": [1, 2], "y": {"b": 2}, "z": 5}));
    }

    #[test]
    fn sections_are_set_and_removed_alone() {
        let doc = ProjectDoc::new();
        doc.apply_json(&object(flow())).unwrap();
        doc.set_section("name", &json!("Other")).unwrap();
        assert_eq!(doc.section("name"), Some(json!("Other")));
        assert!(doc.section("flow").is_some());
        assert!(doc.remove_section("layout").is_some());
        assert!(doc.section("layout").is_none());
        assert!(doc.remove_section("layout").is_none());
    }

    #[test]
    fn refuses_deep_nesting() {
        let mut value = json!(1);
        for _ in 0..(MAX_DEPTH + 2) {
            value = json!({ "a": value });
        }
        let doc = ProjectDoc::new();
        assert!(matches!(doc.apply_json(&object(json!({"deep": value}))), Err(CollabError::Invalid(_))));
    }

    #[test]
    fn refuses_garbage_updates() {
        let doc = ProjectDoc::new();
        assert!(doc.apply_update(&[0xff, 0xff, 0xff, 0x01]).is_err());
        assert!(doc.diff(&[0xff, 0xff]).is_err());
    }
}
