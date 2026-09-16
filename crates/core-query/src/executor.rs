use std::{ cmp::Ordering, collections::{ BinaryHeap, HashMap, HashSet }, u32 };

use core_index::{
    analyzer::analyzer::Analyzer,
    fuzzy::FuzzyOptions,
    numeric_columns::NumericBound,
    posting::{ Posting, PostingList, ops::{ intersection, union } },
    search::{ SearchColumns, SearchIndex, SearchStats },
    types::{ DocId, XPathId },
};
use core_timing::timed;

use crate::{ ScoredPosting, SearchHit, TopHit, ast::Query, scorer::score_term_hybrid };

#[derive(Debug, Clone)]
pub struct FieldFilter {
    pub xpath: XPathId,
    pub kind: FieldFilterKind,
}

#[derive(Debug, Clone)]
pub enum FieldFilterKind {
    Text(Option<Query>),
    Exact(String),
    Range {
        lo: Option<NumericBound>,
        hi: Option<NumericBound>,
    },
    Fuzzy(String, FuzzyOptions),
}

// Turns the AST into a PostingList or SearchHit
pub struct QueryExecutor<'a, I> where I: SearchIndex + SearchStats {
    // Which index are we searching?
    index: &'a I,

    // A query is analyzed also the same way as the index, so we could filter out words, stemming,
    // whaatever
    analyzer: &'a Analyzer,
}

impl<'a, I> QueryExecutor<'a, I> where I: SearchIndex + SearchStats + SearchColumns {
    pub fn new(index: &'a I, analyzer: &'a Analyzer) -> Self {
        Self { index, analyzer }
    }

    #[timed(search)]
    fn execute_optional(&self, query: &Query, xpath: XPathId) -> Option<PostingList> {
        match query {
            Query::Term(term) => self.execute_term(term, xpath),
            Query::Prefix(prefix) => self.execute_prefix(prefix, xpath),
            Query::Wildcard(pattern) => Some(self.execute_wildcard(pattern, xpath)),
            Query::And(parts) => self.execute_and(parts, xpath),
            Query::Or(parts) => self.execute_or(parts, xpath),
            Query::Phrase(terms) => self.execute_phrase_optional(terms, xpath),
            Query::Exact(term) => Some(self.execute_exact(term, xpath)),

            Query::Fuzzy(term, opts) => Some(self.execute_fuzzy(term, xpath, *opts)),
        }
    }

    #[timed(search)]
    pub fn execute(&self, query: &Query, xpath: XPathId) -> PostingList {
        self.execute_optional(query, xpath).unwrap_or_default()
    }

    // Query a term
    #[timed(search)]
    fn execute_term(&self, term: &str, xpath: XPathId) -> Option<PostingList> {
        if term.is_empty() {
            return None;
        }

        Some(self.index.lookup(term, xpath))
    }

    // Prefix query, so for example if we do dat* would find database etc.
    #[timed(search)]
    fn execute_prefix(&self, prefix: &str, xpath: XPathId) -> Option<PostingList> {
        if prefix.is_empty() {
            return None;
        }

        Some(self.index.lookup_prefix(prefix, xpath))
    }

    // Wildcard query, for now, we are not analyzing this, might change later
    #[timed(search)]
    fn execute_wildcard(&self, pattern: &str, xpath: XPathId) -> PostingList {
        let pattern = core_index::wildcard::WildcardPattern::parse(pattern);
        self.index.lookup_wildcard(&pattern, xpath)
    }

    // Boolean AND logic
    // 1. execute all child queries
    // 2. if any query returns nothing it drops entire search
    // 3. sorts the posting lists by shortest first
    // 4. Intersects progressively
    #[timed(search)]
    fn execute_and(&self, parts: &[Query], xpath: XPathId) -> Option<PostingList> {
        let mut ordered: Vec<&Query> = parts.iter().collect();
        ordered.sort_by_key(|part| {
            match part {
                Query::Term(term) => self.index.doc_freq(term, xpath),
                _ => u32::MAX, // non-term subclauses (nested And/Or/Phrase) fetch last
            }
        });

        let mut result: Option<PostingList> = None;
        for part in ordered {
            let Some(list) = self.execute_optional(part, xpath) else {
                continue;
            };
            if list.is_empty() {
                return Some(PostingList::default());
            }
            result = Some(match result {
                Some(current) => {
                    let next = intersection(&current, &list);
                    if next.is_empty() {
                        return Some(next);
                    }
                    next
                }
                None => list,
            });
        }
        result.or(Some(PostingList::default()))
    }

