use std::{cmp::Ordering, collections::BinaryHeap};

use ahash::HashSet;
use core_index::{
    posting::{PostingList, cursor::PostingCursor},
    search::{SearchStats, TermPostings},
    types::{DocId, XPathId},
};
use core_timing::timed;

use crate::scorer::{bm25_score_scaled, bm25_upper_bound};


/// One query term's postings in a field, borrowed from wherever they were read.
pub struct TermRef<'a> {
    pub postings: &'a PostingList,
    pub doc_freq: u32,
    pub max_weight: u16,
}

/// Postings for every query term within one field.
pub struct FieldTerms<'a> {
    pub xpath: XPathId,
    pub terms: Vec<TermRef<'a>>,
}

struct MultiFieldTerm<'a> {
    cursor: PostingCursor<'a>,
    field: usize,
    doc_freq: u32,
    upper_bound: u64,
}

/// Exact top-k over the sum of BM25 contributions of every (term, field) pair.
/// Each pair is an independent WAND cursor scored with its own field's statistics.
#[timed(search)]
pub fn wand_top_k_multi_field<S: SearchStats>(
    stats: &S,
    fields: &[FieldTerms<'_>],
    k: usize,
    restrict: Option<&HashSet<DocId>>,
) -> Vec<WandHit> {
    if k == 0 || fields.is_empty() {
        return Vec::new();
    }

    let field_stats: Vec<_> = fields
        .iter()
        .map(|field| (field.xpath, stats.doc_count(field.xpath), stats.avg_doc_len(field.xpath)))
        .collect();

    let mut terms: Vec<MultiFieldTerm<'_>> = Vec::new();
    for (field_index, field) in fields.iter().enumerate() {
        let doc_count = field_stats[field_index].1;
        for term in &field.terms {
            let cursor = PostingCursor::new(term.postings);
            if cursor.is_exhausted() {
                continue;
            }
            terms.push(MultiFieldTerm {
                cursor,
                field: field_index,
                doc_freq: term.doc_freq,
                upper_bound: bm25_upper_bound(term.max_weight, doc_count, term.doc_freq),
            });
        }
    }

    let mut heap = BinaryHeap::<HeapHit>::with_capacity(k + 1);
    let mut doc_len_cache: Vec<Option<u32>> = vec![None; fields.len()];

    loop {
        terms.retain(|term| !term.cursor.is_exhausted());
        if terms.is_empty() {
            break;
        }
        terms.sort_unstable_by_key(|term| term.cursor.doc_id().unwrap());

        let threshold = if heap.len() < k {
            0
        } else {
            heap.peek().map(|hit| hit.score).unwrap_or(0)
        };

        let mut accumulated_upper_bound = 0u64;
        let mut pivot_index = None;
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
            if restrict.is_none_or(|docs| docs.contains(&pivot_doc)) {
                doc_len_cache.fill(None);
                let mut score = 0u64;
                let mut matched_terms = 0usize;

                for term in &terms {
                    if term.cursor.doc_id() != Some(pivot_doc) {
                        continue;
                    }
                    let (xpath, doc_count, avg_doc_len) = field_stats[term.field];
                    let doc_len = *doc_len_cache[term.field].get_or_insert_with(|| {
                        stats.doc_len(pivot_doc, xpath).unwrap_or(avg_doc_len as u32)
                    });
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

                push_top_k(&mut heap, pivot_doc, score, matched_terms, k);
            }

            for term in &mut terms {
                if term.cursor.doc_id() == Some(pivot_doc) {
                    term.cursor.next();
                }
            }
        } else {
            for term in &mut terms[..pivot_index] {
                term.cursor.advance_to(pivot_doc);
            }
        }
    }

    finish_heap(heap)
}
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

/// The terms of one field that must all occur in the same document, advanced in step.
struct FieldConjunction<'a> {
    xpath: XPathId,
    doc_count: u64,
    avg_doc_len: f32,
    terms: Vec<WandTerm<'a>>,
    started: bool,
}

