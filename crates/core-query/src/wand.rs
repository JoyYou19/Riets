use std::{cmp::Ordering, collections::BinaryHeap};

use ahash::HashSet;
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
//teoretical CLAUDE test
/// Postings for every query term within one field.
pub struct FieldTerms {
    pub xpath: XPathId,
    pub terms: Vec<TermPostings>,
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
    fields: &[FieldTerms],
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
            let cursor = PostingCursor::new(&term.postings);
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

/// One query clause: a single term, or a synonym group whose alternatives
/// are interchangeable (`2 | ii`, `piss | pee | urine`). A group walks the
/// union of its alternatives' posting lists and contributes the best weighted
/// alternative per document, so WAND treats it as one term no matter how many
/// alternatives it has.
pub type WeightedGroup<'a> = Vec<(&'a TermPostings, f64)>;

struct Member<'a> {
    cursor: PostingCursor<'a>,
    doc_freq: u32,
    weight: f64,
}

struct Clause<'a> {
    members: Vec<Member<'a>>,
    upper_bound: u64,
}

impl<'a> Clause<'a> {
    fn new(group: &[(&'a TermPostings, f64)], doc_count: u64) -> Self {
        //Alternatives are scored as one term, with the document frequency of the form the user
        //typed (the first alternative; the first one that occurs at all if the typed form is
        //missing from this field). With their own, a rare synonym such as `viii` for `8` has a
        //far larger idf and the group would score by the rare form; pooling to the largest
        //instead would give a rare typed word the idf of its most common synonym. Using the
        //typed form keeps a typed word's score exactly what it is without the dictionary.
        let pooled_doc_freq = group
            .iter()
            .map(|(term, _)| term.doc_freq)
            .find(|&doc_freq| doc_freq > 0)
            .unwrap_or(0);
        let mut upper_bound = 0u64;
        let members = group
            .iter()
            .map(|&(term, weight)| {
                let bound = bm25_upper_bound(term.max_weight, doc_count, pooled_doc_freq);
                upper_bound = upper_bound.max(weighted(bound, weight));
                Member { cursor: PostingCursor::new(&term.postings), doc_freq: pooled_doc_freq, weight }
            })
            .collect();
        Self { members, upper_bound }
    }

    /// Smallest document any alternative is on; `None` once all are exhausted.
    fn doc_id(&self) -> Option<DocId> {
        self.members.iter().filter_map(|member| member.cursor.doc_id()).min()
    }

    fn advance_to(&mut self, target: DocId) {
        for member in &mut self.members {
            if !member.cursor.is_exhausted() {
                member.cursor.advance_to(target);
            }
        }
    }

    /// Moves every alternative that sits on `doc` past it.
    fn step_past(&mut self, doc: DocId) {
        for member in &mut self.members {
            if member.cursor.doc_id() == Some(doc) {
                member.cursor.next();
            }
        }
    }

    /// Best weighted alternative on `doc` (taking the max rather than the sum
    /// keeps a doc with both `2` and `ii` from being counted twice).
    fn score(&self, doc: DocId, doc_len: u32, avg_doc_len: f32, doc_count: u64) -> u64 {
        self.members
            .iter()
            .filter(|member| member.cursor.doc_id() == Some(doc))
            .map(|member| {
                let posting = member.cursor.current().unwrap();
                let score = bm25_score_scaled(
                    posting.positions.len() as u32,
                    posting.weight,
                    doc_len,
                    avg_doc_len,
                    doc_count,
                    member.doc_freq,
                );
                weighted(score, member.weight)
            })
            .max()
            .unwrap_or(0)
    }
}

fn weighted(score: u64, weight: f64) -> u64 {
    if weight == 1.0 {
        score
    } else {
        ((score as f64) * weight) as u64
    }
}

fn clauses<'a>(groups: &[WeightedGroup<'a>], doc_count: u64) -> Vec<Clause<'a>> {
    groups
        .iter()
        .map(|group| Clause::new(group, doc_count))
        .filter(|clause| clause.doc_id().is_some())
        .collect()
}

fn single_term_groups(term_postings: &[TermPostings]) -> Vec<WeightedGroup<'_>> {
    term_postings.iter().map(|term| vec![(term, 1.0)]).collect()
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
    wand_top_k_groups(stats, xpath, &single_term_groups(term_postings), k, restrict)
}

