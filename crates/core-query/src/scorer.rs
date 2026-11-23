//! The query layer's scoring interface.
//! One `Scorer` per query node (term, AND, OR, ...). A scorer is a forward-only stream of
//! documents with a score and a safe upper bound, so the top-k collector can skip cheaply.
use core_index::{search::SearchStats, types::DocId};

pub trait Scorer {
    ///The document the scorer sits on, or `None` when it is exhausted.
    fn doc(&self) -> Option<DocId>;

    ///Move past the current document.
    fn next(&mut self);

    ///Move forward to the first document >= `target` (never backwards).
    fn advance_to(&mut self, target: DocId);

    ///Score `doc`, which the scorer must already be sitting on.
    fn score(&mut self, stats: &dyn SearchStats, doc: DocId) -> u64;

    ///The largest score this scorer can ever contribute (used for skipping).
    fn upper_bound(&self) -> u64;

    /// The largest score reachable in the current disk block, if the scorer has blocks.
    fn block_bound(&self) -> Option<u64> {
        None
    }

    ///How much of the query this scorer covers (the WAND coverage threshold).
    fn weight(&self) -> f32 {
        0.0
    }
}
