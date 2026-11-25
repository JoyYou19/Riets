//! Disjunction: a document matches if any child is on it; the scores add up.

use core_index::{search::SearchStats, types::DocId};
use roaring::RoaringTreemap;

use crate::{
    collector,
    scorer::{BoxScorer, Scorer},
    wand::WandHit,
};

/// A disjunction: matches any child, sums the children on the document.
pub(crate) struct OrScorer<'a> {
    children: Vec<BoxScorer<'a>>,
    upper_bound: u64,
    coverage: Option<f32>,
}

impl<'a> OrScorer<'a> {
    /// Builds the disjunction; `coverage` is the share of the query a doc must match.
    pub(crate) fn new(children: Vec<BoxScorer<'a>>, coverage: Option<f32>) -> Self {
        let upper_bound = children.iter().map(|child| child.upper_bound()).sum();
        Self {
            children,
            upper_bound,
            coverage,
        }
    }
}

impl Scorer for OrScorer<'_> {
    fn doc(&self) -> Option<DocId> {
        self.children.iter().filter_map(|child| child.doc()).min()
    }

    /// Move past the current document on every child that holds it.
    fn next(&mut self) {
        if let Some(doc) = self.doc() {
            for child in &mut self.children {
                if child.doc() == Some(doc) {
                    child.next();
                }
            }
        }
    }

    /// Move every child that is behind `target` forward to it.
    fn advance_to(&mut self, target: DocId) {
        for child in &mut self.children {
            if child.doc().is_some_and(|doc| doc < target) {
                child.advance_to(target);
            }
        }
    }

    /// Sum the children that are on `doc`.
    fn score(&mut self, stats: &dyn SearchStats, doc: DocId) -> u64 {
        self.children
            .iter_mut()
            .filter(|child| child.doc() == Some(doc))
            .map(|child| child.score(stats, doc))
            .sum()
    }

    /// A safe ceiling: every child's ceiling added up.
    fn upper_bound(&self) -> u64 {
        self.upper_bound
    }

    fn weight(&self) -> f32 {
        self.children.iter().map(|child| child.weight()).sum()
    }

    /// Disjunctions use WAND instead of the default full walk.
    fn top_k(
        &mut self,
        stats: &dyn SearchStats,
        k: usize,
        restrict: Option<&RoaringTreemap>,
    ) -> Vec<WandHit> {
        collector::top_k_with_any(stats, &mut self.children, k, self.coverage, restrict, 0).0
    }
}
