//! The memory graph through the store: facts from notes, multi hop recall
//! fused with notes, periods, review, removal, isolation and erasure.

#![cfg(feature = "graph")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use graphrt_core::memory::{
    ExtractedFacts, ForgetTarget, HitKind, MemoryBank, MemoryKind, MemoryQuery, MemoryScope, MemorySource, MemoryTrust,
    NewMemory,
};
use graphrt_memory::MemoryStore;
use serde_json::json;

fn temp_root(name: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().subsec_nanos();
    std::env::temp_dir().join(format!("graphrt-graph-{name}-{}-{nanos}", std::process::id()))
}

fn user(name: &str) -> MemoryScope {
    MemoryScope::user(None, "support", name)
}

fn note(text: &str, trust: MemoryTrust) -> NewMemory {
    NewMemory::new(text, trust, MemorySource { actor: "agent".into(), ..MemorySource::default() })
        .kind(MemoryKind::Fact)
}

fn facts(value: serde_json::Value) -> ExtractedFacts {
    serde_json::from_value(value).unwrap()
}

fn day(text: &str) -> u64 {
    let parts: Vec<i64> = text.split('-').map(|p| p.parse().unwrap()).collect();
    let (y, m, d) = (parts[0], parts[1], parts[2]);
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + d - 1;
    let days = era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468;
    u64::try_from(days).unwrap() * 86_400_000
}

