use std::{
    cmp::Ordering,
    collections::{BinaryHeap, HashSet},
};

use core_index::{
    posting::{PostingList, cursor::PostingCursor},
    search::{SearchStats, TermPostings},
    types::{DocId, XPathId},
};
use core_timing::timed;

use crate::scorer::{bm25_score_scaled, bm25_upper_bound};

// All state WAND needs for one query term
// EAch term has its own cursor that moves on its own posting list and a score upper bound. The
// cursor tells us which document the term is currently on.
// The upper ound tells us which maximum score this term could cotribute to the document
//
// The upper bound is what allows WAND to prove that some documents simply wont be looked at cause
// they are simply too shit
pub struct WandTerm<'a> {
    pub cursor: PostingCursor<'a>,
    pub doc_freq: u32,
    pub upper_bound: u64,
}

impl<'a> WandTerm<'a> {
    pub fn new(postings: &'a PostingList, doc_freq: u32, max_weight: u16, doc_count: u64) -> Self {
        Self {
            cursor: PostingCursor::new(postings),
            doc_freq,
            upper_bound: bm25_upper_bound(max_weight, doc_count, doc_freq),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WandHit {
    pub doc_id: DocId,
    pub score: u64,
    pub matched_terms: usize,
}

// Entry stored in the top-k heap
//
// Rust binary heap is a max heap but WAND needs quick acces to the worst result instead of the max
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HeapHit {
    doc_id: DocId,
    score: u64,
    matched_terms: usize,
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

fn push_top_k(
    heap: &mut BinaryHeap<HeapHit>,
    doc_id: DocId,
    score: u64,
    matched_terms: usize,
    k: usize,
) {
    let hit = HeapHit {
        doc_id,
        score,
        matched_terms,
    };

    if heap.len() < k {
        heap.push(hit);
        return;
    }

    let worst = heap.peek().unwrap();

    let is_better = score > worst.score || (score == worst.score && doc_id < worst.doc_id);

    if is_better {
        heap.pop();
        heap.push(hit);
    }
}

fn finish_heap(heap: BinaryHeap<HeapHit>) -> Vec<WandHit> {
    let mut hits: Vec<WandHit> = heap
        .into_iter()
        .map(|hit| WandHit {
            doc_id: hit.doc_id,
            score: hit.score,
            matched_terms: hit.matched_terms,
        })
        .collect();

    hits.sort_unstable_by(|a, b| b.score.cmp(&a.score).then_with(|| a.doc_id.cmp(&b.doc_id)));

    hits
}

// returns the top-k documents under additive BM25 using the WAND algorithm.
//
// Each posting list represents one query term. A documents score is the sum of the BM25
// contributions of all query terms that occur in that document.
#[timed(search)]
pub fn wand_top_k<S: SearchStats>(
    stats: &S,
    xpath: XPathId,
    term_postings: &[TermPostings],
    k: usize,
    restrict: Option<&HashSet<DocId>>,
) -> Vec<WandHit> {
    if k == 0 || term_postings.is_empty() {
        return Vec::new();
    }

    let doc_count = stats.doc_count(xpath);
    let avg_doc_len = stats.avg_doc_len(xpath);

    let mut terms: Vec<WandTerm<'_>> = term_postings
        .iter()
        .map(|term| WandTerm::new(&term.postings, term.doc_freq, term.max_weight, doc_count))
        .filter(|term| !term.cursor.is_exhausted())
        .collect();

    let mut heap = BinaryHeap::<HeapHit>::with_capacity(k + 1);

    loop {
        // If a term is exhausted it doesnt contribute simpel
        terms.retain(|term| !term.cursor.is_exhausted());

        if terms.is_empty() {
            break;
        }

        // WAND reasons about terms in increasing order of their current document. We restore the
        // ordering on every iteration cause cursors mess it up
        terms.sort_unstable_by_key(|term| term.cursor.doc_id().unwrap());

        let threshold = if heap.len() < k {
            0
        } else {
            heap.peek().map(|hit| hit.score).unwrap_or(0)
        };

        let mut accumulated_upper_bound = 0u64;
        let mut pivot_index = None;

        // Find the first term where the maximum possible contribution exceeds the current
        // threshold.
        for (index, term) in terms.iter().enumerate() {
            accumulated_upper_bound = accumulated_upper_bound.saturating_add(term.upper_bound);

            if accumulated_upper_bound >= threshold && accumulated_upper_bound > 0 {
                pivot_index = Some(index);
                break;
            }
        }

        let Some(pivot_index) = pivot_index else {
            break;
        };

        let pivot_doc = terms[pivot_index].cursor.doc_id().unwrap();

        let smallest_doc = terms[0].cursor.doc_id().unwrap();

        if smallest_doc == pivot_doc {
            let allowed = restrict.is_none_or(|docs| docs.contains(&pivot_doc));

            if allowed {
                let doc_len = stats
                    .doc_len(pivot_doc, xpath)
                    .unwrap_or(avg_doc_len as u32);

                let mut score = 0u64;
                let mut matched_terms = 0usize;

                for term in &terms {
                    if term.cursor.doc_id() == Some(pivot_doc) {
                        let posting = term.cursor.current().unwrap();

                        score = score.saturating_add(bm25_score_scaled(
                            posting.positions.len() as u32,
                            posting.weight,
                            doc_len,
                            avg_doc_len,
                            doc_count,
                            term.doc_freq,
                        ));

                        matched_terms += 1;
                    }
                }

                push_top_k(&mut heap, pivot_doc, score, matched_terms, k);
            }

            for term in &mut terms {
                if term.cursor.doc_id() == Some(pivot_doc) {
                    term.cursor.next();
                }
            }
        } else {
            // Docs before the pivot cannot become competitive according to the accumulated upper
            // bounds so we skip those bitches
            for term in &mut terms[..pivot_index] {
                term.cursor.advance_to(pivot_doc);
            }
        }
    }

    finish_heap(heap)
}

#[timed(search)]
pub fn conjunctive_top_k<S: SearchStats>(
    stats: &S,
    xpath: XPathId,
    term_postings: &[TermPostings],
    k: usize,
    restrict: Option<&HashSet<DocId>>,
) -> Vec<WandHit> {
    if k == 0 || term_postings.is_empty() {
        return Vec::new();
    }

    if term_postings.iter().any(|term| term.postings.is_empty()) {
        return Vec::new();
    }

    let doc_count = stats.doc_count(xpath);
    let avg_doc_len = stats.avg_doc_len(xpath);

    let mut terms: Vec<WandTerm<'_>> = term_postings
        .iter()
        .map(|term| WandTerm::new(&term.postings, term.doc_freq, term.max_weight, doc_count))
        .collect();

    let mut heap = BinaryHeap::<HeapHit>::with_capacity(k + 1);

    loop {
        if terms.iter().any(|term| term.cursor.is_exhausted()) {
            break;
        }

        let target = terms
            .iter()
            .map(|term| term.cursor.doc_id().unwrap())
            .max()
            .unwrap();

        for term in &mut terms {
            term.cursor.advance_to(target);
        }

        if terms.iter().any(|term| term.cursor.is_exhausted()) {
            break;
        }

        if terms
            .iter()
            .any(|term| term.cursor.doc_id() != Some(target))
        {
            continue;
        }

        // All query terms match this document
        // Only score it if the external filter allows it
        let allowed = restrict.is_none_or(|docs| docs.contains(&target));

        if allowed {
            let doc_len = stats.doc_len(target, xpath).unwrap_or(avg_doc_len as u32);

            let mut score = 0u64;

            for term in &terms {
                let posting = term.cursor.current().unwrap();

                score = score.saturating_add(bm25_score_scaled(
                    posting.positions.len() as u32,
                    posting.weight,
                    doc_len,
                    avg_doc_len,
                    doc_count,
                    term.doc_freq,
                ));
            }

            push_top_k(&mut heap, target, score, terms.len(), k);
        }

        for term in &mut terms {
            term.cursor.next();
        }
    }

    finish_heap(heap)
}