    // Boolean OR
    // Executes every child and unions that into a response
    #[timed(search)]
    fn execute_or(&self, parts: &[Query], xpath: XPathId) -> Option<PostingList> {
        let mut result: Option<PostingList> = None;

        for part in parts {
            let Some(next) = self.execute_optional(part, xpath) else {
                continue;
            };

            result = Some(match result {
                Some(current) => union(&current, &next),
                None => next,
            });
        }

        result
    }

    // Phrase query,
    // ["rust", "document"]
    //
    // A document matches only if rust and database appear in it in order rust + database so
    // position and position + 1
    //
    #[timed(search)]
    fn execute_phrase(&self, terms: &[String], xpath: XPathId) -> PostingList {
        use core_index::posting::Posting;

        if terms.is_empty() {
            return PostingList::default();
        }

        let lists: Vec<PostingList> = terms
            .iter()
            .map(|term| self.index.lookup(term, xpath))
            .collect();

        if lists.iter().any(|list| list.is_empty()) {
            return PostingList::default();
        }

        let mut result = Vec::new();
        let first = lists[0].items();

        for first_posting in first {
            let doc_id = first_posting.doc_id;
            let mut position_lists: Vec<&[u32]> = vec![first_posting.positions.as_slice()];

            let mut all_terms_in_doc = true;

            for list in lists.iter().skip(1) {
                match list.items().binary_search_by_key(&doc_id, |p| p.doc_id) {
                    Ok(index) => {
                        position_lists.push(list.items()[index].positions.as_slice());
                    }
                    Err(_) => {
                        all_terms_in_doc = false;
                        break;
                    }
                }
            }

            if all_terms_in_doc && phrase_matches(&position_lists) {
                result.push(Posting::new(doc_id, first_posting.positions.clone()));
            }
        }

        PostingList::from_items(result)
    }

    #[timed(search)]
    fn execute_fuzzy(&self, raw: &str, xpath: XPathId, opts: FuzzyOptions) -> PostingList {
        let words: Vec<String> = raw
            .split_whitespace()
            .filter_map(|w| {
                self.analyzer
                    .analyze_query(w)
                    .into_iter()
                    .next()
                    .map(|t| t.text)
            })
            .collect();

        if words.is_empty() {
            return PostingList::default();
        }

        let lists: Vec<PostingList> = words
            .iter()
            .map(|w| self.index.lookup_fuzzy(w, xpath, opts))
            .collect();

        let mut iter = lists.into_iter();
        let mut result = iter.next().unwrap_or_default();
        for next in iter {
            result = intersection(&result, &next);
        }
        result
    }
    #[timed(search)]
    fn execute_exact(&self, raw: &str, xpath: XPathId) -> PostingList {
        let raw = raw.trim();

        //incase someone did phrase + exact
        let raw = raw
            .strip_prefix('"')
            .and_then(|s| s.strip_suffix('"'))
            .unwrap_or(raw);

        let words: Vec<&str> = raw.split_whitespace().collect();
        if words.is_empty() {
            return PostingList::default();
        }
        if words.len() == 1 {
            return self.index.lookup(words[0], xpath);
        }

        let lists: Vec<PostingList> = words
            .iter()
            .map(|word| self.index.lookup(word, xpath))
            .collect();

        if lists.iter().any(|list| list.is_empty()) {
            return PostingList::default();
        }

        let mut result = Vec::new();
        let first = lists[0].items();

        for first_posting in first {
            let doc_id = first_posting.doc_id;
            let mut position_lists: Vec<&[u32]> = vec![first_posting.positions.as_slice()];

            let mut all_terms_in_doc = true;
            for list in lists.iter().skip(1) {
                match list.items().binary_search_by_key(&doc_id, |p| p.doc_id) {
                    Ok(index) => position_lists.push(list.items()[index].positions.as_slice()),
                    Err(_) => {
                        all_terms_in_doc = false;
                        break;
                    }
                }
            }

            if all_terms_in_doc && phrase_matches(&position_lists) {
                result.push(Posting::new(doc_id, first_posting.positions.clone()));
            }
        }

        PostingList::from_items(result)
    }

