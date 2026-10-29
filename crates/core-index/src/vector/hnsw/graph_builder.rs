//! Builds the HNSW graph of one vector file and writes it in the `layout` format.
//!
//! CPU heavy: run it on the background pool, never on a query or index-worker thread.
//! Distances use the f32 originals, so neighbour choices are exact.

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::Path;
use std::sync::{Mutex, MutexGuard, PoisonError};

use rayon::prelude::*;

use super::layout::{
    GraphHeader, GraphLayout, MAX_LEVEL, MAX_LINKS_LIMIT, NO_UPPER_SLOTS, header_length,
};
use super::walk::{GraphWalk, NeighbourLists, ScoredNode, SimilarityToTarget, with_visited_set};
use crate::hash::fmix64;
use crate::vector::file::{VectorFile, tmp_path_for, write_zero_padding};
use crate::vector::kernel::dot_f32;

/// Files with fewer rows are scanned instead: an int8 scan of 20k rows takes about a
/// millisecond, which a graph cannot beat by enough to pay for building and loading it.
pub const MIN_ROWS_FOR_GRAPH: usize = 20_000;

/// Graph construction settings.
#[derive(Debug, Clone, Copy)]
pub struct GraphBuildParams {
    /// Most links a node keeps on levels >= 1 (HNSW's `M`); level 0 keeps twice as many.
    pub max_links_upper: usize,
    /// Beam width while inserting (HNSW's `ef_construction`). Wider builds slower and gives
    /// a better graph.
    pub build_beam_width: usize,
    /// Seeds the level draw, so the same file always gets the same levels.
    pub level_seed: u64,
}

impl Default for GraphBuildParams {
    fn default() -> Self {
        Self {
            max_links_upper: 16,
            build_beam_width: 200,
            level_seed: 0x5EED_C0DE_D00D_F00D,
        }
    }
}

impl GraphBuildParams {
    fn validate(&self) -> io::Result<()> {
        if !(2..=MAX_LINKS_LIMIT).contains(&self.max_links_upper) {
            return Err(invalid_input(format!(
                "max_links_upper must be 2..={MAX_LINKS_LIMIT}, got {}",
                self.max_links_upper
            )));
        }
        if self.build_beam_width < self.max_links_upper {
            return Err(invalid_input(format!(
                "build_beam_width ({}) must be at least max_links_upper ({})",
                self.build_beam_width, self.max_links_upper
            )));
        }
        Ok(())
    }
}

/// Builds the graph for `vectors` and writes it atomically to `path`.
/// Returns false, writing nothing, when the file is too small to be worth a graph.
pub fn build_graph_file(
    vectors: &VectorFile,
    path: impl AsRef<Path>,
    params: &GraphBuildParams,
) -> io::Result<bool> {
    if vectors.len() < MIN_ROWS_FOR_GRAPH {
        return Ok(false);
    }
    build_and_write(vectors, path.as_ref(), params).map(|()| true)
}

/// Builds and writes the graph whatever the file size. On failure no file is left at `path`.
pub(crate) fn build_and_write(
    vectors: &VectorFile,
    path: &Path,
    params: &GraphBuildParams,
) -> io::Result<()> {
    let result = GraphUnderConstruction::build(vectors, params)
        .and_then(|graph| graph.write(path, vectors.file_id(), vectors.dims()));
    if result.is_err() {
        let _ = fs::remove_file(tmp_path_for(path));
    }
    result
}

/// Similarity of any node to the node being inserted, on the f32 originals.
struct InsertedNodeTarget<'a> {
    vectors: &'a VectorFile,
    inserted_node: u32,
    inserted_vector: &'a [f32],
}

impl SimilarityToTarget for InsertedNodeTarget<'_> {
    fn similarity(&self, node: u32) -> f32 {
        dot_f32(self.vectors.original(node as usize), self.inserted_vector)
    }

    fn tie_salt(&self) -> u64 {
        u64::from(self.inserted_node)
    }
}

