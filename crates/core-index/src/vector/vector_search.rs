//! Vector search entry point: decides how each segment is searched and merges the results.
//!
//! Every path returns exact f32 cosine scores, so hits from graph-searched and scanned
//! segments compare directly:
//! - selective filter: score only the allowed rows, in f32
//! - graph present: HNSW search (int8 walk, f32 rescore), exact scan if it cannot answer
//! - otherwise: exact scan (int8 over every row for candidates, f32 rescore)

use std::io;
use std::path::PathBuf;
use std::sync::Arc;

use ahash::HashSet;

use super::file::VectorFile;
use super::vector_hit::{TopK, VectorFilter, VectorHit, hit_order};
use super::hnsw::{HnswGraph, RESCORE_OVERSAMPLING};
use super::kernel::{approx_cosine, dot_f32, dot_i8, quantize_query};
use crate::types::{DocId, XPathId};

/// The vectors of one vector field for one live segment, and its graph if one is built.
#[derive(Debug, Clone)]
pub struct SegmentVectors {
    pub segment: PathBuf,
    pub xpath: XPathId,
    pub file: Arc<VectorFile>,
    /// HNSW graph over exactly `file`. Must be cleared whenever `file` is replaced.
    pub graph: Option<Arc<HnswGraph>>,
}

impl SegmentVectors {
    /// Vectors without a graph; the graph is attached later, once built.
    pub fn new(segment: PathBuf, xpath: XPathId, file: Arc<VectorFile>) -> Self {
        Self {
            segment,
            xpath,
            file,
            graph: None,
        }
    }

    /// Best `k` hits of this segment and field that `filter` accepts, best first.
    /// The one place that decides how a segment is searched. `query` must be L2-normalized.
    pub fn search(&self, query: &[f32], k: usize, filter: &VectorFilter<'_>) -> Vec<VectorHit> {
        if k == 0 || self.file.is_empty() || query.len() != self.file.dims() {
            return Vec::new();
        }
        if let Some(graph) = self.graph.as_deref()
            && !filter.is_selective(self.file.len())
        {
            debug_assert!(
                Arc::ptr_eq(graph.vectors(), &self.file),
                "graph attached to a different vector file than the segment's"
            );
            if let Some(hits) = graph.search(query, k, filter) {
                return hits;
            }
        }
        exact_scan(&self.file, query, k, filter)
    }
}

/// Exact top `k` of `file` that `filter` accepts, with f32 scores.
/// With a selective filter only the allowed rows are read; otherwise every row's int8 codes
/// are scanned for `k * RESCORE_OVERSAMPLING` candidates, which are then rescored in f32.
pub(crate) fn exact_scan(
    file: &VectorFile,
    query: &[f32],
    k: usize,
    filter: &VectorFilter<'_>,
) -> Vec<VectorHit> {
    if k == 0 || file.is_empty() || query.len() != file.dims() {
        return Vec::new();
    }
    if let Some(allowed) = filter.allowed()
        && filter.is_selective(file.len())
    {
        return score_allowed_rows(file, query, k, filter, allowed);
    }

    let query_codes = quantize_query(query);
    let row_scale = file.range().scale();
    let mut candidates = TopK::new(k.saturating_mul(RESCORE_OVERSAMPLING));
    for (row, &doc_id) in file.doc_ids().iter().enumerate() {
        if filter.accepts(doc_id) {
            let similarity = dot_i8(file.codes(row), &query_codes.codes);
            candidates.offer(doc_id, approx_cosine(similarity, row_scale, query_codes.scale));
        }
    }
    rescore(file, query, k, &candidates.into_sorted())
}

/// Few allowed documents: find their rows and score them exactly, skipping the int8 stage.
fn score_allowed_rows(
    file: &VectorFile,
    query: &[f32],
    k: usize,
    filter: &VectorFilter<'_>,
    allowed: &HashSet<DocId>,
) -> Vec<VectorHit> {
    let mut rows: Vec<usize> = allowed
        .iter()
        .filter(|&&doc_id| filter.accepts(doc_id))
        .filter_map(|&doc_id| file.position(doc_id))
        .collect();
    // Row order is file order: reads move forward through the mapping.
    rows.sort_unstable();

    let mut top = TopK::new(k);
    for row in rows {
        top.offer(file.doc_id(row), dot_f32(file.original(row), query));
    }
    top.into_sorted()
}