#[tokio::test(flavor = "multi_thread")]
async fn recall_finds_a_fact_two_hops_away_that_no_note_says_in_those_words() {
    let store = MemoryStore::open(temp_root("hops"));
    let alice = user("alice");
    assert!(store.keeps_graph());
    let job = store.remember(&alice, note("Alice works at Acme", MemoryTrust::UserStated)).await.unwrap();
    let place = store.remember(&alice, note("Acme has its office in Jakarta", MemoryTrust::UserStated)).await.unwrap();
    store
        .link(
            &alice,
            &job.id,
            facts(json!({"entities": [{"name": "Alice", "type": "person"}, {"name": "Acme", "type": "organization"}],
            "relations": [{"source": "Alice", "relation": "works_at", "target": "Acme"}]})),
        )
        .await
        .unwrap();
    store
        .link(
            &alice,
            &place.id,
            facts(json!({"relations": [{"source": "Acme", "relation": "office_in", "target": "Jakarta"}]})),
        )
        .await
        .unwrap();
    // "alice" names the person: the walk reaches her employer and its city.
    let hits = store.recall(&alice, &MemoryQuery::new("alice city")).await.unwrap();
    let facts: Vec<&str> = hits.iter().filter(|h| h.kind == HitKind::Fact).map(|h| h.record.text.as_str()).collect();
    assert!(facts.contains(&"Acme office in Jakarta"), "{hits:?}");
    assert_eq!(store.known_entities(&alice, 10).await.unwrap()[0], "Acme", "the most connected first");
    // Another user's recall sees none of it.
    assert!(store.recall(&user("bob"), &MemoryQuery::new("alice city")).await.unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_fact_is_true_for_its_period_and_the_past_can_be_asked() {
    let now = Arc::new(AtomicU64::new(day("2026-04-10")));
    let clock = Arc::clone(&now);
    let store = MemoryStore::open(temp_root("period")).with_clock(Arc::new(move || clock.load(Ordering::Relaxed)));
    let alice = user("alice");
    let old =
        store.remember(&alice, note("Alice worked at Acme from January 2025", MemoryTrust::UserStated)).await.unwrap();
    store
        .link(&alice, &old.id, facts(json!({"relations": [{"source": "Alice", "relation": "works_at", "target": "Acme", "valid_from": "2025-01-10"}]})))
        .await
        .unwrap();
    let new =
        store.remember(&alice, note("Alice moved to Borealis in March 2026", MemoryTrust::UserStated)).await.unwrap();
    store
        .link(&alice, &new.id, facts(json!({"relations": [{"source": "Alice", "relation": "works at", "target": "Borealis", "valid_from": "2026-03-01", "replaces": true}]})))
        .await
        .unwrap();
    let texts = |hits: Vec<graphrt_core::memory::MemoryHit>| -> Vec<String> {
        hits.into_iter().filter(|h| h.kind == HitKind::Fact).map(|h| h.record.text).collect()
    };
    let today = texts(store.recall(&alice, &MemoryQuery::new("alice works")).await.unwrap());
    assert!(today.iter().any(|t| t == "Alice works at Borealis (since 2026-03)"), "{today:?}");
    assert!(!today.iter().any(|t| t.contains("Acme")), "{today:?}");
    let february =
        texts(store.recall(&alice, &MemoryQuery::new("alice works").as_of(day("2026-02-01"))).await.unwrap());
    assert!(february.iter().any(|t| t.starts_with("Alice works at Acme")), "{february:?}");
    let audit = store.audit(None, "support", 100).await.unwrap();
    assert!(audit.iter().any(|l| l["action"] == "graph_close"));
    assert!(!serde_json::to_string(&audit).unwrap().contains("Borealis"), "no text in the audit log");
}

#[tokio::test(flavor = "multi_thread")]
async fn facts_of_a_note_waiting_for_review_wait_too_and_go_with_their_note() {
    let store = MemoryStore::open(temp_root("review"));
    let alice = user("alice");
    let real = store.remember(&alice, note("Alice is on the basic plan", MemoryTrust::UserStated)).await.unwrap();
    store
        .link(
            &alice,
            &real.id,
            facts(json!({"relations": [{"source": "Alice", "relation": "has_plan", "target": "Basic"}]})),
        )
        .await
        .unwrap();
    let planted =
        store.remember(&alice, note("Alice is on the platinum plan", MemoryTrust::ToolDerived)).await.unwrap();
    store
        .link(
            &alice,
            &planted.id,
            facts(json!({"relations": [{"source": "Alice", "relation": "has_plan", "target": "Platinum"}]})),
        )
        .await
        .unwrap();
    let plan = |store: MemoryStore| async move {
        let hits = store.recall(&user("alice"), &MemoryQuery::new("alice plan")).await.unwrap();
        hits.into_iter().filter(|h| h.kind == HitKind::Fact).map(|h| h.record.text).collect::<Vec<_>>()
    };
    let shown = plan(store.clone()).await;
    assert!(shown.iter().all(|t| !t.contains("Platinum")), "a waiting fact is never recalled: {shown:?}");
    // Rejected, the note and its facts are gone, and the plan in use was never closed.
    let owner = MemorySource { actor: "owner".into(), ..MemorySource::default() };
    store.decide(&alice, &planted.id, false, &owner).await.unwrap();
    let export = store.export_user(&alice).await.unwrap();
    let relations = export["graph"]["relations"].as_array().unwrap();
    assert_eq!(relations.len(), 1);
    assert!(relations[0]["closed_ms"].is_null());
    // Forgetting the real note takes its fact along.
    store.forget(&alice, &ForgetTarget::Id(real.id.clone()), &owner).await.unwrap();
    let export = store.export_user(&alice).await.unwrap();
    assert!(export["graph"]["relations"].as_array().unwrap().is_empty());
    // A note that is gone gives nothing to link.
    let error = store.link(&alice, &real.id, ExtractedFacts::default()).await.unwrap_err();
    assert_eq!(error.kind, "memory_not_found");
}

#[tokio::test(flavor = "multi_thread")]
async fn erasing_a_user_removes_their_graph() {
    let root = temp_root("erase");
    let store = MemoryStore::open(&root);
    let alice = user("alice");
    let saved = store.remember(&alice, note("Alice lives in Bandung", MemoryTrust::UserStated)).await.unwrap();
    store
        .link(
            &alice,
            &saved.id,
            facts(json!({"relations": [{"source": "Alice", "relation": "lives_in", "target": "Bandung"}]})),
        )
        .await
        .unwrap();
    let owner = MemorySource { actor: "owner".into(), ..MemorySource::default() };
    store.erase_user(&alice, &owner).await.unwrap();
    let mut left = Vec::new();
    for entry in walkdir(&root) {
        if std::fs::read_to_string(&entry).is_ok_and(|text| text.contains("Bandung")) {
            left.push(entry);
        }
    }
    assert!(left.is_empty(), "{left:?}");
}

fn walkdir(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            out.extend(walkdir(&path));
        } else {
            out.push(path);
        }
    }
    out
}

