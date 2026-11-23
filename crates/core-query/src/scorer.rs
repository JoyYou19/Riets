//! The query layer's scoring interface.
//! One `Scorer` per query node (term, AND, OR, ...). A scorer is a forward-only stream of
//! documents with a score and a safe upper bound, so the top-k collector can skip cheaply.
use core_index::{search::SearchStats, types::DocId};
use roaring::RoaringTreemap;

use crate::wand::WandHit;

///A stream of documents with scores — the unit the top-k collector works on.
pub(crate) trait Scorer {
    /// The document the scorer sits on, or `None` when it is exhausted.
    fn doc(&self) -> Option<DocId>;

    /// Move past the current document.
    fn next(&mut self);

    /// Move forward to the first document >= `target` (never backwards).
    fn advance_to(&mut self, target: DocId);

    /// Score `doc`, which the scorer must already be sitting on.
    fn score(&mut self, stats: &dyn SearchStats, doc: DocId) -> u64;

    /// The largest score this scorer can ever contribute (used for skipping).
    fn upper_bound(&self) -> u64;

    /// How much of the query this scorer covers (the WAND coverage threshold).
    fn weight(&self) -> f32 {
        0.0
    }

    /// The top `k` documents, best first (default: walk every one).
    fn top_k(
        &mut self,
        stats: &dyn SearchStats,
        k: usize,
        restrict: Option<&RoaringTreemap>,
    ) -> Vec<WandHit> {
        crate::collector::collect_all(stats, self, k, restrict)
    }
}

/// A boxed scorer borrowing from the index for `'a`.
pub(crate) type BoxScorer<'a> = Box<dyn Scorer + 'a>;
