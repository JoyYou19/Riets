//! The top-k collector: walks a set of scorers and keeps the k best documents.
use std::{cell::{Cell, RefCell}, cmp::Ordering, collections::BinaryHeap};

use core_index::{num::u64_to_f64, search::SearchStats, types::{DocId, XPathId}};
use roaring::RoaringTreemap;

use crate::{
    ranking::COVERAGE_POWER,
    scorer::{BoxScorer, Scorer},
};
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "float to int `as` saturates, and the factor is in 0..=1"
)]
fn scale_score(score: u64, factor: f64) -> u64 {
    (u64_to_f64(score) * factor) as u64
}
/// Puts the first `moved` entries back in document order after their cursors advanced, and drops
/// the ones that ran out. The others never moved and are still in order, so each moved entry is
/// placed by binary search, instead of sorting every cursor again on every step.
/// 
/// smart logic AI gave to make a bit faster long queries
pub(crate) fn reinsert_moved<T>(
    entries: &mut Vec<T>,
    moved: usize,
    doc: impl Fn(&T) -> Option<DocId>,
    buffer: &mut Vec<T>,
) {
    buffer.extend(entries.drain(..moved.min(entries.len())));
    for entry in buffer.drain(..) {
        let Some(at) = doc(&entry) else {
            continue;
        };
        let index = entries.partition_point(|other| doc(other).is_some_and(|d| d <= at));
        entries.insert(index, entry);
    }
}
/// Where the scorers on the pivot can jump: past the first end of their current blocks, when
/// nothing they score up to there can reach `threshold`. The scorers after them start at the next
/// scorer's document, so until there only these can match.
fn block_skip(
    scorers: &[BoxScorer<'_>],
    on_pivot: usize,
    pivot_doc: DocId,
    threshold: u64,
) -> Option<DocId> {
    if threshold == 0 {
        return None;
    }
    let mut bound = 0u64;
    let mut skip_to = scorers.get(on_pivot).and_then(|scorer| scorer.doc());
    for scorer in scorers.iter().take(on_pivot) {
        match scorer.block_bound() {
            Some((block, last)) => {
                bound = bound.saturating_add(block);
                let after = last.saturating_add(1);
                skip_to = Some(skip_to.map_or(after, |known| known.min(after)));
            }
            None => bound = bound.saturating_add(scorer.upper_bound()),
        }
    }
    // a skip must move forward, or the loop would not end
    skip_to.filter(|&skip| bound < threshold && skip > pivot_doc)
}
/// The statistics with the lengths of the document being scored remembered, so its clauses ask
/// the index once per field instead of once each.
struct DocLens<'s> {
    stats: &'s dyn SearchStats,
    doc: Cell<Option<DocId>>,
    lens: RefCell<Vec<(XPathId, Option<u32>)>>,
}

impl<'s> DocLens<'s> {
    fn new(stats: &'s dyn SearchStats) -> Self {
        Self {
            stats,
            doc: Cell::new(None),
            lens: RefCell::new(Vec::new()),
        }
    }
}

impl SearchStats for DocLens<'_> {
    fn doc_count(&self, xpath: XPathId) -> u64 {
        self.stats.doc_count(xpath)
    }

    fn total_doc_len(&self, xpath: XPathId) -> u64 {
        self.stats.total_doc_len(xpath)
    }

    fn doc_range(&self) -> Option<(DocId, DocId)> {
        self.stats.doc_range()
    }

    fn avg_doc_len(&self, xpath: XPathId) -> f64 {
        self.stats.avg_doc_len(xpath)
    }

    fn doc_len(&self, doc_id: DocId, xpath: XPathId) -> Option<u32> {
        if self.doc.get() != Some(doc_id) {
            self.doc.set(Some(doc_id));
            self.lens.borrow_mut().clear();
        }
        let known = self
            .lens
            .borrow()
            .iter()
            .find(|(field, _)| *field == xpath)
            .map(|&(_, len)| len);
        if let Some(len) = known {
            return len;
        }
        let len = self.stats.doc_len(doc_id, xpath);
        self.lens.borrow_mut().push((xpath, len));
        len
    }
}
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
    let lens = DocLens::new(stats);
    //Score every scorer sitting on `doc`; returns (score, raw, matches, covered).
    let evaluate = |scorers: &mut [BoxScorer<'a>], doc: DocId| -> Option<(u64, u64, usize, bool)> {
        let mut score = 0u64;
        let mut matched_terms = 0usize;
        let mut matched_weight = 0.0f32;
        for scorer in scorers.iter_mut() {
            if scorer.doc() == Some(doc) {
                score = score.saturating_add(scorer.score(&lens, doc));
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
            score = scale_score(score, share.powf(f64::from(COVERAGE_POWER)));
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
    if let Some(docs) = restrict
        .filter(|docs| usize::try_from(docs.len()).is_ok_and(|len| len <= DIRECT_RESTRICT_MAX))
    {
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
    scorers.retain(|scorer| scorer.doc().is_some());
    scorers.sort_unstable_by_key(|scorer| scorer.doc());
    let mut moved= Vec::with_capacity(scorers.len());
    loop {

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

        let (Some(mut pivot_doc), Some(smallest_doc)) = (
            scorers.get(pivot).and_then(|scorer| scorer.doc()),
            scorers.first().and_then(|scorer| scorer.doc()),
        ) else {
            break;
        };

        // A filter: jump to the next allowed document at or after the pivot.
        if let Some(allowed) = restrict {
            let Some(next) = next_allowed(allowed, pivot_doc) else {
                break;
            };
            pivot_doc = next;
        }

        if smallest_doc == pivot_doc {
            let on_pivot = scorers
                .iter()
                .take_while(|scorer| scorer.doc() == Some(pivot_doc))
                .count();
            // a document both lists would still take is never skipped
            let skip_threshold = any_threshold.map_or(pool_threshold, |any| any.min(pool_threshold));
            if let Some(skip_to) = block_skip(scorers, on_pivot, pivot_doc, skip_threshold) {
                for scorer in scorers.iter_mut().take(on_pivot) {
                    scorer.advance_to(skip_to);
                }
                reinsert_moved(scorers, on_pivot, |scorer| scorer.doc(), &mut moved);
                continue;
            }
            if restrict.is_none_or(|docs| docs.contains(pivot_doc))
                && let Some(found) = evaluate(scorers.as_mut_slice(), pivot_doc)
            {
                collect(&mut heap, &mut any_heap, pivot_doc, found);
            }
            for scorer in scorers.iter_mut().take(on_pivot) {
                scorer.next();
            }
            reinsert_moved(scorers, on_pivot, |scorer| scorer.doc(), &mut moved);
        } else {
            // Everything before the pivot is hopeless: skip it.
            let behind = scorers
                .iter()
                .take_while(|scorer| scorer.doc().is_some_and(|doc| doc < pivot_doc))
                .count();
            for scorer in scorers.iter_mut().take(behind) {
                scorer.advance_to(pivot_doc);
            }
            reinsert_moved(scorers, behind, |scorer| scorer.doc(), &mut moved);
        }
    }

    (finish_heap(heap), finish_heap(any_heap))
}

/// Exact top-k for a conjunction: every scorer in a group must be on the document.
pub(crate) fn conjunctive_top_k(
    stats: &dyn SearchStats,
    groups: &mut [Vec<BoxScorer<'_>>],
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
        if restrict.is_none_or(|docs| docs.contains(doc_id)) {
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
pub(crate) fn next_common(scorers: &mut [BoxScorer<'_>], after: Option<DocId>) -> Option<DocId> {
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