/// Exact f32 scores for `candidates`; returns the best `k`.
fn rescore(file: &VectorFile, query: &[f32], k: usize, candidates: &[VectorHit]) -> Vec<VectorHit> {
    let mut top = TopK::new(k);
    for candidate in candidates {
        if let Some(row) = file.position(candidate.doc_id) {
            top.offer(candidate.doc_id, dot_f32(file.original(row), query));
        }
    }
    top.into_sorted()
}

/// Top `k` over every segment's vectors for `xpath` that `filter` accepts.
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
    use crate::hash::fmix64;
    use crate::posting::DeleteSet;
    use crate::vector::file::VectorFileWriter;
    use crate::vector::hnsw::{GraphBuildParams, build_and_write};
    use std::fs;
    use std::path::Path;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("corelamo-vsearch-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn unit(seed: u64, dims: usize) -> Vec<f32> {
        let mut v: Vec<f32> = (0..dims as u64)
            .map(|i| (fmix64(seed ^ (i << 20) ^ 1) >> 40) as f32 / (1u64 << 24) as f32 - 0.5)
            .collect();
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        v.iter_mut().for_each(|x| *x /= norm);
        v
    }

    fn write_file(path: &Path, rows: &[(DocId, Vec<f32>)]) -> Arc<VectorFile> {
        let mut writer = VectorFileWriter::create(path, "m", rows[0].1.len()).expect("writer");
        for (doc, vector) in rows {
            writer.push(*doc, vector).expect("push");
        }
        writer.finish().expect("finish");
        Arc::new(VectorFile::open(path).expect("open"))
    }

    /// Exact f32 top `k` by brute force: the reference every path must agree with.
    fn brute_force(file: &VectorFile, query: &[f32], k: usize, filter: &VectorFilter<'_>) -> Vec<VectorHit> {
        let mut top = TopK::new(k);
        for row in 0..file.len() {
            let doc = file.doc_id(row);
            if filter.accepts(doc) {
                top.offer(doc, dot_f32(file.original(row), query));
            }
        }
        top.into_sorted()
    }

    fn three_segments(dir: &Path) -> Vec<SegmentVectors> {
        vec![
            SegmentVectors::new(dir.join("segment-1.idx"), 3, write_file(&dir.join("a.vec"), &[(1, vec![1.0, 0.0, 0.0])])),
            SegmentVectors::new(
                dir.join("segment-2.idx"),
                3,
                write_file(&dir.join("b.vec"), &[(2, vec![0.707_106_8, 0.707_106_8, 0.0])]),
            ),
            SegmentVectors::new(dir.join("segment-1.idx"), 4, write_file(&dir.join("c.vec"), &[(3, vec![1.0, 0.0, 0.0])])),
        ]
    }

    #[test]
    fn search_segments_only_uses_the_requested_field() {
        let dir = temp_dir("field");
        let vectors = three_segments(&dir);
        let deleted = DeleteSet::new();
        let filter = VectorFilter::new(&deleted, None);
        let hits = search_segments(&vectors, 3, &[1.0, 0.0, 0.0], 10, &filter).expect("search");
        assert_eq!(hits.iter().map(|h| h.doc_id).collect::<Vec<_>>(), vec![1, 2]);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn search_segments_respects_allowed() {
        let dir = temp_dir("allowed");
        let vectors = three_segments(&dir);
        let deleted = DeleteSet::new();
        let allowed: HashSet<DocId> = [2].into_iter().collect();
        let filter = VectorFilter::new(&deleted, Some(&allowed));
        let hits = search_segments(&vectors, 3, &[1.0, 0.0, 0.0], 10, &filter).expect("search");
        assert_eq!(hits.iter().map(|h| h.doc_id).collect::<Vec<_>>(), vec![2]);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn exact_scan_scores_are_f32_and_recall_is_high() {
        let dir = temp_dir("scan");
        let rows: Vec<(DocId, Vec<f32>)> = (1..=2_000).map(|d| (d, unit(d, 64))).collect();
        let file = write_file(&dir.join("s.vec"), &rows);
        let deleted = DeleteSet::new();
        let filter = VectorFilter::new(&deleted, None);
        let mut found = 0;
        for q in 0..50 {
            let query = unit(1_000_000 + q, 64);
            let reference: HashSet<DocId> = brute_force(&file, &query, 10, &filter).iter().map(|h| h.doc_id).collect();
            let hits = exact_scan(&file, &query, 10, &filter);
            for hit in &hits {
                let row = file.position(hit.doc_id).expect("row");
                assert_eq!(hit.score, dot_f32(file.original(row), &query), "score must be exact f32");
            }
            found += hits.iter().filter(|h| reference.contains(&h.doc_id)).count();
        }
        let recall = found as f64 / 500.0;
        assert!(recall >= 0.98, "int8 candidates + rescore recall@10 = {recall}");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn selective_filter_is_exact() {
        let dir = temp_dir("selective");
        let rows: Vec<(DocId, Vec<f32>)> = (1..=1_000).map(|d| (d, unit(d, 16))).collect();
        let file = write_file(&dir.join("s.vec"), &rows);
        let deleted = DeleteSet::new();
        let allowed: HashSet<DocId> = [5, 50, 500, 999].into_iter().collect();
        let filter = VectorFilter::new(&deleted, Some(&allowed));
        assert!(filter.is_selective(file.len()));
        let query = unit(42, 16);
        assert_eq!(exact_scan(&file, &query, 3, &filter), brute_force(&file, &query, 3, &filter));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn graph_that_cannot_answer_falls_back_to_the_scan() {
        let dir = temp_dir("fallback");
        let rows: Vec<(DocId, Vec<f32>)> = (1..=2_000).map(|d| (d, unit(d, 16))).collect();
        let file = write_file(&dir.join("s.1.vec"), &rows);
        let graph_path = dir.join("s.1.hnsw");
        build_and_write(&file, &graph_path, &GraphBuildParams::default()).expect("build");
        let graph = Arc::new(HnswGraph::open(&graph_path, Arc::clone(&file)).expect("open graph"));
        let segment = SegmentVectors {
            segment: dir.join("s.idx"),
            xpath: 1,
            file: Arc::clone(&file),
            graph: Some(graph),
        };

        // Not selective (100 * 32 >= 2000), but too few accepted for k = 200: graph says None.
        let deleted = DeleteSet::new();
        let allowed: HashSet<DocId> = (1..=100).collect();
        let filter = VectorFilter::new(&deleted, Some(&allowed));
        assert!(!filter.is_selective(file.len()));
        let query = unit(7, 16);
        assert_eq!(segment.search(&query, 200, &filter), brute_force(&file, &query, 200, &filter));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn empty_results_for_zero_k_and_wrong_dims() {
        let dir = temp_dir("edges");
        let file = write_file(&dir.join("s.vec"), &[(1, vec![1.0, 0.0, 0.0])]);
        let segment = SegmentVectors::new(dir.join("s.idx"), 1, file);
        let deleted = DeleteSet::new();
        let filter = VectorFilter::new(&deleted, None);
        assert!(segment.search(&[1.0, 0.0, 0.0], 0, &filter).is_empty());
        assert!(segment.search(&[1.0, 0.0], 5, &filter).is_empty());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn merge_hits_dedups_and_limits() {
        let hit = |doc_id, score| VectorHit { doc_id, score };
        let merged = merge_hits(vec![vec![hit(1, 0.9), hit(2, 0.4)], vec![hit(1, 0.8), hit(3, 0.6)]], 2);
        assert_eq!(merged, vec![hit(1, 0.9), hit(3, 0.6)]);
    }
}