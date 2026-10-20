use crate::types::DocId;
use std::io;

use super::file::SegmentVectors;
use crate::posting::DeleteSet;
use crate::types::XPathId;
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VectorHit {
    pub doc_id: DocId,
    /// Cosine similarity in [-1, 1]; higher is closer.
    pub score: f32,
}

/// Keeps the `k` highest-scoring hits, sorted best first.
pub(crate) struct TopK {
    k: usize,
    hits: Vec<VectorHit>,
}

/// Exact top-k over every segment's vectors for `xpath`, skipping deleted documents.
pub fn search_segments(
    vectors: &[SegmentVectors],
    xpath: XPathId,
    query: &[f32],
    k: usize,
    deleted: &DeleteSet,
) -> io::Result<Vec<VectorHit>> {
    let mut lists = Vec::new();
    for entry in vectors.iter().filter(|entry| entry.xpath == xpath) {
        lists.push(entry.file.search(query, k, deleted)?);
    }
    Ok(merge_hits(lists, k))
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
        if self.hits.len() == self.k && score <= self.hits[self.k - 1].score {
            return;
        }
        let pos = self.hits.partition_point(|hit| hit.score >= score);
        self.hits.insert(pos, VectorHit { doc_id, score });
        self.hits.truncate(self.k);
    }

    pub(crate) fn into_sorted(self) -> Vec<VectorHit> {
        self.hits
    }
}

/// Combines per-segment (or per-shard) result lists into one top-k list.
/// If a document appears more than once, its best score is kept.
pub fn merge_hits(lists: Vec<Vec<VectorHit>>, k: usize) -> Vec<VectorHit> {
    let mut all: Vec<VectorHit> = lists.into_iter().flatten().collect();
    all.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.doc_id.cmp(&b.doc_id)));
    let mut seen = ahash::HashSet::default();
    all.retain(|hit| seen.insert(hit.doc_id));
    all.truncate(k);
    all
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
        assert!(top.into_sorted().is_empty());
    }

    #[test]
    fn merge_hits_dedups_and_limits() {
        let a = vec![VectorHit { doc_id: 1, score: 0.9 }, VectorHit { doc_id: 2, score: 0.4 }];
        let b = vec![VectorHit { doc_id: 1, score: 0.8 }, VectorHit { doc_id: 3, score: 0.6 }];
        let merged = merge_hits(vec![a, b], 2);
        assert_eq!(merged, vec![
            VectorHit { doc_id: 1, score: 0.9 },
            VectorHit { doc_id: 3, score: 0.6 },
        ]);
    }
        #[test]
    fn search_segments_only_uses_the_requested_field() {
        use crate::vector::{VectorFile, VectorFileWriter};
        use std::sync::Arc;

        let dir = std::env::temp_dir().join(format!("corelamo-vsearch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let make = |name: &str, doc: DocId, row: [i8; 3]| {
            let path = dir.join(name);
            let mut w = VectorFileWriter::create(&path, "m", 3, 127.0).unwrap();
            w.push(doc, &row).unwrap();
            w.finish().unwrap();
            Arc::new(VectorFile::open(&path).unwrap())
        };
        let vectors = vec![
            SegmentVectors { segment: dir.join("segment-1.idx"), xpath: 3, file: make("a.vec", 1, [127, 0, 0]) },
            SegmentVectors { segment: dir.join("segment-2.idx"), xpath: 3, file: make("b.vec", 2, [90, 90, 0]) },
            SegmentVectors { segment: dir.join("segment-1.idx"), xpath: 4, file: make("c.vec", 3, [127, 0, 0]) },
        ];

        let hits = search_segments(&vectors, 3, &[1.0, 0.0, 0.0], 10, &DeleteSet::new()).unwrap();
        assert_eq!(hits.iter().map(|h| h.doc_id).collect::<Vec<_>>(), vec![1, 2]);
    }
}