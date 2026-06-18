use std::sync::Arc;

use core_index::types::{DocId, Position};

pub struct ScoredPosting {
    pub doc_id: DocId,
    pub positions: Arc<[Position]>,
    pub weight_sum: u32,
    pub matched_terms: usize,
    pub density: f32,
}
