use core_index::types::DocId;

/// A document's score for one query part. The positions are not kept: nothing reads them after
/// scoring, and copying them cost an allocation per posting.
#[derive(Clone, Debug)]
pub struct ScoredPosting {
    pub doc_id: DocId,
    pub score: u64,
    pub matched_terms: usize,
    pub density: f32,
}