/// The graph while it is being built. Each (node, level) list has its own lock, and an insert
/// holds at most one lock at a time, so parallel inserts cannot deadlock.
struct GraphUnderConstruction {
    max_links_upper: usize,
    max_links_level0: usize,
    entry_node: u32,
    /// Highest level of each node.
    node_levels: Vec<u8>,
    /// `neighbour_lists[node][level]`, for levels 0..=node_levels[node].
    neighbour_lists: Vec<Box<[Mutex<Vec<u32>>]>>,
}

impl NeighbourLists for GraphUnderConstruction {
    fn neighbours_into(&self, node: u32, level: usize, out: &mut Vec<u32>) {
        out.clear();
        out.extend_from_slice(&lock(&self.neighbour_lists[node as usize][level]));
    }
}

impl GraphUnderConstruction {
    fn build(vectors: &VectorFile, params: &GraphBuildParams) -> io::Result<Self> {
        params.validate()?;
        let node_count = vectors.len();
        if node_count == 0 {
            return Err(invalid_input("cannot build a graph over zero rows"));
        }
        if u32::try_from(node_count).is_err() {
            return Err(invalid_input(format!(
                "{node_count} rows exceed the u32 node ids of a graph"
            )));
        }

        let level_multiplier = 1.0 / (params.max_links_upper as f64).ln();
        let node_levels: Vec<u8> = (0..node_count)
            .map(|node| random_level(params.level_seed, node, level_multiplier))
            .collect();
        // Highest level wins; among equals the lowest node id, so the choice is deterministic.
        let entry_node = node_levels
            .iter()
            .enumerate()
            .max_by_key(|&(node, &level)| (level, std::cmp::Reverse(node)))
            .map_or(0, |(node, _)| node as u32);

        let max_links_level0 = params.max_links_upper * 2;
        let neighbour_lists = node_levels
            .iter()
            .map(|&top| {
                (0..=usize::from(top))
                    .map(|level| {
                        let capacity = if level == 0 { max_links_level0 } else { params.max_links_upper };
                        Mutex::new(Vec::with_capacity(capacity + 1))
                    })
                    .collect::<Vec<_>>()
                    .into_boxed_slice()
            })
            .collect();

        let graph = Self {
            max_links_upper: params.max_links_upper,
            max_links_level0,
            entry_node,
            node_levels,
            neighbour_lists,
        };
        (0..node_count as u32)
            .into_par_iter()
            .filter(|&node| node != entry_node)
            .for_each(|node| graph.insert(vectors, node, params.build_beam_width));
        graph.connect_unreachable(vectors   );
        Ok(graph)
    }

    fn max_links_at(&self, level: usize) -> usize {
        if level == 0 { self.max_links_level0 } else { self.max_links_upper }
    }

    /// Inserts `node` (Malkov & Yashunin, algorithm 1).
    fn insert(&self, vectors: &VectorFile, node: u32, build_beam_width: usize) {
        let target = InsertedNodeTarget {
            vectors,
            inserted_node: node,
            inserted_vector: vectors.original(node as usize),
        };
        let walk = GraphWalk {
            lists: self,
            target: &target,
            node_count: self.node_levels.len(),
        };
        let node_level = usize::from(self.node_levels[node as usize]);
        let top_level = usize::from(self.node_levels[self.entry_node as usize]);

        with_visited_set(|visited| {
            let mut scratch = Vec::with_capacity(self.max_links_level0);
            let mut chosen = Vec::with_capacity(self.max_links_level0);

            // Above the node's own levels: just get close.
            let closest = walk.greedy_descend(
                walk.score(self.entry_node),
                top_level,
                node_level + 1,
                &mut scratch,
            );
            let mut entry_points = vec![closest];

            // On the node's levels: find candidates, link to a diverse subset of them.
            for level in (0..=node_level.min(top_level)).rev() {
                let mut candidates = walk
                    .search_level(
                        &entry_points,
                        build_beam_width,
                        level,
                        |_| true,
                        usize::MAX,
                        visited,
                        &mut scratch,
                    )
                    .best_first;
                candidates.retain(|candidate| candidate.node != node);

                select_diverse_neighbours(vectors, &candidates, self.max_links_upper, &mut chosen);
                // Nobody links to `node` on this level yet, so its list cannot have changed.
                lock(&self.neighbour_lists[node as usize][level]).clone_from(&chosen);
                for &neighbour in &chosen {
                    self.link_back(vectors, neighbour, node, level);
                }
                if !candidates.is_empty() {
                    entry_points = candidates;
                }
            }
        });
    }

