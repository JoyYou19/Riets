//! Graph traversal shared by graph construction and query search.
//!
//! The walk knows nothing about files: it asks `NeighbourLists` for a node's links and
//! `SimilarityToTarget` for how close a node is to whatever is being searched for (a query,
//! or a node being inserted). Higher similarity is closer.

use std::cell::RefCell;
use std::cmp::{Ordering, Reverse};
use std::collections::BinaryHeap;

use crate::hash::fmix64;

/// Seed for tie-break hashes; non-zero because `fmix64(0) == 0`.
const TIE_BREAK_SEED: u64 = 0x9E37_79B9_7F4A_7C15;

/// Where the walk gets a node's links from.
pub(crate) trait NeighbourLists: Sync {
    /// Replaces `out` with the neighbours of `node` on `level`. `node` is on `level`.
    fn neighbours_into(&self, node: u32, level: usize, out: &mut Vec<u32>);
}

/// How close a node is to the walk's target. Must be finite.
pub(crate) trait SimilarityToTarget {
    fn similarity(&self, node: u32) -> f32;
     fn tie_salt(&self) -> u64 {
        0
    }
}

/// A node and its similarity to the target.
///
/// Ordered by similarity, then a hash of the node, then the node. Ties must not be broken by
/// node id alone: with many identical vectors every neighbour list would keep the same
/// highest ids and the other duplicates would lose all incoming links. The hash spreads
/// ties evenly and deterministically.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ScoredNode {
    pub(crate) similarity: f32,
    tie_break: u32,
    pub(crate) node: u32,
}

impl ScoredNode {
    pub(crate) fn new(node: u32, similarity: f32) -> Self {
        Self::salted(node, similarity, 0)
    }

    /// Like `new`, with ties broken in an order that depends on `salt`. Neighbour lists salt
    /// with their owner, so lists full of equal similarities keep different nodes instead of
    /// all keeping the same few.
    pub(crate) fn salted(node: u32, similarity: f32, salt: u64) -> Self {
        Self {
            similarity,
            tie_break: fmix64(u64::from(node) ^ TIE_BREAK_SEED ^ fmix64(salt)) as u32,
            node,
        }
    }
}

impl Ord for ScoredNode {
    fn cmp(&self, other: &Self) -> Ordering {
        self.similarity
            .total_cmp(&other.similarity)
            .then(self.tie_break.cmp(&other.tie_break))
            .then(self.node.cmp(&other.node))
    }
}

impl PartialOrd for ScoredNode {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for ScoredNode {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for ScoredNode {}

/// Which nodes the current search has already looked at. Each search gets a new mark, so
/// starting a search is O(1) instead of clearing the whole buffer.
#[derive(Default)]
pub(crate) struct VisitedSet {
    marks: Vec<u32>,
    current_mark: u32,
}

impl VisitedSet {
    /// Forgets every node visited before; sized for `node_count` nodes.
    pub(crate) fn start_new_search(&mut self, node_count: usize) {
        if self.marks.len() < node_count {
            self.marks.resize(node_count, 0);
        }
        self.current_mark = self.current_mark.wrapping_add(1);
        if self.current_mark == 0 {
            // Wrapped after 4 billion searches: old marks could now look current.
            self.marks.fill(0);
            self.current_mark = 1;
        }
    }

