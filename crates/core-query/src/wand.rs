use std::collections::BinaryHeap;

use core_index::{
    posting::cursor::TermCursor,
    search::SearchStats,
    types::{DocId, XPathId},
};
use core_timing::timed;
use roaring::RoaringTreemap;

use crate::{
    bm25::{bm25_score_scaled, bm25_upper_bound}, collector::{HeapHit, finish_heap, next_allowed, push_top_k, reinsert_moved}, scorer::Scorer,
};

pub use crate::collector::WandHit;

/// One query term's postings in a field, borrowed from wherever they were read.
pub struct TermRef<'a> {
    pub cursor: TermCursor<'a>,
    pub doc_freq: u32,
    pub max_weight: u16,
}

struct MultiFieldTerm<'a> {
    cursor: TermCursor<'a>,
    field: usize,
    doc_freq: u32,
    upper_bound: u64,
}
pub struct WandTerm<'a> {
    pub cursor: TermCursor<'a>,
    pub doc_freq: u32,
    pub upper_bound: u64,
}

impl<'a> WandTerm<'a> {
    #[must_use]
    pub fn new(cursor: TermCursor<'a>, doc_freq: u32, max_weight: u16, doc_count: u64) -> Self {
        Self {
            cursor,
            doc_freq,
            upper_bound: bm25_upper_bound(max_weight, doc_count, doc_freq),
        }
    }
}
/// Postings for every query term within one field.
pub struct FieldTerms<'a> {
    pub xpath: XPathId,
    pub terms: Vec<TermRef<'a>>,
}
/// Per searched field: its xpath, document count and average document length.
type FieldStat = (XPathId, u64, f64);
/// The first term whose summed upper bounds can reach `threshold`.
fn pivot_position(terms: &[MultiFieldTerm<'_>], threshold: u64) -> Option<usize> {
    let mut accumulated = 0u64;
    terms.iter().position(|term| {
        accumulated = accumulated.saturating_add(term.upper_bound);
        accumulated >= threshold && accumulated > 0
    })
}

/// Block-max: if the blocks the first `on_pivot` terms are in cannot beat the threshold together,
/// no doc up to the end of the shortest of them can either: the doc to jump to.
fn block_skip_target(
    terms: &[MultiFieldTerm<'_>],
    on_pivot: usize,
    pivot_doc: DocId,
    threshold: u64,
    field_stats: &[FieldStat],
) -> Option<DocId> {
    let mut block_bound = 0u64;
    let mut skip_to = terms.get(on_pivot).and_then(|term| term.cursor.doc_id());
    for term in terms.iter().take(on_pivot) {
        // no block bound (memory lists, unions, unknown doc lengths): the term's
        // whole-list bound, which holds for every doc and does not limit the skip
        let Some(entry) = term.cursor.block_entry().filter(|e| e.min_doc_len > 0) else {
            block_bound = block_bound.saturating_add(term.upper_bound);
            continue;
        };
        let &(_, doc_count, avg_doc_len) = field_stats.get(term.field)?;
        block_bound = block_bound.saturating_add(bm25_score_scaled(
            entry.max_tf,
            entry.max_weight,
            Some(entry.min_doc_len),
            avg_doc_len,
            doc_count,
            term.doc_freq,
        ));
        let after_block = entry.last_doc_id.saturating_add(1);
        skip_to = Some(skip_to.map_or(after_block, |known| known.min(after_block)));
    }
    // `skip > pivot_doc`: a skip must move forward, or the loop would not end
    skip_to.filter(|&skip| block_bound < threshold && skip > pivot_doc)
}

/// The summed score and the number of terms of the document the cursors are onu.
pub enum Cache{
    Data(Option<u32>),
    NotYetFetched,
    None
    
}
fn score_pivot<S: SearchStats>(
    stats: &S,
    terms: &mut [MultiFieldTerm<'_>],
    field_stats: &[FieldStat],
    doc_len_cache: &mut [Option<Option<u32>>],
    pivot_doc: DocId,
) -> (u64, usize) {
    doc_len_cache.fill(None);
    let mut score = 0u64;
    let mut matched_terms = 0usize;
    for term in terms {
        if term.cursor.doc_id() != Some(pivot_doc) {
            continue;
        }
        let Some(&(xpath, doc_count, avg_doc_len)) = field_stats.get(term.field) else {
            continue;
        };
        let doc_len = match doc_len_cache.get_mut(term.field) {
            Some(slot) => *slot.get_or_insert_with(|| stats.doc_len(pivot_doc, xpath)),
            None => stats.doc_len(pivot_doc, xpath),
        };
        score = score.saturating_add(bm25_score_scaled(
            term.cursor.term_freq(),
            term.cursor.weight(),
            doc_len,
            avg_doc_len,
            doc_count,
            term.doc_freq,
        ));
        matched_terms += 1;
    }
    (score, matched_terms)
}
/// Exact top-k over the sum of BM25 contributions of every (term, field) pair.
/// Each pair is an independent WAND cursor scored with its own field's statistics.

#[timed(search)]
pub fn wand_top_k_multi_field<'a, S: SearchStats>(
    stats: &S,
    fields: Vec<FieldTerms<'a>>,
    k: usize,
    restrict: Option<&RoaringTreemap>,
) -> Vec<WandHit> {
    if k == 0 || fields.is_empty() {
        return Vec::new();
    }

    let field_stats: Vec<FieldStat> = fields
        .iter()
        .map(|field| {
            (
                field.xpath,
                stats.doc_count(field.xpath),
                stats.avg_doc_len(field.xpath),
            )
        })
        .collect();
    let mut terms: Vec<MultiFieldTerm<'a>> = Vec::new();
    for (field_index, (field, &(_, doc_count, _))) in
        fields.into_iter().zip(&field_stats).enumerate()
    {
        for term in field.terms {
            if term.cursor.is_exhausted() {
                continue;
            }
            terms.push(MultiFieldTerm {
                cursor: term.cursor,
                field: field_index,
                doc_freq: term.doc_freq,
                upper_bound: bm25_upper_bound(term.max_weight, doc_count, term.doc_freq),
            });
        }
    }

    let mut heap = BinaryHeap::<HeapHit>::with_capacity(k.saturating_add(1));
    let mut doc_len_cache: Vec<Option<Option<u32>>> = vec![None; field_stats.len()];
    terms.sort_unstable_by_key(|term| term.cursor.doc_id());
    let mut moved = Vec::with_capacity(terms.len());
    loop {
       

        let threshold = if heap.len() < k {
            0
        } else {
            heap.peek().map_or(0, |hit| hit.score)
        };

        let Some(pivot_index) = pivot_position(&terms, threshold) else {
            break;
        };
        let (Some(mut pivot_doc), Some(smallest_doc)) = (
            terms.get(pivot_index).and_then(|term| term.cursor.doc_id()),
            terms.first().and_then(|term| term.cursor.doc_id()),
        ) else {
            break;
        };
        if let Some(allowed) = restrict {
            let Some(next) = next_allowed(allowed, pivot_doc) else {
                break;
            };
            pivot_doc = next;
        }

        if smallest_doc == pivot_doc {
            // terms are sorted by doc: the ones on the pivot are a prefix
            let on_pivot = terms
                .iter()
                .take_while(|term| term.cursor.doc_id() == Some(pivot_doc))
                .count();

            if threshold > 0
                && let Some(skip_to) =
                    block_skip_target(&terms, on_pivot, pivot_doc, threshold, &field_stats)
            {
                for term in terms.iter_mut().take(on_pivot) {
                    term.cursor.advance_to(skip_to);
                }
                reinsert_moved(&mut terms, on_pivot, |term| term.cursor.doc_id(), &mut moved);
                continue;
            }

            if restrict.is_none_or(|docs| docs.contains(pivot_doc)) {
                let (score, matched_terms) = score_pivot(
                    stats,
                    &mut terms,
                    &field_stats,
                    &mut doc_len_cache,
                    pivot_doc,
                );
                push_top_k(&mut heap, pivot_doc, score, score, matched_terms, k);
            }

            for term in terms.iter_mut().take(on_pivot) {
                term.cursor.next();
            }
            reinsert_moved(&mut terms, on_pivot, |term| term.cursor.doc_id(), &mut moved);
          } else {
            let behind = terms
                .iter()
                .take_while(|term| term.cursor.doc_id().is_some_and(|doc| doc < pivot_doc))
                .count();
            for term in terms.iter_mut().take(behind) {
                term.cursor.advance_to(pivot_doc);
            }
            reinsert_moved(&mut terms, behind, |term| term.cursor.doc_id(), &mut moved);
        }
    }

    finish_heap(heap)
}

/// The terms of one field that must all occur in the same document, advanced in step.
struct FieldConjunction<'a> {
    xpath: XPathId,
    doc_count: u64,
    avg_doc_len: f64,
    terms: Vec<WandTerm<'a>>,
    started: bool,
}

impl<'a> FieldConjunction<'a> {
    /// `None` when the field cannot match: no terms, or a term without postings.
    fn new<S: SearchStats>(stats: &S, field: FieldTerms<'a>) -> Option<Self> {
        if field.terms.is_empty() || field.terms.iter().any(|term| term.cursor.is_exhausted()) {
            return None;
        }

        let doc_count = stats.doc_count(field.xpath);
        Some(Self {
            xpath: field.xpath,
            doc_count,
            avg_doc_len: stats.avg_doc_len(field.xpath),
            terms: field
                .terms
                .into_iter()
                .map(|term| WandTerm::new(term.cursor, term.doc_freq, term.max_weight, doc_count))
                .collect(),
            started: false,
        })
    }

    /// The next document that contains every term, after the one returned last.
    fn next_match(&mut self) -> Option<DocId> {
        if self.started {
            for term in &mut self.terms {
                term.cursor.next();
            }
        }
        self.started = true;

        loop {
            if self.terms.iter().any(|term| term.cursor.is_exhausted()) {
                return None;
            }

            let target = self
                .terms
                .iter()
                .filter_map(|term| term.cursor.doc_id())
                .max()?;

            for term in &mut self.terms {
                term.cursor.advance_to(target);
            }

            if self.terms.iter().any(|term| term.cursor.is_exhausted()) {
                return None;
            }

            if self
                .terms
                .iter()
                .all(|term| term.cursor.doc_id() == Some(target))
            {
                return Some(target);
            }
        }
    }

    /// The BM25 score of the document the conjunction is currently on.
    fn score<S: SearchStats>(&mut self, stats: &S, doc_id: DocId) -> u64 {
        let doc_len = stats.doc_len(doc_id, self.xpath);

        let mut score = 0u64;
        for term in &mut self.terms {
            score = score.saturating_add(bm25_score_scaled(
                term.cursor.term_freq(),
                term.cursor.weight(),
                doc_len,
                self.avg_doc_len,
                self.doc_count,
                term.doc_freq,
            ));
        }
        score
    }
}

/// `conjunctive_top_k` for several fields at once: a document's score is the sum of its scores
/// in every field that holds all the terms.
#[timed(search)]
pub fn conjunctive_top_k_multi_field<'a, S: SearchStats>(
    stats: &S,
    fields: Vec<FieldTerms<'a>>,
    k: usize,
    restrict: Option<&RoaringTreemap>,
) -> Vec<WandHit> {
    if k == 0 {
        return Vec::new();
    }

    let mut conjunctions: Vec<FieldConjunction<'a>> = fields
        .into_iter()
        .filter_map(|field| FieldConjunction::new(stats, field))
        .collect();
    let mut current: Vec<Option<DocId>> = conjunctions
        .iter_mut()
        .map(FieldConjunction::next_match)
        .collect();

    let mut heap = BinaryHeap::<HeapHit>::with_capacity(k.saturating_add(1));

    while let Some(doc_id) = current.iter().flatten().min().copied() {
        if restrict.is_none_or(|docs| docs.contains(doc_id)) {
            let mut score = 0u64;
            let mut matched_terms = 0usize;

            for (conjunction, at) in conjunctions.iter_mut().zip(&current) {
                if *at == Some(doc_id) {
                    score = score.saturating_add(conjunction.score(stats, doc_id));
                    matched_terms += conjunction.terms.len();
                }
            }

            push_top_k(&mut heap, doc_id, score, score, matched_terms, k);
        }

        for (conjunction, at) in conjunctions.iter_mut().zip(current.iter_mut()) {
            if *at == Some(doc_id) {
                *at = conjunction.next_match();
            }
        }
    }

    finish_heap(heap)
}

