use std::cmp::Ordering;

use core_index::types::DocId;

#[derive(Debug, Clone, PartialEq)]
pub struct SearchHit {
    pub doc_id: DocId,
    pub matched_terms: usize,
    pub weight_sum: u32,
    pub distance_factor: f32,
    pub score: f32,
    pub tier: u8,
}

#[derive(Debug, Clone)]
pub struct TopHit(pub SearchHit);

impl PartialEq for TopHit {
    fn eq(&self, other: &Self) -> bool {
        self.0.score == other.0.score && self.0.doc_id == other.0.doc_id
    }
}

impl Eq for TopHit {}

impl PartialOrd for TopHit {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for TopHit {
    // BinaryHeap evicts its greatest element, so "greater" means "worse result":
    // a higher tier, then a lower score, then a higher doc id.
    fn cmp(&self, other: &Self) -> Ordering {
        self.0
            .tier
            .cmp(&other.0.tier)
            .then_with(|| {
                other
                    .0
                    .score
                    .partial_cmp(&self.0.score)
                    .unwrap_or(Ordering::Equal)
            })
            .then_with(|| self.0.doc_id.cmp(&other.0.doc_id))
    }
}