    /// Marks `node` visited. Returns true if it had not been visited in this search.
    pub(crate) fn mark(&mut self, node: u32) -> bool {
        let slot = &mut self.marks[node as usize];
        if *slot == self.current_mark {
            false
        } else {
            *slot = self.current_mark;
            true
        }
    }
}

thread_local! {
    /// One visited buffer per thread, reused across searches; grows to the largest graph
    /// this thread has walked.
    static VISITED_SET: RefCell<VisitedSet> = RefCell::new(VisitedSet::default());
}

/// Runs `f` with this thread's visited buffer.
/// Panics if called again from inside `f`: a walk never starts another walk.
pub(crate) fn with_visited_set<R>(f: impl FnOnce(&mut VisitedSet) -> R) -> R {
    VISITED_SET.with_borrow_mut(f)
}

/// Result of searching one level.
#[derive(Debug)]
pub(crate) struct LevelSearch {
    /// Accepted nodes found, best first, at most `beam_width`.
    pub(crate) best_first: Vec<ScoredNode>,
    /// True if the walk stopped because it used up its similarity computations.
    pub(crate) stopped_by_budget: bool,
}

/// A walk over one graph towards one target.
pub(crate) struct GraphWalk<'a, L: ?Sized, S: ?Sized> {
    pub(crate) lists: &'a L,
    pub(crate) target: &'a S,
    pub(crate) node_count: usize,
}

impl<L, S> GraphWalk<'_, L, S>
where
    L: NeighbourLists + ?Sized,
    S: SimilarityToTarget + ?Sized,
{
     pub(crate) fn score(&self, node: u32) -> ScoredNode {
        ScoredNode::salted(node, self.target.similarity(node), self.target.tie_salt())
    }

    /// Greedy descent through the upper levels: on each level from `from_level` down to
    /// `down_to_level` (inclusive), keep stepping to the most similar neighbour until no
    /// neighbour is more similar. Cheap way to get close to the target before the wide
    /// search on the levels below. Returns `start` if the range is empty.
    pub(crate) fn greedy_descend(
        &self,
        start: ScoredNode,
        from_level: usize,
        down_to_level: usize,
        scratch: &mut Vec<u32>,
    ) -> ScoredNode {
        let mut current = start;
        for level in (down_to_level..=from_level).rev() {
            loop {
                self.lists.neighbours_into(current.node, level, scratch);
                let best_neighbour = scratch.iter().map(|&n| self.score(n)).max();
                match best_neighbour {
                    // Strictly better each step, so this cannot loop forever.
                    Some(better) if better > current => current = better,
                    _ => break,
                }
            }
        }
        current
    }

    /// Best-first beam search on one level (Malkov & Yashunin, algorithm 2).
    ///
    /// Keeps the `beam_width` best nodes that `accept` allows. Repeatedly expands the most
    /// similar unexpanded node; stops when that node is worse than the worst kept node and
    /// the beam is full, or after `max_similarity_computations`.
    ///
    /// A node `accept` rejects is never kept, but it is still expanded when it beats the
    /// worst kept node (or while fewer than `beam_width` are kept), so the walk can pass
    /// through rejected regions near the target. Far-away rejected nodes are not expanded.
    pub(crate) fn search_level(
        &self,
        entry_points: &[ScoredNode],
        beam_width: usize,
        level: usize,
        accept: impl Fn(u32) -> bool,
        max_similarity_computations: usize,
        visited: &mut VisitedSet,
        scratch: &mut Vec<u32>,
    ) -> LevelSearch {
        visited.start_new_search(self.node_count);
        let mut to_expand: BinaryHeap<ScoredNode> = BinaryHeap::with_capacity(beam_width * 2);
        // Min-heap: the top is the worst kept node.
        let mut kept: BinaryHeap<Reverse<ScoredNode>> = BinaryHeap::with_capacity(beam_width + 1);

        for &entry in entry_points {
            if visited.mark(entry.node) {
                to_expand.push(entry);
                if accept(entry.node) {
                    keep_within_beam(&mut kept, entry, beam_width);
                }
            }
        }

        let mut computations = 0usize;
        let mut stopped_by_budget = false;
        while let Some(candidate) = to_expand.pop() {
            let beam_full = kept.len() >= beam_width;
            if beam_full && kept.peek().is_some_and(|Reverse(worst)| candidate < *worst) {
                break;
            }

            self.lists.neighbours_into(candidate.node, level, scratch);
            for &neighbour in scratch.iter() {
                if !visited.mark(neighbour) {
                    continue;
                }
                computations += 1;
                let scored = self.score(neighbour);
                let could_enter_beam = kept.len() < beam_width
                    || kept.peek().is_some_and(|Reverse(worst)| scored > *worst);
                if could_enter_beam {
                    to_expand.push(scored);
                    if accept(neighbour) {
                        keep_within_beam(&mut kept, scored, beam_width);
                    }
                }
            }

            if computations >= max_similarity_computations {
                stopped_by_budget = true;
                break;
            }
        }

        // Sorting Reverse ascending gives similarity descending: best first.
        let best_first = kept.into_sorted_vec().into_iter().map(|Reverse(n)| n).collect();
        LevelSearch {
            best_first,
            stopped_by_budget,
        }
    }
}

fn keep_within_beam(kept: &mut BinaryHeap<Reverse<ScoredNode>>, node: ScoredNode, beam_width: usize) {
    kept.push(Reverse(node));
    if kept.len() > beam_width {
        kept.pop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Nodes 0..n on a line, each linked to its direct neighbours on level 0 only.
    struct LineGraph {
        node_count: u32,
    }

    impl NeighbourLists for LineGraph {
        fn neighbours_into(&self, node: u32, _level: usize, out: &mut Vec<u32>) {
            out.clear();
            if node > 0 {
                out.push(node - 1);
            }
            if node + 1 < self.node_count {
                out.push(node + 1);
            }
        }
    }

    /// Closer to `target` on the line means more similar.
    struct DistanceOnLine {
        target: u32,
    }

    impl SimilarityToTarget for DistanceOnLine {
        fn similarity(&self, node: u32) -> f32 {
            -(node.abs_diff(self.target) as f32)
        }
    }

    fn search(target: u32, beam: usize, accept: impl Fn(u32) -> bool, budget: usize) -> LevelSearch {
        let graph = LineGraph { node_count: 100 };
        let similarity = DistanceOnLine { target };
        let walk = GraphWalk { lists: &graph, target: &similarity, node_count: 100 };
        let entry = walk.score(0);
        with_visited_set(|visited| {
            walk.search_level(&[entry], beam, 0, accept, budget, visited, &mut Vec::new())
        })
    }

    #[test]
    fn finds_the_target_and_its_neighbours_best_first() {
        let result = search(60, 3, |_| true, usize::MAX);
        let nodes: Vec<u32> = result.best_first.iter().map(|n| n.node).collect();
        assert_eq!(nodes[0], 60);
        assert_eq!(nodes.len(), 3);
        assert!(nodes[1..].iter().all(|n| *n == 59 || *n == 61));
        assert!(!result.stopped_by_budget);
    }

    #[test]
    fn rejected_nodes_are_never_returned_but_still_walked_through() {
        let result = search(60, 3, |node| node % 2 == 1, usize::MAX);
        assert!(result.best_first.iter().all(|n| n.node % 2 == 1));
        assert!(result.best_first.iter().any(|n| n.node == 59 || n.node == 61));
    }

    #[test]
    fn budget_stops_the_walk() {
        let result = search(90, 3, |_| true, 10);
        assert!(result.stopped_by_budget);
    }

    #[test]
    fn greedy_descend_climbs_to_the_best_node() {
        let graph = LineGraph { node_count: 100 };
        let similarity = DistanceOnLine { target: 42 };
        let walk = GraphWalk { lists: &graph, target: &similarity, node_count: 100 };
        let best = walk.greedy_descend(walk.score(0), 0, 0, &mut Vec::new());
        assert_eq!(best.node, 42);
        let untouched = walk.greedy_descend(walk.score(5), 0, 1, &mut Vec::new());
        assert_eq!(untouched.node, 5, "empty level range returns the start");
    }

    #[test]
    fn visited_set_survives_mark_wraparound() {
        let mut visited = VisitedSet::default();
        visited.start_new_search(4);
        assert!(visited.mark(2));
        assert!(!visited.mark(2));
        visited.current_mark = u32::MAX;
        visited.start_new_search(4);
        assert_eq!(visited.current_mark, 1);
        assert!(visited.mark(2), "wraparound must forget old marks");
    }

    #[test]
    fn ties_do_not_follow_node_order() {
        let ties: Vec<ScoredNode> = (0..64).map(|n| ScoredNode::new(n, 1.0)).collect();
        let best = ties.iter().max().expect("non-empty").node;
        assert_ne!(best, 63, "highest id must not always win a tie");
    }
}