    /// Adds `new_node` to `neighbour`'s list on `level`, pruning the list if it overflows.
    fn link_back(&self, vectors: &VectorFile, neighbour: u32, new_node: u32, level: usize) {
        let max_links = self.max_links_at(level);
        let mut list = lock(&self.neighbour_lists[neighbour as usize][level]);
        if list.contains(&new_node) {
            return;
        }
        list.push(new_node);
        if list.len() <= max_links {
            return;
        }

        let base = vectors.original(neighbour as usize);
        let mut by_similarity: Vec<ScoredNode> = list
            .iter()
            .map(|&linked| {
                let similarity = dot_f32(base, vectors.original(linked as usize));
                ScoredNode::salted(linked, similarity, u64::from(neighbour))
            })
            .collect();
        by_similarity.sort_unstable_by(|a, b| b.cmp(a));
        let mut kept = Vec::with_capacity(max_links + 1);
        select_diverse_neighbours(vectors, &by_similarity, max_links, &mut kept);
        *list = kept;
    }

    fn write(&self, path: &Path, vector_file_id: u64, dims: usize) -> io::Result<()> {
        let node_count = self.node_levels.len();
        let mut upper_slot_starts = Vec::with_capacity(node_count);
        let mut upper_slot_count: u32 = 0;
        for &level in &self.node_levels {
            if level == 0 {
                upper_slot_starts.push(NO_UPPER_SLOTS);
            } else {
                upper_slot_starts.push(upper_slot_count);
                upper_slot_count = upper_slot_count
                    .checked_add(u32::from(level))
                    .filter(|&count| count != NO_UPPER_SLOTS)
                    .ok_or_else(|| invalid_input("too many upper level slots"))?;
            }
        }

        let header = GraphHeader {
            vector_file_id,
            node_count: node_count as u32,
            dims: u32::try_from(dims).map_err(|_| invalid_input("dims exceed u32"))?,
            max_links_upper: self.max_links_upper as u16,
            max_links_level0: self.max_links_level0 as u16,
            entry_node: self.entry_node,
            top_level: self.node_levels[self.entry_node as usize],
            upper_slot_count,
        };
        let layout = GraphLayout::new(&header)
            .ok_or_else(|| invalid_input("graph file size overflows usize"))?;

        let tmp_path = tmp_path_for(path);
        let file = OpenOptions::new().write(true).create(true).truncate(true).open(&tmp_path)?;
        let mut out = BufWriter::new(file);

        header.write_to(&mut out)?;
        debug_assert_eq!(header_length(), layout.node_levels_offset);

        out.write_all(&self.node_levels)?;
        write_zero_padding(
            &mut out,
            layout.node_levels_offset + node_count,
            layout.upper_slot_starts_offset,
        )?;

        for start in &upper_slot_starts {
            out.write_all(&start.to_le_bytes())?;
        }
        write_zero_padding(
            &mut out,
            layout.upper_slot_starts_offset + node_count * 4,
            layout.level0_lists_offset,
        )?;

        for lists in &self.neighbour_lists {
            write_slot(&mut out, &lock(&lists[0]), self.max_links_level0)?;
        }
        write_zero_padding(
            &mut out,
            layout.level0_lists_offset + node_count * layout.level0_slot_length,
            layout.upper_lists_offset,
        )?;

        for lists in &self.neighbour_lists {
            for upper in lists.iter().skip(1) {
                write_slot(&mut out, &lock(upper), self.max_links_upper)?;
            }
        }

        let file = out.into_inner().map_err(io::IntoInnerError::into_error)?;
        file.sync_all()?;
        let written = file.metadata()?.len();
        if written != layout.total_length as u64 {
            return Err(io::Error::other(format!(
                "graph file {} assembled to {written} bytes, expected {}",
                tmp_path.display(),
                layout.total_length
            )));
        }
        drop(file);
        fs::rename(&tmp_path, path)?;
        if let Some(dir) = path.parent() {
            File::open(dir)?.sync_all()?;
        }
        Ok(())
    }
        /// Repair pass: gives every node that no search could reach on level 0 an incoming link.
    ///
    /// Pruning in `link_back` can drop a node from every list that linked to it; HNSW
    /// usually accepts that, but here it would hide documents from graph search until the
    /// next compaction. Each unreachable node is linked from its most similar reachable
    /// node. Adding a link can push another out of a full list, so this repeats until
    /// everything is reachable (a few rounds at most in practice).
    fn connect_unreachable(&self, vectors: &VectorFile) {
        const MAX_ROUNDS: usize = 8;
        for _ in 0..MAX_ROUNDS {
            let reachable = self.reachable_on_level0();
            let stranded: Vec<u32> = (0..self.node_levels.len() as u32)
                .filter(|&node| !reachable[node as usize])
                .collect();
            if stranded.is_empty() {
                return;
            }
            for node in stranded {
                if let Some(anchor) = self.nearest_reachable(vectors, node, &reachable) {
                    self.force_link(vectors, anchor, node);
                }
            }
        }
    }

