//! Search over the notes of a scope: BM25 by keyword, cosine similarity when
//! notes have vectors, reciprocal rank fusion of the two, then factors for
//! age and trust. Also the test for a note that says the same as another.

use std::collections::{HashMap, HashSet};

use graphrt_core::memory::MemoryTrust;

/// BM25 parameters, the usual ones.
const K1: f32 = 1.2;
const B: f32 = 0.75;
/// Reciprocal rank fusion constant.
const RRF_K: f32 = 60.0;
/// Half life of the age factor, from when a note was last used.
const HALF_LIFE_MS: f64 = 90.0 * 24.0 * 3600.0 * 1000.0;
/// Two notes whose token sets overlap this much say the same.
pub const SAME_NOTE_OVERLAP: f64 = 0.85;

pub use graphrt_core::memory::{stem, words};

/// Keyword index of the notes of one scope, in note order.
#[derive(Debug, Default)]
pub struct Index {
    docs: Vec<HashMap<String, u32>>,
    lengths: Vec<u32>,
    frequency: HashMap<String, u32>,
    average: f32,
}

impl Index {
    /// Index the texts of `notes` (text and tags, as one document each).
    pub fn build<'a>(documents: impl Iterator<Item = &'a str>) -> Self {
        let mut index = Self::default();
        for document in documents {
            index.push(document);
        }
        index
    }

    fn push(&mut self, document: &str) {
        let mut counts: HashMap<String, u32> = HashMap::new();
        let tokens = words(document);
        let length = u32::try_from(tokens.len()).unwrap_or(u32::MAX);
        for token in tokens {
            *counts.entry(token).or_default() += 1;
        }
        for token in counts.keys() {
            *self.frequency.entry(token.clone()).or_default() += 1;
        }
        self.docs.push(counts);
        self.lengths.push(length);
        let total: u64 = self.lengths.iter().map(|l| u64::from(*l)).sum();
        self.average = total as f32 / self.lengths.len().max(1) as f32;
    }

    /// BM25 score of every document that shares a word with `query`, best first.
    pub fn search(&self, query: &str) -> Vec<(usize, f32)> {
        let terms: HashSet<String> = words(query).into_iter().collect();
        if terms.is_empty() {
            return Vec::new();
        }
        let n = self.docs.len() as f32;
        let mut scored: Vec<(usize, f32)> = Vec::new();
        for (doc, counts) in self.docs.iter().enumerate() {
            let length = self.lengths.get(doc).copied().unwrap_or(0) as f32;
            let mut score = 0.0;
            for term in &terms {
                let Some(tf) = counts.get(term) else { continue };
                let df = self.frequency.get(term).copied().unwrap_or(0) as f32;
                let idf = ((n - df + 0.5) / (df + 0.5) + 1.0).ln();
                let tf = *tf as f32;
                score += idf * tf * (K1 + 1.0) / (tf + K1 * (1.0 - B + B * length / self.average.max(1.0)));
            }
            if score > 0.0 {
                scored.push((doc, score));
            }
        }
        scored.sort_by(|a, b| b.1.total_cmp(&a.1));
        scored
    }
}

/// Fuse ranked lists (each best first) by reciprocal rank: a note ranked
/// well by several signals rises.
pub fn fuse(lists: &[Vec<(usize, f32)>]) -> HashMap<usize, f32> {
    let mut fused: HashMap<usize, f32> = HashMap::new();
    for list in lists {
        for (rank, (doc, _)) in list.iter().enumerate() {
            *fused.entry(*doc).or_default() += 1.0 / (RRF_K + rank as f32 + 1.0);
        }
    }
    fused
}

/// The weight of a note by how long ago it was used: 1 now, 0.5 after the half life.
pub fn age_factor(last_used_ms: u64, now_ms: u64) -> f32 {
    let age = now_ms.saturating_sub(last_used_ms) as f64;
    (0.5_f64.powf(age / HALF_LIFE_MS)).max(0.05) as f32
}

/// The weight of a note by where its text came from.
pub fn trust_factor(trust: MemoryTrust) -> f32 {
    match trust {
        MemoryTrust::Owner => 1.2,
        MemoryTrust::UserStated => 1.1,
        MemoryTrust::ModelInferred => 1.0,
        MemoryTrust::ModelAfterTools | MemoryTrust::ToolDerived => 0.8,
    }
}

/// Text in the form two notes are compared in: lower case, words only.
pub fn normalized(text: &str) -> String {
    graphrt_core::memory::tokens(text).join(" ")
}

/// Share of the words of the two texts that both have (Jaccard over the word sets).
pub fn overlap(a: &str, b: &str) -> f64 {
    let a: HashSet<String> = words(a).into_iter().collect();
    let b: HashSet<String> = words(b).into_iter().collect();
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let shared = a.intersection(&b).count();
    shared as f64 / (a.len() + b.len() - shared) as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyword_search_ranks_the_matching_note_first() {
        let index = Index::build(
            ["prefers invoices as pdf", "lives in jakarta", "invoices go to the billing address"].into_iter(),
        );
        let hits = index.search("invoice pdf");
        assert_eq!(hits.first().map(|h| h.0), Some(0));
        assert_eq!(hits.len(), 2, "\"invoice\" finds \"invoices\" too");
        assert!(index.search("tokyo").is_empty());
        assert!(index.search("").is_empty());
    }

    #[test]
    fn fusion_rewards_agreement() {
        let fused = fuse(&[vec![(1, 3.0), (2, 2.0)], vec![(2, 0.9), (3, 0.8)]]);
        assert!(fused[&2] > fused[&1]);
        assert!(fused[&1] > fused[&3]);
    }

    #[test]
    fn age_and_trust_weigh_a_hit() {
        let day = 24 * 3600 * 1000;
        assert!((age_factor(0, 0) - 1.0).abs() < 1e-6);
        assert!((age_factor(0, 90 * day) - 0.5).abs() < 1e-3);
        assert!(age_factor(0, 10_000 * day) >= 0.05);
        assert!(trust_factor(MemoryTrust::Owner) > trust_factor(MemoryTrust::ToolDerived));
    }

    #[test]
    fn notes_that_say_the_same_are_found() {
        assert_eq!(normalized("Prefers  INVOICES as PDF."), "prefers invoices as pdf");
        assert!(overlap("prefers invoices as pdf", "Prefers invoices as PDF files") >= 0.8);
        assert!(overlap("lives in jakarta", "prefers tea") < 0.1);
    }
}
