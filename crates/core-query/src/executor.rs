use std::{
    cmp::Ordering,
    collections::{BinaryHeap, HashMap},
    u32,
};

use core_index::{
    analyzer::analyzer::Analyzer,
    posting::{
        ops::{intersection, union},
        PostingList,
    },
    search::SearchIndex,
    types::XPathId,
};

use crate::{ast::Query, ScoredPosting, SearchHit, TopHit};

pub struct QueryExecutor<'a, I: SearchIndex> {
    index: &'a I,
    analyzer: &'a Analyzer,
}

impl<'a, I: SearchIndex> QueryExecutor<'a, I> {
    pub fn new(index: &'a I, analyzer: &'a Analyzer) -> Self {
        Self { index, analyzer }
    }

    pub fn execute(&self, query: &Query, xpath: XPathId) -> PostingList {
        match query {
            Query::Term(term) => self.execute_term(term, xpath),
            Query::Prefix(prefix) => self.execute_prefix(prefix, xpath),
            Query::Wildcard(pattern) => self.execute_wildcard(pattern, xpath),
            Query::And(parts) => self.execute_and(parts, xpath),
            Query::Or(parts) => self.execute_or(parts, xpath),
            Query::Phrase(terms) => self.execute_phrase(terms, xpath),
        }
    }

    fn execute_term(&self, term: &str, xpath: XPathId) -> PostingList {
        let analyzed = self.analyzer.analyze(term);

        let Some(token) = analyzed.first() else {
            return PostingList::default();
        };

        self.index.lookup(&token.text, xpath)
    }

    fn execute_prefix(&self, prefix: &str, xpath: XPathId) -> PostingList {
        let analyzed = self.analyzer.analyze(prefix);

        let Some(token) = analyzed.first() else {
            return PostingList::default();
        };

        self.index.lookup_prefix(&token.text, xpath)
    }

    fn execute_wildcard(&self, pattern: &str, xpath: XPathId) -> PostingList {
        let pattern = core_index::wildcard::WildcardPattern::parse(pattern);
        self.index.lookup_wildcard(&pattern, xpath)
    }

    fn execute_and(&self, parts: &[Query], xpath: XPathId) -> PostingList {
        if parts.is_empty() {
            return PostingList::default();
        }

        let mut lists: Vec<PostingList> =
            parts.iter().map(|part| self.execute(part, xpath)).collect();

        if lists.iter().any(|list| list.is_empty()) {
            return PostingList::default();
        }

        lists.sort_by_key(|list| list.len());

        let mut iter = lists.into_iter();
        let mut result = iter.next().unwrap();

        for next in iter {
            result = intersection(&result, &next);
        }

        result
    }

    fn execute_or(&self, parts: &[Query], xpath: XPathId) -> PostingList {
        let mut result = PostingList::default();

        for part in parts {
            let next = self.execute(part, xpath);
            result = union(&result, &next);
        }

        result
    }

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

    pub fn search(&self, query: &Query, xpath: XPathId) -> Vec<SearchHit> {
        let scored = self.execute_scored(query, xpath);

        let mut hits: Vec<SearchHit> = scored
            .into_iter()
            .map(|p| {
                let score = p.weight_sum as f32 * p.density;

                SearchHit {
                    doc_id: p.doc_id,
                    matched_terms: p.matched_terms,
                    weight_sum: p.weight_sum,
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

    pub fn search_top_k(&self, query: &Query, xpath: XPathId, k: usize) -> Vec<SearchHit> {
        if k == 0 {
            return Vec::new();
        }

        let scored = self.execute_scored(query, xpath);
        let mut heap: BinaryHeap<TopHit> = BinaryHeap::with_capacity(k + 1);

        for p in scored {
            let score = p.weight_sum as f32 * p.density;

            let hit = SearchHit {
                doc_id: p.doc_id,
                matched_terms: p.matched_terms,
                weight_sum: p.weight_sum,
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

        let mut by_doc = HashMap::<u64, SearchHit>::new();

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

    pub fn search_all_xpaths(
        &self,
        query: &Query,
        xpaths: impl IntoIterator<Item = XPathId>,
    ) -> Vec<SearchHit> {
        use std::collections::BTreeMap;

        let mut by_doc = BTreeMap::<u64, SearchHit>::new();

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

    fn execute_scored(&self, query: &Query, xpath: XPathId) -> Vec<ScoredPosting> {
        match query {
            Query::Term(term) => {
                let postings = self.execute_term(term, xpath);
                crate::scorer::score_term(&postings)
            }
            Query::And(parts) => self.execute_scored_and(parts, xpath),
            _ => {
                let postings = self.execute(query, xpath);
                crate::scorer::score_term(&postings)
            }
        }
    }

    fn execute_scored_and(&self, parts: &[Query], xpath: XPathId) -> Vec<ScoredPosting> {
        if parts.is_empty() {
            return Vec::new();
        }

        let mut lists: Vec<PostingList> =
            parts.iter().map(|part| self.execute(part, xpath)).collect();

        if lists.iter().any(|postings| postings.is_empty()) {
            return Vec::new();
        }

        lists.sort_by_key(|postings| postings.len());

        let mut iter = lists.into_iter();
        let first = iter.next().unwrap();

        let mut result = crate::scorer::score_term(&first);

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

            if !positions.binary_search(&expected).is_ok() {
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