impl<'a> FieldConjunction<'a> {
    /// `None` when the field cannot match: no terms, or a term without postings.
    fn new<S: SearchStats>(stats: &S, field: &FieldTerms<'a>) -> Option<Self> {
        if field.terms.is_empty() || field.terms.iter().any(|term| term.postings.is_empty()) {
            return None;
        }

        let doc_count = stats.doc_count(field.xpath);
        Some(Self {
            xpath: field.xpath,
            doc_count,
            avg_doc_len: stats.avg_doc_len(field.xpath),
            terms: field
                .terms
                .iter()
                .map(|term| WandTerm::new(term.postings, term.doc_freq, term.max_weight, doc_count))
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
                .map(|term| term.cursor.doc_id().unwrap())
                .max()
                .unwrap();

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
    fn score<S: SearchStats>(&self, stats: &S, doc_id: DocId) -> u64 {
        let doc_len = stats
            .doc_len(doc_id, self.xpath)
            .unwrap_or(self.avg_doc_len as u32);

        let mut score = 0u64;
        for term in &self.terms {
            let posting = term.cursor.current().unwrap();
            score = score.saturating_add(bm25_score_scaled(
                posting.positions.len() as u32,
                posting.weight,
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
pub fn conjunctive_top_k_multi_field<S: SearchStats>(
    stats: &S,
    fields: &[FieldTerms<'_>],
    k: usize,
    restrict: Option<&HashSet<DocId>>,
) -> Vec<WandHit> {
    if k == 0 {
        return Vec::new();
    }

    let mut conjunctions: Vec<FieldConjunction<'_>> = fields
        .iter()
        .filter_map(|field| FieldConjunction::new(stats, field))
        .collect();
    let mut current: Vec<Option<DocId>> = conjunctions
        .iter_mut()
        .map(|conjunction| conjunction.next_match())
        .collect();

    let mut heap = BinaryHeap::<HeapHit>::with_capacity(k + 1);

    while let Some(doc_id) = current.iter().flatten().min().copied() {
        if restrict.is_none_or(|docs| docs.contains(&doc_id)) {
            let mut score = 0u64;
            let mut matched_terms = 0usize;

            for (conjunction, at) in conjunctions.iter().zip(&current) {
                if *at == Some(doc_id) {
                    score = score.saturating_add(conjunction.score(stats, doc_id));
                    matched_terms += conjunction.terms.len();
                }
            }

            push_top_k(&mut heap, doc_id, score, matched_terms, k);
        }

        for (conjunction, at) in conjunctions.iter_mut().zip(current.iter_mut()) {
            if *at == Some(doc_id) {
                *at = conjunction.next_match();
            }
        }
    }

    finish_heap(heap)
}

/// One way a query clause can match, scored with the statistics of field `xpath` (the exact field
/// for an exact match). 
pub struct FormRef<'a> {
    pub xpath: XPathId,
    /// Which searched field the form is read in.d
    pub field: usize,
    pub lists: Vec<TermRef<'a>>,
    /// Share of its score a match counts for, in thousandths.
    pub weight: u16,
}

/// The query clauses within one field: a word has one form, a synonym group several. A clause may
/// also hold forms of several fields (a word that may match in any of them, cross_fields): the
/// forms of one field are next to each other.
pub struct FieldClauses<'a> {
    pub clauses: Vec<Vec<FormRef<'a>>>,
}

struct FormCursor<'a> {
    field: usize,
    // how rare the form's words are together (the sum of their idf in the form's field): what a
    // match of this form is worth when a query's coverage is measured (see `wand_top_k_grouped`)
    idf: f32,
    cursors: Vec<PostingCursor<'a>>,
    doc_freqs: Vec<u32>,
    xpath: XPathId,
    doc_count: u64,
    avg_doc_len: f32,
    weight: u16,
    upper_bound: u64,
    // the document every cursor is on, `None` once there is none left
    doc: Option<DocId>,
}

impl<'a> FormCursor<'a> {
    /// `None` when the form cannot match (a list without postings).
    fn new<S: SearchStats>(stats: &S, form: &FormRef<'a>) -> Option<Self> {
        if form.lists.is_empty() || form.lists.iter().any(|list| list.postings.is_empty()) {
            return None;
        }
        let doc_count = stats.doc_count(form.xpath);
        let upper_bound = form.lists
            .iter()
            .map(|list| bm25_upper_bound(list.max_weight, doc_count, list.doc_freq))
            .fold(0u64, u64::saturating_add);
        let rarity = |doc_freq: u32| -> f32 {
            let n = doc_count as f32;
            let df = (doc_freq as f32).min(n);
            if n <= 0.0 || df <= 0.0 {
                return 0.0;
            }
            ((n - df + 0.5) / (df + 0.5) + 1.0).ln()
        };
        let mut cursor = Self {
            field: form.field,
            idf: form.lists.iter().map(|list| rarity(list.doc_freq)).sum(),
            cursors: form.lists
                .iter()
                .map(|list| PostingCursor::new(list.postings))
                .collect(),
            doc_freqs: form.lists
                .iter()
                .map(|list| list.doc_freq)
                .collect(),
            xpath: form.xpath,
            doc_count,
            avg_doc_len: stats.avg_doc_len(form.xpath),
            weight: form.weight,
            upper_bound: weighted(upper_bound, form.weight),
            doc: None,
        };
        cursor.align();
        Some(cursor)
    }

    /// Moves to the first document at or after the cursors' positions that all of them are on.
    fn align(&mut self) {
        loop {
            let mut target = None;
            for cursor in &self.cursors {
                let Some(doc) = cursor.doc_id() else {
                    self.doc = None;
                    return;
                };
                target = Some(target.map_or(doc, |known: DocId| known.max(doc)));
            }
            let Some(target) = target else {
                self.doc = None;
                return;
            };
            for cursor in &mut self.cursors {
                cursor.advance_to(target);
            }
            if self.cursors.iter().all(|cursor| cursor.doc_id() == Some(target)) {
                self.doc = Some(target);
                return;
            }
        }
    }

    fn advance_to(&mut self, target: DocId) {
        if self.doc.is_some_and(|doc| doc < target) {
            for cursor in &mut self.cursors {
                cursor.advance_to(target);
            }
            self.align();
        }
    }

    fn step_past(&mut self, doc: DocId) {
        if self.doc == Some(doc) {
            self.cursors[0].next();
            self.align();
        }
    }

    /// The weighted score of the document the form is on.
    fn score<S: SearchStats>(&self, stats: &S, doc: DocId) -> u64 {
        let doc_len = stats.doc_len(doc, self.xpath).unwrap_or(self.avg_doc_len as u32);
        let mut score = 0u64;
        for (cursor, &doc_freq) in self.cursors.iter().zip(&self.doc_freqs) {
            let posting = cursor.current().unwrap();
            score = score.saturating_add(
                bm25_score_scaled(
                    posting.positions.len() as u32,
                    posting.weight,
                    doc_len,
                    self.avg_doc_len,
                    self.doc_count,
                    doc_freq
                )
            );
        }
        weighted(score, self.weight)
    }
}

fn weighted(score: u64, weight: u16) -> u64 {
    score.saturating_mul(u64::from(weight)) / 1000
}

/// One query clause in one field, walked as a single cursor: the union of its forms' documents.
/// A document scores by its best form, so a synonym group counts as one word however many forms
/// it has.
struct ClauseCursor<'a> {
    forms: Vec<FormCursor<'a>>,
    upper_bound: u64,
    // what matching the clause is worth for coverage: the idf of its form in the field where the
    // word is most common, among the fields most documents have. A word is weighed by how rare
    // it is in the text, not in short fields like a title where almost every word is rare
    weight: f32,
}
impl<'a> ClauseCursor<'a> {
    /// `None` when no form of the clause can match in the field.
    fn new<S: SearchStats>(stats: &S, forms: &[FormRef<'a>]) -> Option<Self> {
        let forms: Vec<FormCursor<'a>> = forms
            .iter()
            .filter_map(|form| FormCursor::new(stats, form))
            .collect();
        if forms.is_empty() {
            return None;
        }
        let upper_bound = best_per_field(forms.iter().map(|form| (form.field, form.upper_bound)));
        let largest = forms.iter().map(|form| form.doc_count).max().unwrap_or(0);
        let weight = forms
            .iter()
            .filter(|form| form.doc_count == largest)
            .map(|form| form.idf)
            .reduce(f32::min)
            .unwrap_or(0.0);
        Some(Self { forms, upper_bound, weight })
    }

    fn doc_id(&self) -> Option<DocId> {
        self.forms.iter().filter_map(|form| form.doc).min()
    }

    fn advance_to(&mut self, target: DocId) {
        for form in &mut self.forms {
            form.advance_to(target);
        }
    }

    fn step_past(&mut self, doc: DocId) {
        for form in &mut self.forms {
            form.step_past(doc);
        }
    }

    fn score<S: SearchStats>(&self, stats: &S, doc: DocId) -> u64 {
        best_per_field(
            self.forms
                .iter()
                .filter(|form| form.doc == Some(doc))
                .map(|form| (form.field, form.score(stats, doc)))
        )
    }
}

/// The best value of each field, added up over the fields; the values of one field come one after
/// another (see `FieldClauses`).
fn best_per_field(values: impl Iterator<Item = (usize, u64)>) -> u64 {
    let mut total = 0u64;
    let mut current: Option<(usize, u64)> = None;
    for (field, value) in values {
        current = match current {
            Some((at, best)) if at == field => Some((at, best.max(value))),
            Some((_, best)) => {
                total = total.saturating_add(best);
                Some((field, value))
            }
            None => Some((field, value)),
        };
    }
    total.saturating_add(current.map_or(0, |(_, best)| best))
}
/// Up to this many allowed documents are visited one by one (see `wand_top_k_grouped`).
const DIRECT_RESTRICT_MAX: usize = 4096;

/// `wand_top_k_multi_field` for clauses that may be synonym groups: exact top-k over the sum,
/// over every field and clause, of the clause's best weighted form in that field.
///
/// `coverage: Some(share)` ranks by how much of the query a document covers, with one clause per
/// query word (cross_fields): a clause is worth its word's idf, a document must be on clauses
/// worth at least `share` of all of them, and its score is multiplied by (covered / all)², so
/// missing a rare word costs much more than missing a common one. `None` adds the clauses up as
/// they are (any clause is enough).
#[timed(search)]
pub fn wand_top_k_grouped<S: SearchStats>(
    stats: &S,
    fields: &[FieldClauses<'_>],
    k: usize,
    coverage: Option<f32>,
    restrict: Option<&HashSet<DocId>>
) -> Vec<WandHit> {
    if k == 0 {
        return Vec::new();
    }

    let mut clauses: Vec<ClauseCursor<'_>> = fields
        .iter()
        .flat_map(|field| {
            field.clauses.iter().filter_map(|forms| ClauseCursor::new(stats, forms))
        })
        .collect();

    let mut heap = BinaryHeap::<HeapHit>::with_capacity(k + 1);

    // the whole query's weight (the words the index has), and the part a document has to cover
    let total_weight: f32 = clauses.iter().map(|clause| clause.weight).sum();
    let needed: f32 = coverage.map_or(0.0, |share| share.clamp(0.0, 1.0) * total_weight);
    // float sums of the same weights in another order may differ in the last bits
    let covers = |weight: f32| weight + 1e-4 >= needed;
    // a document's score from the clauses on it, scaled by how much of the query it covers;
    // `None` when it covers too little (or nothing)
    let evaluate = |clauses: &[ClauseCursor<'_>], doc: DocId| -> Option<(u64, usize)> {
        let mut score = 0u64;
        let mut matched_terms = 0usize;
        let mut matched_weight = 0.0f32;
        for clause in clauses {
            if clause.doc_id() == Some(doc) {
                score = score.saturating_add(clause.score(stats, doc));
                matched_terms += 1;
                matched_weight += clause.weight;
            }
        }
        if matched_terms == 0 || !covers(matched_weight) {
            return None;
        }
        if coverage.is_some() && total_weight > 0.0 {
            let covered = f64::from((matched_weight / total_weight).min(1.0));
            score = (score as f64 * covered * covered) as u64;
        }
        Some((score, matched_terms))
    };

    // a few allowed documents: go to each of them (the cursors gallop) instead of walking every
    // posting of every clause to find them
    if let Some(docs) = restrict.filter(|docs| docs.len() <= DIRECT_RESTRICT_MAX) {
        let mut targets: Vec<DocId> = docs.iter().copied().collect();
        targets.sort_unstable();
        for doc in targets {
            for clause in &mut clauses {
                clause.advance_to(doc);
            }
            if let Some((score, matched_terms)) = evaluate(&clauses, doc) {
                push_top_k(&mut heap, doc, score, matched_terms, k);
            }
        }
        return finish_heap(heap);
    }

    loop {
        clauses.retain(|clause| clause.doc_id().is_some());
        if clauses.is_empty() {
            break;
        }
        clauses.sort_unstable_by_key(|clause| clause.doc_id().unwrap());

        let threshold = if heap.len() < k {
            0
        } else {
            heap.peek()
                .map(|hit| hit.score)
                .unwrap_or(0)
        };

        // the first clause where the clauses up to it can both beat the threshold and cover
        // enough of the query: a document before its document is on fewer of them than that.
        // (A covered score is never above the plain sum, so the plain bounds stay safe.)
        let mut accumulated_upper_bound = 0u64;
        let mut accumulated_weight = 0.0f32;
        let mut pivot_index = None;
        for (index, clause) in clauses.iter().enumerate() {
            accumulated_upper_bound = accumulated_upper_bound.saturating_add(clause.upper_bound);
            accumulated_weight += clause.weight;
            if
                accumulated_upper_bound >= threshold &&
                accumulated_upper_bound > 0 &&
                covers(accumulated_weight)
            {
                pivot_index = Some(index);
                break;
            }
        }
        let Some(pivot_index) = pivot_index else {
            break;
        };

        let pivot_doc = clauses[pivot_index].doc_id().unwrap();
        let smallest_doc = clauses[0].doc_id().unwrap();

        if smallest_doc == pivot_doc {
            if restrict.is_none_or(|docs| docs.contains(&pivot_doc)) {
                if let Some((score, matched_terms)) = evaluate(&clauses, pivot_doc) {
                    push_top_k(&mut heap, pivot_doc, score, matched_terms, k);
                }
            }

            for clause in &mut clauses {
                clause.step_past(pivot_doc);
            }
        } else {
            for clause in &mut clauses[..pivot_index] {
                clause.advance_to(pivot_doc);
            }
        }
    }

    finish_heap(heap)
}

/// `conjunctive_top_k_multi_field` for clauses that may be synonym groups: every clause has to
/// match in the same field (by any of its forms); a document's score is the sum of its scores in
/// every field where they all do.
#[timed(search)]
pub fn conjunctive_top_k_grouped<S: SearchStats>(
    stats: &S,
    fields: &[FieldClauses<'_>],
    k: usize,
    restrict: Option<&HashSet<DocId>>
) -> Vec<WandHit> {
    if k == 0 {
        return Vec::new();
    }

    // every clause of a field, or none when one of them cannot match there
    let mut conjunctions: Vec<Vec<ClauseCursor<'_>>> = fields
        .iter()
        .filter_map(|field| {
            if field.clauses.is_empty() {
                return None;
            }
            field.clauses
                .iter()
                .map(|forms| ClauseCursor::new(stats, forms))
                .collect::<Option<Vec<_>>>()
        })
        .collect();
    let mut current: Vec<Option<DocId>> = conjunctions
        .iter_mut()
        .map(|clauses| next_common(clauses, None))
        .collect();

    let mut heap = BinaryHeap::<HeapHit>::with_capacity(k + 1);

    while let Some(doc_id) = current.iter().flatten().min().copied() {
        if restrict.is_none_or(|docs| docs.contains(&doc_id)) {
            let mut score = 0u64;
            let mut matched_terms = 0usize;
            for (clauses, at) in conjunctions.iter().zip(&current) {
                if *at == Some(doc_id) {
                    for clause in clauses {
                        score = score.saturating_add(clause.score(stats, doc_id));
                    }
                    matched_terms += clauses.len();
                }
            }
            push_top_k(&mut heap, doc_id, score, matched_terms, k);
        }

        for (clauses, at) in conjunctions.iter_mut().zip(current.iter_mut()) {
            if *at == Some(doc_id) {
                *at = next_common(clauses, Some(doc_id));
            }
        }
    }

    finish_heap(heap)
}

/// The next document every clause is on, after `after`.
fn next_common(clauses: &mut [ClauseCursor<'_>], after: Option<DocId>) -> Option<DocId> {
    if let Some(doc) = after {
        for clause in clauses.iter_mut() {
            clause.step_past(doc);
        }
    }
    loop {
        let mut target = None;
        for clause in clauses.iter() {
            let doc = clause.doc_id()?;
            target = Some(target.map_or(doc, |known: DocId| known.max(doc)));
        }
        let target = target?;
        for clause in clauses.iter_mut() {
            clause.advance_to(target);
        }
        if clauses.iter().all(|clause| clause.doc_id() == Some(target)) {
            return Some(target);
        }
    }
}
