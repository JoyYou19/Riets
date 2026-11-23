//! The top-k collector: walks a set of scorers and keeps the k best documents.
use std::{cmp::Ordering, collections::BinaryHeap};

use core_index::{search::SearchStats, types::DocId};
use roaring::RoaringTreemap;

use crate::{
    ranking::COVERAGE_POWER,
    scorer::{BoxScorer, Scorer},
};

//Top `k` documents by summed scorer score (WAND), plus the top `any_k` documents holding
// any scorer by plain score. Returns `(main, any)`.
pub(crate) fn top_k_with_any<'a>(
    stats: &dyn SearchStats,
    scorers: &mut Vec<BoxScorer<'a>>,
    k: usize,
    coverage: Option<f32>,
    restrict: Option<&RoaringTreemap>,
    any_k: usize,
) -> (Vec<WandHit>, Vec<WandHit>) {
    if k == 0 && any_k == 0 {
        return (Vec::new(), Vec::new());
    }

    let mut heap = BinaryHeap::<HeapHit>::with_capacity(k + 1);
    let mut any_heap = BinaryHeap::<HeapHit>::with_capacity(any_k + 1);

    //The query's total weight, and the share a document must cover for the main list.
    let total_weight: f32 = scorers.iter().map(|scorer| scorer.weight()).sum();
    let needed: f32 = coverage.map_or(0.0, |share| share.clamp(0.0, 1.0) * total_weight);
    //Tiny slack: float sums of the same weights can differ in the last bits.
    let covers = |weight: f32| weight + 1e-3 >= needed;

    //Score every scorer sitting on `doc`; returns (score, raw, matches, covered).
    let evaluate = |scorers: &mut [BoxScorer<'a>], doc: DocId| -> Option<(u64, u64, usize, bool)> {
        let mut score = 0u64;
        let mut matched_terms = 0usize;
        let mut matched_weight = 0.0f32;
        for scorer in scorers.iter_mut() {
            if scorer.doc() == Some(doc) {
                score = score.saturating_add(scorer.score(stats, doc));
                matched_terms += 1;
                matched_weight += scorer.weight();
            }
        }
        if matched_terms == 0 {
            return None;
        }
        let covered = covers(matched_weight);
        let raw = score;
        if covered && coverage.is_some() && total_weight > 0.0 {
            let share = f64::from((matched_weight / total_weight).min(1.0));
            score = ((score as f64) * share.powf(f64::from(COVERAGE_POWER))) as u64;
        }
        Some((score, raw, matched_terms, covered))
    };

    // Push a scored document into the lists that accept it.
    let collect =
        |heap: &mut BinaryHeap<HeapHit>,
         any_heap: &mut BinaryHeap<HeapHit>,
         doc: DocId,
         (score, raw, matched_terms, covered): (u64, u64, usize, bool)| {
            if covered && k > 0 {
                push_top_k(heap, doc, score, raw, matched_terms, k);
            }
            if any_k > 0 {
                push_top_k(any_heap, doc, raw, raw, matched_terms, any_k);
            }
        };

    // A small allow-list: visit each allowed document directly (the cursors gallop).
    if let Some(docs) = restrict.filter(|docs| docs.len() <= DIRECT_RESTRICT_MAX as u64) {
        let mut targets: Vec<DocId> = docs.iter().collect();
        targets.sort_unstable();
        for doc in targets {
            for scorer in scorers.iter_mut() {
                scorer.advance_to(doc);
            }
            if let Some(found) = evaluate(scorers.as_mut_slice(), doc) {
                collect(&mut heap, &mut any_heap, doc, found);
            }
        }
        return (finish_heap(heap), finish_heap(any_heap));
    }

    loop {
        scorers.retain(|scorer| scorer.doc().is_some());
        if scorers.is_empty() {
            break;
        }
        scorers.sort_unstable_by_key(|scorer| scorer.doc().unwrap());

        // The score a document must beat: the worse of the two lists' k-th best.
        let pool_threshold = if k == 0 {
            u64::MAX
        } else if heap.len() < k {
            0
        } else {
            heap.peek().map_or(0, |hit| hit.score)
        };
        let any_threshold = (any_k > 0).then(|| {
            if any_heap.len() < any_k {
                0
            } else {
                any_heap.peek().map_or(0, |hit| hit.score)
            }
        });

        // Pivot: the first prefix that can still beat the threshold and cover enough.
        let mut accumulated_bound = 0u64;
        let mut accumulated_weight = 0.0f32;
        let mut pivot = None;
        for (index, scorer) in scorers.iter().enumerate() {
            accumulated_bound = accumulated_bound.saturating_add(scorer.upper_bound());
            accumulated_weight += scorer.weight();
            let for_pool = accumulated_bound >= pool_threshold && covers(accumulated_weight);
            let for_any = any_threshold.is_some_and(|any| accumulated_bound >= any);
            if accumulated_bound > 0 && (for_pool || for_any) {
                pivot = Some(index);
                break;
            }
        }
        let Some(pivot) = pivot else {
            break;
        };

        let mut pivot_doc = scorers[pivot].doc().unwrap();
        let smallest_doc = scorers[0].doc().unwrap();

        // A filter: jump to the next allowed document at or after the pivot.
        if let Some(allowed) = restrict {
            let Some(next) = next_allowed(allowed, pivot_doc) else {
                break;
            };
            pivot_doc = next;
        }

        if smallest_doc == pivot_doc {
            // Block-max is not ported yet: it only affects speed, not results.
            if restrict.is_none_or(|docs| docs.contains(pivot_doc))
                && let Some(found) = evaluate(scorers.as_mut_slice(), pivot_doc)
            {
                collect(&mut heap, &mut any_heap, pivot_doc, found);
            }
            // Step past only the scorers that are on the pivot document.
            for scorer in scorers.iter_mut() {
                if scorer.doc() == Some(pivot_doc) {
                    scorer.next();
                }
            }
        } else {
            // Everything before the pivot is hopeless: skip it.
            for scorer in scorers.iter_mut() {
                if scorer.doc().is_some_and(|doc| doc < pivot_doc) {
                    scorer.advance_to(pivot_doc);
                }
            }
        }
    }

    (finish_heap(heap), finish_heap(any_heap))
}