async fn link(
    store: &MemoryStore,
    scope: &MemoryScope,
    text: &str,
    trust: MemoryTrust,
    relation: serde_json::Value,
) -> String {
    let saved = store.remember(scope, note(text, trust)).await.unwrap();
    store.link(scope, &saved.id, facts(json!({"relations": [relation]}))).await.unwrap();
    saved.id
}

fn open_relations(export: &serde_json::Value) -> Vec<String> {
    export["graph"]["relations"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r["closed_ms"].is_null())
        .map(|r| r["text"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_fact_said_by_two_notes_outlives_the_first_and_forgetting_a_replacement_reopens_the_old() {
    let store = MemoryStore::open(temp_root("support"));
    let alice = user("alice");
    let owner = MemorySource { actor: "owner".into(), ..MemorySource::default() };
    let rel = |target: &str| json!({"source": "Alice", "relation": "lives_in", "target": target});
    let first = link(&store, &alice, "Alice lives in Bandung", MemoryTrust::UserStated, rel("Bandung")).await;
    link(&store, &alice, "Alice still lives in Bandung", MemoryTrust::UserStated, rel("Bandung")).await;
    store.forget(&alice, &ForgetTarget::Id(first), &owner).await.unwrap();
    assert_eq!(open_relations(&store.export_user(&alice).await.unwrap()), ["Alice lives in Bandung"]);
    // A move replaces it; forgetting the move brings Bandung back.
    let moved = link(&store, &alice, "Alice moved to Medan", MemoryTrust::UserStated, rel("Medan")).await;
    assert_eq!(open_relations(&store.export_user(&alice).await.unwrap()), ["Alice lives in Medan"]);
    store.forget(&alice, &ForgetTarget::Id(moved), &owner).await.unwrap();
    assert_eq!(open_relations(&store.export_user(&alice).await.unwrap()), ["Alice lives in Bandung"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn trust_and_time_decide_what_closes_what() {
    let store = MemoryStore::open(temp_root("order"));
    let alice = user("alice");
    let owner = MemorySource { actor: "owner".into(), ..MemorySource::default() };
    let job = |target: &str, from: &str| json!({"source": "Alice", "relation": "works_at", "target": target, "valid_from": from});
    link(&store, &alice, "Alice works at Borealis since 2026", MemoryTrust::UserStated, job("Borealis", "2026-03-01"))
        .await;
    // An older job, told later, does not close the newer one.
    link(&store, &alice, "Alice worked at Acme from 2024", MemoryTrust::UserStated, job("Acme", "2024-01-01")).await;
    let open = open_relations(&store.export_user(&alice).await.unwrap());
    assert!(open.contains(&"Alice works at Borealis".to_owned()), "{open:?}");
    // A page's claim, approved, cannot close what the user said.
    let planted =
        link(&store, &alice, "Alice works at Evil Corp", MemoryTrust::ToolDerived, job("Evil Corp", "2026-06-01"))
            .await;
    store.decide(&alice, &planted, true, &owner).await.unwrap();
    let open = open_relations(&store.export_user(&alice).await.unwrap());
    assert!(open.contains(&"Alice works at Borealis".to_owned()), "{open:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_end_stated_by_a_note_that_waits_applies_when_it_is_approved() {
    let store = MemoryStore::open(temp_root("pending-end"));
    let alice = user("alice");
    let owner = MemorySource { actor: "owner".into(), ..MemorySource::default() };
    link(
        &store,
        &alice,
        "Alice works at Acme",
        MemoryTrust::ToolDerived,
        json!({"source": "Alice", "relation": "works_at", "target": "Acme"}),
    )
    .await;
    let first = store.list(&alice, &graphrt_memory::ListFilter::default()).await.unwrap()[0].id.clone();
    store.decide(&alice, &first, true, &owner).await.unwrap();
    let ended = link(
        &store,
        &alice,
        "Alice left Acme in March",
        MemoryTrust::ToolDerived,
        json!({"source": "Alice", "relation": "works_at", "target": "Acme", "valid_to": "2026-03-01"}),
    )
    .await;
    assert_eq!(open_relations(&store.export_user(&alice).await.unwrap()), ["Alice works at Acme"], "waits");
    store.decide(&alice, &ended, true, &owner).await.unwrap();
    assert!(open_relations(&store.export_user(&alice).await.unwrap()).is_empty(), "applied on approval");
}

#[tokio::test(flavor = "multi_thread")]
async fn facts_of_an_expired_note_are_not_recalled_and_a_fact_counts_as_a_use_of_its_note() {
    let now = Arc::new(AtomicU64::new(1_000_000));
    let clock = Arc::clone(&now);
    let store = MemoryStore::open(temp_root("expired")).with_clock(Arc::new(move || clock.load(Ordering::Relaxed)));
    let alice = user("alice");
    let event = store
        .remember(&alice, note("Alice ordered a lamp", MemoryTrust::UserStated).kind(MemoryKind::Event))
        .await
        .unwrap();
    store
        .link(
            &alice,
            &event.id,
            facts(json!({"relations": [{"source": "Alice", "relation": "ordered", "target": "Lamp"}]})),
        )
        .await
        .unwrap();
    let hits = store.recall(&alice, &MemoryQuery::new("lamp").touch()).await.unwrap();
    assert!(hits.iter().any(|h| h.kind == HitKind::Fact));
    let listed = store.list(&alice, &graphrt_memory::ListFilter::default()).await.unwrap();
    assert_eq!(listed[0].uses, 1, "the note behind the fact was used");
    now.fetch_add(91 * 24 * 3600 * 1000, Ordering::Relaxed);
    let hits = store.recall(&alice, &MemoryQuery::new("lamp")).await.unwrap();
    assert!(hits.is_empty(), "{hits:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_note_that_waits_never_renames_an_entity_in_use_and_the_agents_own_graph_is_read_by_every_user() {
    let store = MemoryStore::open(temp_root("alias"));
    let agent = MemoryScope::agent(None, "support");
    let mut by_owner = note("The office is in Jakarta", MemoryTrust::Owner);
    by_owner.source.actor = "owner".into();
    let office = store.remember(&agent, by_owner).await.unwrap();
    store
        .link(
            &agent,
            &office.id,
            facts(json!({"relations": [{"source": "Office", "relation": "located_in", "target": "Jakarta"}]})),
        )
        .await
        .unwrap();
    // A proposal (pending at the agent level) tries to make "Evil" an alias of the office.
    let proposal = store.remember(&agent, note("Evil is the office", MemoryTrust::UserStated)).await.unwrap();
    store
        .link(
            &agent,
            &proposal.id,
            facts(json!({"entities": [{"name": "Evil", "type": "place", "aliases": ["Office"]}]})),
        )
        .await
        .unwrap();
    let export = store.export_user(&agent).await.unwrap();
    let entities = export["graph"]["entities"].as_array().unwrap();
    assert!(
        entities.iter().all(|e| e["aliases"].as_array().is_none_or(|a| a.iter().all(|x| x != "Evil"))),
        "{entities:?}"
    );
    // Any user's recall sees the agent's facts.
    let hits = store.recall(&user("bob"), &MemoryQuery::new("office")).await.unwrap();
    assert!(hits.iter().any(|h| h.kind == HitKind::Fact && h.record.text.contains("Jakarta")), "{hits:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn forgetting_the_middle_of_a_chain_reopens_nothing_a_newer_fact_replaces() {
    let store = MemoryStore::open(temp_root("chain"));
    let alice = user("alice");
    let owner = MemorySource { actor: "owner".into(), ..MemorySource::default() };
    let home =
        |city: &str, from: &str| json!({"source": "Alice", "relation": "lives_in", "target": city, "valid_from": from});
    link(&store, &alice, "Alice lived in Bandung", MemoryTrust::UserStated, home("Bandung", "2020-01-01")).await;
    let medan =
        link(&store, &alice, "Alice moved to Medan", MemoryTrust::UserStated, home("Medan", "2023-01-01")).await;
    link(&store, &alice, "Alice moved to Jakarta", MemoryTrust::UserStated, home("Jakarta", "2025-01-01")).await;
    store.forget(&alice, &ForgetTarget::Id(medan), &owner).await.unwrap();
    assert_eq!(open_relations(&store.export_user(&alice).await.unwrap()), ["Alice lives in Jakarta"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_note_that_ages_out_leaves_the_history_it_made() {
    let now = Arc::new(AtomicU64::new(day("2026-01-01")));
    let clock = Arc::clone(&now);
    let store = MemoryStore::open(temp_root("aged")).with_clock(Arc::new(move || clock.load(Ordering::Relaxed)));
    let alice = user("alice");
    let job = |target: &str, from: &str| json!({"source": "Alice", "relation": "works_at", "target": target, "valid_from": from});
    link(&store, &alice, "Alice worked at Acme", MemoryTrust::UserStated, job("Acme", "2024-01-01")).await;
    // An event kind note expires: what it replaced stays replaced.
    let change = store
        .remember(&alice, note("Alice started at Borealis", MemoryTrust::UserStated).kind(MemoryKind::Event))
        .await
        .unwrap();
    store.link(&alice, &change.id, facts(json!({"relations": [job("Borealis", "2025-06-01")]}))).await.unwrap();
    now.fetch_add(91 * 24 * 3600 * 1000, Ordering::Relaxed);
    store.remember(&alice, note("Alice likes tea", MemoryTrust::UserStated)).await.unwrap();
    let open = open_relations(&store.export_user(&alice).await.unwrap());
    assert!(!open.contains(&"Alice works at Acme".to_owned()), "{open:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_relation_is_as_trusted_as_its_best_note_left_and_a_rebuild_adds_nothing() {
    let store = MemoryStore::open(temp_root("trust-left"));
    let alice = user("alice");
    let owner = MemorySource { actor: "owner".into(), ..MemorySource::default() };
    let plan = json!({"source": "Alice", "relation": "has_plan", "target": "Gold"});
    let mut by_owner = note("Alice is on gold", MemoryTrust::Owner);
    by_owner.source.actor = "owner".into();
    let owned = store.remember(&alice, by_owner).await.unwrap();
    store.link(&alice, &owned.id, facts(json!({"relations": [plan.clone()]}))).await.unwrap();
    let said = link(&store, &alice, "I am on gold", MemoryTrust::ModelInferred, plan.clone()).await;
    store.forget(&alice, &ForgetTarget::Id(owned.id.clone()), &owner).await.unwrap();
    let export = store.export_user(&alice).await.unwrap();
    assert_eq!(export["graph"]["relations"][0]["trust"], "model_inferred");
    // Reading the same note again changes nothing.
    store.link(&alice, &said, facts(json!({"relations": [plan]}))).await.unwrap();
    assert_eq!(store.export_user(&alice).await.unwrap()["graph"]["relations"].as_array().unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_user_saying_what_a_page_proposed_makes_it_current_and_closes_the_old() {
    let store = MemoryStore::open(temp_root("confirm"));
    let alice = user("alice");
    let job = |target: &str| json!({"source": "Alice", "relation": "works_at", "target": target});
    link(&store, &alice, "Alice works at Acme", MemoryTrust::UserStated, job("Acme")).await;
    link(&store, &alice, "Alice works at Borealis", MemoryTrust::ToolDerived, job("Borealis")).await;
    assert_eq!(open_relations(&store.export_user(&alice).await.unwrap()).len(), 2, "the proposal waits");
    link(&store, &alice, "I work at Borealis now", MemoryTrust::UserStated, job("Borealis")).await;
    let export = store.export_user(&alice).await.unwrap();
    let open = open_relations(&export);
    assert_eq!(open, ["Alice works at Borealis"], "{export}");
}
