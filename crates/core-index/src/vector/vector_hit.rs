use std::cmp::Ordering;

use ahash::HashSet;


use crate::posting::DeleteSet;
use crate::types::DocId;

/// A filter matching fewer than rows / this many documents is cheaper to check document by
/// document than to search the whole segment.
const SELECTIVE_FILTER_RATIO: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VectorHit {
    pub doc_id: DocId,
    /// Cosine similarity in [-1, 1]; higher is closer.
    pub score: f32,
}

/// Which documents a vector search may return: not deleted, and inside `allowed` when a
/// filter is given. Every search method (exact scan, HNSW) asks only this.
#[derive(Clone, Copy)]
pub struct VectorFilter<'a> {
    deleted: &'a DeleteSet,
    allowed: Option<&'a HashSet<DocId>>,
}

impl<'a> VectorFilter<'a> {
    pub fn new(deleted: &'a DeleteSet, allowed: Option<&'a HashSet<DocId>>) -> Self {
        Self { deleted, allowed }
    }

    pub fn accepts(&self, doc_id: DocId) -> bool {
        !self.deleted.contains(doc_id)
            && self.allowed.is_none_or(|allowed| allowed.contains(&doc_id))
    }

    pub fn allowed(&self) -> Option<&'a HashSet<DocId>> {
        self.allowed
    }

    /// True when the filter keeps so few of `rows` documents that checking them one by one
    /// beats searching the whole segment.
    pub fn is_selective(&self, rows: usize) -> bool {
        self.allowed
            .is_some_and(|allowed| allowed.len().saturating_mul(SELECTIVE_FILTER_RATIO) < rows)
    }
}

/// Result order everywhere: higher score first, then lower doc id, so ties never depend on
/// the order documents were offered or segments were merged.
pub(crate) fn hit_order(a: &VectorHit, b: &VectorHit) -> Ordering {
    b.score.total_cmp(&a.score).then(a.doc_id.cmp(&b.doc_id))
}

/// Keeps the `k` best hits by `hit_order`, sorted best first.
pub(crate) struct TopK {
    k: usize,
    hits: Vec<VectorHit>,
}

impl TopK {
    pub(crate) fn new(k: usize) -> Self {
        Self {
            k,
            hits: Vec::with_capacity(k.saturating_add(1)),
        }
    }

    pub(crate) fn offer(&mut self, doc_id: DocId, score: f32) {
        if self.k == 0 || score.is_nan() {
            return;
        }
        let hit = VectorHit { doc_id, score };
        if self.hits.len() == self.k && hit_order(&hit, &self.hits[self.k - 1]) != Ordering::Less {
            return;
        }
        let pos = self
            .hits
            .partition_point(|existing| hit_order(existing, &hit) == Ordering::Less);
        self.hits.insert(pos, hit);
        self.hits.truncate(self.k);
    }
     /// Score of the current k-th hit, once `k` hits are held; anything scoring below it
    /// cannot enter. Lets callers skip work for candidates that cannot make the cut.
    pub(crate) fn threshold(&self) -> Option<f32> {
        (self.hits.len() == self.k && self.k > 0).then(|| self.hits[self.k - 1].score)
    }
    pub(crate) fn into_sorted(self) -> Vec<VectorHit> {
        self.hits
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn top_k_keeps_best_in_order() {
        let mut top = TopK::new(2);
        for (doc, score) in [(1, 0.1), (2, 0.9), (3, 0.5), (4, f32::NAN), (5, 0.7)] {
            top.offer(doc, score);
        }
        let docs: Vec<DocId> = top.into_sorted().iter().map(|h| h.doc_id).collect();
        assert_eq!(docs, vec![2, 5]);
    }

    #[test]
    fn top_k_zero_returns_nothing() {
        let mut top = TopK::new(0);
        top.offer(1, 1.0);
        assert!(top.threshold().is_none());
        assert!(top.into_sorted().is_empty());
    }

    #[test]
    fn top_k_ties_keep_lowest_doc_id_whatever_the_order() {
        let mut top = TopK::new(1);
        top.offer(5, 0.5);
        top.offer(3, 0.5);
        assert_eq!(top.into_sorted()[0].doc_id, 3);
    }

    #[test]
    fn threshold_appears_once_full() {
        let mut top = TopK::new(2);
        top.offer(1, 0.9);
        assert!(top.threshold().is_none());
        top.offer(2, 0.4);
        assert_eq!(top.threshold(), Some(0.4));
        top.offer(3, 0.6);
        assert_eq!(top.threshold(), Some(0.6));
    }

    #[test]
    fn filter_accepts_respects_deletes_and_allowed() {
        let mut deleted = DeleteSet::new();
        deleted.delete(2);
        let allowed: HashSet<DocId> = [1, 2].into_iter().collect();
        let filter = VectorFilter::new(&deleted, Some(&allowed));
        assert!(filter.accepts(1));
        assert!(!filter.accepts(2), "deleted wins over allowed");
        assert!(!filter.accepts(3), "outside allowed");
    }

    #[test]
    fn filter_selectivity() {
        let deleted = DeleteSet::new();
        let small: HashSet<DocId> = [1, 2].into_iter().collect();
        assert!(VectorFilter::new(&deleted, Some(&small)).is_selective(1_000));
        assert!(!VectorFilter::new(&deleted, Some(&small)).is_selective(10));
        assert!(!VectorFilter::new(&deleted, None).is_selective(1_000));
    }
}

