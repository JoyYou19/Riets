use core_index::{
    analyzer::analyzer::Analyzer,
    fuzzy::{ FuzzyExpansion, FuzzyOptions, FuzzySpec },
    posting::{ Posting, PostingList, ops::intersection },
    search::{ SearchIndex, SearchNumeric, SearchStats, TermPostings },
    types::{ DocId, XPathId },
};
use std::{ cmp::Ordering, collections::{ BinaryHeap, HashMap, hash_map::Entry }, u32 };

use ahash::{ HashSet, HashSetExt };

use core_protocol::command_reponse_definitions::Fuzziness;
use core_timing::timed;

use crate::{
    ScoredPosting,
    SearchHit,
    TopHit,
    ast::Query,
    resolver::{ FieldCtx, FieldQuery, SameElementBinding },
    scorer::{ fuzzy_decay, score_term_hybrid, score_term_into },
    syn::{ QueryNode, SynonymDictionary, tokenize },
    wand::{
        WandHit,
        WeightedGroup,
        conjunctive_top_k_groups,
        wand_top_k_groups,
    },
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
pub struct QueryExecutor<'a, I> where I: SearchIndex + SearchStats {
    // Which index are we searching?
    index: &'a I,

    // A query is analyzed also the same way as the index, so we could filter out words, stemming,
    // whaatever
    analyzer: &'a Analyzer,
    array_groups: Vec<Vec<FieldCtx>>,
}