    /// Level-0 reachability from the entry node.
    fn reachable_on_level0(&self) -> Vec<bool> {
        let mut reachable = vec![false; self.node_levels.len()];
        let mut stack = vec![self.entry_node];
        reachable[self.entry_node as usize] = true;
        let mut links = Vec::with_capacity(self.max_links_level0);
        while let Some(node) = stack.pop() {
            self.neighbours_into(node, 0, &mut links);
            for &next in &links {
                if !reachable[next as usize] {
                    reachable[next as usize] = true;
                    stack.push(next);
                }
            }
        }
        reachable
    }

    /// The most similar reachable node to `node`: one of its own links if any is reachable,
    /// otherwise the best result of a level-0 search from the entry node.
    fn nearest_reachable(&self, vectors: &VectorFile, node: u32, reachable: &[bool]) -> Option<u32> {
        let target = InsertedNodeTarget {
            vectors,
            inserted_node: node,
            inserted_vector: vectors.original(node as usize),
        };
        let walk = GraphWalk {
            lists: self,
            target: &target,
            node_count: self.node_levels.len(),
        };
        let mut links = Vec::with_capacity(self.max_links_level0);
        self.neighbours_into(node, 0, &mut links);
        let from_own_links = links
            .iter()
            .filter(|&&linked| reachable[linked as usize])
            .map(|&linked| walk.score(linked))
            .max();
        if let Some(best) = from_own_links {
            return Some(best.node);
        }
        with_visited_set(|visited| {
            let entry = walk.score(self.entry_node);
            walk.search_level(
                &[entry],
                self.max_links_level0,
                0,
                |candidate| candidate != node && reachable[candidate as usize],
                usize::MAX,
                visited,
                &mut links,
            )
            .best_first
            .first()
            .map(|best| best.node)
        })
    }

