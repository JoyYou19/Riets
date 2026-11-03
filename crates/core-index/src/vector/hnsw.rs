//! HNSW graph of one vector file: the facade for `hnsw/` and the query-side search.
//!
//! The graph stores row numbers only; every score comes from the vector file, so a missing
//! graph costs speed and never correctness. A graph is bound to the exact vector file it was
//! built from by that file's `file_id`.
//!
//! Search: int8 walk (greedy descent, then a beam search on level 0 that respects the
//! filter), then the beam's candidates are rescored with the f32 originals.
mod graph_builder;
mod layout;
mod walk;

pub use graph_builder::{GraphBuildParams, MIN_ROWS_FOR_GRAPH, build_graph_file};
use layout::{GraphHeader, GraphLayout, NO_UPPER_SLOTS, read_u32};
use walk::{GraphWalk, NeighbourLists, SimilarityToTarget, with_visited_set};

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use memmap2::Mmap;

use super::file::VectorFile;
use super::vector_hit::{TopK, VectorFilter, VectorHit};
use super::kernel::{dot_f32, dot_i8, quantize_query};
use crate::types::XPathId;

/// Smallest beam for a search; raised to `k * RESCORE_OVERSAMPLING` for larger `k`.
/// The main recall/speed knob.
pub const DEFAULT_SEARCH_BEAM_WIDTH: usize = 64;
/// The beam holds at least this many candidates per requested hit, so the f32 rescore has
/// room to fix the int8 order.
pub(crate) const RESCORE_OVERSAMPLING: usize = 3;
/// A search computing more than node_count / this many similarities gives up and lets the
/// exact scan answer: past that, the random-access walk costs as much as a sequential scan.
const BUDGET_FRACTION_OF_NODES: usize = 4;
/// Budget floor per beam slot, so small graphs still get a complete walk.
const MIN_COMPUTATIONS_PER_BEAM_SLOT: usize = 64;

/// `segment-12.idx` + xpath 3 -> `segment-12.3.hnsw`, next to `segment-12.3.vec`.
pub fn hnsw_file_path(segment_path: &Path, xpath: XPathId) -> PathBuf {
    segment_path.with_extension(format!("{xpath}.hnsw"))
}

/// Read-only, memory-mapped HNSW graph, validated against the vector file it indexes.
#[derive(Debug)]
pub struct HnswGraph {
    mmap: Mmap,
    path: PathBuf,
    vectors: Arc<VectorFile>,
    header: GraphHeader,
    layout: GraphLayout,
}

impl HnswGraph {
    /// Opens the graph at `path` for `vectors`. `InvalidData` means the file is corrupt or
    /// belongs to another vector file: delete it and rebuild.
    pub fn open(path: impl AsRef<Path>, vectors: Arc<VectorFile>) -> io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        let file = File::open(&path)?;
        // SAFETY: graph files are never modified after the rename in the builder, and a
        // replacement is a new inode, so the mapping cannot change underneath us.
        let mmap = unsafe { Mmap::map(&file)? };

        let header = GraphHeader::read_from(&mmap, &path)?;
        if header.vector_file_id != vectors.file_id() {
            return Err(corrupt(&path, "built from a different vector file"));
        }
        if header.node_count as usize != vectors.len() || header.dims as usize != vectors.dims() {
            return Err(corrupt(
                &path,
                format!(
                    "{} nodes of {} dims, vector file has {} rows of {} dims",
                    header.node_count,
                    header.dims,
                    vectors.len(),
                    vectors.dims()
                ),
            ));
        }
        let layout = GraphLayout::new(&header).ok_or_else(|| corrupt(&path, "size overflows usize"))?;
        if layout.total_length != mmap.len() {
            return Err(corrupt(&path, format!("length {} does not match its header", mmap.len())));
        }
        if (mmap.as_ptr() as usize) % 4 != 0 {
            return Err(corrupt(&path, "mapping is not aligned"));
        }