impl<'a, I> QueryExecutor<'a, I> where I: SearchIndex + SearchStats + SearchNumeric {
    pub fn new(
        index: &'a I,
        analyzer: &'a Analyzer,
        array_groups: Vec<Vec<(XPathId, Option<XPathId>)>>
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

    fn execute_top_k_retrieval(
        &self,
        query: &Query,
        ctx: FieldCtx,
        k: usize,
        restrict: Option<&HashSet<DocId>>
    ) -> Option<Vec<WandHit>> {
        let xpath = ctx.xpath;

        //terms, and synonym groups of terms: one grouped search per field
        //terms, phrases and synonym groups of them: one grouped search per field
        if let Some((conjunctive, groups)) = term_groups(query, ctx.exact_xpath.is_some()) {
            let fetched: Vec<Vec<(TermPostings, f64)>> = groups
                .iter()
                .map(|group| {
                    group
                        .iter()
                        .map(|(alt, weight)| {
                            let postings = match alt {
                                Alt::Term(term) => self.index.lookup_term(term, xpath),
                                Alt::Exact(term) => {
                                    let xp = ctx.exact_xpath.unwrap_or(ctx.xpath);
                                    self.index.lookup_term(term, xp)
                                }
                                Alt::Phrase(words) => self.phrase_as_term(words, xpath),
                                Alt::ExactPhrase(words) => {
                                    let xp = ctx.exact_xpath.unwrap_or(ctx.xpath);
                                    self.phrase_as_term(words, xp)
                                }
                            };
                            (postings, *weight)
                        })
                        .collect()
                })
                .collect();
            let clauses: Vec<WeightedGroup<'_>> = fetched
                .iter()
                .map(|group|
                    group
                        .iter()
                        .map(|(postings, weight)| (postings, *weight))
                        .collect()
                )
                .collect();
            return Some(
                if conjunctive {
                    conjunctive_top_k_groups(self.index, xpath, &clauses, k, restrict)
                } else {
                    wand_top_k_groups(self.index, xpath, &clauses, k, restrict)
                }
            );
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
        let lists: Vec<PostingList> = terms
            .iter()
            .map(|term| self.index.lookup(term, xpath))
            .collect();
        phrase_postings(&lists)
    }

    //Kindof the fuzzy entry point the whole porno logic starts here
    #[timed(search)]
    fn execute_fuzzy(
        &self,
        raw: &str,
        xpath: XPathId,
        fuzziness: Fuzziness,
        spec: FuzzySpec
    ) -> PostingList {
        //"butman and robin" -> [butman, robin]
        let words = self.fuzzy_words(raw);

        if words.is_empty() {
            return PostingList::default();
        }

        let lists: Vec<PostingList> = words
            .iter()
            .map(|w| { self.index.lookup_fuzzy(w, xpath, fuzzy_options(w, fuzziness, spec)) })
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
    //a phrase's matching documents as a term-like posting list, so it takes part in WAND like any term
    fn phrase_as_term(&self, words: &[String], xpath: XPathId) -> TermPostings {
        let postings = self.execute_phrase(words, xpath);
        TermPostings {
            doc_freq: postings.len() as u32,
            max_weight: postings.items().iter().map(|p| p.weight).max().unwrap_or(0),
            postings,
        }
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
        match words.as_slice() {
            [] => PostingList::default(),
            [word] => self.index.lookup(word, xpath),
            _ => {
                let lists: Vec<PostingList> = words
                    .iter()
                    .map(|word| self.index.lookup(word, xpath))
                    .collect();
                phrase_postings(&lists)
            }
        }
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
            Query::Term(v) =>
                self.eval_leaf(ctxs, mode, |ctx| {
                    (self.execute_term(v, ctx.xpath).unwrap_or_default(), ctx.xpath)
                }),
            Query::Prefix(v) =>
                self.eval_leaf(ctxs, mode, |ctx| {
                    (self.execute_prefix(v, ctx.xpath).unwrap_or_default(), ctx.xpath)
                }),
            Query::Wildcard(p) =>
                self.eval_leaf(ctxs, mode, |ctx| {
                    (self.execute_wildcard(p, ctx.xpath), ctx.xpath)
                }),
            Query::Phrase(terms) =>
                self.eval_leaf(ctxs, mode, |ctx| {
                    (self.execute_phrase(terms, ctx.xpath), ctx.xpath)
                }),
            Query::Exact(v) =>
                self.eval_leaf(ctxs, mode, |ctx| {
                    let xp = ctx.exact_xpath.unwrap_or(ctx.xpath);
                    (self.execute_exact(v, xp), xp)
                }),
            Query::Fuzzy(v, fz, spec) =>
                match mode {
                    EvalMode::Filter => {
                        let mut out = HashSet::new();
                        for &ctx in ctxs {
                            out.extend(
                                self.posting_ids(&self.execute_fuzzy(v, ctx.xpath, *fz, *spec))
                            );
                        }
                        EvalOutcome::Docs(out)
                    }
                    EvalMode::Rank => {
                        let mut acc: HashMap<DocId, ScoredPosting> = HashMap::new();
                        for &ctx in ctxs {
                            for hit in self.execute_scored_fuzzy(v, ctx.xpath, *fz, *spec) {
                                add_into(&mut acc, hit);
                            }
                        }
                        EvalOutcome::Scored(acc.into_values().collect())
                    }
                }
            Query::Synonym(parts) => self.eval_synonym(parts, ctxs, mode), //sinonimiem
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
        fetch: impl Fn(FieldCtx) -> (PostingList, XPathId)
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
                    for hit in score_term_hybrid(
                        self.index,
                        &postings,
                        xpath,
                        postings.len() as f32
                    ) {
                        add_into(&mut acc, hit);
                    }
                }
                EvalOutcome::Scored(acc.into_values().collect())
            }
        }
    }

