use core_index::{
    analyzer::analyzer::Analyzer,
    fuzzy::{FuzzyExpansion, FuzzyOptions, FuzzySpec},
    posting::{Posting, PostingList, ops::intersection},
    search::{SearchIndex, SearchNumeric, SearchStats, TermPostings},
    types::{DocId, XPathId},
};
use std::{
    cmp::Ordering,
    collections::{BinaryHeap, HashMap, hash_map::Entry},
    u32,
};

use ahash::{HashSet, HashSetExt};

use core_protocol::command_reponse_definitions::Fuzziness;
use core_timing::timed;

use crate::{
    ScoredPosting, SearchHit, TopHit,
    ast::Query,
    resolver::{FieldCtx, FieldQuery, SameElementBinding},
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

//determining whether to filter or rank
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvalMode {
    //for pure filtering — return doc ids
    Filter,
    //for filtering+ranking — return scored postings
    Rank,
}

#[derive(Debug, Clone)]
pub enum EvalOutcome {
    Docs(HashSet<DocId>),
    Scored(Vec<ScoredPosting>),
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
    array_groups: Vec<Vec<FieldCtx>>,
}

impl<'a, I> QueryExecutor<'a, I>
where
    I: SearchIndex + SearchStats + SearchNumeric,
{
    pub fn new(
        index: &'a I,
        analyzer: &'a Analyzer,
        array_groups: Vec<Vec<(XPathId, Option<XPathId>)>>,
    ) -> Self {
        let array_groups = array_groups
            .into_iter()
            .map(|group| {
                group
                    .into_iter()
                    .map(|(xpath, exact_xpath)| FieldCtx {
                        xpath,
                        exact_xpath,
                        row_keyed: true,
                    })
                    .collect()
            })
            .collect();
        Self {
            index,
            analyzer,
            array_groups,
        }
    }

    #[timed(search)]
    fn fetch_term_postings(&self, terms: &[&str], xpath: XPathId) -> Vec<TermPostings> {
        terms
            .iter()
            .map(|term| self.index.lookup_term(term, xpath))
            .collect()
    }

    //WAND/conjunctive fast path for flat term lists (skips the general traversal)
    fn execute_top_k_retrieval(
        &self,
        query: &Query,
        xpath: XPathId,
        k: usize,
        restrict: Option<&HashSet<DocId>>,
    ) -> Option<Vec<WandHit>> {
        //disjunctive flat-term cases (Term / Wand / Or) -> additive WAND
        if let Some(terms) = additive_terms(query) {
            let postings = self.fetch_term_postings(&terms, xpath);
            return Some(wand_top_k(self.index, xpath, &postings, k, restrict));
        }

        //conjunctive flat-term case (And) -> conjunctive top-k
        if let Query::And(parts) = query {
            if parts.iter().all(|p| matches!(p, Query::Term(_))) {
                let terms: Vec<&str> = parts
                    .iter()
                    .filter_map(|p| match p {
                        Query::Term(t) => Some(t.as_str()),
                        _ => None,
                    })
                    .collect();
                let postings = self.fetch_term_postings(&terms, xpath);
                return Some(conjunctive_top_k(self.index, xpath, &postings, k, restrict));
            }
        }

        None
    }

    //Lookup a term
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

    // Wildcard query
    #[timed(search)]
    fn execute_wildcard(&self, pattern: &str, xpath: XPathId) -> PostingList {
        let pattern = core_index::wildcard::WildcardPattern::parse(pattern);
        self.index.lookup_wildcard(&pattern, xpath)
    }

    // Phrase query,
    // ["rust", "document"]
    // A document matches only if rust and database appear in it in order rust + database so
    // position and position + 1
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

        //incase someone did phrase + exact we can be forgiving and drop the ""
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

    fn ascend(&self, rows: HashSet<DocId>, levels: u32) -> HashSet<DocId> {
        let mut rows = rows;
        for _ in 0..levels {
            rows = rows
                .into_iter()
                .filter_map(|r| self.index.parent_of_row(r))
                .collect();
        }
        rows
    }

    // ===== single AST traversal: filter or filter+rank =====

    #[timed(search)]
    pub fn evaluate(&self, query: &Query, ctxs: &[FieldCtx], mode: EvalMode) -> EvalOutcome {
        match query {
            Query::Term(v) => self.eval_leaf(ctxs, mode, |ctx| {
                (
                    self.execute_term(v, ctx.xpath).unwrap_or_default(),
                    ctx.xpath,
                )
            }),
            Query::Prefix(v) => self.eval_leaf(ctxs, mode, |ctx| {
                (
                    self.execute_prefix(v, ctx.xpath).unwrap_or_default(),
                    ctx.xpath,
                )
            }),
            Query::Wildcard(p) => self.eval_leaf(ctxs, mode, |ctx| {
                (self.execute_wildcard(p, ctx.xpath), ctx.xpath)
            }),
            Query::Phrase(terms) => self.eval_leaf(ctxs, mode, |ctx| {
                (self.execute_phrase(terms, ctx.xpath), ctx.xpath)
            }),
            Query::Exact(v) => self.eval_leaf(ctxs, mode, |ctx| {
                let xp = ctx.exact_xpath.unwrap_or(ctx.xpath);
                (self.execute_exact(v, xp), xp)
            }),
            Query::Fuzzy(v, fz, spec) => match mode {
                EvalMode::Filter => {
                    let mut out = HashSet::new();
                    for &ctx in ctxs {
                        out.extend(self.posting_ids(&self.execute_fuzzy(v, ctx.xpath, *fz, *spec)));
                    }
                    EvalOutcome::Docs(out)
                }
                EvalMode::Rank => {
                    let mut acc: HashMap<DocId, ScoredPosting> = HashMap::new();
                    for &ctx in ctxs {
                        for hit in self.execute_scored_fuzzy(v, ctx.xpath, *fz, *spec) {
                            match acc.entry(hit.doc_id) {
                                Entry::Vacant(e) => {
                                    e.insert(hit);
                                }
                                Entry::Occupied(mut e) => {
                                    let cur = e.get_mut();
                                    cur.score = cur.score.saturating_add(hit.score);
                                    cur.matched_terms =
                                        cur.matched_terms.saturating_add(hit.matched_terms);
                                }
                            }
                        }
                    }
                    EvalOutcome::Scored(acc.into_values().collect())
                }
            },
            Query::Range(r) => {
                let mut out = HashSet::new();
                for &ctx in ctxs {
                    out.extend(self.posting_ids(&self.index.numeric_range(ctx.xpath, r.lo, r.hi)));
                }
                EvalOutcome::Docs(out)
            }
            Query::Wand(parts) | Query::Or(parts) => self.eval_union(parts, ctxs, mode),
            Query::And(parts) => self.eval_and(parts, ctxs, mode),
            //same_element is resolved by the filter driver (needs its binding tree)
            Query::SameElement(_) => EvalOutcome::Docs(HashSet::new()),

            Query::MatchAll => EvalOutcome::Docs(HashSet::new()),
        }
    }

    fn eval_leaf(
        &self,
        ctxs: &[FieldCtx],
        mode: EvalMode,
        fetch: impl Fn(FieldCtx) -> (PostingList, XPathId),
    ) -> EvalOutcome {
        match mode {
            EvalMode::Filter => {
                let mut out = HashSet::new();
                for &ctx in ctxs {
                    let (postings, _) = fetch(ctx);
                    out.extend(self.posting_ids(&postings));
                }
                EvalOutcome::Docs(out)
            }
            EvalMode::Rank => {
                let mut acc: HashMap<DocId, ScoredPosting> = HashMap::new();
                for &ctx in ctxs {
                    let (postings, xpath) = fetch(ctx);
                    for hit in
                        score_term_hybrid(self.index, &postings, xpath, postings.len() as f32)
                    {
                        match acc.entry(hit.doc_id) {
                            Entry::Vacant(e) => {
                                e.insert(hit);
                            }
                            Entry::Occupied(mut e) => {
                                let cur = e.get_mut();
                                cur.score = cur.score.saturating_add(hit.score);
                                cur.matched_terms =
                                    cur.matched_terms.saturating_add(hit.matched_terms);
                            }
                        }
                    }
                }
                EvalOutcome::Scored(acc.into_values().collect())
            }
        }
    }

    fn posting_ids(&self, postings: &PostingList) -> HashSet<DocId> {
        postings.items().iter().map(|p| p.doc_id).collect()
    }

    fn eval_union(&self, parts: &[Query], ctxs: &[FieldCtx], mode: EvalMode) -> EvalOutcome {
        match mode {
            EvalMode::Filter => {
                let mut out = HashSet::new();
                for part in parts {
                    if let EvalOutcome::Docs(docs) = self.evaluate(part, ctxs, EvalMode::Filter) {
                        out.extend(docs);
                    }
                }
                EvalOutcome::Docs(out)
            }
            EvalMode::Rank => EvalOutcome::Scored(self.score_union(parts, ctxs)),
        }
    }

    fn score_union(&self, parts: &[Query], ctxs: &[FieldCtx]) -> Vec<ScoredPosting> {
        let mut by_doc: HashMap<DocId, ScoredPosting> = HashMap::new();
        for part in parts {
            if let EvalOutcome::Scored(scored) = self.evaluate(part, ctxs, EvalMode::Rank) {
                for hit in scored {
                    match by_doc.entry(hit.doc_id) {
                        Entry::Vacant(e) => {
                            e.insert(hit);
                        }
                        Entry::Occupied(mut e) => {
                            let cur = e.get_mut();
                            cur.score = cur.score.saturating_add(hit.score);
                            cur.matched_terms = cur.matched_terms.saturating_add(hit.matched_terms);
                        }
                    }
                }
            }
        }
        by_doc.into_values().collect()
    }

    //AND logic: filter = intersection, rank = intersection + additive scores
    fn eval_and(&self, parts: &[Query], ctxs: &[FieldCtx], mode: EvalMode) -> EvalOutcome {
        let mut candidates: Option<HashSet<DocId>> = None;
        for part in parts {
            if let EvalOutcome::Docs(docs) = self.evaluate(part, ctxs, EvalMode::Filter) {
                candidates = Some(match candidates {
                    Some(cur) => cur.intersection(&docs).copied().collect(),
                    None => docs,
                });
                if candidates.as_ref().is_some_and(|s| s.is_empty()) {
                    return match mode {
                        EvalMode::Filter => EvalOutcome::Docs(HashSet::new()),
                        EvalMode::Rank => EvalOutcome::Scored(Vec::new()),
                    };
                }
            }
        }
        let candidates = candidates.unwrap_or_default();

        match mode {
            EvalMode::Filter => EvalOutcome::Docs(candidates),
            EvalMode::Rank => {
                let mut by_doc: HashMap<DocId, ScoredPosting> = HashMap::new();
                for part in parts {
                    if let EvalOutcome::Scored(scored) = self.evaluate(part, ctxs, EvalMode::Rank) {
                        for hit in scored {
                            if !candidates.contains(&hit.doc_id) {
                                continue;
                            }
                            match by_doc.entry(hit.doc_id) {
                                Entry::Vacant(e) => {
                                    e.insert(hit);
                                }
                                Entry::Occupied(mut e) => {
                                    let cur = e.get_mut();
                                    cur.score = cur.score.saturating_add(hit.score);
                                    cur.matched_terms =
                                        cur.matched_terms.saturating_add(hit.matched_terms);
                                }
                            }
                        }
                    }
                }
                EvalOutcome::Scored(by_doc.into_values().collect())
            }
        }
    }

    //walk same_element bindings recursively (mirrors Query::SameElement nesting)
    fn same_element_rows(
        &self,
        children: &[Query],
        binding: &SameElementBinding,
    ) -> HashSet<DocId> {
        let mut rows: Option<HashSet<DocId>> = None;
        for (child, clause) in children.iter().zip(&binding.clauses) {
            let r: HashSet<DocId> = match &clause.nested {
                Some(nested) => {
                    let Query::SameElement(inner_children) = child else {
                        continue;
                    };
                    self.same_element_rows(inner_children, nested)
                }
                None => match self.evaluate(child, &[clause.ctx], EvalMode::Filter) {
                    EvalOutcome::Docs(d) => d,
                    _ => HashSet::new(),
                },
            };
            let r = self.ascend(r, clause.depth.saturating_sub(binding.depth));
            rows = Some(match rows {
                Some(cur) => cur.intersection(&r).copied().collect(),
                None => r,
            });
        }
        rows.unwrap_or_default()
    }

    //same-element relevance: evaluate the full AST over each array group, resolve rows -> docs
    fn score_array_groups(
        &self,
        query: &Query,
        restrict: Option<&HashSet<DocId>>,
    ) -> Vec<SearchHit> {
        let mut out = Vec::new();
        for group in &self.array_groups {
            let EvalOutcome::Scored(scored) = self.evaluate(query, group, EvalMode::Rank) else {
                continue;
            };
            out.extend(self.resolve_group_hits(scored, restrict));
        }
        out
    }

    //row ids -> doc ids, keep the best element per doc
    fn resolve_group_hits(
        &self,
        scored: Vec<ScoredPosting>,
        restrict: Option<&HashSet<DocId>>,
    ) -> Vec<SearchHit> {
        let mut best: HashMap<DocId, (u64, usize)> = HashMap::new();
        for p in scored {
            let Some(doc) = self.index.doc_of_row(p.doc_id) else {
                continue;
            };
            let e = best.entry(doc).or_insert((0, 0));
            e.0 = e.0.max(p.score);
            e.1 = e.1.max(p.matched_terms);
        }
        best.into_iter()
            .filter(|(doc, _)| restrict.is_none_or(|a| a.contains(doc)))
            .map(|(doc, (score, matched))| SearchHit {
                doc_id: doc,
                matched_terms: matched,
                weight_sum: (score / 1000).min(u32::MAX as u64) as u32,
                distance_factor: 1.0,
                score: score as f32 / 1000.0,
            })
            .collect()
    }

    #[timed(search)]
    pub fn filter_doc_ids(&self, filters: &HashMap<String, FieldQuery>) -> Option<HashSet<DocId>> {
        if filters.is_empty() {
            return None;
        }

        let mut restrict: Option<HashSet<DocId>> = None;
        for fq in filters.values() {
            let matched: HashSet<DocId> = match &fq.query {
                Query::SameElement(children) => {
                    let binding = fq
                        .same_element
                        .as_ref()
                        .expect("same_element binding missing");
                    let rows = self.same_element_rows(children, binding);
                    self.index.resolve_array_rows(&rows)
                }
                _ => {
                    let EvalOutcome::Docs(raw) =
                        self.evaluate(&fq.query, &[fq.ctx], EvalMode::Filter)
                    else {
                        continue;
                    };
                    if fq.row_keyed {
                        self.index.resolve_array_rows(&raw)
                    } else {
                        raw
                    }
                }
            };

            restrict = Some(match restrict {
                Some(cur) => cur.intersection(&matched).copied().collect(),
                None => matched,
            });
            if restrict.as_ref().is_some_and(|s| s.is_empty()) {
                return restrict;
            }
        }
        restrict
    }

    //passing xpaths: rank the query against every search field, merge additively
    #[timed(search)]
    pub fn rank(
        &self,
        query: Option<&Query>,
        ctxs: &[FieldCtx],
        k: usize,
        restrict: Option<&HashSet<DocId>>,
        include_array_groups: bool,
    ) -> Vec<SearchHit> {
        if k == 0 || restrict.is_some_and(|d| d.is_empty()) || self.index.doc_range().is_none() {
            return Vec::new();
        }

        //no query
        let Some(query) = query else {
            //filter-only search
            return match restrict {
                Some(allowed) => top_k_from_hits(
                    allowed.iter().copied().map(|doc_id| SearchHit {
                        doc_id,
                        matched_terms: 0,
                        weight_sum: 0,
                        distance_factor: 1.0,
                        score: 0.0,
                    }),
                    k,
                ),
                None => Vec::new(),
            };
        };

        //explicit match_all
        if let Query::MatchAll = query {
            return match restrict {
                Some(allowed) => top_k_from_hits(
                    allowed.iter().copied().map(|doc_id| SearchHit {
                        doc_id,
                        matched_terms: 0,
                        weight_sum: 0,
                        distance_factor: 1.0,
                        score: 0.0,
                    }),
                    k,
                ),
                None => match self.index.doc_range() {
                    Some((min, max)) => top_k_from_hits(
                        (min..=max)
                            .filter(|id| !self.index.is_deleted(*id))
                            .map(|doc_id| SearchHit {
                                doc_id,
                                matched_terms: 0,
                                weight_sum: 0,
                                distance_factor: 1.0,
                                score: 0.0,
                            }),
                        k,
                    ),
                    None => Vec::new(),
                },
            };
        };

        let mut by_doc: HashMap<DocId, SearchHit> = HashMap::new();

        //same-element relevance across array subfields (bonus on top of field search)
        if include_array_groups {
            for hit in self.score_array_groups(query, restrict) {
                by_doc
                    .entry(hit.doc_id)
                    .and_modify(|e| {
                        e.matched_terms = e.matched_terms.saturating_add(hit.matched_terms);
                        e.weight_sum = e.weight_sum.saturating_add(hit.weight_sum);
                        e.distance_factor = e.distance_factor.max(hit.distance_factor);
                        e.score += hit.score;
                    })
                    .or_insert(hit);
            }
        }

        for &ctx in ctxs {
            if !ctx.row_keyed {
                if let Some(hits) = self.execute_top_k_retrieval(query, ctx.xpath, k, restrict) {
                    for h in hits {
                        let hit = wand_hit_to_search_hit(h);
                        by_doc
                            .entry(hit.doc_id)
                            .and_modify(|e| {
                                e.score += hit.score;
                                e.matched_terms = e.matched_terms.saturating_add(hit.matched_terms);
                                e.distance_factor = e.distance_factor.max(hit.distance_factor);
                            })
                            .or_insert(hit);
                    }
                    continue;
                }
            }

            let EvalOutcome::Scored(scored) = self.evaluate(query, &[ctx], EvalMode::Rank) else {
                continue;
            };

            //row-keyed field: resolve row -> doc, keep the best element per doc
            if ctx.row_keyed {
                for hit in self.resolve_group_hits(scored, restrict) {
                    by_doc
                        .entry(hit.doc_id)
                        .and_modify(|e| {
                            e.score += hit.score;
                            e.matched_terms = e.matched_terms.saturating_add(hit.matched_terms);
                            e.distance_factor = e.distance_factor.max(hit.distance_factor);
                        })
                        .or_insert(hit);
                }
                continue;
            }

            for p in scored {
                if !restrict.is_none_or(|a| a.contains(&p.doc_id)) {
                    continue;
                }
                let hit = SearchHit {
                    doc_id: p.doc_id,
                    matched_terms: p.matched_terms,
                    weight_sum: (p.score / 1000).min(u32::MAX as u64) as u32,
                    distance_factor: p.density,
                    score: ((p.score as f32) / 1000.0) * p.density,
                };
                by_doc
                    .entry(p.doc_id)
                    .and_modify(|e| {
                        e.score += hit.score;
                        e.matched_terms = e.matched_terms.saturating_add(hit.matched_terms);
                        e.distance_factor = e.distance_factor.max(hit.distance_factor);
                    })
                    .or_insert(hit);
            }
        }

        top_k_from_hits(by_doc.into_values(), k)
    }

    //INFO: Norca sito hujnu centaas izprast kkur 1h, seit visam ir jabut safe, ne passaprotami,
    //bet safe robezaas
    //optimizations:
    //      Posting:
    //    pub positions: SmallVec<[Position; INLINE_POSITIONS]>,
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
        score: (hit.score as f32) / 1000.0,
    }
}

fn additive_terms(query: &Query) -> Option<Vec<&str>> {
    match query {
        Query::Term(term) => Some(vec![term.as_str()]),
        Query::Wand(parts) | Query::Or(parts)
            if parts.iter().all(|part| matches!(part, Query::Term(_))) =>
        {
            Some(
                parts
                    .iter()
                    .filter_map(|part| match part {
                        Query::Term(term) => Some(term.as_str()),
                        _ => None,
                    })
                    .collect(),
            )
        }
        _ => None,
    }
}

/*
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
}*/
