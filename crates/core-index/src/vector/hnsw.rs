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

pub use graph_builder::{
    GraphBuildParams,
    MIN_ROWS_FOR_GRAPH,
    build_graph_file,
    extend_graph_file,
};
use layout::{ GraphHeader, GraphLayout, NO_UPPER_SLOTS };
use walk::{ GraphWalk, NeighbourLists, SimilarityToTarget, with_visited_set };

use std::fs::File;
use std::io;
use std::path::{ Path, PathBuf };
use std::sync::Arc;

use memmap2::Mmap;

use super::file::VectorFile;
use super::kernel::{ dot_f32, dot_i8, quantize_query };
use super::vector_hit::{ TopK, VectorFilter, VectorHit };
use crate::disk::format::array_at;
use crate::types::XPathId;

/// Smallest beam for a search; raised to `k * RESCORE_OVERSAMPLING` for larger `k`.
/// The main recall/speed knob.
pub const DEFAULT_SEARCH_BEAM_WIDTH: usize = 64;
/// The beam holds at least this many candidates per requested hit, so the f32 rescore has
/// room to fix the int8 order.
pub(crate) const RESCORE_OVERSAMPLING: usize = 3;
/// A search computing more than `node_count` / this many similarities gives up and lets the
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
    fn u32_at(&self, at: usize) -> Option<u32> {
        array_at(&self.mmap, at).map(u32::from_le_bytes)
    }
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
        if
            (header.node_count as usize) != vectors.len() ||
            (header.dims as usize) != vectors.dims()
        {
            return Err(
                corrupt(
                    &path,
                    format!(
                        "{} nodes of {} dims, vector file has {} rows of {} dims",
                        header.node_count,
                        header.dims,
                        vectors.len(),
                        vectors.dims()
                    )
                )
            );
        }
        let layout = GraphLayout::new(&header).ok_or_else(||
            corrupt(&path, "size overflows usize")
        )?;
        if layout.total_length != mmap.len() {
            return Err(corrupt(&path, format!("length {} does not match its header", mmap.len())));
        }
        if !(mmap.as_ptr() as usize).is_multiple_of(4) {
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
                    return Err(
                        corrupt(&self.path, format!("level-0 node {node} owns upper slots"))
                    );
                }
            } else {
                if (slot_start as usize) != next_upper_slot {
                    return Err(
                        corrupt(&self.path, format!("bad upper slot start for node {node}"))
                    );
                }
                next_upper_slot += level;
            }

            for list_level in 0..=level {
                let max_links = self.max_links_at(list_level);
                let offset = self.layout.slot_offset(node, list_level, slot_start);
                let Some(length) = self.u32_at(offset) else {
                    return Err(corrupt(&self.path, "list slot outside the file"));
                };
                if (length as usize) > max_links {
                    return Err(corrupt(&self.path, format!("node {node} has {length} links")));
                }
                for &linked in self.neighbour_list(node, list_level) {
                    let linked = linked as usize;
                    if
                        linked >= node_count ||
                        linked == node ||
                        self.node_level(linked) < list_level
                    {
                        return Err(
                            corrupt(
                                &self.path,
                                format!(
                                    "node {node} links to invalid node {linked} on level {list_level}"
                                )
                            )
                        );
                    }
                }
            }
        }
        if next_upper_slot != (self.header.upper_slot_count as usize) {
            return Err(corrupt(&self.path, "upper slot count does not match node levels"));
        }
        Ok(())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The vector file this graph indexes.
    pub const fn vectors(&self) -> &Arc<VectorFile> {
        &self.vectors
    }

    pub const fn node_count(&self) -> usize {
        self.header.node_count as usize
    }

    fn node_level(&self, node: usize) -> usize {
        self.mmap.get(self.layout.node_levels_offset + node).map_or(0, |&level| usize::from(level))
    }

    fn upper_slot_start(&self, node: usize) -> u32 {
        self.u32_at(self.layout.upper_slot_starts_offset + node * 4).unwrap_or(NO_UPPER_SLOTS)
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
        let length = self
            .u32_at(offset)
            .map_or(0, |len| len as usize)
            .min(self.max_links_at(level));
        let Some(bytes) = self.mmap.get(offset + 4..offset + 4 + length * 4) else {
            return &[];
        };
        
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
    pub fn search(
        &self,
        query: &[f32],
        k: usize,
        filter: &VectorFilter<'_>
    ) -> Option<Vec<VectorHit>> {
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
        let budget = (self.node_count() / BUDGET_FRACTION_OF_NODES).max(
            beam_width.saturating_mul(MIN_COMPUTATIONS_PER_BEAM_SLOT)
        );
        let accept = |node: u32| filter.accepts(self.vectors.doc_id(node as usize));

        let level0 = with_visited_set(|visited| {
            let mut scratch = Vec::with_capacity(usize::from(self.header.max_links_level0));
            let start = walk.score(self.header.entry_node);
            let closest = walk.greedy_descend(
                start,
                usize::from(self.header.top_level),
                1,
                &mut scratch
            );
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
    io::Error::new(io::ErrorKind::InvalidData, format!("hnsw file {}: {msg}", path.display()))
}