    /// Adds `node` to `anchor`'s level-0 list. A full list loses its least similar link.
    fn force_link(&self, vectors: &VectorFile, anchor: u32, node: u32) {
        let mut list = lock(&self.neighbour_lists[anchor as usize][0]);
        if list.contains(&node) {
            return;
        }
        if list.len() < self.max_links_level0 {
            list.push(node);
            return;
        }
        let base = vectors.original(anchor as usize);
        let weakest = list
            .iter()
            .enumerate()
            .min_by(|&(_, &a), &(_, &b)| {
                let a = ScoredNode::salted(a, dot_f32(base, vectors.original(a as usize)), u64::from(anchor));
                let b = ScoredNode::salted(b, dot_f32(base, vectors.original(b as usize)), u64::from(anchor));
                a.cmp(&b)
            })
            .map(|(position, _)| position);
        if let Some(position) = weakest {
            list[position] = node;
        }
    }
}

/// Neighbour selection heuristic (Malkov & Yashunin, algorithm 4), keeping pruned links.
///
/// `candidates` hold similarities to the base node, best first. A candidate is kept when it
/// is more similar to the base than to every neighbour already kept. That keeps links
/// pointing in different directions, so searches can cross between clusters. Slots left
/// over go to the best rejected candidates, so groups of identical vectors still get linked.
fn select_diverse_neighbours(
    vectors: &VectorFile,
    candidates: &[ScoredNode],
    max_links: usize,
    chosen: &mut Vec<u32>,
) {
    chosen.clear();
    let mut rejected = Vec::new();
    for candidate in candidates {
        if chosen.len() == max_links {
            break;
        }
        let candidate_vector = vectors.original(candidate.node as usize);
        let points_new_direction = chosen.iter().all(|&kept| {
            dot_f32(candidate_vector, vectors.original(kept as usize)) < candidate.similarity
        });
        if points_new_direction {
            chosen.push(candidate.node);
        } else {
            rejected.push(candidate.node);
        }
    }
    let free_slots = max_links - chosen.len();
    chosen.extend(rejected.into_iter().take(free_slots));
}

/// Draws a node's highest level: floor(-ln(U) / ln(M)) with U uniform in (0, 1], capped at
/// `MAX_LEVEL`. About 1 in M nodes reaches level 1, 1 in M² level 2, and so on.
fn random_level(seed: u64, node: usize, level_multiplier: f64) -> u8 {
    let bits = fmix64(seed ^ fmix64(node as u64 ^ 0xD6E8_FEB8_6659_FD93));
    let uniform = ((bits >> 11) as f64 + 1.0) / (1u64 << 53) as f64;
    let level = (-uniform.ln() * level_multiplier).floor();
    if level >= f64::from(MAX_LEVEL) {
        MAX_LEVEL
    } else {
        level as u8
    }
}