        let graph = Self {
            mmap,
            path,
            vectors,
            header,
            layout,
        };
        graph.validate_links()?;
        Ok(graph)
    }

    /// Checks every level, slot start and link once, so searches never index out of bounds.
    /// Reads the whole file; done at open, never per query.
    fn validate_links(&self) -> io::Result<()> {
        let node_count = self.node_count();
        let top_level = usize::from(self.header.top_level);
        if self.node_level(self.header.entry_node as usize) != top_level {
            return Err(corrupt(&self.path, "entry node is not on the top level"));
        }

        let mut next_upper_slot = 0usize;
        for node in 0..node_count {
            let level = self.node_level(node);
            if level > top_level {
                return Err(corrupt(&self.path, format!("node {node} above the top level")));
            }
            let slot_start = self.upper_slot_start(node);
            if level == 0 {
                if slot_start != NO_UPPER_SLOTS {
                    return Err(corrupt(&self.path, format!("level-0 node {node} owns upper slots")));
                }
            } else {
                if slot_start as usize != next_upper_slot {
                    return Err(corrupt(&self.path, format!("bad upper slot start for node {node}")));
                }
                next_upper_slot += level;
            }

            for list_level in 0..=level {
                let max_links = self.max_links_at(list_level);
                let offset = self.layout.slot_offset(node, list_level, slot_start);
                let length = read_u32(&self.mmap, offset) as usize;
                if length > max_links {
                    return Err(corrupt(&self.path, format!("node {node} has {length} links")));
                }
                for &linked in self.neighbour_list(node, list_level) {
                    let linked = linked as usize;
                    if linked >= node_count || linked == node || self.node_level(linked) < list_level {
                        return Err(corrupt(
                            &self.path,
                            format!("node {node} links to invalid node {linked} on level {list_level}"),
                        ));
                    }
                }
            }
        }
        if next_upper_slot != self.header.upper_slot_count as usize {
            return Err(corrupt(&self.path, "upper slot count does not match node levels"));
        }
        Ok(())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The vector file this graph indexes.
    pub fn vectors(&self) -> &Arc<VectorFile> {
        &self.vectors
    }

    pub fn node_count(&self) -> usize {
        self.header.node_count as usize
    }

    fn node_level(&self, node: usize) -> usize {
        usize::from(self.mmap[self.layout.node_levels_offset + node])
    }

    fn upper_slot_start(&self, node: usize) -> u32 {
        read_u32(&self.mmap, self.layout.upper_slot_starts_offset + node * 4)
    }

    fn max_links_at(&self, level: usize) -> usize {
        if level == 0 {
            usize::from(self.header.max_links_level0)
        } else {
            usize::from(self.header.max_links_upper)
        }
    }

    /// `node`'s links on `level`, read in place. `node` must be on `level`, and the length
    /// must have been checked (always true after `open`).
    fn neighbour_list(&self, node: usize, level: usize) -> &[u32] {
        let offset = self.layout.slot_offset(node, level, self.upper_slot_start(node));
        let length = (read_u32(&self.mmap, offset) as usize).min(self.max_links_at(level));
        let bytes = &self.mmap[offset + 4..offset + 4 + length * 4];
        // SAFETY: every section starts on a 64-byte boundary of a mapping checked to be 4-byte
        // aligned at open, and slots are whole multiples of 4 bytes, so `bytes` is 4-byte
        // aligned. It holds `length` little-endian u32s (little-endian host enforced in
        // file.rs), and every bit pattern is a valid u32.
        #[allow(clippy::cast_ptr_alignment, reason = "alignment checked at open")]
        unsafe {
            std::slice::from_raw_parts(bytes.as_ptr().cast::<u32>(), length)
        }
    }

    /// Approximate top `k` of the vector file that `filter` accepts, scored exactly.
    ///
    /// Returns `None` when the walk cannot vouch for its answer: it used up its budget, or
    /// found fewer than `k` accepted rows. The caller then runs the exact scan.
    /// `query` must be L2-normalized.
    pub fn search(&self, query: &[f32], k: usize, filter: &VectorFilter<'_>) -> Option<Vec<VectorHit>> {
        if k == 0 {
            return Some(Vec::new());
        }
        if query.len() != self.vectors.dims() {
            return None;
        }

        let query_codes = quantize_query(query);
        let target = QueryCodesTarget {
            vectors: &self.vectors,
            query_codes: &query_codes.codes,
        };
        let walk = GraphWalk {
            lists: self,
            target: &target,
            node_count: self.node_count(),
        };
        let beam_width = DEFAULT_SEARCH_BEAM_WIDTH.max(k.saturating_mul(RESCORE_OVERSAMPLING));
        let budget = (self.node_count() / BUDGET_FRACTION_OF_NODES)
            .max(beam_width.saturating_mul(MIN_COMPUTATIONS_PER_BEAM_SLOT));
        let accept = |node: u32| filter.accepts(self.vectors.doc_id(node as usize));

        let level0 = with_visited_set(|visited| {
            let mut scratch = Vec::with_capacity(usize::from(self.header.max_links_level0));
            let start = walk.score(self.header.entry_node);
            let closest = walk.greedy_descend(start, usize::from(self.header.top_level), 1, &mut scratch);
            walk.search_level(&[closest], beam_width, 0, accept, budget, visited, &mut scratch)
        });

        if level0.stopped_by_budget || level0.best_first.len() < k.min(self.node_count()) {
            return None;
        }

        let mut top = TopK::new(k);
        for candidate in &level0.best_first {
            let row = candidate.node as usize;
            top.offer(self.vectors.doc_id(row), dot_f32(self.vectors.original(row), query));
        }
        Some(top.into_sorted())
    }
}