pub struct FormRef<'a> {
    pub xpath: XPathId,
    /// Which searched field the form is read in.d
    pub field: usize,
    pub lists: Vec<TermRef<'a>>,
    /// Share of its score a match counts for, in thousandths.
    pub weight: u16,
    /// `true`: the form matches a doc holding any of its lists and scores the ones it holds
    /// (a synonym group's typed words); `false`: every list must be on the doc.
    pub any: bool,
    pub phrase: Option<Vec<u32>>,
}

/// The query clauses within one field: a word has one form, a synonym group several. A clause may
/// also hold forms of several fields (a word that may match in any of them, `cross_fields)`: the
/// forms of one field are next to each other.
pub struct FieldClauses<'a> {
    pub clauses: Vec<Vec<FormRef<'a>>>,
}

/// Conjunctive top-k for clauses that may be synonym groups, through the generic collector.
#[timed(search)]
pub fn conjunctive_top_k_grouped<'a, S: SearchStats>(
    stats: &S,
    fields: Vec<FieldClauses<'a>>,
    k: usize,
    restrict: Option<&RoaringTreemap>,
) -> Vec<WandHit> {
    let mut groups: Vec<Vec<Box<dyn Scorer + 'a>>> = fields
        .into_iter()
        .filter_map(|field| {
            if field.clauses.is_empty() {
                return None;
            }
            field
                .clauses
                .into_iter()
                .map(|forms| crate::scorers::clause_scorer(stats, forms))
                .collect::<Option<Vec<_>>>()
        })
        .collect();
    crate::collector::conjunctive_top_k(stats, &mut groups, k, restrict)
}

/// Top-k over flat grouped clauses (WAND), through the generic collector.
#[timed(search)]
pub fn wand_top_k_grouped_with_any<'a, S: SearchStats>(
    stats: &S,
    fields: Vec<FieldClauses<'a>>,
    k: usize,
    coverage: Option<f32>,
    restrict: Option<&RoaringTreemap>,
    any_k: usize,
) -> (Vec<WandHit>, Vec<WandHit>) {
    let mut scorers: Vec<Box<dyn Scorer + 'a>> = fields
        .into_iter()
        .flat_map(|field| field.clauses)
        .filter_map(|forms| crate::scorers::clause_scorer(stats, forms))
        .collect();
    crate::collector::top_k_with_any(stats, &mut scorers, k, coverage, restrict, any_k)
}

/// `wand_top_k_grouped_with_any` without the extra "any" list.
#[timed(search)]
pub fn wand_top_k_grouped<S: SearchStats>(
    stats: &S,
    fields: Vec<FieldClauses<'_>>,
    k: usize,
    coverage: Option<f32>,
    restrict: Option<&RoaringTreemap>,
) -> Vec<WandHit> {
    wand_top_k_grouped_with_any(stats, fields, k, coverage, restrict, 0).0
}