/// Exact top-k for a conjunction: every scorer in a group must be on the document.
pub(crate) fn conjunctive_top_k<'a>(
    stats: &dyn SearchStats,
    groups: &mut Vec<Vec<BoxScorer<'a>>>,
    k: usize,
    restrict: Option<&RoaringTreemap>,
) -> Vec<WandHit> {
    if k == 0 {
        return Vec::new();
    }

    let mut current: Vec<Option<DocId>> = groups
        .iter_mut()
        .map(|group| next_common(group.as_mut_slice(), None))
        .collect();

    let mut heap = BinaryHeap::<HeapHit>::with_capacity(k + 1);

    while let Some(doc_id) = current.iter().flatten().min().copied() {
        if restrict.as_ref().is_none_or(|docs| docs.contains(doc_id)) {
            let mut score = 0u64;
            let mut matched_terms = 0usize;
            for (group, at) in groups.iter_mut().zip(&current) {
                if *at == Some(doc_id) {
                    for scorer in group.iter_mut() {
                        score = score.saturating_add(scorer.score(stats, doc_id));
                    }
                    matched_terms += group.len();
                }
            }
            push_top_k(&mut heap, doc_id, score, score, matched_terms, k);
        }

        for (group, at) in groups.iter_mut().zip(current.iter_mut()) {
            if *at == Some(doc_id) {
                *at = next_common(group.as_mut_slice(), Some(doc_id));
            }
        }
    }

    finish_heap(heap)
}

/// The next document every scorer in the group is on, after `after`.
pub(crate) fn next_common<'a>(
    scorers: &mut [BoxScorer<'a>],
    after: Option<DocId>,
) -> Option<DocId> {
    if after.is_some() {
        for scorer in scorers.iter_mut() {
            scorer.next();
        }
    }
    loop {
        let mut target = None;
        for scorer in scorers.iter() {
            let doc = scorer.doc()?;
            target = Some(target.map_or(doc, |known: DocId| known.max(doc)));
        }
        let target = target?;
        for scorer in scorers.iter_mut() {
            scorer.advance_to(target);
        }
        if scorers.iter().all(|scorer| scorer.doc() == Some(target)) {
            return Some(target);
        }
    }
}

///Walk every document a scorer produces and keep the top k
pub(crate) fn collect_all<S: Scorer + ?Sized>(
    stats: &dyn SearchStats,
    scorer: &mut S,
    k: usize,
    restrict: Option<&RoaringTreemap>,
) -> Vec<WandHit> {
    let mut heap = BinaryHeap::<HeapHit>::with_capacity(k + 1);
    while let Some(doc) = scorer.doc() {
        if restrict.is_none_or(|docs| docs.contains(doc)) {
            let score = scorer.score(stats, doc);
            push_top_k(&mut heap, doc, score, score, 1, k);
        }
        scorer.next();
    }
    finish_heap(heap)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WandHit {
    pub doc_id: DocId,
    /// Top-k score, after coverage scaling.
    pub score: u64,
    /// The plain BM25 sum.
    pub raw: u64,
    pub matched_terms: usize,
}

/// Heap entry ordered so the worst hit is on top (for eviction).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct HeapHit {
    pub(crate) doc_id: DocId,
    pub(crate) score: u64,
    pub(crate) raw: u64,
    pub(crate) matched_terms: usize,
}

impl Ord for HeapHit {
    fn cmp(&self, other: &Self) -> Ordering {
        other
            .score
            .cmp(&self.score)
            .then_with(|| self.doc_id.cmp(&other.doc_id))
    }
}

impl PartialOrd for HeapHit {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// The smallest allowed doc id >= `d`, or `None` when the filter has nothing left.
pub(crate) fn next_allowed(allowed: &RoaringTreemap, d: DocId) -> Option<DocId> {
    if d == 0 {
        allowed.select(0)
    } else {
        allowed.select(allowed.rank(d - 1))
    }
}

/// Keeps the heap at `k` entries, keeping the best score (ties by smaller doc id).
pub(crate) fn push_top_k(
    heap: &mut BinaryHeap<HeapHit>,
    doc_id: DocId,
    score: u64,
    raw: u64,
    matched_terms: usize,
    k: usize,
) {
    let hit = HeapHit {
        doc_id,
        score,
        raw,
        matched_terms,
    };
    if heap.len() < k {
        heap.push(hit);
        return;
    }
    let Some(worst) = heap.peek() else {
        heap.push(hit);
        return;
    };
    if score > worst.score || (score == worst.score && doc_id < worst.doc_id) {
        heap.pop();
        heap.push(hit);
    }
}

/// Drains the heap into hits, best first.
pub(crate) fn finish_heap(heap: BinaryHeap<HeapHit>) -> Vec<WandHit> {
    let mut hits: Vec<WandHit> = heap
        .into_iter()
        .map(|hit| WandHit {
            doc_id: hit.doc_id,
            score: hit.score,
            raw: hit.raw,
            matched_terms: hit.matched_terms,
        })
        .collect();
    hits.sort_unstable_by(|a, b| b.score.cmp(&a.score).then_with(|| a.doc_id.cmp(&b.doc_id)));
    hits
}

/// Up to this many allowed documents are visited one by one.
pub(crate) const DIRECT_RESTRICT_MAX: usize = 4096;
