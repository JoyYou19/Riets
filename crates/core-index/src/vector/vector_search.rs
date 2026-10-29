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

