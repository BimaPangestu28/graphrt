//! The memory graph (cargo feature `graph`): entities and the relations
//! between them, each with the period in which it was true, built from the
//! notes of a scope and searched a few hops wide.
//!
//! A relation that a newer one contradicts is closed, never deleted, so the
//! graph answers what was true at a given time. A relation keeps the ids of
//! the notes it was read from and takes their status: it waits for review
//! while its note does, and goes when its last note goes.

use std::collections::{HashMap, HashSet, VecDeque};

use graphrt_core::memory::{ExtractedFacts, ExtractedRelation, MemoryRecord, MemoryStatus, MemoryTrust};
use serde::{Deserialize, Serialize};
use serde_json::{Value as Json, json};

use crate::gate::{clean_text, mask_secrets};
use crate::search::{Index, words};

/// Bounds of one scope's graph.
pub(crate) const MAX_ENTITIES: usize = 2_000;
pub(crate) const MAX_RELATIONS: usize = 10_000;
const NAME_CHARS: usize = 200;
const TEXT_CHARS: usize = 500;
const RELATION_CHARS: usize = 40;
/// Entities and relations one note may add.
const PER_NOTE: usize = 32;
/// Hops of a search, and relations followed from one entity.
const HOPS: usize = 2;
const PER_ENTITY: usize = 20;
/// Two names this alike (character trigrams) name the same entity.
const SAME_NAME: f64 = 0.9;

/// Relations that hold one value at a time: a new one closes the one before.
const ONE_AT_A_TIME: [&str; 14] = [
    "works_at",
    "employed_by",
    "lives_in",
    "located_in",
    "based_in",
    "married_to",
    "reports_to",
    "has_plan",
    "on_plan",
    "role",
    "job_title",
    "email",
    "phone",
    "address",
];

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct GraphFile {
    pub(crate) format: u32,
    #[serde(default)]
    pub(crate) entities: Vec<Entity>,
    #[serde(default)]
    pub(crate) relations: Vec<Relation>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Entity {
    pub(crate) id: String,
    pub(crate) name: String,
    #[serde(rename = "type")]
    pub(crate) kind: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) aliases: Vec<String>,
    pub(crate) created_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Relation {
    pub(crate) id: String,
    pub(crate) source: String,
    pub(crate) relation: String,
    pub(crate) target: String,
    pub(crate) text: String,
    /// When it was true in the world, as the text said.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) valid_from_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) valid_to_ms: Option<u64>,
    /// When the store learned it, and when a newer relation retired it.
    pub(crate) recorded_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) closed_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) closed_by: Option<String>,
    /// The notes it was read from.
    pub(crate) notes: Vec<String>,
    pub(crate) trust: MemoryTrust,
    pub(crate) status: MemoryStatus,
    /// The text said it replaces what was true before.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(crate) replaces: bool,
    /// When the note it was first read from was written: the order of two
    /// relations without dates.
    #[serde(default)]
    pub(crate) noted_ms: u64,
    /// What set `valid_to_ms` (a relation that replaced this one, or a note
    /// that said it ended), so the end goes when that goes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) end_set_by: Option<String>,
    /// An end a note waiting for review states: applied when it is approved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) pending_end: Option<PendingEnd>,
    /// The trust of each note it stands on: the relation is as trusted as the
    /// most trusted note left.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub(crate) support: std::collections::BTreeMap<String, MemoryTrust>,
}

impl Relation {
    /// A relation of a file written before `support` existed: every note it
    /// has counts at the relation's trust.
    fn seed_support(&mut self) {
        if self.support.is_empty() {
            for note in &self.notes {
                self.support.insert(note.clone(), self.trust);
            }
        }
    }

    /// Count `note` as saying this relation.
    fn add_note(&mut self, note: &MemoryRecord) {
        self.seed_support();
        if !self.notes.contains(&note.id) {
            self.notes.push(note.id.clone());
        }
        self.support.insert(note.id.clone(), note.trust);
        self.trust = self.support.values().copied().max_by_key(|t| trust_rank(*t)).unwrap_or(self.trust);
    }

