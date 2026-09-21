use core_index::{
    analyzer::analyzer::Analyzer,
    fuzzy::{FuzzyExpansion, FuzzyOptions, FuzzySpec},
    posting::{
        Posting, PostingList,
        ops::{intersect_ids, intersection, restrict_to, union},
    },
    search::{SearchColumns, SearchIndex, SearchStats, TermPostings},
    types::{DocId, XPathId},
};
use std::{
    cmp::Ordering,
    collections::{BinaryHeap, HashMap, HashSet, hash_map::Entry},
    u32,
};

use core_protocol::command_reponse_definitions::Fuzziness;
use core_timing::timed;

use crate::{
    ScoredPosting, SearchHit, TopHit,
    ast::Query,
    resolver::MatchOp,
    scorer::{fuzzy_decay, score_term_hybrid, score_term_into},
    wand::{WandHit, conjunctive_top_k, wand_top_k},
};

#[derive(Debug, Clone)]
pub struct WordSuggestions {
    pub word: String,
    pub suggestions: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct DidYouMeanReport {
    pub original: String,
    //best possible correction
    pub corrected: String,
    pub words: Vec<WordSuggestions>,
}

impl DidYouMeanReport {
    pub fn empty(original: &str) -> Self {
        DidYouMeanReport {
            original: original.to_string(),
            corrected: "".to_string(),
            words: Vec::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct FieldFilter {
    pub xpath: XPathId,
    pub kind: MatchOp,
}

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
    I: SearchIndex + SearchStats + SearchColumns,
{
    pub fn new(index: &'a I, analyzer: &'a Analyzer) -> Self {
        Self { index, analyzer }
    }

    #[timed(search)]
    fn fetch_term_postings(&self, terms: &[&str], xpath: XPathId) -> Vec<TermPostings> {
        terms
            .iter()
            .map(|term| self.index.lookup_term(term, xpath))
            .collect()
    }

    fn execute_top_k_retrieval(
        &self,
        query: &Query,
        xpath: XPathId,
        k: usize,
        restrict: Option<&HashSet<DocId>>,
    ) -> Option<Vec<WandHit>> {
        match query {
            Query::Term(term) => {
                let terms = [term.as_str()];
                let postings = self.fetch_term_postings(&terms, xpath);

                Some(wand_top_k(self.index, xpath, &postings, k, restrict))
            }

            Query::Search(parts) if parts.iter().all(|part| matches!(part, Query::Term(_))) => {
                let terms: Vec<&str> = parts
                    .iter()
                    .filter_map(|part| match part {
                        Query::Term(term) => Some(term.as_str()),
                        _ => None,
                    })
                    .collect();

                let postings = self.fetch_term_postings(&terms, xpath);

                Some(wand_top_k(self.index, xpath, &postings, k, restrict))
            }

            Query::Or(parts) if parts.iter().all(|part| matches!(part, Query::Term(_))) => {
                let terms: Vec<&str> = parts
                    .iter()
                    .filter_map(|part| match part {
                        Query::Term(term) => Some(term.as_str()),
                        _ => None,
                    })
                    .collect();

                let postings = self.fetch_term_postings(&terms, xpath);

                Some(conjunctive_top_k(self.index, xpath, &postings, k, restrict))
            }

            Query::And(parts) if parts.iter().all(|part| matches!(part, Query::Term(_))) => {
                let terms: Vec<&str> = parts
                    .iter()
                    .filter_map(|part| match part {
                        Query::Term(term) => Some(term.as_str()),
                        _ => None,
                    })
                    .collect();

                let postings = self.fetch_term_postings(&terms, xpath);

                Some(conjunctive_top_k(self.index, xpath, &postings, k, restrict))
            }

            _ => None,
        }
    }

    //Vai dokuments vispar der querijam
    #[timed(search)]
    fn execute_optional(&self, query: &Query, xpath: XPathId) -> Option<PostingList> {
        match query {
            Query::Term(term) => self.execute_term(term, xpath),
            Query::Prefix(prefix) => self.execute_prefix(prefix, xpath),
            Query::Wildcard(pattern) => Some(self.execute_wildcard(pattern, xpath)),

            Query::Search(parts) => self.execute_or(parts, xpath),

            Query::And(parts) => self.execute_and(parts, xpath),
            Query::Or(parts) => self.execute_or(parts, xpath),
            Query::Phrase(terms) => self.execute_phrase_optional(terms, xpath),
            Query::Exact(term) => Some(self.execute_exact(term, xpath)),
            Query::Fuzzy(term, fuzziness, spec) => {
                Some(self.execute_fuzzy(term, xpath, *fuzziness, *spec))
            }
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

    //Kindof the fuzzy entry point the whole porno logic starts here
    #[timed(search)]
    fn execute_fuzzy(
        &self,
        raw: &str,
        xpath: XPathId,
        fuzziness: Fuzziness,
        spec: FuzzySpec,
    ) -> PostingList {
        //"butman and robin" -> [butman, robin]
        let words = self.fuzzy_words(raw);

        if words.is_empty() {
            return PostingList::default();
        }

        let lists: Vec<PostingList> = words
            .iter()
            .map(|w| {
                self.index
                    .lookup_fuzzy(w, xpath, fuzzy_options(w, fuzziness, spec))
            })
            .collect();

        let mut iter = lists.into_iter();
        let mut result = iter.next().unwrap_or_default();
        for next in iter {
            result = intersection(&result, &next);
        }
        result
    }

    //Splits the string into fuzzable words split by white space and lowercased
    #[timed(search)]
    fn fuzzy_words(&self, raw: &str) -> Vec<String> {
        fuzzable_words(self.analyzer, raw)
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

    // TODO: THIS IS OLD WE TEST FIRST
    // #[timed(search)]
    // pub fn search_top_k(&self, query: &Query, xpath: XPathId, k: usize) -> Vec<SearchHit> {
    //     if k == 0 {
    //         return Vec::new();
    //     }
    //
    //     let scored = self.execute_scored(query, xpath);
    //     let mut heap: BinaryHeap<TopHit> = BinaryHeap::with_capacity(k + 1);
    //
    //     for p in scored {
    //         let score = ((p.score as f32) / 1000.0) * p.density;
    //
    //         let hit = SearchHit {
    //             doc_id: p.doc_id,
    //             matched_terms: p.matched_terms,
    //             weight_sum: (p.score / 1000).min(u32::MAX as u64) as u32,
    //             distance_factor: p.density,
    //             score,
    //         };
    //
    //         heap.push(TopHit(hit));
    //
    //         if heap.len() > k {
    //             heap.pop();
    //         }
    //     }
    //
    //     let mut hits: Vec<SearchHit> = heap.into_iter().map(|hit| hit.0).collect();
    //
    //     hits.sort_by(|a, b| {
    //         b.score
    //             .partial_cmp(&a.score)
    //             .unwrap_or(Ordering::Equal)
    //             .then_with(|| a.doc_id.cmp(&b.doc_id))
    //     });
    //
    //     hits
    // }

    pub fn search_top_k(&self, query: &Query, xpath: XPathId, k: usize) -> Vec<SearchHit> {
        self.search_top_k_restricted(query, xpath, k, None)
    }

    fn search_top_k_restricted(
        &self,
        query: &Query,
        xpath: XPathId,
        k: usize,
        restrict: Option<&HashSet<DocId>>,
    ) -> Vec<SearchHit> {
        if k == 0 || restrict.is_some_and(|docs| docs.is_empty()) {
            return Vec::new();
        }

        if let Some(hits) = self.execute_top_k_retrieval(query, xpath, k, restrict) {
            return hits.into_iter().map(wand_hit_to_search_hit).collect();
        }

        // Complex-query fallback.
        let scored = self.execute_scored(query, xpath);

        let hits = scored
            .into_iter()
            .filter(|p| restrict.is_none_or(|allowed| allowed.contains(&p.doc_id)))
            .map(|p| SearchHit {
                doc_id: p.doc_id,
                matched_terms: p.matched_terms,
                weight_sum: (p.score / 1000).min(u32::MAX as u64) as u32,
                distance_factor: p.density,
                score: ((p.score as f32) / 1000.0) * p.density,
            });

        top_k_from_hits(hits, k)
    }

    // INFO: Currently we might want to think about other ways of implementing the idea
    // of searching within all xpaths, cause it still happens independently.
    #[timed(search)]
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

    #[timed(search)]
    pub fn filter_doc_ids(&self, filters: &HashMap<String, FieldFilter>) -> Option<HashSet<DocId>> {
        if filters.is_empty() {
            return None;
        }

        let mut restrict: Option<HashSet<DocId>> = None;

        for filter in filters.values() {
            let matched: HashSet<DocId> = match &filter.kind {
                MatchOp::Query(query) => match query {
                    Some(query) => self
                        .execute(query, filter.xpath)
                        .items()
                        .iter()
                        .map(|p| p.doc_id)
                        .collect(),
                    None => HashSet::new(),
                },
                MatchOp::Range { lo, hi } => self
                    .index
                    .column_range(filter.xpath, *lo, *hi)
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
        restrict: Option<&HashSet<DocId>>,
    ) -> Vec<SearchHit> {
        if k == 0 || restrict.is_some_and(|docs| docs.is_empty()) {
            return Vec::new();
        }

        let Some(query) = query else {
            return match restrict {
                Some(allowed) => {
                    let hits = allowed.iter().copied().map(|doc_id| SearchHit {
                        doc_id,
                        matched_terms: 0,
                        weight_sum: 0,
                        distance_factor: 1.0,
                        score: 0.0,
                    });

                    top_k_from_hits(hits, k)
                }

                None => Vec::new(),
            };
        };

        let xpaths: Vec<XPathId> = xpaths.into_iter().collect();

        if xpaths.is_empty() {
            return Vec::new();
        }

        if xpaths.len() == 1 {
            return self.search_top_k_restricted(query, xpaths[0], k, restrict);
        }

        let mut by_doc = HashMap::<DocId, SearchHit>::new();

        for xpath in xpaths {
            for hit in self.search_top_k_restricted(query, xpath, k, restrict) {
                by_doc
                    .entry(hit.doc_id)
                    .and_modify(|existing| {
                        existing.matched_terms =
                            existing.matched_terms.saturating_add(hit.matched_terms);

                        existing.weight_sum = existing.weight_sum.saturating_add(hit.weight_sum);

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
    #[timed(search)]
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

    //vai der + relevance
    fn execute_scored(&self, query: &Query, xpath: XPathId) -> Vec<ScoredPosting> {
        match query {
            Query::Term(term) => {
                let postings = self.execute_term(term, xpath).unwrap_or_default();
                let doc_freq = postings.len() as f32;

                score_term_hybrid(self.index, &postings, xpath, doc_freq)
            }

            Query::Search(parts) => self.execute_scored_search(parts, xpath),

            Query::And(parts) => self.execute_scored_and(parts, xpath),

            Query::Fuzzy(raw, fuzziness, spec) => {
                self.execute_scored_fuzzy(raw, xpath, *fuzziness, *spec)
            }

            _ => {
                let postings = self.execute(query, xpath);
                let true_df = postings.len() as f32;
                score_term_hybrid(self.index, &postings, xpath, true_df)
            }
        }
    }

    //INFO: Norca sito hujnu centaas izprast kkur 1h, seit visam ir jabut safe, ne passaprotami,
    //bet safe robezaas

    //optimizations:
    //      Posting:
    //    pub positions: SmallVec<[Position; INLINE_POSITIONS]>,
    #[timed(search)]
    fn execute_scored_search(&self, parts: &[Query], xpath: XPathId) -> Vec<ScoredPosting> {
        let mut by_doc = HashMap::<DocId, ScoredPosting>::new();

        for part in parts {
            let Query::Term(term) = part else {
                // INFO: Well support complex Search children separately
                continue;
            };

            let postings = self.execute_term(term, xpath).unwrap_or_default();

            let doc_freq = postings.len() as f32;

            let scored = score_term_hybrid(self.index, &postings, xpath, doc_freq);

            for hit in scored {
                match by_doc.entry(hit.doc_id) {
                    Entry::Vacant(entry) => {
                        entry.insert(hit);
                    }

                    Entry::Occupied(mut entry) => {
                        let existing = entry.get_mut();

                        existing.score = existing.score.saturating_add(hit.score);

                        existing.matched_terms =
                            existing.matched_terms.saturating_add(hit.matched_terms);
                    }
                }
            }
        }

        by_doc.into_values().collect()
    }

    #[timed(search)]
    fn execute_scored_fuzzy(
        &self,
        raw: &str,
        xpath: XPathId,
        fuzziness: Fuzziness,
        spec: FuzzySpec,
    ) -> Vec<ScoredPosting> {
        //"butman and robin" -> "butman" "robin"
        let words = self.fuzzy_words(raw);

        if words.is_empty() {
            return Vec::new();
        }

        //blad ja kaads iedeva 64 vardu queriju mums overflow notiks fuck that shit fuzzy taapat
        //buutu par leenu
        if words.len() >= 64 {
            return Vec::new();
        }

        // doc_id -> (scored_hit + a 0000000011 number that represents how many words matched in the
        // query with X changes)
        let mut acc: HashMap<DocId, (ScoredPosting, u64)> = HashMap::new();

        //caching look-up-ed values since its the slow part cause some fuzzed expansions could lead
        //to a different document
        let mut scored_buf: Vec<ScoredPosting> = Vec::new();
        let mut doc_len: HashMap<DocId, f32> = HashMap::new();

        for (word_index, word) in words.iter().enumerate() {
            //how many edits for this word
            let opts = fuzzy_options(word, fuzziness, spec);
            let bit = 1u64 << word_index;

            //guess words based on max edit count
            let mut expansions = self.index.fuzzy_expansions(word, xpath, opts);
            //best guesses for the words
            rank_and_cap(&mut expansions, spec.max_expansions);

            //iterate all possible hits
            for expansion in expansions {
                //                      the long function needs wand kip
                let postings = self.index.lookup(&expansion.term, xpath);

                scored_buf.clear();
                let true_df = self.index.doc_freq(&expansion.term, xpath) as f32;
                //BM25 scoring
                score_term_into(
                    self.index,
                    &postings,
                    xpath,
                    true_df,
                    &mut doc_len,
                    &mut scored_buf,
                );

                let decay = fuzzy_decay(expansion.edits);

                for mut scored in scored_buf.drain(..) {
                    //apply the edit decay to the score
                    scored.score = ((scored.score as f32) * decay) as u64;

                    match acc.entry(scored.doc_id) {
                        Entry::Occupied(mut slot) => {
                            let (hit, mask) = slot.get_mut();

                            if scored.score > hit.score {
                                hit.positions = scored.positions.clone();
                                hit.density = hit.density.max(scored.density);
                            }

                            hit.score = hit.score.saturating_add(scored.score);
                            hit.matched_terms = hit.matched_terms.saturating_add(1);
                            //the first word got a hit, so the next one just "adds" 1 or << 1
                            //basically
                            *mask |= bit;
                        }
                        Entry::Vacant(slot) => {
                            slot.insert((scored, bit));
                        }
                    }
                }
            }
        }
        //                          number of "1" is the query_word_count \/
        //remember a few lines above? this shit is basically the 000000011111111
        let all_words = (1u64 << words.len()) - 1;

        acc.into_values()
            .filter(|(_, mask)| *mask == all_words)
            .map(|(hit, _)| hit)
            .collect()
    }

    #[timed(search)]
    fn execute_scored_and(&self, parts: &[Query], xpath: XPathId) -> Vec<ScoredPosting> {
        struct TermFetch<'a> {
            term: Option<&'a str>,
            postings: PostingList,
            doc_freq: u32,
        }

        let mut fetched: Vec<TermFetch> = Vec::with_capacity(parts.len());

        for part in parts {
            let (term, postings, doc_freq) = match part {
                Query::Term(term) => {
                    let postings = self.execute_term(term, xpath).unwrap_or_default();

                    let doc_freq = postings.len() as u32;

                    (Some(term.as_str()), postings, doc_freq)
                }

                _ => {
                    let postings = self.execute_optional(part, xpath).unwrap_or_default();
                    (None, postings, u32::MAX)
                }
            };

            if postings.is_empty() {
                return Vec::new();
            }

            fetched.push(TermFetch {
                term,
                postings,
                doc_freq,
            });
        }

        if fetched.is_empty() {
            return Vec::new();
        }

        fetched.sort_by_key(|f| f.postings.len());

        let mut candidates: Vec<DocId> = fetched[0]
            .postings
            .items()
            .iter()
            .map(|posting| posting.doc_id)
            .collect();

        for f in &fetched[1..] {
            candidates = intersect_ids(&candidates, &f.postings);

            if candidates.is_empty() {
                return Vec::new();
            }
        }

        let mut by_doc = HashMap::<DocId, ScoredPosting>::with_capacity(candidates.len());

        for f in &fetched {
            if f.term.is_none() {
                continue;
            }

            let restricted = restrict_to(&f.postings, &candidates);

            if restricted.is_empty() {
                continue;
            }

            let scored = score_term_hybrid(self.index, &restricted, xpath, f.doc_freq as f32);

            for hit in scored {
                match by_doc.entry(hit.doc_id) {
                    Entry::Vacant(entry) => {
                        entry.insert(hit);
                    }

                    Entry::Occupied(mut entry) => {
                        let existing = entry.get_mut();

                        existing.score = existing.score.saturating_add(hit.score);

                        existing.matched_terms =
                            existing.matched_terms.saturating_add(hit.matched_terms);
                    }
                }
            }
        }

        by_doc.into_values().collect()
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

    let mut hits: Vec<SearchHit> = heap.into_iter().map(|hit| hit.0).collect();

    hits.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(Ordering::Equal)
            .then_with(|| a.doc_id.cmp(&b.doc_id))
    });

    hits
}

//derives how much a word can be fuzzed basically
pub fn fuzzy_options(word: &str, fuzziness: Fuzziness, spec: FuzzySpec) -> FuzzyOptions {
    FuzzyOptions {
        max_edits: fuzziness.resolve(word),
        prefix_length: spec.prefix_length,
        max_expansions: spec.max_expansions,
    }
}

pub fn rank_and_cap(expansions: &mut Vec<FuzzyExpansion>, max_expansions: usize) {
    expansions.sort_by(|a, b| {
        a.edits
            //edit count
            .cmp(&b.edits)
            //the more documents this appeared in the better
            .then_with(|| b.doc_freq.cmp(&a.doc_freq))
            //alphabet 3000
            .then_with(|| a.term.cmp(&b.term))
    });

    //0 means "no cap", so callers have an escape hatch.
    if max_expansions > 0 {
        expansions.truncate(max_expansions);
    }
}

//fuzzable words for did_you_mean
pub fn fuzzable_words(analyzer: &Analyzer, raw: &str) -> Vec<String> {
    raw.split_whitespace()
        .filter_map(|w| analyzer.analyze_query(w).into_iter().next().map(|t| t.text))
        .collect()
}

fn wand_hit_to_search_hit(hit: WandHit) -> SearchHit {
    SearchHit {
        doc_id: hit.doc_id,
        matched_terms: hit.matched_terms,
        weight_sum: (hit.score / 1000).min(u32::MAX as u64) as u32,
        distance_factor: 1.0,
        score: hit.score as f32 / 1000.0,
    }
}

#[cfg(test)]
fn exhaustive_conjunctive_top_k<S: SearchStats>(
    stats: &S,
    xpath: XPathId,
    posting_lists: &[PostingList],
    doc_frequencies: &[u32],
    k: usize,
) -> Vec<WandHit> {
    use std::collections::HashMap;

    if k == 0 || posting_lists.is_empty() {
        return Vec::new();
    }

    let doc_count = stats.doc_count(xpath);
    let avg_doc_len = stats.avg_doc_len(xpath);

    let required_terms = posting_lists.len();

    let mut scores = HashMap::<DocId, (u64, usize)>::new();

    for (postings, &doc_frequency) in posting_lists.iter().zip(doc_frequencies) {
        for posting in postings.items() {
            use crate::scorer::bm25_score_scaled;

            let doc_len = stats
                .doc_len(posting.doc_id, xpath)
                .unwrap_or(avg_doc_len as u32);

            let contribution = bm25_score_scaled(
                posting.positions.len() as u32,
                posting.weight,
                doc_len,
                avg_doc_len,
                doc_count,
                doc_frequency,
            );

            let entry = scores.entry(posting.doc_id).or_insert((0, 0));

            entry.0 = entry.0.saturating_add(contribution);

            entry.1 += 1;
        }
    }

    let mut hits: Vec<WandHit> = scores
        .into_iter()
        .filter(|(_, (_, matched_terms))| *matched_terms == required_terms)
        .map(|(doc_id, (score, matched_terms))| WandHit {
            doc_id,
            score,
            matched_terms,
        })
        .collect();

    hits.sort_unstable_by(|a, b| b.score.cmp(&a.score).then_with(|| a.doc_id.cmp(&b.doc_id)));

    hits.truncate(k);

    hits
}
