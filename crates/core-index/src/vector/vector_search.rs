use std::cmp::Ordering;
use std::io;

use ahash::HashSet;

use super::file::SegmentVectors;
use crate::posting::DeleteSet;
use crate::types::{DocId, XPathId};

/// A filter matching fewer than rows / this many documents is cheaper to check document by
/// document than to search the whole segment (scan now, graph walk later).
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
fn hit_order(a: &VectorHit, b: &VectorHit) -> Ordering {
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

    pub(crate) fn into_sorted(self) -> Vec<VectorHit> {
        self.hits
    }
}

/// Top-k over every segment's vectors for `xpath` that `filter` accepts.
pub fn search_segments(
    vectors: &[SegmentVectors],
    xpath: XPathId,
    query: &[f32],
    k: usize,
    filter: &VectorFilter<'_>,
) -> io::Result<Vec<VectorHit>> {
    let lists: Vec<Vec<VectorHit>> = vectors
        .iter()
        .filter(|entry| entry.xpath == xpath)
        .map(|entry| entry.search(query, k, filter))
        .collect();
    Ok(merge_hits(lists, k))
}

/// Combines per-segment (or per-shard) result lists into one top-k list.
/// If a document appears more than once, its best score is kept.
pub fn merge_hits(lists: Vec<Vec<VectorHit>>, k: usize) -> Vec<VectorHit> {
    let mut all: Vec<VectorHit> = lists.into_iter().flatten().collect();
    all.sort_by(hit_order);
    let mut seen = HashSet::default();
    all.retain(|hit| seen.insert(hit.doc_id));
    all.truncate(k);
    all
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vector::{VectorFile, VectorFileWriter};
    use std::sync::Arc;

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
    fn merge_hits_dedups_and_limits() {
        let a = vec![
            VectorHit {
                doc_id: 1,
                score: 0.9,
            },
            VectorHit {
                doc_id: 2,
                score: 0.4,
            },
        ];
        let b = vec![
            VectorHit {
                doc_id: 1,
                score: 0.8,
            },
            VectorHit {
                doc_id: 3,
                score: 0.6,
            },
        ];
        let merged = merge_hits(vec![a, b], 2);
        assert_eq!(
            merged,
            vec![
                VectorHit {
                    doc_id: 1,
                    score: 0.9
                },
                VectorHit {
                    doc_id: 3,
                    score: 0.6
                },
            ]
        );
    }

    #[test]
    fn filter_selectivity() {
        let deleted = DeleteSet::new();
        let small: HashSet<DocId> = [1, 2].into_iter().collect();
        assert!(VectorFilter::new(&deleted, Some(&small)).is_selective(1_000));
        assert!(!VectorFilter::new(&deleted, Some(&small)).is_selective(10));
        assert!(!VectorFilter::new(&deleted, None).is_selective(1_000));
    }

    fn vector_fixture(name: &str) -> (std::path::PathBuf, Vec<SegmentVectors>) {
        let dir =
            std::env::temp_dir().join(format!("corelamo-vsearch-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let make = |file: &str, doc: DocId, row: [i8; 3]| {
            let path = dir.join(file);
            let mut writer = VectorFileWriter::create(&path, "m", 3, 127.0).unwrap();
            writer.push(doc, &row).unwrap();
            writer.finish().unwrap();
            Arc::new(VectorFile::open(&path).unwrap())
        };
        let vectors = vec![
            SegmentVectors {
                segment: dir.join("segment-1.idx"),
                xpath: 3,
                file: make("a.vec", 1, [127, 0, 0]),
            },
            SegmentVectors {
                segment: dir.join("segment-2.idx"),
                xpath: 3,
                file: make("b.vec", 2, [90, 90, 0]),
            },
            SegmentVectors {
                segment: dir.join("segment-1.idx"),
                xpath: 4,
                file: make("c.vec", 3, [127, 0, 0]),
            },
        ];
        (dir, vectors)
    }

    #[test]
    fn search_segments_only_uses_the_requested_field() {
        let (dir, vectors) = vector_fixture("field");
        let deleted = DeleteSet::new();
        let filter = VectorFilter::new(&deleted, None);
        let hits = search_segments(&vectors, 3, &[1.0, 0.0, 0.0], 10, &filter).unwrap();
        assert_eq!(
            hits.iter().map(|h| h.doc_id).collect::<Vec<_>>(),
            vec![1, 2]
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn search_segments_respects_allowed() {
        let (dir, vectors) = vector_fixture("allowed");
        let deleted = DeleteSet::new();
        let allowed: HashSet<DocId> = [2].into_iter().collect();
        let filter = VectorFilter::new(&deleted, Some(&allowed));
        let hits = search_segments(&vectors, 3, &[1.0, 0.0, 0.0], 10, &filter).unwrap();
        assert_eq!(hits.iter().map(|h| h.doc_id).collect::<Vec<_>>(), vec![2]);
        let _ = std::fs::remove_dir_all(dir);
    }
}