    #[timed(search)]
    fn execute_phrase_optional(&self, terms: &[String], xpath: XPathId) -> Option<PostingList> {
        if terms.is_empty() {
            return None;
        }

        Some(self.execute_phrase(terms, xpath))
    }

    // Full search in the entire database
    #[timed(search)]
    pub fn search(&self, query: &Query, xpath: XPathId) -> Vec<SearchHit> {
        let scored = self.execute_scored(query, xpath);

        let mut hits: Vec<SearchHit> = scored
            .into_iter()
            .map(|p| {
                let score = ((p.score as f32) / 1000.0) * p.density;

                SearchHit {
                    doc_id: p.doc_id,
                    matched_terms: p.matched_terms,
                    weight_sum: (p.score / 1000).min(u32::MAX as u64) as u32,
                    distance_factor: p.density,
                    score,
                }
            })
            .collect();

        hits.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap()
                .then_with(|| a.doc_id.cmp(&b.doc_id))
        });

        hits
    }

    // Most basic top K search, searches a single Xpath
    #[timed(search)]
    pub fn search_top_k(&self, query: &Query, xpath: XPathId, k: usize) -> Vec<SearchHit> {
        if k == 0 {
            return Vec::new();
        }

        let scored = self.execute_scored(query, xpath);
        let mut heap: BinaryHeap<TopHit> = BinaryHeap::with_capacity(k + 1);

        for p in scored {
            let score = ((p.score as f32) / 1000.0) * p.density;

            let hit = SearchHit {
                doc_id: p.doc_id,
                matched_terms: p.matched_terms,
                weight_sum: (p.score / 1000).min(u32::MAX as u64) as u32,
                distance_factor: p.density,
                score,
            };

            heap.push(TopHit(hit));

            if heap.len() > k {
                heap.pop();
            }
        }

        let mut hits: Vec<SearchHit> = heap
            .into_iter()
            .map(|hit| hit.0)
            .collect();

        hits.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(Ordering::Equal)
                .then_with(|| a.doc_id.cmp(&b.doc_id))
        });

        hits
    }

    // INFO: Currently we might want to think about other ways of implementing the idea
    // of searching within all xpaths, cause it still happens independently.
    #[timed(search)]
    pub fn search_all_xpaths_top_k(
        &self,
        query: &Query,
        xpaths: impl IntoIterator<Item = XPathId>,
        k: usize
    ) -> Vec<SearchHit> {
        if k == 0 {
            return Vec::new();
        }

        let mut by_doc = HashMap::<DocId, SearchHit>::new();

        for xpath in xpaths {
            for hit in self.search_top_k(query, xpath, k) {
                by_doc
                    .entry(hit.doc_id)
                    .and_modify(|existing| {
                        existing.matched_terms += hit.matched_terms;
                        existing.weight_sum += hit.weight_sum;
                        existing.distance_factor = existing.distance_factor.max(
                            hit.distance_factor
                        );
                        existing.score += hit.score;
                    })
                    .or_insert(hit);
            }
        }

        let mut heap: BinaryHeap<TopHit> = BinaryHeap::with_capacity(k + 1);

        for hit in by_doc.into_values() {
            heap.push(TopHit(hit));

            if heap.len() > k {
                heap.pop();
            }
        }

        let mut hits: Vec<SearchHit> = heap
            .into_iter()
            .map(|hit| hit.0)
            .collect();

        hits.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(Ordering::Equal)
                .then_with(|| a.doc_id.cmp(&b.doc_id))
        });

        hits
    }

    #[timed(search)]
    pub fn resolve_filters(
        &self,
        filters: &HashMap<String, FieldFilter>
    ) -> Option<HashSet<DocId>> {
        if filters.is_empty() {
            return None;
        }

        let mut restrict: Option<HashSet<DocId>> = None;

        for filter in filters.values() {
            let matched: HashSet<DocId> = match &filter.kind {
                FieldFilterKind::Text(query) =>
                    match query {
                        Some(query) =>
                            self
                                .execute(query, filter.xpath)
                                .items()
                                .iter()
                                .map(|p| p.doc_id)
                                .collect(),
                        None => HashSet::new(),
                    }
                FieldFilterKind::Range { lo, hi } =>
                    self.index
                        .column_range(filter.xpath, *lo, *hi)
                        .items()
                        .iter()
                        .map(|p| p.doc_id)
                        .collect(),

                FieldFilterKind::Exact(term) =>
                    self
                        .execute_exact(term, filter.xpath)
                        .items()
                        .iter()
                        .map(|p| p.doc_id)
                        .collect(),

                FieldFilterKind::Fuzzy(term, opts) =>
                    self
                        .execute_fuzzy(term, filter.xpath, *opts)
                        .items()
                        .iter()
                        .map(|p| p.doc_id)
                        .collect(),
            };

            restrict = Some(match restrict {
                Some(current) => current.intersection(&matched).copied().collect(),
                None => matched,
            });

            if restrict.as_ref().is_some_and(|s| s.is_empty()) {
                return restrict;
            }
        }

        restrict
    }

    #[timed(search)]
    pub fn search_all_xpaths_top_k_restricted(
        &self,
        query: Option<&Query>,
        xpaths: impl IntoIterator<Item = XPathId>,
        k: usize,
        restrict: Option<&HashSet<DocId>>
    ) -> Vec<SearchHit> {
        if k == 0 {
            return Vec::new();
        }

        let Some(query) = query else {
            let Some(allowed) = restrict else {
                return Vec::new();
            };
            let mut ids: Vec<DocId> = allowed.iter().copied().collect();
            ids.sort();
            return ids
                .into_iter()
                .take(k)
                .map(|doc_id| SearchHit {
                    doc_id,
                    matched_terms: 0,
                    weight_sum: 0,
                    distance_factor: 0.0,
                    score: 1.0,
                })
                .collect();
        };

        if restrict.is_some_and(|s| s.is_empty()) {
            return Vec::new();
        }

        let mut by_doc = HashMap::<DocId, SearchHit>::new();

        for xpath in xpaths {
            let scored = self.execute_scored(query, xpath);
            for p in scored {
                if let Some(allowed) = restrict {
                    if !allowed.contains(&p.doc_id) {
                        continue;
                    }
                }
                let hit = SearchHit {
                    doc_id: p.doc_id,
                    matched_terms: p.matched_terms,
                    weight_sum: (p.score / 1000).min(u32::MAX as u64) as u32,
                    distance_factor: p.density,
                    score: ((p.score as f32) / 1000.0) * p.density,
                };
                by_doc
                    .entry(hit.doc_id)
                    .and_modify(|existing| {
                        existing.matched_terms += hit.matched_terms;
                        existing.weight_sum = existing.weight_sum.saturating_add(hit.weight_sum);
                        existing.distance_factor = existing.distance_factor.max(
                            hit.distance_factor
                        );
                        existing.score += hit.score;
                    })
                    .or_insert(hit);
            }
        }

        top_k_from_hits(by_doc.into_values(), k)
    }

    // Search the entire database all xpaths
    #[timed(search)]
    pub fn search_all_xpaths(
        &self,
        query: &Query,
        xpaths: impl IntoIterator<Item = XPathId>
    ) -> Vec<SearchHit> {
        use std::collections::BTreeMap;

        let mut by_doc = BTreeMap::<DocId, SearchHit>::new();

        for xpath in xpaths {
            for hit in self.search(query, xpath) {
                by_doc
                    .entry(hit.doc_id)
                    .and_modify(|existing| {
                        existing.matched_terms += hit.matched_terms;
                        existing.weight_sum += hit.weight_sum;
                        existing.distance_factor = existing.distance_factor.max(
                            hit.distance_factor
                        );
                        existing.score += hit.score;
                    })
                    .or_insert(hit);
            }
        }

        by_doc.into_values().collect()
    }

    // Converts a query
    #[timed(search)]
    fn execute_scored(&self, query: &Query, xpath: XPathId) -> Vec<ScoredPosting> {
        match query {
            Query::Term(term) => {
                let postings = self.execute_term(term, xpath).unwrap_or_default();
                let true_df = self.index.doc_freq(term, xpath) as f32;
                score_term_hybrid(self.index, &postings, xpath, true_df)
            }
            Query::And(parts) => self.execute_scored_and(parts, xpath),
            _ => {
                let postings = self.execute(query, xpath);
                // No single term here (Or/Phrase/Wildcard/Fuzzy) — doc_freq()
                // needs one term string, and none of these variants reduce to
                // one. Falling back to the restricted list's own length, same
                // as before this change. This is a real simplification (not
                // true corpus-wide df for whatever compound query this is) but
                // it's a separate, pre-existing approximation from the
                // restrict_to_doc_ids bug this true_df param was added to fix,
                // which specifically hits Query::Term inside an And.
                let true_df = postings.len() as f32;
                score_term_hybrid(self.index, &postings, xpath, true_df)
            }
        }
    }

    #[timed(search)]
   fn execute_scored_and(&self, parts: &[Query], xpath: XPathId) -> Vec<ScoredPosting> {
    // Phase 1: cheap doc-id intersection to find surviving candidates,
    // ordered smallest-doc_freq-first so expensive lists are only fetched
    // if a rarer term hasn't already emptied the intersection.
    let mut ordered: Vec<&Query> = parts.iter().collect();
    ordered.sort_by_key(|part| match part {
        Query::Term(term) => self.index.doc_freq(term, xpath),
        _ => u32::MAX,
    });

    let mut candidate_ids: Option<PostingList> = None;
    for part in &ordered {
        let Some(list) = self.execute_optional(part, xpath) else { continue; };
        if list.is_empty() {
            return Vec::new();
        }
        candidate_ids = Some(match candidate_ids {
            Some(current) => {
                let next = intersection(&current, &list);
                if next.is_empty() {
                    return Vec::new();
                }
                next
            }
            None => list,
        });
    }
    let Some(candidate_ids) = candidate_ids else {
        return Vec::new();
    };

    // Phase 2: score each term against the restricted candidate set, using
    // true_df from the *unrestricted* term, then combine terms.
    let mut term_scores: Option<Vec<ScoredPosting>> = None;
    for part in &ordered {
        let Query::Term(term) = part else { continue; }; // non-term parts skipped — no per-term score path for these yet

        let full_postings = self.execute_term(term, xpath).unwrap_or_default();
        // intersection() against candidate_ids IS the restriction — no
        // separate restrict_to_doc_ids needed.
        let restricted = intersection(&full_postings, &candidate_ids);
        if restricted.is_empty() {
            continue;
        }

        let true_df = self.index.doc_freq(term, xpath) as f32;

        term_scores = Some(match term_scores {
            Some(acc) => crate::scorer::scored_and(&acc, &restricted),
            None => score_term_hybrid(self.index, &restricted, xpath, true_df),
        });
    }

    term_scores.unwrap_or_default()
} 
}

#[timed(search)]
fn phrase_matches(position_lists: &[&[u32]]) -> bool {
    if position_lists.is_empty() {
        return false;
    }

    for &start in position_lists[0] {
        let mut matched = true;
        for (offset, positions) in position_lists.iter().enumerate().skip(1) {
            let expected = start + (offset as u32);

            if positions.binary_search(&expected).is_err() {
                matched = false;
                break;
            }
        }

        if matched {
            return true;
        }
    }

    false
}

#[timed(search)]
fn top_k_from_hits(hits: impl IntoIterator<Item = SearchHit>, k: usize) -> Vec<SearchHit> {
    let mut heap: BinaryHeap<TopHit> = BinaryHeap::with_capacity(k + 1);

    for hit in hits {
        heap.push(TopHit(hit));

        if heap.len() > k {
            heap.pop();
        }
    }

    let mut hits: Vec<SearchHit> = heap
        .into_iter()
        .map(|hit| hit.0)
        .collect();

    hits.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(Ordering::Equal)
            .then_with(|| a.doc_id.cmp(&b.doc_id))
    });

    hits
}
