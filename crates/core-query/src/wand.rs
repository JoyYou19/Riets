use std::{cmp::Ordering, collections::BinaryHeap};

use core_index::{
    posting::cursor::TermCursor,
    search::SearchStats,
    types::{DocId, XPathId},
};
use core_timing::timed;
use roaring::RoaringTreemap;

use crate::bm25::{bm25_score_scaled, bm25_upper_bound};
use crate::{executor::phrase_matches, scorer::Scorer};

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

/// Smallest allowed doc id `>= d`, or `None` when the filter has no documents left.
pub(crate) fn next_allowed(allowed: &RoaringTreemap, d: DocId) -> Option<DocId> {
    if d == 0 {
        allowed.select(0)
    } else {
        allowed.select(allowed.rank(d - 1))
    }
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

    let field_stats: Vec<_> = fields
        .iter()
        .map(|field| {
            (
                field.xpath,
                stats.doc_count(field.xpath),
                stats.avg_doc_len(field.xpath),
            )
        })
        .collect();
    let field_count = fields.len();
    let mut terms: Vec<MultiFieldTerm<'a>> = Vec::new();
    for (field_index, field) in fields.into_iter().enumerate() {
        let doc_count = field_stats[field_index].1;
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

    let mut heap = BinaryHeap::<HeapHit>::with_capacity(k + 1);
    let mut doc_len_cache: Vec<Option<u32>> = vec![None; field_count];

    loop {
        terms.retain(|term| !term.cursor.is_exhausted());
        if terms.is_empty() {
            break;
        }
        terms.sort_unstable_by_key(|term| term.cursor.doc_id().unwrap());

        let threshold = if heap.len() < k {
            0
        } else {
            heap.peek().map_or(0, |hit| hit.score)
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

        let mut pivot_doc = terms[pivot_index].cursor.doc_id().unwrap();
        let smallest_doc = terms[0].cursor.doc_id().unwrap();
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

            // block-max: if the blocks these terms are in cannot beat the threshold together,
            // no doc up to the end of the shortest of them can either: jump past it
            if threshold > 0 {
                let mut block_bound = 0u64;
                let mut skip_to = terms.get(on_pivot).and_then(|term| term.cursor.doc_id());
                for term in &terms[..on_pivot] {
                    // no block bound (memory lists, unions, unknown doc lengths): the term's
                    // whole-list bound, which holds for every doc and does not limit the skip
                    let Some(entry) = term.cursor.block_entry().filter(|e| e.min_doc_len > 0)
                    else {
                        block_bound = block_bound.saturating_add(term.upper_bound);
                        continue;
                    };
                    let (_, doc_count, avg_doc_len) = field_stats[term.field];
                    block_bound = block_bound.saturating_add(bm25_score_scaled(
                        entry.max_tf,
                        entry.max_weight,
                        entry.min_doc_len,
                        avg_doc_len,
                        doc_count,
                        term.doc_freq,
                    ));
                    let after_block = entry.last_doc_id.saturating_add(1);
                    skip_to = Some(skip_to.map_or(after_block, |known| known.min(after_block)));
                }
                // `skip_to > pivot_doc`: a skip must move forward, or the loop would not end
                if block_bound < threshold
                    && let Some(skip_to) = skip_to
                    && skip_to > pivot_doc
                {
                    for term in &mut terms[..on_pivot] {
                        term.cursor.advance_to(skip_to);
                    }
                    continue;
                }
            }

            if restrict
                .as_ref()
                .is_none_or(|docs| docs.contains(pivot_doc))
            {
                doc_len_cache.fill(None);
                let mut score = 0u64;
                let mut matched_terms = 0usize;

                for term in &mut terms {
                    if term.cursor.doc_id() != Some(pivot_doc) {
                        continue;
                    }
                    let (xpath, doc_count, avg_doc_len) = field_stats[term.field];
                    let doc_len = *doc_len_cache[term.field].get_or_insert_with(|| {
                        stats
                            .doc_len(pivot_doc, xpath)
                            .unwrap_or(avg_doc_len as u32)
                    });

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
                push_top_k(&mut heap, pivot_doc, score, score, matched_terms, k);
            }

            for term in &mut terms {
                if term.cursor.doc_id() == Some(pivot_doc) {
                    term.cursor.next();
                }
            }
        } else {
            for term in &mut terms {
                if term.cursor.doc_id().is_some_and(|doc| doc < pivot_doc) {
                    term.cursor.advance_to(pivot_doc);
                }
            }
        }
    }

    finish_heap(heap)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WandHit {
    pub doc_id: DocId,
    //raw top-k score, scaled by coverage
    pub score: u64,
    //the plain bm25 sum
    pub raw: u64,
    pub matched_terms: usize,
}

// Entry stored in the top-k heap
//
// Rust binary heap is a max heap but WAND needs quick acces to the worst result instead of the max
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

    let is_better = score > worst.score || (score == worst.score && doc_id < worst.doc_id);

    if is_better {
        heap.pop();
        heap.push(hit);
    }
}

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
    fn score<S: SearchStats>(&mut self, stats: &S, doc_id: DocId) -> u64 {
        let doc_len = stats
            .doc_len(doc_id, self.xpath)
            .unwrap_or(self.avg_doc_len as u32);

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

    let mut heap = BinaryHeap::<HeapHit>::with_capacity(k + 1);

    while let Some(doc_id) = current.iter().flatten().min().copied() {
        if restrict.as_ref().is_none_or(|docs| docs.contains(doc_id)) {
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

struct FormCursor<'a> {
    field: usize,
    idf: f32,
    cursors: Vec<TermCursor<'a>>,
    doc_freqs: Vec<u32>,
    xpath: XPathId,
    doc_count: u64,
    avg_doc_len: f32,
    weight: u16,
    upper_bound: u64,
    // any of the lists is enough (the typed words of a synonym group), or all of them
    any: bool,
    // the document every cursor is on, `None` once there is none left
    doc: Option<DocId>,
    phrase: Option<Vec<u32>>,
    positions: Vec<Vec<u32>>,
}

impl<'a> FormCursor<'a> {
    /// `None` when the form cannot match: no lists, or (all-form) a list without postings, or
    /// (any-form) no list with postings.
    fn new<S: SearchStats>(stats: &S, form: FormRef<'a>) -> Option<Self> {
        let cannot_match = if form.any {
            form.lists.iter().all(|list| list.cursor.is_exhausted())
        } else {
            form.lists.iter().any(|list| list.cursor.is_exhausted())
        };
        if form.lists.is_empty() || cannot_match {
            return None;
        }
        let doc_count = stats.doc_count(form.xpath);
        // both kinds: at most every list's best score added up
        let list_bounds: Vec<u64> = form
            .lists
            .iter()
            .map(|list| bm25_upper_bound(list.max_weight, doc_count, list.doc_freq))
            .collect();
        let upper_bound = list_bounds.iter().copied().fold(0u64, u64::saturating_add);
        let rarity = |doc_freq: u32| -> f32 {
            let n = doc_count as f32;
            let df = (doc_freq as f32).min(n);
            if n <= 0.0 || df <= 0.0 {
                return 0.0;
            }
            ((n - df + 0.5) / (df + 0.5) + 1.0).ln()
        };
        let idf = form.lists.iter().map(|list| rarity(list.doc_freq)).sum();
        let doc_freqs = form.lists.iter().map(|list| list.doc_freq).collect();
        let mut cursor = Self {
            field: form.field,
            idf,
            positions: vec![Vec::new(); form.lists.len()],
            phrase: form.phrase,
            cursors: form.lists.into_iter().map(|list| list.cursor).collect(),
            doc_freqs,
            xpath: form.xpath,
            doc_count,
            avg_doc_len: stats.avg_doc_len(form.xpath),
            weight: form.weight,
            upper_bound: weighted(upper_bound, form.weight),
            any: form.any,
            doc: None,
        };
        cursor.align();
        Some(cursor)
    }

    /// Moves to the form's next document at or after the cursors' positions: the smallest doc of
    /// any list (any-form), or the first doc every list is on (all-form).
    fn align(&mut self) {
        if self.any {
            self.doc = self.cursors.iter().filter_map(TermCursor::doc_id).min();
            return;
        }
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
            if self
                .cursors
                .iter()
                .all(|cursor| cursor.doc_id() == Some(target))
            {
                let Some(offsets) = &self.phrase else {
                    self.doc = Some(target);
                    return;
                };
                // a phrase form: the words must also be in order here
                let read = self
                    .cursors
                    .iter_mut()
                    .zip(self.positions.iter_mut())
                    .all(|(cursor, buffer)| cursor.positions_into(buffer).is_ok());
                let lists: Vec<&[u32]> = self.positions.iter().map(Vec::as_slice).collect();
                if read && phrase_matches(&lists, offsets) {
                    self.doc = Some(target);
                    return;
                }
                // not a phrase here: next doc, and align again
                self.cursors[0].next();
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
            if self.any {
                // every list on this doc moves on; the others are already past it
                for cursor in &mut self.cursors {
                    if cursor.doc_id() == Some(doc) {
                        cursor.next();
                    }
                }
            } else {
                self.cursors[0].next();
            }
            self.align();
        }
    }

    /// The weighted score of the document the form is on: the lists on it, added up.
    fn score(&mut self, stats: &dyn SearchStats, doc: DocId) -> u64 {
        let doc_len = stats
            .doc_len(doc, self.xpath)
            .unwrap_or(self.avg_doc_len as u32);
        let mut score = 0u64;
        for (cursor, &doc_freq) in self.cursors.iter_mut().zip(&self.doc_freqs) {
            // all-forms: every list is on the doc; any-forms: only some are
            if cursor.doc_id() != Some(doc) {
                continue;
            }
            score = score.saturating_add(bm25_score_scaled(
                cursor.term_freq(),
                cursor.weight(),
                doc_len,
                self.avg_doc_len,
                self.doc_count,
                doc_freq,
            ));
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
    fn new<S: SearchStats>(stats: &S, forms: Vec<FormRef<'a>>) -> Option<Self> {
        let forms: Vec<FormCursor<'a>> = forms
            .into_iter()
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
        Some(Self {
            forms,
            upper_bound,
            weight,
        })
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

    fn score(&mut self, stats: &dyn SearchStats, doc: DocId) -> u64 {
        best_per_field(
            self.forms
                .iter_mut()
                .filter(|form| form.doc == Some(doc))
                .map(|form| (form.field, form.score(stats, doc))),
        )
    }
}

//temp bridge
impl Scorer for ClauseCursor<'_> {
    fn doc(&self) -> Option<DocId> {
        self.doc_id()
    }

    fn next(&mut self) {
        if let Some(doc) = self.doc_id() {
            self.step_past(doc);
        }
    }

    fn advance_to(&mut self, target: DocId) {
        ClauseCursor::advance_to(self, target);
    }

    fn score(&mut self, stats: &dyn SearchStats, doc: DocId) -> u64 {
        ClauseCursor::score(self, stats, doc)
    }

    fn upper_bound(&self) -> u64 {
        self.upper_bound
    }

    fn weight(&self) -> f32 {
        self.weight
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
pub(crate) const DIRECT_RESTRICT_MAX: usize = 4096;

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
                .map(|forms| ClauseCursor::new(stats, forms))
                .collect::<Option<Vec<_>>>()
                .map(|cursors| {
                    cursors
                        .into_iter()
                        .map(|cursor| Box::new(cursor) as Box<dyn Scorer + 'a>)
                        .collect()
                })
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
        .filter_map(|forms| ClauseCursor::new(stats, forms))
        .map(|cursor| Box::new(cursor) as Box<dyn Scorer + 'a>)
        .collect();
    crate::collector::top_k_with_any(stats, &mut scorers, k, coverage, restrict, any_k)
}

/// `wand_top_k_grouped_with_any` without the extra "any" list.
#[timed(search)]
pub fn wand_top_k_grouped<'a, S: SearchStats>(
    stats: &S,
    fields: Vec<FieldClauses<'a>>,
    k: usize,
    coverage: Option<f32>,
    restrict: Option<&RoaringTreemap>,
) -> Vec<WandHit> {
    wand_top_k_grouped_with_any(stats, fields, k, coverage, restrict, 0).0
}