/// Writes one list slot: length, the links, then zeros up to `max_links`.
fn write_slot(out: &mut impl Write, links: &[u32], max_links: usize) -> io::Result<()> {
    debug_assert!(links.len() <= max_links, "neighbour list longer than its slot");
    out.write_all(&(links.len() as u32).to_le_bytes())?;
    for &link in links {
        out.write_all(&link.to_le_bytes())?;
    }
    for _ in links.len()..max_links {
        out.write_all(&0u32.to_le_bytes())?;
    }
    Ok(())
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // A panicked insert leaves a list that is still a valid list of node ids.
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn invalid_input(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, msg.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vector::VectorFileWriter;
    use crate::vector::hnsw::layout::GraphHeader;
    use std::collections::VecDeque;
    use std::path::PathBuf;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("corelamo-gbuild-{name}-{}", std::process::id()));
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

    fn vector_file(dir: &Path, rows: impl Iterator<Item = Vec<f32>>, dims: usize) -> VectorFile {
        let path = dir.join("s.1.vec");
        let mut writer = VectorFileWriter::create(&path, "m", dims).expect("create");
        for (i, row) in rows.enumerate() {
            writer.push(i as u64 + 1, &row).expect("push");
        }
        writer.finish().expect("finish");
        VectorFile::open(&path).expect("open")
    }

    /// Nodes reachable on level 0 from the entry node.
    fn reachable_on_level0(graph: &GraphUnderConstruction) -> usize {
        let mut seen = vec![false; graph.node_levels.len()];
        let mut queue = VecDeque::from([graph.entry_node]);
        seen[graph.entry_node as usize] = true;
        let mut count = 1;
        while let Some(node) = queue.pop_front() {
            for &next in lock(&graph.neighbour_lists[node as usize][0]).iter() {
                if !seen[next as usize] {
                    seen[next as usize] = true;
                    count += 1;
                    queue.push_back(next);
                }
            }
        }
        count
    }

    #[test]
    fn lists_stay_within_limits_and_link_valid_nodes() {
        let dir = temp_dir("limits");
        let vectors = vector_file(&dir, (0..2_000).map(|i| unit(i, 32)), 32);
        let graph = GraphUnderConstruction::build(&vectors, &GraphBuildParams::default()).expect("build");
        for (node, lists) in graph.neighbour_lists.iter().enumerate() {
            for (level, list) in lists.iter().enumerate() {
                let list = lock(list);
                assert!(list.len() <= graph.max_links_at(level));
                assert!(!list.contains(&(node as u32)), "self link");
                assert!(list.iter().all(|&n| usize::from(graph.node_levels[n as usize]) >= level));
            }
        }
        assert_eq!(reachable_on_level0(&graph), 2_000);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn identical_vectors_all_stay_reachable() {
        let dir = temp_dir("dupes");
        let vectors = vector_file(&dir, (0..500).map(|_| vec![1.0, 0.0, 0.0, 0.0]), 4);
        let graph = GraphUnderConstruction::build(&vectors, &GraphBuildParams::default()).expect("build");
        assert_eq!(reachable_on_level0(&graph), 500);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn written_header_describes_the_graph() {
        let dir = temp_dir("write");
        let vectors = vector_file(&dir, (0..300).map(|i| unit(i, 16)), 16);
        let graph = GraphUnderConstruction::build(&vectors, &GraphBuildParams::default()).expect("build");
        let path = dir.join("s.1.hnsw");
        graph.write(&path, vectors.file_id(), vectors.dims()).expect("write");
        let bytes = fs::read(&path).expect("read");
        let header = GraphHeader::read_from(&bytes, &path).expect("header");
        assert_eq!(header.vector_file_id, vectors.file_id());
        assert_eq!(header.node_count, 300);
        assert_eq!(header.entry_node, graph.entry_node);
        assert_eq!(bytes.len(), GraphLayout::new(&header).expect("layout").total_length);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn small_files_get_no_graph() {
        let dir = temp_dir("small");
        let vectors = vector_file(&dir, (0..100).map(|i| unit(i, 8)), 8);
        let path = dir.join("s.1.hnsw");
        assert!(!build_graph_file(&vectors, &path, &GraphBuildParams::default()).expect("build"));
        assert!(!path.exists());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn invalid_params_are_rejected() {
        let too_few_links = GraphBuildParams { max_links_upper: 1, ..GraphBuildParams::default() };
        let narrow_beam = GraphBuildParams { build_beam_width: 4, ..GraphBuildParams::default() };
        assert!(too_few_links.validate().is_err());
        assert!(narrow_beam.validate().is_err());
    }

    #[test]
    fn levels_are_deterministic_and_mostly_zero() {
        let multiplier = 1.0 / 16f64.ln();
        let levels: Vec<u8> = (0..10_000).map(|n| random_level(42, n, multiplier)).collect();
        let again: Vec<u8> = (0..10_000).map(|n| random_level(42, n, multiplier)).collect();
        assert_eq!(levels, again);
        let at_zero = levels.iter().filter(|&&l| l == 0).count();
        // Expected share at level 0 is 1 - 1/16 ≈ 94%.
        assert!((9_200..=9_600).contains(&at_zero), "{at_zero} of 10000 at level 0");
    }
}