    fn posting_ids(&self, postings: &PostingList) -> HashSet<DocId> {
        postings
            .items()
            .iter()
            .map(|p| p.doc_id)
            .collect()
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

    //synonym alternatives: union for filtering, best alternative per doc for ranking
    //(summing would double-credit docs that contain both "CaP" and "calcium phosphate")
    //typed form (parts[0]) counts fully, dictionary alternatives less,
    //so "piss" ranks Piss* titles above Urine* titles while still finding both

    fn eval_synonym(&self, parts: &[Query], ctxs: &[FieldCtx], mode: EvalMode) -> EvalOutcome {
        if mode == EvalMode::Filter {
            return self.eval_union(parts, ctxs, mode);
        }
        let mut by_doc: HashMap<DocId, ScoredPosting> = HashMap::new();
        for (index, part) in parts.iter().enumerate() {
            let EvalOutcome::Scored(scored) = self.evaluate(part, ctxs, EvalMode::Rank) else {
                continue;
            };
            let weight = match part {
                Query::Term(term) => alternative_weight(index, term),
                _ if index == 0 => 1.0,
                _ => SYNONYM_WEIGHT,
            };
            for mut hit in scored {
                hit.score = ((hit.score as f64) * weight) as u64;
                keep_best(&mut by_doc, hit.doc_id, hit, |h: &ScoredPosting| h.score);
            }
        }
        EvalOutcome::Scored(by_doc.into_values().collect())
    }

    fn score_union(&self, parts: &[Query], ctxs: &[FieldCtx]) -> Vec<ScoredPosting> {
        let mut by_doc: HashMap<DocId, ScoredPosting> = HashMap::new();
        for part in parts {
            if let EvalOutcome::Scored(scored) = self.evaluate(part, ctxs, EvalMode::Rank) {
                for hit in scored {
                    add_into(&mut by_doc, hit);
                }
            }
        }
        by_doc.into_values().collect()
    }

    //AND logic: filter = intersection, rank = intersection + additive scores
    #[timed(search)]
    fn eval_and(&self, parts: &[Query], ctxs: &[FieldCtx], mode: EvalMode) -> EvalOutcome {
        match mode {
            EvalMode::Filter => {
                let mut candidates: Option<HashSet<DocId>> = None;
                for part in parts {
                    if let EvalOutcome::Docs(docs) = self.evaluate(part, ctxs, EvalMode::Filter) {
                        candidates = Some(match candidates {
                            Some(cur) => cur.intersection(&docs).copied().collect(),
                            None => docs,
                        });
                        if candidates.as_ref().is_some_and(|s| s.is_empty()) {
                            return EvalOutcome::Docs(HashSet::new());
                        }
                    }
                }
                EvalOutcome::Docs(candidates.unwrap_or_default())
            }
            EvalMode::Rank => {
                let mut scored: Option<HashMap<DocId, ScoredPosting>> = None;
                //children that only filter (ranges) restrict without adding score
                let mut restrictions: Vec<HashSet<DocId>> = Vec::new();
                for part in parts {
                    match self.evaluate(part, ctxs, EvalMode::Rank) {
                        EvalOutcome::Docs(docs) => {
                            if docs.is_empty() {
                                return EvalOutcome::Scored(Vec::new());
                            }
                            restrictions.push(docs);
                        }
                        EvalOutcome::Scored(hits) => {
                            let next: HashMap<DocId, ScoredPosting> = match scored {
                                None =>
                                    hits
                                        .into_iter()
                                        .map(|hit| (hit.doc_id, hit))
                                        .collect(),
                                Some(mut acc) => {
                                    let mut next = HashMap::with_capacity(
                                        acc.len().min(hits.len())
                                    );
                                    for hit in hits {
                                        if let Some(mut cur) = acc.remove(&hit.doc_id) {
                                            cur.score = cur.score.saturating_add(hit.score);
                                            cur.matched_terms = cur.matched_terms.saturating_add(
                                                hit.matched_terms
                                            );
                                            next.insert(hit.doc_id, cur);
                                        }
                                    }
                                    next
                                }
                            };
                            if next.is_empty() {
                                return EvalOutcome::Scored(Vec::new());
                            }
                            scored = Some(next);
                        }
                    }
                }
                let Some(scored) = scored else {
                    return EvalOutcome::Scored(Vec::new());
                };
                EvalOutcome::Scored(
                    scored
                        .into_values()
                        .filter(|hit| restrictions.iter().all(|docs| docs.contains(&hit.doc_id)))
                        .collect()
                )
            }
        }
    }

    //walk same_element bindings recursively (mirrors Query::SameElement nesting)
    fn same_element_rows(
        &self,
        children: &[Query],
        binding: &SameElementBinding
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
                None =>
                    match self.evaluate(child, &[clause.ctx], EvalMode::Filter) {
                        EvalOutcome::Docs(d) => d,
                        _ => HashSet::new(),
                    }
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
        restrict: Option<&HashSet<DocId>>
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
        restrict: Option<&HashSet<DocId>>
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
                score: (score as f32) / 1000.0,
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
                    let binding = fq.same_element.as_ref().expect("same_element binding missing");
                    let rows = self.same_element_rows(children, binding);
                    self.index.resolve_array_rows(&rows)
                }
                _ => {
                    let EvalOutcome::Docs(raw) = self.evaluate(
                        &fq.query,
                        &[fq.ctx],
                        EvalMode::Filter
                    ) else {
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
        include_array_groups: bool
    ) -> Vec<SearchHit> {
        if k == 0 || restrict.is_some_and(|d| d.is_empty()) || self.index.doc_range().is_none() {
            return Vec::new();
        }

        //no query
        let Some(query) = query else {
            return match restrict {
                Some(allowed) => top_k_from_hits(allowed.iter().copied().map(unscored_hit), k),
                None => Vec::new(),
            };
        };

        if let Query::MatchAll = query {
            return match (restrict, self.index.doc_range()) {
                (Some(allowed), _) => top_k_from_hits(allowed.iter().copied().map(unscored_hit), k),
                (None, Some((min, max))) =>
                    top_k_from_hits(
                        (min..=max).filter(|id| !self.index.is_deleted(*id)).map(unscored_hit),
                        k
                    ),
                (None, None) => Vec::new(),
            };
        }

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
                if let Some(hits) = self.execute_top_k_retrieval(query, ctx, k, restrict) {
                    for hit in hits {
                        merge_hit(&mut by_doc, wand_hit_to_search_hit(hit));
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
                    merge_hit(&mut by_doc, hit);
                }
                continue;
            }

            for p in scored {
                if !restrict.is_none_or(|a| a.contains(&p.doc_id)) {
                    continue;
                }
                merge_hit(&mut by_doc, SearchHit {
                    doc_id: p.doc_id,
                    matched_terms: p.matched_terms,
                    weight_sum: (p.score / 1000).min(u32::MAX as u64) as u32,
                    distance_factor: p.density,
                    score: ((p.score as f32) / 1000.0) * p.density,
                });
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
        spec: FuzzySpec
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
                    &mut scored_buf
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
/// Docs where every list's term occurs at consecutive positions, in order.
fn phrase_postings(lists: &[PostingList]) -> PostingList {
    if lists.is_empty() || lists.iter().any(|list| list.is_empty()) {
        return PostingList::default();
    }
    let mut result = Vec::new();
    'docs: for first in lists[0].items() {
        let mut position_lists: Vec<&[u32]> = vec![first.positions.as_slice()];
        for list in &lists[1..] {
            match list.items().binary_search_by_key(&first.doc_id, |p| p.doc_id) {
                Ok(index) => position_lists.push(list.items()[index].positions.as_slice()),
                Err(_) => {
                    continue 'docs;
                }
            }
        }
        if phrase_matches(&position_lists) {
            //Posting::new leaves the weight at 0, and both BM25 and WAND's upper bound scale
            //with it, so a phrase hit would never contribute to a score
            let mut posting = Posting::new(first.doc_id, first.positions.clone());
            posting.weight = first.weight;
            result.push(posting);
        }
    }
    PostingList::from_items(result)
}

/// Sums scores of the same doc (same doc found by several terms or fields).
fn add_into(acc: &mut HashMap<DocId, ScoredPosting>, hit: ScoredPosting) {
    match acc.entry(hit.doc_id) {
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

/// Keeps only the higher-scoring hit per doc (interchangeable alternatives).
fn keep_best<T>(acc: &mut HashMap<DocId, T>, doc_id: DocId, item: T, score: fn(&T) -> u64) {
    match acc.entry(doc_id) {
        Entry::Vacant(e) => {
            e.insert(item);
        }
        Entry::Occupied(mut e) => {
            if score(&item) > score(e.get()) {
                e.insert(item);
            }
        }
    }
}

fn merge_hit(by_doc: &mut HashMap<DocId, SearchHit>, hit: SearchHit) {
    by_doc
        .entry(hit.doc_id)
        .and_modify(|e| {
            e.score += hit.score;
            e.matched_terms = e.matched_terms.saturating_add(hit.matched_terms);
            e.distance_factor = e.distance_factor.max(hit.distance_factor);
        })
        .or_insert(hit);
}

fn unscored_hit(doc_id: DocId) -> SearchHit {
    SearchHit { doc_id, matched_terms: 0, weight_sum: 0, distance_factor: 1.0, score: 0.0 }
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
// fn note_general_path(query: &Query) {
//     use std::sync::{Mutex, OnceLock};
//     static SEEN: OnceLock<Mutex<std::collections::HashSet<String>>> = OnceLock::new();
//     let Ok(mut seen) = SEEN.get_or_init(Default::default).lock() else { return };
//     let text = format!("{query:?}");
//     //each distinct query shape is printed once, at most 40 in total
//     if seen.len() < 40 && seen.insert(text.clone()) {
//         eprintln!("GENERAL PATH {text}");
//     }
// }
//fuzzable words for did_you_mean
pub fn fuzzable_words(analyzer: &Analyzer, raw: &str) -> Vec<String> {
    raw.split_whitespace()
        .filter_map(|w|
            analyzer
                .analyze_query(w)
                .into_iter()
                .next()
                .map(|t| t.text)
        )
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



//vibe vibe vibe
type Combinator = fn(Vec<Query>) -> Query;
//cap on plain-term variants run through the top-k fast path; queries with more
//(several large synonym groups in one query) use the general path instead
const MAX_SYNONYM_VARIANTS: usize = 64;
const SYNONYM_WEIGHT: f64 = 0.7;

struct Variant<'q> {
    terms: Vec<&'q str>,
    conjunctive: bool,
    weight: f64,
    //look the terms up in the field's case-preserving index
    exact: bool,
    phrase: bool,
}
#[timed(search)]
/// Plain-term searches that together answer `query`, each with its weight,
/// or `None` when the query needs the general path.
fn synonym_variants(query: &Query) -> Option<Vec<Variant<'_>>> {
    match query {
        Query::Synonym(alternatives) => {
            if alternatives.len() > MAX_SYNONYM_VARIANTS {
                return None;
            }
            alternatives
                .iter()
                .enumerate()
                .map(|(index, alternative)| {
                    let weight_for = |term: &str| alternative_weight(index, term);
                    let fixed_weight = if index == 0 { 1.0 } else { SYNONYM_WEIGHT };
                    match alternative {
                        Query::Term(term) =>
                            Some(Variant {
                                terms: vec![term.as_str()],
                                conjunctive: false,
                                weight: weight_for(term),
                                exact: false,
                                phrase: false,
                            }),
                        Query::And(parts) =>
                            Some(Variant {
                                terms: plain_terms(parts)?,
                                conjunctive: true,
                                weight: fixed_weight,
                                exact: false,
                                phrase: false,
                            }),
                        Query::Wand(parts) | Query::Or(parts) =>
                            Some(Variant {
                                terms: plain_terms(parts)?,
                                conjunctive: false,
                                weight: fixed_weight,
                                exact: false,
                                phrase: false,
                            }),
                        Query::Phrase(words) =>
                            Some(Variant {
                                terms: words.iter().map(String::as_str).collect(),
                                conjunctive: true,
                                weight: fixed_weight,
                                exact: false,
                                phrase: true,
                            }),
                        Query::Exact(text) =>
                            Some(Variant {
                                terms: text.split_whitespace().collect(),
                                conjunctive: true,
                                weight: fixed_weight,
                                exact: true,
                                phrase: true,
                            }),
                        _ => None,
                    }
                })
                .collect()
        }
        Query::And(parts) | Query::Wand(parts) | Query::Or(parts) => {
            let conjunctive = matches!(query, Query::And(_));
            let mut has_synonym = false;
            let mut combos: Vec<(Vec<&str>, f64)> = vec![(Vec::with_capacity(parts.len()), 1.0)];
            for part in parts {
                let options: Vec<(&str, f64)> = match part {
                    Query::Term(term) => vec![(term.as_str(), 1.0)],
                    Query::Synonym(alternatives) => {
                        has_synonym = true;
                        let mut options = Vec::with_capacity(alternatives.len());
                        for (index, alternative) in alternatives.iter().enumerate() {
                            let Query::Term(term) = alternative else {
                                return None;
                            };
                            options.push((term.as_str(), alternative_weight(index, term)));
                        }
                        options
                    }
                    _ => {
                        return None;
                    }
                };
                if combos.len() * options.len() > MAX_SYNONYM_VARIANTS {
                    return None;
                }
                combos = combos
                    .iter()
                    .flat_map(|(terms, weight)| {
                        options.iter().map(move |&(term, option_weight)| {
                            let mut terms = terms.clone();
                            terms.push(term);
                            (terms, weight * option_weight)
                        })
                    })
                    .collect();
            }
            has_synonym.then(|| {
                combos
                    .into_iter()
                    .map(|(terms, weight)| Variant {
                        terms,
                        conjunctive,
                        weight,
                        exact: false,
                        phrase: false,
                    })
                    .collect()
            })
        }
        _ => None,
    }
}
/// One alternative of a clause, borrowed from the query.
enum Alt<'q> {
    Term(&'q str),
    Exact(&'q str),
    Phrase(Vec<String>),
    ExactPhrase(Vec<String>),
}

/// Terms, phrases and synonym groups of them, combined with And/Wand/Or: the weighted
/// clauses for one grouped search. bool = conjunctive.
///
/// `exact_field`: does this field have a case-preserving index? Exact alternatives can
/// only be answered there. Without one they match nothing (as in the general path), so
/// they are dropped; with one, the query needs the general path.
fn term_groups(query: &Query, exact_field: bool) -> Option<(bool, Vec<Vec<(Alt<'_>, f64)>>)> {
    match query {
        Query::Term(_) | Query::Phrase(_) | Query::Synonym(_) => {
            Some((false, vec![clause(query, exact_field)?]))
        }
        Query::And(parts) | Query::Wand(parts) | Query::Or(parts) => {
            let groups = parts
                .iter()
                .map(|part| clause(part, exact_field))
                .collect::<Option<Vec<_>>>()?;
            Some((matches!(query, Query::And(_)), groups))
        }
        _ => None,
    }
}

fn clause(part: &Query, exact_field: bool) -> Option<Vec<(Alt<'_>, f64)>> {
    match part {
        Query::Term(term) => Some(vec![(Alt::Term(term), 1.0)]),
        Query::Phrase(words) => Some(vec![(Alt::Phrase(words.to_vec()), 1.0)]),

        Query::Synonym(alternatives) => {
            let mut out = Vec::with_capacity(alternatives.len());
            for (index, alternative) in alternatives.iter().enumerate() {
                let typed_or_synonym = if index == 0 { 1.0 } else { SYNONYM_WEIGHT };
                match alternative {
                    Query::Term(term) => {
                        out.push((Alt::Term(term), alternative_weight(index, term)));
                    }
                    Query::Phrase(words) => {
                        out.push((Alt::Phrase(words.to_vec()), typed_or_synonym));
                    }
                    Query::Exact(text) if exact_field => {
                        let words: Vec<&str> = text.split_whitespace().collect();
                        if words.len() == 1 {
                            out.push((Alt::Exact(words[0]), typed_or_synonym));
                        } else {
                            out.push((
                                Alt::ExactPhrase(
                                    words
                                        .iter()
                                        .map(|&s| s.to_owned())
                                        .collect()
                                ),
                                typed_or_synonym,
                            ));
                        }
                    }
                    Query::Exact(_) if !exact_field => {}
                    Query::And(parts) => {
                        let terms = plain_terms(parts)?;
                        out.push((
                            Alt::Phrase(
                                terms
                                    .iter()
                                    .map(|&s| s.to_owned())
                                    .collect()
                            ),
                            typed_or_synonym,
                        ));
                    }
                    _ => {
                        return None;
                    }
                }
            }
            Some(out)
        }
        _ => None,
    }
}

fn plain_terms(parts: &[Query]) -> Option<Vec<&str>> {
    parts
        .iter()
        .map(|part| {
            match part {
                Query::Term(term) => Some(term.as_str()),
                _ => None,
            }
        })
        .collect()
}

/// Roman numerals and digit strings are exact equivalents of each other,
/// not loose synonyms, so they never get the synonym penalty.
fn alternative_weight(index: usize, term: &str) -> f64 {
    let numeral =
        !term.is_empty() &&
        (term.bytes().all(|b| b.is_ascii_digit()) || term.bytes().all(|b| b"ivxlcdm".contains(&b)));
    if index == 0 || numeral {
        1.0
    } else {
        SYNONYM_WEIGHT
    }
}
#[timed(search)]
pub fn expand_synonyms(query: Query, dictionary: &SynonymDictionary) -> Query {
    match query {
        Query::Term(text) if is_multi_word(&text) => expand_multi_word(text, dictionary),
        Query::Term(word) => {
            let expanded = expand_run(vec![word], dictionary, Query::Wand);
            match <[Query; 1]>::try_from(expanded) {
                Ok([only]) => only,
                Err(many) => Query::Wand(many),
            }
        }
        Query::Wand(subs) => Query::Wand(expand_children(subs, dictionary, Query::Wand)),
        Query::And(subs) => Query::And(expand_children(subs, dictionary, Query::And)),
        Query::Or(subs) => Query::Or(expand_children(subs, dictionary, Query::Or)),
        //children map 1:1 onto same_element bindings, so they are never merged
        Query::SameElement(children) =>
            Query::SameElement(
                children
                    .into_iter()
                    .map(|child| expand_synonyms(child, dictionary))
                    .collect()
            ),
        other => other,
    }
}

fn is_multi_word(text: &str) -> bool {
    text.split_whitespace().nth(1).is_some()
}

/// The parser hands a plain string query over as one `Term` holding the whole
/// text ("Henry 8"), which `analyze_query` later splits into an `And` of its
/// words. Expand it as exactly that `And`, so a match can target single words
/// inside it. Untouched text is returned as it was.
#[timed(search)]
fn expand_multi_word(text: String, dictionary: &SynonymDictionary) -> Query {
    let words: Vec<String> = text.split_whitespace().map(str::to_owned).collect();
    let expanded = expand_run(words, dictionary, Query::And);
    if expanded.iter().all(|part| matches!(part, Query::Term(_))) {
        return Query::Term(text);
    }
    Query::And(expanded)
}
#[timed(search)]
fn expand_children(
    subs: Vec<Query>,
    dictionary: &SynonymDictionary,
    wrap: Combinator
) -> Vec<Query> {
    let mut out = Vec::with_capacity(subs.len());
    let mut run: Vec<String> = Vec::new();
    for sub in subs {
        match sub {
            Query::Term(word) if !is_multi_word(&word) => run.push(word),
            other => {
                if !run.is_empty() {
                    out.extend(expand_run(std::mem::take(&mut run), dictionary, wrap));
                }
                out.push(expand_synonyms(other, dictionary));
            }
        }
    }
    if !run.is_empty() {
        out.extend(expand_run(run, dictionary, wrap));
    }
    out
}
#[timed(search)]
/// Expands consecutive sibling terms (in query order). `wrap` is the parent
/// combinator, used to rebuild the user's original words as one alternative.
fn expand_run(words: Vec<String>, dictionary: &SynonymDictionary, wrap: Combinator) -> Vec<Query> {
    let mut tokens: Vec<&str> = Vec::new();
    let mut owner: Vec<usize> = Vec::new();
    let mut word_start: Vec<usize> = Vec::with_capacity(words.len() + 1);
    for (index, word) in words.iter().enumerate() {
        word_start.push(tokens.len());
        for token in tokenize(word) {
            tokens.push(token);
            owner.push(index);
        }
    }
    word_start.push(tokens.len());

    let mut boundary = vec![false; tokens.len() + 1];
    for &start in &word_start {
        boundary[start] = true;
    }

    let mut out = Vec::with_capacity(words.len());
    let mut index = 0;
    while index < words.len() {
        let start = word_start[index];
        let end = word_start[index + 1];
        if start == end {
            //pure punctuation: leave it to the analyzer exactly as before
            out.push(Query::Term(words[index].clone()));
            index += 1;
            continue;
        }

        if
            let Some((len, node)) = dictionary.match_at(
                &tokens[start..],
                |len| boundary[start + len]
            )
        {
            let last = owner[start + len - 1];
            let covered = &words[index..=last];
            let mut alternatives = Vec::new();
            if covered.len() > 1 || len > 1 {
                alternatives.push(original_form(covered, wrap));
                push_alternatives(node, &mut alternatives);
                let typed: Vec<String> = tokens[start..start + len]
                    .iter()
                    .map(|t| t.to_lowercase())
                    .collect();
                alternatives.retain(|alt| !matches!(alt, Query::Phrase(words) if *words == typed));
            } else {
                push_alternatives(node, &mut alternatives);
                move_typed_form_first(&mut alternatives, &words[index]);
                //the dictionary's phrase for the very words typed only re-finds what the
                //original form already finds, and costs a phrase scan
                let typed: Vec<String> = tokens[start..start + len]
                    .iter()
                    .map(|t| t.to_lowercase())
                    .collect();
                alternatives.retain(|alt| !matches!(alt, Query::Phrase(words) if *words == typed));
            }
            out.push(synonym(alternatives));
            index = last + 1;
            continue;
        }

        if end - start == 1 {
            if let Some(node) = dictionary.expand_number(tokens[start]) {
                let mut alternatives = Vec::new();
                push_alternatives(node, &mut alternatives);
                out.push(synonym(alternatives));
                index += 1;
                continue;
            }
        }

        out.push(Query::Term(words[index].clone()));
        index += 1;
    }
    out
}

/// The executor scores `Synonym` parts[0] at full weight and the rest at
/// `SYNONYM_WEIGHT`, so the form the user typed must come first. For a single
/// word that is the matched dictionary variant: `Term` for a case-insensitive
/// entry (`piss`), `Exact` for a case-sensitive one (`CaP`).

fn move_typed_form_first(alternatives: &mut [Query], word: &str) {
    let typed = alternatives.iter().position(|alternative| {
        match alternative {
            Query::Exact(text) => text == word,
            Query::Term(term) => term.eq_ignore_ascii_case(word) || *term == word.to_lowercase(),
            _ => false,
        }
    });
    if let Some(position) = typed {
        alternatives[..=position].rotate_right(1);
    }
}

fn original_form(words: &[String], wrap: Combinator) -> Query {
    match words {
        [word] => Query::Term(word.clone()),
        _ => wrap(words.iter().cloned().map(Query::Term).collect()),
    }
}

fn push_alternatives(node: QueryNode, out: &mut Vec<Query>) {
    match node {
        QueryNode::AnyOf(alternatives) => {
            for alternative in alternatives {
                push_alternatives(alternative, out);
            }
        }
        QueryNode::Term(word) => out.push(Query::Term(word)),
        QueryNode::Phrase(words) => out.push(Query::Phrase(words)),
        QueryNode::Exact(words) => out.push(Query::Exact(words.join(" "))),
    }
}

fn synonym(mut alternatives: Vec<Query>) -> Query {
    if alternatives.len() == 1 {
        if let Some(only) = alternatives.pop() {
            return only;
        }
    }
    Query::Synonym(alternatives)
}
