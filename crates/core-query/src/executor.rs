use std::{
    cmp::Ordering,
    collections::{BinaryHeap, HashMap, HashSet},
    u32,
};

use core_index::{
    analyzer::analyzer::Analyzer,
    document::{IndexPolicy, policy::IndexKind},
    posting::{
        PostingList,
        ops::{intersection, union},
    },
    search::{SearchIndex, SearchStats},
    types::{DocId, XPathId},
};
use core_protocol::errors::CorelamoError;

use crate::{
    ScoredPosting, SearchHit, TopHit, ast::Query, planner::QueryPlan,
    query_string_parser::parse_and_analyze,
};

// Turns the AST into a PostingList or SearchHit
pub struct QueryExecutor<'a, I>
where
    I: SearchIndex + SearchStats,
{
    // Which index are we searching?
    index: &'a I,

    // A query is analyzed also the same way as the index, so we could filter out words, stemming,
    // whaatever
    analyzer: &'a Analyzer,
}

impl<'a, I> QueryExecutor<'a, I>
where
    I: SearchIndex + SearchStats,
{
    pub fn new(index: &'a I, analyzer: &'a Analyzer) -> Self {
        Self { index, analyzer }
    }

    fn execute_optional(&self, query: &Query, xpath: XPathId) -> Option<PostingList> {
        match query {
            Query::Term(term) => self.execute_term(term, xpath),
            Query::Prefix(prefix) => self.execute_prefix(prefix, xpath),
            Query::Wildcard(pattern) => Some(self.execute_wildcard(pattern, xpath)),
            Query::And(parts) => self.execute_and(parts, xpath),
            Query::Or(parts) => self.execute_or(parts, xpath),
            Query::Phrase(terms) => self.execute_phrase_optional(terms, xpath),
        }
    }

    pub fn execute(&self, query: &Query, xpath: XPathId) -> PostingList {
        self.execute_optional(query, xpath).unwrap_or_default()
    }

    // Query a term
    fn execute_term(&self, term: &str, xpath: XPathId) -> Option<PostingList> {
        let analyzed = self.analyzer.analyze(term);
        let token = analyzed.first()?;

        Some(self.index.lookup(&token.text, xpath))
    }

    // Prefix query, so for example if we do dat* would find database etc.
    fn execute_prefix(&self, prefix: &str, xpath: XPathId) -> Option<PostingList> {
        let analyzed = self.analyzer.analyze(prefix);
        let token = analyzed.first()?;

        Some(self.index.lookup_prefix(&token.text, xpath))
    }

    // Wildcard query, for now, we are not analyzing this, might change later
    fn execute_wildcard(&self, pattern: &str, xpath: XPathId) -> PostingList {
        let pattern = core_index::wildcard::WildcardPattern::parse(pattern);
        self.index.lookup_wildcard(&pattern, xpath)
    }

    // Boolean AND logic
    // 1. execute all child queries
    // 2. if any query returns nothing it drops entire search
    // 3. sorts the posting lists by shortest first
    // 4. Intersects progressively
    fn execute_and(&self, parts: &[Query], xpath: XPathId) -> Option<PostingList> {
        let mut lists = Vec::new();

        for part in parts {
            let Some(list) = self.execute_optional(part, xpath) else {
                continue;
            };

            if list.is_empty() {
                return Some(PostingList::default());
            }

            lists.push(list);
        }

        if lists.is_empty() {
            return None;
        }

        lists.sort_by_key(|list| list.len());

        let mut iter = lists.into_iter();
        let mut result = iter.next().unwrap();

        for next in iter {
            result = intersection(&result, &next);
        }

        Some(result)
    }

    // Boolean OR
    // Executes every child and unions that into a response
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
    fn execute_phrase(&self, terms: &[String], xpath: XPathId) -> PostingList {
        use core_index::posting::Posting;

        if terms.is_empty() {
            return PostingList::default();
        }

        let analyzed_terms: Vec<String> = terms
            .iter()
            .filter_map(|term| self.analyzer.analyze(term).first().map(|t| t.text.clone()))
            .collect();

        if analyzed_terms.len() != terms.len() {
            return PostingList::default();
        }

        let lists: Vec<PostingList> = analyzed_terms
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

    fn execute_phrase_optional(&self, terms: &[String], xpath: XPathId) -> Option<PostingList> {
        if terms.is_empty() {
            return None;
        }

        Some(self.execute_phrase(terms, xpath))
    }

    // Full search in the entire database
    pub fn search(&self, query: &Query, xpath: XPathId) -> Vec<SearchHit> {
        let scored = self.execute_scored(query, xpath);

        let mut hits: Vec<SearchHit> = scored
            .into_iter()
            .map(|p| {
                let score = p.score as f32 / 1000.0 * p.density;

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
    pub fn search_top_k(&self, query: &Query, xpath: XPathId, k: usize) -> Vec<SearchHit> {
        if k == 0 {
            return Vec::new();
        }

        let scored = self.execute_scored(query, xpath);
        let mut heap: BinaryHeap<TopHit> = BinaryHeap::with_capacity(k + 1);

        for p in scored {
            let score = p.score as f32 / 1000.0 * p.density;

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

        let mut hits: Vec<SearchHit> = heap.into_iter().map(|hit| hit.0).collect();

        hits.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(Ordering::Equal)
                .then_with(|| a.doc_id.cmp(&b.doc_id))
        });

        hits
    }

    pub fn search_plan_top_k(&self, plan: &QueryPlan, xpath: XPathId, k: usize) -> Vec<SearchHit> {
        if k == 0 {
            return Vec::new();
        }

        let candidate_k = (k * 50).max(100).min(2_000);

        let mut by_doc: HashMap<u64, SearchHit> = self
            .search_top_k(&plan.retrieval, xpath, candidate_k)
            .into_iter()
            .map(|hit| (hit.doc_id, hit))
            .collect();

        if by_doc.is_empty() {
            return Vec::new();
        }

        for signal in &plan.signals {
            let postings = self.execute(&signal.query, xpath);

            if postings.is_empty() {
                if signal.required {
                    return Vec::new();
                }
                continue;
            }

            if signal.required {
                by_doc.retain(|doc_id, _| {
                    postings
                        .items()
                        .binary_search_by_key(doc_id, |p| p.doc_id)
                        .is_ok()
                });
            }

            for posting in postings.items() {
                let Some(existing) = by_doc.get_mut(&posting.doc_id) else {
                    continue;
                };

                let signal_score =
                    (posting.weight as f32) * signal.boost * (1.0 + posting.positions.len() as f32);

                existing.score += signal_score;
                existing.weight_sum = existing
                    .weight_sum
                    .saturating_add(signal_score.max(0.0) as u32);
                existing.matched_terms += 1;
            }
        }

        top_k_from_hits(by_doc.into_values(), k)
    }

    pub fn search_plan_all_xpaths_top_k(
        &self,
        plan: &QueryPlan,
        xpaths: impl IntoIterator<Item = XPathId>,
        k: usize,
    ) -> Vec<SearchHit> {
        if k == 0 {
            return Vec::new();
        }

        let xpaths: Vec<_> = xpaths.into_iter().collect();
        let candidate_k = (k * 50).max(100).min(2_000);

        let mut by_doc = HashMap::<DocId, SearchHit>::new();

        // One retrieval phase across all fields.
        for xpath in &xpaths {
            for hit in self.search_top_k(&plan.retrieval, *xpath, candidate_k) {
                by_doc
                    .entry(hit.doc_id)
                    .and_modify(|existing| {
                        existing.matched_terms += hit.matched_terms;
                        existing.weight_sum = existing.weight_sum.saturating_add(hit.weight_sum);
                        existing.distance_factor =
                            existing.distance_factor.max(hit.distance_factor);
                        existing.score += hit.score;
                    })
                    .or_insert(hit);
            }
        }

        if by_doc.is_empty() {
            return Vec::new();
        }

        // Rerank only candidate docs.
        for signal in &plan.signals {
            let mut required_seen = std::collections::HashSet::new();

            for xpath in &xpaths {
                let postings = self.execute(&signal.query, *xpath);

                if postings.is_empty() {
                    continue;
                }

                for posting in postings.items() {
                    let Some(existing) = by_doc.get_mut(&posting.doc_id) else {
                        continue;
                    };

                    required_seen.insert(posting.doc_id);

                    let signal_score = posting.weight as f32
                        * signal.boost
                        * (1.0 + posting.positions.len() as f32);

                    existing.score += signal_score;
                    existing.weight_sum = existing
                        .weight_sum
                        .saturating_add(signal_score.max(0.0) as u32);
                    existing.matched_terms += 1;
                }
            }

            if signal.required {
                by_doc.retain(|doc_id, _| required_seen.contains(doc_id));
                if by_doc.is_empty() {
                    return Vec::new();
                }
            }
        }

        top_k_from_hits(by_doc.into_values(), k)
    }

    // INFO: Currently we might want to think about other ways of implementing the idea
    // of searching within all xpaths, cause it still happens independently.
    pub fn search_all_xpaths_top_k(
        &self,
        query: &Query,
        xpaths: impl IntoIterator<Item = XPathId>,
        k: usize,
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
                        existing.distance_factor =
                            existing.distance_factor.max(hit.distance_factor);
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

        let mut hits: Vec<SearchHit> = heap.into_iter().map(|hit| hit.0).collect();

        hits.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(Ordering::Equal)
                .then_with(|| a.doc_id.cmp(&b.doc_id))
        });

        hits
    }

    pub fn resolve_filters(
        &self,
        filters: &HashMap<String, String>,
        policy: &IndexPolicy,
    ) -> Result<Option<HashSet<DocId>>, CorelamoError> {
        if filters.is_empty() {
            return Ok(None);
        }

        let mut restrict: Option<HashSet<DocId>> = None;

        for (field_name, term) in filters {
            if term.trim().is_empty() {
                continue;
            }

            let field = policy
                .fields
                .iter()
                .find(|f| &f.name == field_name)
                .filter(|f| f.index == IndexKind::Text)
                .ok_or_else(|| CorelamoError::PathNotIndexed(field_name.clone()))?;

            let matched: HashSet<DocId> = match parse_and_analyze(term, self.analyzer)? {
                Some(query) => self
                    .execute(&query, field.xpath)
                    .items()
                    .iter()
                    .map(|p| p.doc_id)
                    .collect(),
                None => HashSet::new(),
            };

            restrict = Some(match restrict {
                Some(current) => current.intersection(&matched).copied().collect(),
                None => matched,
            });

            if restrict.as_ref().is_some_and(|s| s.is_empty()) {
                return Ok(restrict);
            }
        }

        Ok(restrict)
    }

    pub fn search_top_k_restricted(
        &self,
        query: &Query,
        xpath: XPathId,
        k: usize,
        restrict: Option<&HashSet<DocId>>,
    ) -> Vec<SearchHit> {
        if k == 0 {
            return Vec::new();
        }
        if restrict.is_some_and(|s| s.is_empty()) {
            return Vec::new();
        }

        let scored = self.execute_scored(query, xpath);
        let mut heap: BinaryHeap<TopHit> = BinaryHeap::with_capacity(k + 1);

        for p in scored {
            if let Some(allowed) = restrict {
                if !allowed.contains(&p.doc_id) {
                    continue;
                }
            }

            let score = p.score as f32 / 1000.0 * p.density;
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

        let mut hits: Vec<SearchHit> = heap.into_iter().map(|hit| hit.0).collect();
        hits.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(Ordering::Equal)
                .then_with(|| a.doc_id.cmp(&b.doc_id))
        });
        hits
    }

    pub fn search_all_xpaths_top_k_restricted(
        &self,
        query: Option<&Query>,
        xpaths: impl IntoIterator<Item = XPathId>,
        k: usize,
        restrict: Option<&HashSet<DocId>>,
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
            for hit in self.search_top_k_restricted(query, xpath, k, restrict) {
                by_doc
                    .entry(hit.doc_id)
                    .and_modify(|existing| {
                        existing.matched_terms += hit.matched_terms;
                        existing.weight_sum += hit.weight_sum;
                        existing.distance_factor =
                            existing.distance_factor.max(hit.distance_factor);
                        existing.score += hit.score;
                    })
                    .or_insert(hit);
            }
        }

        top_k_from_hits(by_doc.into_values(), k)
    }

    // Search the entire database all xpaths
    pub fn search_all_xpaths(
        &self,
        query: &Query,
        xpaths: impl IntoIterator<Item = XPathId>,
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
                        existing.distance_factor =
                            existing.distance_factor.max(hit.distance_factor);
                        existing.score += hit.score;
                    })
                    .or_insert(hit);
            }
        }

        by_doc.into_values().collect()
    }

    // Converts a query
    fn execute_scored(&self, query: &Query, xpath: XPathId) -> Vec<ScoredPosting> {
        match query {
            Query::Term(term) => {
                let postings = self.execute_term(term, xpath).unwrap_or_default();
                crate::scorer::score_term_hybrid(self.index, &postings, xpath)
            }
            Query::And(parts) => self.execute_scored_and(parts, xpath),
            _ => {
                let postings = self.execute(query, xpath);
                crate::scorer::score_term_hybrid(self.index, &postings, xpath)
            }
        }
    }

    fn execute_scored_and(&self, parts: &[Query], xpath: XPathId) -> Vec<ScoredPosting> {
        let mut lists = Vec::new();

        for part in parts {
            let Some(postings) = self.execute_optional(part, xpath) else {
                continue;
            };

            if postings.is_empty() {
                return Vec::new();
            }

            lists.push(postings);
        }

        if lists.is_empty() {
            return Vec::new();
        }

        lists.sort_by_key(|postings| postings.len());

        let mut iter = lists.into_iter();
        let first = iter.next().unwrap();

        let mut result = crate::scorer::score_term_hybrid(self.index, &first, xpath);

        for postings in iter {
            result = crate::scorer::scored_and(&result, &postings);
        }

        result
    }
}

fn phrase_matches(position_lists: &[&[u32]]) -> bool {
    if position_lists.is_empty() {
        return false;
    }

    for &start in position_lists[0] {
        let mut matched = true;
        for (offset, positions) in position_lists.iter().enumerate().skip(1) {
            let expected = start + offset as u32;

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

fn top_k_from_hits(hits: impl IntoIterator<Item = SearchHit>, k: usize) -> Vec<SearchHit> {
    let mut heap: BinaryHeap<TopHit> = BinaryHeap::with_capacity(k + 1);

    for hit in hits {
        heap.push(TopHit(hit));

        if heap.len() > k {
            heap.pop();
        }
    }

    let mut hits: Vec<SearchHit> = heap.into_iter().map(|hit| hit.0).collect();

    hits.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(Ordering::Equal)
            .then_with(|| a.doc_id.cmp(&b.doc_id))
    });

    hits
}
