use std::cmp::Ordering;

use roaring::RoaringTreemap;

use crate::posting::DeleteSet;
use crate::types::DocId;

/// A filter matching fewer than rows / this many documents is cheaper to check document by
/// document than to search the whole segment.
const SELECTIVE_FILTER_RATIO: u64 = 32;

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
    allowed: Option<&'a RoaringTreemap>,
}

impl<'a> VectorFilter<'a> {
    pub const fn new(deleted: &'a DeleteSet, allowed: Option<&'a RoaringTreemap>) -> Self {
        Self { deleted, allowed }
    }

    pub fn accepts(&self, doc_id: DocId) -> bool {
        !self.deleted.contains(doc_id)
            && self.allowed.is_none_or(|allowed| allowed.contains(doc_id))
    }

    pub const fn allowed(&self) -> Option<&'a RoaringTreemap> {
        self.allowed
    }

    /// True when the filter keeps so few of `rows` documents that checking them one by one
    /// beats searching the whole segment.
       pub fn is_selective(&self, rows: usize) -> bool {
        self.allowed.is_some_and(|allowed| {
            allowed.len().saturating_mul(SELECTIVE_FILTER_RATIO)
                < u64::try_from(rows).unwrap_or(u64::MAX)
        })
    }
}

/// Result order everywhere: higher score first, then lower doc id, so ties never depend on
/// the order documents were offered or segments were merged.
pub(super) fn hit_order(a: &VectorHit, b: &VectorHit) -> Ordering {
    b.score.total_cmp(&a.score).then(a.doc_id.cmp(&b.doc_id))
}

/// Keeps the `k` best hits by `hit_order`, sorted best first.
pub(super) struct TopK {
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
        if self.hits.len() == self.k
            && self
                .hits
                .last()
                .is_some_and(|worst| hit_order(&hit, worst) != Ordering::Less)
        {
            return;
        }
        let pos = self
            .hits
            .partition_point(|existing| hit_order(existing, &hit) == Ordering::Less);
        self.hits.insert(pos, hit);
        self.hits.truncate(self.k);
    }

    pub(crate) fn into_sorted(self) -> Vec<VectorHit> {
        self.hits
    }
}