/// WAND where each clause is a term or a synonym group. A document's score
/// is the sum over clauses of each clause's best weighted alternative.
#[timed(search)]
pub fn wand_top_k_groups<S: SearchStats>(
    stats: &S,
    xpath: XPathId,
    groups: &[WeightedGroup<'_>],
    k: usize,
    restrict: Option<&HashSet<DocId>>,
) -> Vec<WandHit> {
    if k == 0 || groups.is_empty() {
        return Vec::new();
    }

    let doc_count = stats.doc_count(xpath);
    let avg_doc_len = stats.avg_doc_len(xpath);
    let mut clauses = clauses(groups, doc_count);
    let mut heap = BinaryHeap::<HeapHit>::with_capacity(k + 1);

    loop {
        // If a clause is exhausted it doesnt contribute simpel
        clauses.retain(|clause| clause.doc_id().is_some());
        if clauses.is_empty() {
            break;
        }

        // WAND reasons about clauses in increasing order of their current document. We restore
        // the ordering on every iteration cause cursors mess it up
        clauses.sort_unstable_by_key(|clause| clause.doc_id().unwrap());

        let threshold = if heap.len() < k {
            0
        } else {
            heap.peek().map(|hit| hit.score).unwrap_or(0)
        };

        // Find the first clause where the maximum possible contribution reaches the threshold.
        let mut accumulated_upper_bound = 0u64;
        let mut pivot_index = None;
        for (index, clause) in clauses.iter().enumerate() {
            accumulated_upper_bound = accumulated_upper_bound.saturating_add(clause.upper_bound);
            if accumulated_upper_bound >= threshold && accumulated_upper_bound > 0 {
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
                let doc_len = stats.doc_len(pivot_doc, xpath).unwrap_or(avg_doc_len as u32);
                let mut score = 0u64;
                let mut matched_terms = 0usize;
                for clause in &clauses {
                    if clause.doc_id() == Some(pivot_doc) {
                        score = score.saturating_add(clause.score(pivot_doc, doc_len, avg_doc_len, doc_count));
                        matched_terms += 1;
                    }
                }
                push_top_k(&mut heap, pivot_doc, score, matched_terms, k);
            }

            for clause in &mut clauses {
                clause.step_past(pivot_doc);
            }
        } else {
            // Docs before the pivot cannot become competitive according to the accumulated upper
            // bounds so we skip those bitches
            for clause in &mut clauses[..pivot_index] {
                clause.advance_to(pivot_doc);
            }
        }
    }

    finish_heap(heap)
}

/// Top-k documents where the terms occur as a phrase: every term present, at
/// consecutive positions, in order. Same walk as `conjunctive_top_k`, with the
/// position check done on the postings the cursors already sit on, so a
/// phrase costs about as much as an AND of its words.
#[timed(search)]
pub fn phrase_top_k<S: SearchStats>(
    stats: &S,
    xpath: XPathId,
    term_postings: &[TermPostings],
    k: usize,
    restrict: Option<&HashSet<DocId>>,
) -> Vec<WandHit> {
    conjunctive_walk(stats, xpath, &single_term_groups(term_postings), k, restrict, true)
}

/// Positions of term i must include start + i for some start of term 0.
fn consecutive(position_lists: &[&[u32]]) -> bool {
    let Some((first, rest)) = position_lists.split_first() else {
        return false;
    };
    first.iter().any(|&start| {
        rest.iter()
            .enumerate()
            .all(|(offset, positions)| positions.binary_search(&(start + offset as u32 + 1)).is_ok())
    })
}

#[timed(search)]
pub fn conjunctive_top_k<S: SearchStats>(
    stats: &S,
    xpath: XPathId,
    term_postings: &[TermPostings],
    k: usize,
    restrict: Option<&HashSet<DocId>>,
) -> Vec<WandHit> {
    conjunctive_top_k_groups(stats, xpath, &single_term_groups(term_postings), k, restrict)
}

/// Every clause must match (any alternative of a group counts); score is the
/// sum over clauses of each clause's best weighted alternative.
#[timed(search)]
pub fn conjunctive_top_k_groups<S: SearchStats>(
    stats: &S,
    xpath: XPathId,
    groups: &[WeightedGroup<'_>],
    k: usize,
    restrict: Option<&HashSet<DocId>>,
) -> Vec<WandHit> {
    conjunctive_walk(stats, xpath, groups, k, restrict, false)
}

/// `phrase` requires single-term clauses, in phrase order.
/// Exact scores of the listed documents in one field, computed the way the top-k walks
/// compute them. `docs` must be sorted ascending without duplicates.
///
/// Conjunctive: a document counts only when every clause matches it; otherwise when any
/// clause does. Only the listed documents are visited: each clause seeks from one to the
/// next instead of walking its whole posting list, so a few hundred documents cost a few
/// hundred seeks per clause.
pub fn score_docs_groups<S: SearchStats>(
    stats: &S,
    xpath: XPathId,
    groups: &[WeightedGroup<'_>],
    docs: &[DocId],
    conjunctive: bool,
) -> Vec<WandHit> {
    if docs.is_empty() || groups.is_empty() {
        return Vec::new();
    }
    let doc_count = stats.doc_count(xpath);
    let avg_doc_len = stats.avg_doc_len(xpath);
    let mut clauses = clauses(groups, doc_count);
    if clauses.is_empty() || (conjunctive && clauses.len() != groups.len()) {
        return Vec::new();
    }
    let mut out = Vec::new();
    for &doc in docs {
        let mut matched = 0usize;
        for clause in &mut clauses {
            clause.advance_to(doc);
            if clause.doc_id() == Some(doc) {
                matched += 1;
            }
        }
        if matched == 0 || (conjunctive && matched != clauses.len()) {
            continue;
        }
        let doc_len = stats.doc_len(doc, xpath).unwrap_or(avg_doc_len as u32);
        let score = clauses.iter().fold(0u64, |sum, clause| {
            sum.saturating_add(clause.score(doc, doc_len, avg_doc_len, doc_count))
        });
        out.push(WandHit { doc_id: doc, score, matched_terms: matched });
    }
    out
}

fn conjunctive_walk<S: SearchStats>(
    stats: &S,
    xpath: XPathId,
    groups: &[WeightedGroup<'_>],
    k: usize,
    restrict: Option<&HashSet<DocId>>,
    phrase: bool,
) -> Vec<WandHit> {
    if k == 0 || groups.is_empty() {
        return Vec::new();
    }

    let doc_count = stats.doc_count(xpath);
    let avg_doc_len = stats.avg_doc_len(xpath);
    let mut clauses = clauses(groups, doc_count);
    //a clause with no postings at all means no document can match every clause
    if clauses.len() != groups.len() {
        return Vec::new();
    }
    let mut heap = BinaryHeap::<HeapHit>::with_capacity(k + 1);

    loop {
        let Some(target) = clauses
            .iter()
            .map(Clause::doc_id)
            .collect::<Option<Vec<DocId>>>()
            .and_then(|docs| docs.into_iter().max())
        else {
            break;
        };

        for clause in &mut clauses {
            clause.advance_to(target);
        }

        let mut all_on_target = true;
        for clause in &clauses {
            match clause.doc_id() {
                None => return finish_heap(heap),
                Some(doc) if doc != target => all_on_target = false,
                Some(_) => {}
            }
        }
        if !all_on_target {
            continue;
        }

        let in_order = !phrase || {
            let positions: Vec<&[u32]> = clauses
                .iter()
                .map(|clause| clause.members[0].cursor.current().unwrap().positions.as_slice())
                .collect();
            consecutive(&positions)
        };

        // All clauses match this document
        // Only score it if the external filter allows it
        if in_order && restrict.is_none_or(|docs| docs.contains(&target)) {
            let doc_len = stats.doc_len(target, xpath).unwrap_or(avg_doc_len as u32);
            let score = clauses.iter().fold(0u64, |sum, clause| {
                sum.saturating_add(clause.score(target, doc_len, avg_doc_len, doc_count))
            });
            push_top_k(&mut heap, target, score, clauses.len(), k);
        }

        for clause in &mut clauses {
            clause.step_past(target);
        }
    }

    finish_heap(heap)
}