impl NeighbourLists for HnswGraph {
    fn neighbours_into(&self, node: u32, level: usize, out: &mut Vec<u32>) {
        out.clear();
        out.extend_from_slice(self.neighbour_list(node as usize, level));
    }
}

/// Int8 similarity of a node to the query. Not divided by the scales: every node of one file
/// shares the same scale, so the order is the same and this value never leaves the walk.
struct QueryCodesTarget<'a> {
    vectors: &'a VectorFile,
    query_codes: &'a [u8],
}

impl SimilarityToTarget for QueryCodesTarget<'_> {
    fn similarity(&self, node: u32) -> f32 {
        dot_i8(self.vectors.codes(node as usize), self.query_codes) as f32
    }
}

fn corrupt(path: &Path, msg: impl std::fmt::Display) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("hnsw file {}: {msg}", path.display()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hash::fmix64;
    use crate::posting::DeleteSet;
    use crate::types::DocId;
    use crate::vector::file::VectorFileWriter;
    use ahash::HashSet;
    use std::fs;

    const DIMS: usize = 32;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("corelamo-hnsw-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn noise(seed: u64) -> f32 {
        (fmix64(seed ^ 0xABCD) >> 40) as f32 / (1u64 << 24) as f32 - 0.5
    }

    fn normalized(mut v: Vec<f32>) -> Vec<f32> {
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        v.iter_mut().for_each(|x| *x /= norm);
        v
    }

    /// Vectors in 20 clusters, the shape real embeddings have; uniform noise is too easy.
    fn clustered(index: u64) -> Vec<f32> {
        let cluster = index % 20;
        normalized(
            (0..DIMS as u64)
                .map(|d| noise(cluster * 1_000 + d) + 0.3 * noise(index * 7_919 + d))
                .collect(),
        )
    }

    /// Vector file with doc ids 10, 20, 30, ... and its graph.
    fn fixture(dir: &Path, rows: u64) -> HnswGraph {
        let vec_path = dir.join("s.1.vec");
        let mut writer = VectorFileWriter::create(&vec_path, "m", DIMS).expect("writer");
        for i in 0..rows {
            writer.push((i + 1) * 10, &clustered(i)).expect("push");
        }
        writer.finish().expect("finish");
        let vectors = Arc::new(VectorFile::open(&vec_path).expect("open vectors"));
        let graph_path = dir.join("s.1.hnsw");
        graph_builder::build_and_write(&vectors, &graph_path, &GraphBuildParams::default())
            .expect("build");
        HnswGraph::open(&graph_path, vectors).expect("open graph")
    }

    fn exact_top(graph: &HnswGraph, query: &[f32], k: usize, filter: &VectorFilter<'_>) -> Vec<VectorHit> {
        let vectors = graph.vectors();
        let mut top = TopK::new(k);
        for row in 0..vectors.len() {
            let doc = vectors.doc_id(row);
            if filter.accepts(doc) {
                top.offer(doc, dot_f32(vectors.original(row), query));
            }
        }
        top.into_sorted()
    }

    #[test]
    fn recall_at_10_is_high() {
        let dir = temp_dir("recall");
        let graph = fixture(&dir, 5_000);
        let deleted = DeleteSet::new();
        let filter = VectorFilter::new(&deleted, None);
        let mut found = 0;
        for q in 0..100 {
            let query = clustered(1_000_000 + q);
            let exact: HashSet<DocId> = exact_top(&graph, &query, 10, &filter).iter().map(|h| h.doc_id).collect();
            let approx = graph.search(&query, 10, &filter).expect("unfiltered search answers");
            found += approx.iter().filter(|h| exact.contains(&h.doc_id)).count();
        }
        let recall = found as f64 / 1_000.0;
        assert!(recall >= 0.95, "recall@10 = {recall}");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn scores_are_exact_f32() {
        let dir = temp_dir("scores");
        let graph = fixture(&dir, 2_000);
        let deleted = DeleteSet::new();
        let filter = VectorFilter::new(&deleted, None);
        let query = clustered(77_777);
        for hit in graph.search(&query, 5, &filter).expect("answer") {
            let row = graph.vectors().position(hit.doc_id).expect("row");
            assert_eq!(hit.score, dot_f32(graph.vectors().original(row), &query));
        }
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn filter_and_deletes_are_respected() {
        let dir = temp_dir("filter");
        let graph = fixture(&dir, 3_000);
        let mut deleted = DeleteSet::new();
        for doc in (10..=30_000).step_by(30) {
            deleted.delete(doc);
        }
        let allowed: HashSet<DocId> = (10..=30_000).step_by(20).collect();
        let filter = VectorFilter::new(&deleted, Some(&allowed));
        for q in 0..20 {
            if let Some(hits) = graph.search(&clustered(500_000 + q), 10, &filter) {
                assert_eq!(hits.len(), 10);
                assert!(hits.iter().all(|h| filter.accepts(h.doc_id)));
            }
        }
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn too_few_accepted_rows_returns_none() {
        let dir = temp_dir("fallback");
        let graph = fixture(&dir, 2_000);
        let deleted = DeleteSet::new();
        let allowed: HashSet<DocId> = [10, 20, 30].into_iter().collect();
        let filter = VectorFilter::new(&deleted, Some(&allowed));
        assert!(graph.search(&clustered(1), 10, &filter).is_none());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn zero_k_and_wrong_dims() {
        let dir = temp_dir("edges");
        let graph = fixture(&dir, 500);
        let deleted = DeleteSet::new();
        let filter = VectorFilter::new(&deleted, None);
        assert_eq!(graph.search(&clustered(1), 0, &filter), Some(Vec::new()));
        assert!(graph.search(&[1.0, 0.0], 5, &filter).is_none());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn graph_for_a_rewritten_vector_file_is_rejected() {
        let dir = temp_dir("stale");
        let graph = fixture(&dir, 500);
        let graph_path = graph.path().to_path_buf();
        drop(graph);

        // Same rows, new write: new file_id, so the old graph must not open.
        let vec_path = dir.join("s.1.vec");
        let mut writer = VectorFileWriter::create(&vec_path, "m", DIMS).expect("writer");
        for i in 0..500 {
            writer.push((i + 1) * 10, &clustered(i)).expect("push");
        }
        writer.finish().expect("finish");
        let vectors = Arc::new(VectorFile::open(&vec_path).expect("open"));
        let err = HnswGraph::open(&graph_path, vectors).expect_err("stale graph");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn truncated_graph_is_invalid_data() {
        let dir = temp_dir("truncated");
        let graph = fixture(&dir, 500);
        let graph_path = graph.path().to_path_buf();
        let vectors = Arc::clone(graph.vectors());
        drop(graph);
        let bytes = fs::read(&graph_path).expect("read");
        fs::write(&graph_path, &bytes[..bytes.len() - 4]).expect("truncate");
        let err = HnswGraph::open(&graph_path, vectors).expect_err("truncated");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        let _ = fs::remove_dir_all(dir);
    }
}