    /// Stop counting the notes of `gone`.
    fn drop_notes(&mut self, gone: &[String]) {
        self.seed_support();
        self.notes.retain(|note| !gone.contains(note));
        self.support.retain(|note, _| !gone.contains(note));
        if let Some(best) = self.support.values().copied().max_by_key(|t| trust_rank(*t)) {
            self.trust = best;
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PendingEnd {
    pub(crate) note: String,
    pub(crate) to_ms: u64,
}

impl Relation {
    /// When it started, for ordering: the date the text gave, else when its note was written.
    fn start(&self) -> u64 {
        self.valid_from_ms.unwrap_or(if self.noted_ms > 0 { self.noted_ms } else { self.recorded_ms })
    }

    /// Whether it was true at `at_ms`.
    fn valid_at(&self, at_ms: u64) -> bool {
        let from = self.valid_from_ms.unwrap_or(0);
        // Closed without an end in the world: true until it was closed.
        let to = self.valid_to_ms.or(self.closed_ms).unwrap_or(u64::MAX);
        from <= at_ms && at_ms < to
    }

    /// The sentence a recall shows, with its period.
    pub(crate) fn sentence(&self) -> String {
        let mut text = self.text.clone();
        match (self.valid_from_ms, self.valid_to_ms.or(self.closed_ms)) {
            (Some(from), Some(to)) => text.push_str(&format!(" (from {} until {})", month(from), month(to))),
            (Some(from), None) => text.push_str(&format!(" (since {})", month(from))),
            (None, Some(to)) => text.push_str(&format!(" (until {})", month(to))),
            (None, None) => {}
        }
        text
    }
}

/// `YYYY-MM` of a time in milliseconds.
fn month(ms: u64) -> String {
    let day = day_of(ms);
    day.get(..7).unwrap_or(&day).to_owned()
}

/// `YYYY-MM-DD` of a time in milliseconds since the epoch.
fn day_of(ms: u64) -> String {
    let days = i64::try_from(ms / 86_400_000).unwrap_or(0);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Milliseconds of `YYYY-MM-DD` (or `YYYY-MM`, the first of the month).
pub(crate) fn parse_day(text: &str) -> Option<u64> {
    let mut parts = text.trim().split('-');
    let y: i64 = parts.next()?.parse().ok()?;
    let m: i64 = parts.next()?.parse().ok()?;
    let d: i64 = parts.next().map_or(Some(1), |d| d.get(..2.min(d.len())).and_then(|d| d.parse().ok()))?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) || !(1900..=2200).contains(&y) {
        return None;
    }
    // Days from civil, after Howard Hinnant.
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    u64::try_from(days).ok().map(|days| days * 86_400_000)
}

fn trigrams(text: &str) -> HashSet<String> {
    let padded: Vec<char> = format!("  {}  ", text.to_lowercase()).chars().collect();
    padded.windows(3).map(|w| w.iter().collect()).collect()
}

fn similarity(a: &str, b: &str) -> f64 {
    let (a, b) = (trigrams(a), trigrams(b));
    let shared = a.intersection(&b).count();
    if shared == 0 { 0.0 } else { (2 * shared) as f64 / (a.len() + b.len()) as f64 }
}

/// A name as an entity keeps it: cleaned, masked, bounded.
fn clean_name(name: &str) -> String {
    let masked = mask_secrets(&clean_text(name), &[]).text;
    masked
        .chars()
        .filter(|c| !matches!(c, '<' | '>' | '[' | ']'))
        .take(NAME_CHARS)
        .collect::<String>()
        .trim()
        .to_owned()
}

/// A relation name as the graph keeps it: `works_at`.
fn clean_relation(relation: &str) -> String {
    let mut out = String::new();
    for c in relation.trim().chars().flat_map(char::to_lowercase) {
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else if !out.ends_with('_') && !out.is_empty() {
            out.push('_');
        }
    }
    out.trim_end_matches('_').chars().take(RELATION_CHARS).collect()
}

fn clean_kind(kind: &str) -> String {
    match kind.trim().to_lowercase().as_str() {
        k @ ("person" | "organization" | "place" | "product" | "event") => k.to_owned(),
        _ => "other".to_owned(),
    }
}

fn key(name: &str) -> String {
    words(name).join(" ")
}

/// Keyword indexes of a graph: its entities (names and aliases) and its
/// relations (their text), in file order. Built once per version of the file.
#[derive(Debug, Default)]
pub(crate) struct GraphIndex {
    entities: Index,
    relations: Index,
}

impl GraphIndex {
    pub(crate) fn build(graph: &GraphFile) -> Self {
        let entity_docs: Vec<String> =
            graph.entities.iter().map(|e| format!("{} {}", e.name, e.aliases.join(" "))).collect();
        Self {
            entities: Index::build(entity_docs.iter().map(String::as_str)),
            relations: Index::build(graph.relations.iter().map(|r| r.text.as_str())),
        }
    }
}

impl GraphFile {
    /// The entity `name` means: the same name or alias, else a name nearly the
    /// same with the same type (kept as an alias), else a new entity.
    ///
    /// A note that waits for review (`may_change` false) never changes an
    /// entity in use: it adds no alias and no type to one.
    fn resolve(
        &mut self,
        name: &str,
        kind: &str,
        aliases: &[String],
        may_change: bool,
        now: u64,
        id: &mut dyn FnMut() -> String,
    ) -> Option<String> {
        let wanted = key(name);
        if wanted.is_empty() {
            return None;
        }
        let same = self.entities.iter().position(|e| {
            key(&e.name) == wanted
                || e.aliases.iter().any(|a| key(a) == wanted)
                || aliases.iter().any(|a| key(a) == key(&e.name))
        });
        let near = || {
            self.entities
                .iter()
                .position(|e| (kind == "other" || e.kind == kind) && similarity(&e.name, name) >= SAME_NAME)
        };
        if let Some(at) = same.or_else(near) {
            let entity = self.entities.get_mut(at)?;
            if !may_change {
                return Some(entity.id.clone());
            }
            for alias in std::iter::once(name.to_owned()).chain(aliases.iter().map(|a| clean_name(a))) {
                if !alias.is_empty()
                    && key(&alias) != key(&entity.name)
                    && !entity.aliases.iter().any(|a| key(a) == key(&alias))
                    && entity.aliases.len() < 8
                {
                    entity.aliases.push(alias);
                }
            }
            if entity.kind == "other" && kind != "other" {
                entity.kind = kind.to_owned();
            }
            return Some(entity.id.clone());
        }
        if self.entities.len() >= MAX_ENTITIES {
            return None;
        }
        let entity = Entity {
            id: id(),
            name: name.to_owned(),
            kind: kind.to_owned(),
            aliases: aliases
                .iter()
                .map(|a| clean_name(a))
                .filter(|a| !a.is_empty() && key(a) != wanted)
                .take(8)
                .collect(),
            created_ms: now,
        };
        let entity_id = entity.id.clone();
        self.entities.push(entity);
        Some(entity_id)
    }

    fn name_of(&self, id: &str) -> &str {
        self.entities.iter().find(|e| e.id == id).map_or("?", |e| e.name.as_str())
    }

    /// Add `facts` read from `note`. Returns how many relations are new, and the audit lines.
    pub(crate) fn link(
        &mut self,
        note: &MemoryRecord,
        facts: &ExtractedFacts,
        now: u64,
        id: &mut dyn FnMut() -> String,
    ) -> (usize, Vec<Json>) {
        let mut added = 0;
        let mut lines = Vec::new();
        let active = note.status == MemoryStatus::Active;
        for entity in facts.entities.iter().take(PER_NOTE) {
            let name = clean_name(&entity.name);
            if !name.is_empty() {
                let _ = self.resolve(&name, &clean_kind(&entity.kind), &entity.aliases, active, now, id);
            }
        }
        // What the text says has ended first: it gives its end to the open
        // relation, before a new one of the same kind could close that one.
        let mut ordered: Vec<&ExtractedRelation> = facts.relations.iter().take(PER_NOTE).collect();
        ordered.sort_by_key(|r| r.valid_to.is_none());
        for relation in ordered {
            let (source_name, target_name) = (clean_name(&relation.source), clean_name(&relation.target));
            let name = clean_relation(&relation.relation);
            if source_name.is_empty() || target_name.is_empty() || name.is_empty() {
                continue;
            }
            let kind_of = |n: &str| {
                facts
                    .entities
                    .iter()
                    .find(|e| key(&clean_name(&e.name)) == key(n))
                    .map_or_else(|| "other".to_owned(), |e| clean_kind(&e.kind))
            };
            let (source_kind, target_kind) = (kind_of(&source_name), kind_of(&target_name));
            let Some(source) = self.resolve(&source_name, &source_kind, &[], active, now, id) else { continue };
            let Some(target) = self.resolve(&target_name, &target_kind, &[], active, now, id) else { continue };
            let valid_from_ms = relation.valid_from.as_deref().and_then(parse_day);
            let valid_to_ms = relation.valid_to.as_deref().and_then(parse_day);
            // A note read again (a rebuild) adds nothing it already said, open or closed.
            if self
                .relations
                .iter()
                .any(|r| r.source == source && r.target == target && r.relation == name && r.notes.contains(&note.id))
            {
                continue;
            }
            let open_same = self
                .relations
                .iter_mut()
                .find(|r| r.source == source && r.target == target && r.relation == name && r.closed_ms.is_none());
            if let Some(existing) = open_same {
                let was_pending = existing.status == MemoryStatus::Pending;
                existing.add_note(note);
                // Another note that says it: saved, so the fact outlives the first one.
                lines.push(json!({"action": "graph_support", "id": existing.id, "note": note.id}));
                // The text says when it ended: the open relation gets its end, at
                // once from a note in use, on approval from one that waits.
                // Never from a writer of less trust than the relation's.
                if let Some(to) = valid_to_ms
                    && trust_rank(existing.trust) <= trust_rank(note.trust)
                {
                    if note.status == MemoryStatus::Active {
                        existing.valid_to_ms = Some(to);
                        existing.closed_ms = Some(now);
                        existing.end_set_by = Some(note.id.clone());
                        lines.push(json!({"action": "graph_close", "id": existing.id, "closed_by": note.id}));
                    } else {
                        existing.pending_end = Some(PendingEnd { note: note.id.clone(), to_ms: to });
                    }
                }
                // A note in use makes the relation in use and may date it. A note
                // that waits changes nothing in use.
                if note.status == MemoryStatus::Active {
                    existing.status = MemoryStatus::Active;
                    existing.valid_from_ms = existing.valid_from_ms.or(valid_from_ms);
                    // It just came into use: it replaces what it replaces now.
                    if was_pending {
                        let id = existing.id.clone();
                        lines.extend(self.close_before(&id, now));
                    }
                }
                continue;
            }
            // Relations that wait take at most a tenth of the room: a flood of
            // proposals cannot push out what is in use.
            let waiting = self.relations.iter().filter(|r| r.status == MemoryStatus::Pending).count();
            if self.relations.len() >= MAX_RELATIONS || (!active && waiting >= MAX_RELATIONS / 10) {
                lines.push(json!({"action": "graph_full", "note": note.id}));
                break;
            }
            let text: String =
                format!("{} {} {}", self.name_of(&source), name.replace('_', " "), self.name_of(&target))
                    .chars()
                    .take(TEXT_CHARS)
                    .collect();
            let relation_id = id();
            self.relations.push(Relation {
                id: relation_id.clone(),
                source,
                relation: name,
                target,
                text,
                valid_from_ms,
                valid_to_ms,
                recorded_ms: now,
                // What the text says has ended is history from the start.
                closed_ms: valid_to_ms.map(|_| now),
                closed_by: None,
                notes: vec![note.id.clone()],
                trust: note.trust,
                status: note.status,
                replaces: relation.replaces,
                noted_ms: note.created_ms,
                end_set_by: valid_to_ms.map(|_| note.id.clone()),
                pending_end: None,
                support: std::iter::once((note.id.clone(), note.trust)).collect(),
            });
            added += 1;
            lines.push(
                json!({"action": "graph_add", "id": relation_id, "note": note.id, "status": note.status.as_str()}),
            );
            if note.status == MemoryStatus::Active {
                lines.extend(self.close_before(&relation_id, now));
            }
        }
        (added, lines)
    }

    /// Close what relation `id` replaces: the open relations of the same
    /// source and name that hold one value at a time (or that it says it
    /// replaces), when they are not newer and not of more trust.
    fn close_before(&mut self, id: &str, now: u64) -> Vec<Json> {
        let Some(new) = self.relations.iter().find(|r| r.id == id).cloned() else { return Vec::new() };
        // Only what is true now replaces anything: a relation that ended is history.
        if new.valid_to_ms.is_some() || !(new.replaces || ONE_AT_A_TIME.contains(&new.relation.as_str())) {
            return Vec::new();
        }
        let mut lines = Vec::new();
        for old in self.relations.iter_mut() {
            let replaced = old.id != new.id
                && old.source == new.source
                && old.relation == new.relation
                && old.target != new.target
                && old.closed_ms.is_none()
                && old.status == MemoryStatus::Active
                && trust_rank(old.trust) <= trust_rank(new.trust)
                && old.start() <= new.start();
            if replaced {
                old.closed_ms = Some(now);
                old.closed_by = Some(new.id.clone());
                if old.valid_to_ms.is_none() {
                    old.valid_to_ms = Some(new.start());
                    old.end_set_by = Some(new.id.clone());
                }
                lines.push(json!({"action": "graph_close", "id": old.id, "closed_by": new.id}));
            }
        }
        lines
    }

    /// Follow what happened to notes: `gone` were removed, `undone` of them
    /// by a person (forget, reject), `approved` came into use.
    ///
    /// A relation whose last note went goes too. When a person removed it, what
    /// it had closed opens again (and is closed again by anything newer that
    /// replaces it). A note that merely aged out or made room leaves its
    /// closures: that history stays true.
    pub(crate) fn notes_changed(
        &mut self,
        gone: &[String],
        undone: &[String],
        approved: &[String],
        now: u64,
    ) -> Vec<Json> {
        let mut lines = Vec::new();
        if !gone.is_empty() {
            let mut undone_relations: Vec<String> = Vec::new();
            for relation in &mut self.relations {
                let had_undone = relation.notes.iter().any(|n| undone.contains(n));
                relation.drop_notes(gone);
                if relation.notes.is_empty() && had_undone {
                    undone_relations.push(relation.id.clone());
                }
                if relation.pending_end.as_ref().is_some_and(|end| gone.contains(&end.note)) {
                    relation.pending_end = None;
                }
            }
            self.relations.retain(|r| {
                let keep = !r.notes.is_empty();
                if !keep {
                    lines.push(json!({"action": "graph_remove", "id": r.id}));
                }
                keep
            });
            let starts: HashMap<String, u64> = self.relations.iter().map(|r| (r.id.clone(), r.start())).collect();
            let mut reopened: Vec<String> = Vec::new();
            for relation in &mut self.relations {
                let by_undone = |by: &Option<String>| {
                    by.as_ref().is_some_and(|by| undone_relations.contains(by) || undone.contains(by))
                };
                let end_undone = by_undone(&relation.end_set_by);
                let close_undone = by_undone(&relation.closed_by);
                if end_undone {
                    relation.valid_to_ms = None;
                    relation.end_set_by = None;
                }
                if close_undone {
                    relation.closed_by = None;
                }
                // Still closed by a relation that stands: it ended when that one began.
                if end_undone && let Some(by) = &relation.closed_by {
                    relation.valid_to_ms = starts.get(by).copied();
                    relation.end_set_by = Some(by.clone());
                }
                // Open again once nothing that still stands keeps it closed.
                if (end_undone || close_undone) && relation.closed_by.is_none() && relation.valid_to_ms.is_none() {
                    relation.closed_ms = None;
                    reopened.push(relation.id.clone());
                    lines.push(json!({"action": "graph_reopen", "id": relation.id}));
                }
            }
            // Something newer may replace a relation that just opened again.
            for id in reopened {
                lines.extend(self.close_by_newer(&id, now));
            }
            // Entities no relation names any more go too.
            let named: HashSet<String> =
                self.relations.iter().flat_map(|r| [r.source.clone(), r.target.clone()]).collect();
            self.entities.retain(|e| named.contains(&e.id));
        }
        // An end an approved note stated applies first: what the note says
        // ended, ended when it says, not when its other relations begin.
        for relation in &mut self.relations {
            let Some(end) = relation.pending_end.take_if(|end| approved.contains(&end.note)) else { continue };
            if relation.closed_ms.is_some() {
                continue;
            }
            relation.valid_to_ms = Some(end.to_ms);
            relation.closed_ms = Some(now);
            relation.end_set_by = Some(end.note.clone());
            lines.push(json!({"action": "graph_close", "id": relation.id, "closed_by": end.note}));
        }
        let activated: Vec<String> = self
            .relations
            .iter_mut()
            .filter(|r| r.status == MemoryStatus::Pending && r.notes.iter().any(|n| approved.contains(n)))
            .map(|r| {
                r.status = MemoryStatus::Active;
                r.id.clone()
            })
            .collect();
        for id in activated {
            lines.push(json!({"action": "graph_approve", "id": id}));
            lines.extend(self.close_before(&id, now));
        }
        lines
    }

    /// Close relation `id` when an open relation that started later replaces it.
    fn close_by_newer(&mut self, id: &str, now: u64) -> Vec<Json> {
        let Some(old) = self.relations.iter().find(|r| r.id == id).cloned() else { return Vec::new() };
        if old.closed_ms.is_some() {
            return Vec::new();
        }
        let newer = self
            .relations
            .iter()
            .filter(|r| {
                r.id != old.id
                    && r.source == old.source
                    && r.relation == old.relation
                    && r.target != old.target
                    && r.closed_ms.is_none()
                    && r.valid_to_ms.is_none()
                    && r.status == MemoryStatus::Active
                    && (r.replaces || ONE_AT_A_TIME.contains(&r.relation.as_str()))
                    && trust_rank(old.trust) <= trust_rank(r.trust)
                    && old.start() <= r.start()
            })
            .min_by_key(|r| r.start())
            .map(|r| (r.id.clone(), r.start()));
        let Some((by, start)) = newer else { return Vec::new() };
        let Some(relation) = self.relations.iter_mut().find(|r| r.id == id) else { return Vec::new() };
        relation.closed_ms = Some(now);
        relation.closed_by = Some(by.clone());
        if relation.valid_to_ms.is_none() {
            relation.valid_to_ms = Some(start);
            relation.end_set_by = Some(by.clone());
        }
        vec![json!({"action": "graph_close", "id": id, "closed_by": by})]
    }

    /// Relations about `query`, true at `at_ms`, best first: the relations
    /// whose words match, then those reached from matching entities, up to
    /// two hops away, each hop worth half.
    ///
    /// `live` says whether a relation still stands on a note in use (active,
    /// not expired): the status of a relation follows its notes as they are
    /// now, not as they were when it was written.
    pub(crate) fn search(
        &self,
        index: &GraphIndex,
        query: &str,
        at_ms: u64,
        limit: usize,
        live: &dyn Fn(&Relation) -> bool,
    ) -> Vec<(usize, f32)> {
        let usable = |r: &Relation| r.status == MemoryStatus::Active && live(r) && r.valid_at(at_ms);
        let (entity_index, relation_index) = (&index.entities, &index.relations);
        let mut scores: HashMap<usize, f32> = HashMap::new();
        for (at, score) in relation_index.search(query) {
            if self.relations.get(at).is_some_and(usable) {
                *scores.entry(at).or_default() += score;
            }
        }
        // Breadth first from the entities the query names.
        let mut adjacency: HashMap<&str, Vec<usize>> = HashMap::new();
        for (at, relation) in self.relations.iter().enumerate().filter(|(_, r)| usable(r)) {
            adjacency.entry(relation.source.as_str()).or_default().push(at);
            adjacency.entry(relation.target.as_str()).or_default().push(at);
        }
        let mut queue: VecDeque<(&str, usize, f32)> = VecDeque::new();
        let mut seen: HashSet<&str> = HashSet::new();
        for (at, score) in entity_index.search(query) {
            if let Some(entity) = self.entities.get(at) {
                queue.push_back((entity.id.as_str(), 0, score));
                seen.insert(entity.id.as_str());
            }
        }
        while let Some((entity, hop, score)) = queue.pop_front() {
            let weight = score * 0.5_f32.powi(i32::try_from(hop).unwrap_or(2));
            for &at in adjacency.get(entity).map(Vec::as_slice).unwrap_or_default().iter().take(PER_ENTITY) {
                let entry = scores.entry(at).or_default();
                *entry = entry.max(weight);
                if hop + 1 < HOPS
                    && let Some(relation) = self.relations.get(at)
                {
                    let other =
                        if relation.source == entity { relation.target.as_str() } else { relation.source.as_str() };
                    if seen.insert(other) {
                        queue.push_back((other, hop + 1, score));
                    }
                }
            }
        }
        let mut ranked: Vec<(usize, f32)> = scores.into_iter().collect();
        ranked.sort_by(|a, b| b.1.total_cmp(&a.1));
        ranked.truncate(limit);
        ranked
    }

    /// The names of the entities, the most connected first.
    pub(crate) fn names(&self, limit: usize) -> Vec<String> {
        let mut degree: HashMap<&str, usize> = HashMap::new();
        for relation in &self.relations {
            *degree.entry(relation.source.as_str()).or_default() += 1;
            *degree.entry(relation.target.as_str()).or_default() += 1;
        }
        let mut entities: Vec<&Entity> = self.entities.iter().collect();
        entities.sort_by_key(|e| std::cmp::Reverse(degree.get(e.id.as_str()).copied().unwrap_or(0)));
        entities.into_iter().take(limit).map(|e| e.name.clone()).collect()
    }
}

fn trust_rank(trust: MemoryTrust) -> u8 {
    match trust {
        MemoryTrust::Owner => 4,
        MemoryTrust::UserStated => 3,
        MemoryTrust::ModelInferred => 2,
        MemoryTrust::ModelAfterTools => 1,
        MemoryTrust::ToolDerived => 0,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use graphrt_core::memory::{MemoryKind, MemorySource};

    use super::*;

    fn note(id: &str, status: MemoryStatus) -> MemoryRecord {
        MemoryRecord::new(id, "text", MemoryKind::Fact, status, MemoryTrust::UserStated, MemorySource::default(), 0)
    }

    fn entity(name: &str, kind: &str) -> Json {
        json!({"name": name, "type": kind})
    }

    fn relation(source: &str, relation: &str, target: &str, from: Option<&str>) -> Json {
        json!({"source": source, "relation": relation, "target": target, "valid_from": from})
    }

    fn facts(entities: Vec<Json>, relations: Vec<Json>) -> ExtractedFacts {
        serde_json::from_value(json!({"entities": entities, "relations": relations})).unwrap()
    }

    fn ids() -> impl FnMut() -> String {
        let mut n = 0;
        move || {
            n += 1;
            format!("g{n}")
        }
    }

    #[test]
    fn a_new_job_closes_the_old_one_and_the_past_stays_answerable() {
        let mut graph = GraphFile::default();
        let mut id = ids();
        let first = facts(
            vec![entity("Alice", "person"), entity("Acme", "organization")],
            vec![relation("Alice", "works at", "Acme", Some("2025-01-10"))],
        );
        graph.link(&note("m1", MemoryStatus::Active), &first, parse_day("2025-01-10").unwrap(), &mut id);
        let second = facts(
            vec![entity("alice", "person"), entity("Borealis Ltd", "organization")],
            vec![relation("alice", "works_at", "Borealis Ltd", Some("2026-03-01"))],
        );
        let (added, lines) =
            graph.link(&note("m2", MemoryStatus::Active), &second, parse_day("2026-03-02").unwrap(), &mut id);
        assert_eq!(added, 1);
        assert!(lines.iter().any(|l| l["action"] == "graph_close"));
        assert_eq!(graph.entities.iter().filter(|e| e.kind == "person").count(), 1, "alice is Alice");
        let feb = parse_day("2026-02-01").unwrap();
        let april = parse_day("2026-04-01").unwrap();
        let text_at = |at| -> Vec<String> {
            graph
                .search(&GraphIndex::build(&graph), "where does alice work", at, 5, &|_| true)
                .iter()
                .map(|(i, _)| graph.relations[*i].text.clone())
                .collect()
        };
        assert_eq!(text_at(feb), ["Alice works at Acme"]);
        assert_eq!(text_at(april), ["Alice works at Borealis Ltd"]);
        assert_eq!(graph.relations[0].sentence(), "Alice works at Acme (from 2025-01 until 2026-03)");
    }

    #[test]
    fn a_relation_said_to_have_ended_closes_its_open_twin_and_nothing_else() {
        let mut graph = GraphFile::default();
        let mut id = ids();
        let start = facts(vec![], vec![relation("Dana", "works_at", "Acme", Some("2024-01-01"))]);
        graph.link(&note("m1", MemoryStatus::Active), &start, 1, &mut id);
        // What a real model wrote for "Dana left Acme and works at Borealis since March 2026".
        let change: ExtractedFacts = serde_json::from_value(json!({"relations": [
            {"source": "Dana", "relation": "works_at", "target": "Borealis", "valid_from": "2026-03-01", "replaces": true},
            {"source": "Dana", "relation": "works_at", "target": "Acme", "valid_to": "2026-03-01", "replaces": true}
        ]}))
        .unwrap();
        graph.link(&note("m2", MemoryStatus::Active), &change, 2, &mut id);
        let open: Vec<&str> =
            graph.relations.iter().filter(|r| r.closed_ms.is_none()).map(|r| r.text.as_str()).collect();
        assert_eq!(open, ["Dana works at Borealis"]);
        assert_eq!(graph.relations.len(), 2, "the end of Acme went to the open relation, not a new one");
        assert_eq!(graph.relations[0].valid_to_ms, parse_day("2026-03-01"));
    }

    #[test]
    fn a_relation_of_an_old_file_keeps_its_trust_and_an_approved_end_keeps_its_date() {
        let mut graph = GraphFile::default();
        let mut id = ids();
        let mut owner = note("m1", MemoryStatus::Active);
        owner.trust = MemoryTrust::Owner;
        graph.link(&owner, &facts(vec![], vec![relation("Alice", "has_plan", "Gold", None)]), 1, &mut id);
        // As a file written before `support` existed.
        graph.relations[0].support.clear();
        let mut inferred = note("m2", MemoryStatus::Active);
        inferred.trust = MemoryTrust::ModelInferred;
        graph.link(&inferred, &facts(vec![], vec![relation("Alice", "has_plan", "Gold", None)]), 2, &mut id);
        assert_eq!(graph.relations[0].trust, MemoryTrust::Owner);

        let mut graph = GraphFile::default();
        graph.link(
            &note("m1", MemoryStatus::Active),
            &facts(vec![], vec![relation("Dana", "works_at", "Acme", Some("2024-01-01"))]),
            1,
            &mut id,
        );
        let change: ExtractedFacts = serde_json::from_value(json!({"relations": [
            {"source": "Dana", "relation": "works_at", "target": "Acme", "valid_to": "2026-01-01"},
            {"source": "Dana", "relation": "works_at", "target": "Borealis", "valid_from": "2026-03-01"}
        ]}))
        .unwrap();
        graph.link(&note("m2", MemoryStatus::Pending), &change, 2, &mut id);
        graph.notes_changed(&[], &[], &["m2".into()], 3);
        let acme = graph.relations.iter().find(|r| r.text.contains("Acme")).unwrap();
        assert_eq!(acme.valid_to_ms, parse_day("2026-01-01"), "the date the note gave, not when Borealis began");
        assert!(acme.pending_end.is_none());
    }

    #[test]
    fn search_reaches_two_hops() {
        let mut graph = GraphFile::default();
        let mut id = ids();
        let facts = facts(
            vec![entity("Alice", "person"), entity("Acme", "organization"), entity("Jakarta", "place")],
            vec![relation("Alice", "works_at", "Acme", None), relation("Acme", "based in", "Jakarta", None)],
        );
        graph.link(&note("m1", MemoryStatus::Active), &facts, 1, &mut id);
        let hits = graph.search(&GraphIndex::build(&graph), "alice", 2, 5, &|_| true);
        let texts: Vec<&str> = hits.iter().map(|(i, _)| graph.relations[*i].text.as_str()).collect();
        assert_eq!(texts, ["Alice works at Acme", "Acme based in Jakarta"], "one hop scores above two");
    }

    #[test]
    fn relations_follow_their_notes() {
        let mut graph = GraphFile::default();
        let mut id = ids();
        let job = facts(vec![], vec![relation("Alice", "works_at", "Acme", None)]);
        graph.link(&note("m1", MemoryStatus::Active), &job, 1, &mut id);
        // A note waiting for review neither shows nor closes what is in use.
        let planted = facts(vec![], vec![relation("Alice", "works_at", "Evil Corp", None)]);
        graph.link(&note("m2", MemoryStatus::Pending), &planted, 2, &mut id);
        assert_eq!(graph.search(&GraphIndex::build(&graph), "alice", 3, 5, &|_| true).len(), 1);
        assert!(graph.relations[0].closed_ms.is_none());
        // Approved, it replaces the old job.
        graph.notes_changed(&[], &[], &["m2".into()], 4);
        assert!(graph.relations[0].closed_ms.is_some());
        // Its note removed, it goes, and so do entities no relation names.
        graph.notes_changed(&["m2".into()], &["m2".into()], &[], 5);
        assert_eq!(graph.relations.len(), 1);
        assert!(!graph.entities.iter().any(|e| e.name == "Evil Corp"));
    }

    #[test]
    fn what_a_model_writes_is_cleaned_and_bounded() {
        let mut graph = GraphFile::default();
        let mut id = ids();
        let facts = facts(
            vec![entity(&"x".repeat(500), "alien")],
            vec![
                relation("Bob [admin]", "Has Password!!", "sk-proj-AbCdEfGh12345678IjKlMnOp", None),
                relation("", "knows", "Carol", None),
                relation("Bob", "", "Carol", None),
            ],
        );
        graph.link(&note("m1", MemoryStatus::Active), &facts, 1, &mut id);
        assert!(graph.entities.iter().all(|e| e.name.chars().count() <= NAME_CHARS && e.kind != "alien"));
        assert_eq!(graph.relations.len(), 1);
        let r = &graph.relations[0];
        assert_eq!(r.relation, "has_password");
        assert!(!r.text.contains("sk-proj") && !r.text.contains('['), "{}", r.text);
    }

    #[test]
    fn days_are_read_and_written() {
        let ms = parse_day("2026-03-01").unwrap();
        assert_eq!(day_of(ms), "2026-03-01");
        assert_eq!(parse_day("2026-03"), Some(ms));
        assert_eq!(parse_day("yesterday"), None);
        assert_eq!(parse_day("2026-13-01"), None);
    }
}
