use std::cmp::Ordering;

use core_index::types::DocId;

/// One ranked document. The cascade fills the raw evidence (`base`, `mask`, `distance_factor`,
/// `name_share`); the final scoring turns it into `score`, the same way for every hit.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SearchHit {
    pub doc_id: DocId,
    pub matched_terms: usize,
    pub weight_sum: u32,
    /// How close together the query's words are in the document's best field, 0..=1.
    pub distance_factor: f32,
    pub score: f32,
    pub tier: u8,
    pub level: u8,
    /// BM25 over every field, before coverage, density or the name bonus.
    pub base: f32,
    /// Bit `i` set: query clause `i` matched in some field (by any of its synonym forms).
    pub mask: u64,
    /// Share of the query's weight a name field of this document holds, 0..=1.
    pub name_share: f32,
}

/// The one order of ranked hits everywhere: best tier first, then highest score, then lowest
/// doc id. `total_cmp` keeps the order total even if a score is NaN.
pub fn rank_order(a: &SearchHit, b: &SearchHit) -> Ordering {
    a.tier
        .cmp(&b.tier)
        .then_with(|| a.level.cmp(&b.level))
        .then_with(|| b.score.total_cmp(&a.score))
        .then_with(|| a.doc_id.cmp(&b.doc_id))
}

#[derive(Debug, Clone)]
pub struct TopHit(pub SearchHit);

impl PartialEq for TopHit {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for TopHit {}

impl PartialOrd for TopHit {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for TopHit {
    // BinaryHeap evicts its greatest element, so "greater" means "worse result".
    fn cmp(&self, other: &Self) -> Ordering {
        rank_order(&self.0, &other.0)
    }
}
