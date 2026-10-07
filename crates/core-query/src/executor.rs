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
    planner::{ MAX_GAP, QueryPlanner, QuerySignal },
    resolver::{ FieldCtx, FieldQuery, SameElementBinding },
    scorer::{ fuzzy_decay, score_term_hybrid, score_term_into },
    syn::{ QueryNode, SynonymDictionary, tokenize },
    wand::{
        WandHit,
        WeightedGroup,
        conjunctive_top_k_groups,
        phrase_scores,
        score_docs_groups,
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

//Exact top-k across fields deepens the per-field lists by 4x per round when the first round
//cannot prove its result; this caps the depth so a pathological query cannot loop for long.
const MAX_EXACT_DEPTH: usize = 1 << 16;

//Phrase signals reorder this many of the best documents (or the page, if larger). A fixed
//pool keeps pages up to this size consistent with each other.
const RERANK_POOL: usize = 200;

/// One field's postings for a query, fetched once and walked as often as needed.
struct FieldPostings {
    xpath: XPathId,
    conjunctive: bool,
    groups: Vec<Vec<(TermPostings, f64)>>,
}

impl FieldPostings {
    fn clauses(&self) -> Vec<WeightedGroup<'_>> {
        self.groups
            .iter()
            .map(|group|
                group
                    .iter()
                    .map(|(postings, weight)| (postings, *weight))
                    .collect()
            )
            .collect()
    }
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

    /// The field's postings when the fast path can rank this query there, `None` otherwise.
    fn field_postings(&self, query: &Query, ctx: FieldCtx) -> Option<FieldPostings> {
        //terms, phrases and synonym groups of them: one grouped search per field
        let (conjunctive, groups) = term_groups(query, ctx.exact_xpath.is_some())?;
        let xpath = ctx.xpath;
        let groups = groups
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
        Some(FieldPostings { xpath, conjunctive, groups })
    }

    fn field_top_k(
        &self,
        field: &FieldPostings,
        k: usize,
        restrict: Option<&HashSet<DocId>>
    ) -> Vec<WandHit> {
        let clauses = field.clauses();
        if field.conjunctive {
            conjunctive_top_k_groups(self.index, field.xpath, &clauses, k, restrict)
        } else {
            wand_top_k_groups(self.index, field.xpath, &clauses, k, restrict)
        }
    }

    /// Adds, for each phrase signal and field, the signal's boost times the phrase words' score
    /// in that field to every document where the typed words stand next to each other, then
    /// re-sorts. Uses the postings already decoded for the query: no new lookups.
    fn apply_phrase_signals(
        &self,
        fields: &[FieldPostings],
        signals: &[QuerySignal],
        hits: &mut Vec<SearchHit>
    ) {
        let mut docs: Vec<DocId> = hits.iter().map(|hit| hit.doc_id).collect();
        docs.sort_unstable();
        let mut bonus: HashMap<DocId, f32> = HashMap::new();
        //signals refer to the clauses of the top-level AND, which a conjunctive field keeps in order
        for field in fields.iter().filter(|field| field.conjunctive) {
            let clauses = field.clauses();
            for signal in signals {
                let Some(groups) = clauses.get(signal.clauses.clone()) else {
                    continue;
                };
                for (doc, score) in phrase_scores(self.index, field.xpath, groups, &docs, MAX_GAP) {
                    *bonus.entry(doc).or_insert(0.0) += signal.boost * ((score as f32) / 1000.0);
                }
            }
        }
        if bonus.is_empty() {
            return;
        }
        for hit in hits.iter_mut() {
            if let Some(extra) = bonus.get(&hit.doc_id) {
                hit.score += extra;
            }
        }
        hits.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(Ordering::Equal)
                .then_with(|| a.doc_id.cmp(&b.doc_id))
        });
    }

        fn exact_top_k(
        &self,
        fields: &[FieldPostings],
        complete: HashMap<DocId, SearchHit>,
        k: usize,
        restrict: Option<&HashSet<DocId>>
    ) -> Vec<SearchHit> {
        //a deeper first round costs WAND little and usually proves the result at once;
        //starting at exactly k needed about two rounds per query
        let mut depth = k.max(64).saturating_mul(2);
        let mut rounds = 1;
        loop {
            let mut candidates: HashSet<DocId> = complete.keys().copied().collect();
            let mut outside_bound = 0.0f32;
            let mut truncated = false;
            for field in fields {
                let hits = self.field_top_k(field, depth, restrict);
                if hits.len() >= depth {
                    truncated = true;
                    //best first, so the last hit has the lowest score in the list
                    outside_bound += hits.last().map_or(0.0, |hit| (hit.score as f32) / 1000.0);
                }
                candidates.extend(hits.iter().map(|hit| hit.doc_id));
            }

            let mut docs: Vec<DocId> = candidates.into_iter().collect();
            docs.sort_unstable();
            let mut totals: HashMap<DocId, SearchHit> = complete
                .values()
                .map(|hit| (hit.doc_id, copy_hit(hit)))
                .collect();
            for field in fields {
                let clauses = field.clauses();
                for hit in score_docs_groups(self.index, field.xpath, &clauses, &docs, field.conjunctive) {
                    merge_hit(&mut totals, wand_hit_to_search_hit(hit));
                }
            }

            let top = top_k_from_hits(totals.into_values(), k);
            //the small margin keeps float rounding from proving a result that is not proven
            let proven =
                !truncated ||
                (top.len() >= k && top[k - 1].score > outside_bound * (1.0 + 1e-6));
            if proven || depth >= MAX_EXACT_DEPTH {
                if std::env::var_os("CORELAMO_DEBUG_EXACT").is_some() {
                    eprintln!(
                        "exact top-{k}: {rounds} round(s), final depth {depth}, {} candidates, proven: {proven}",
                        docs.len()
                    );
                }
                return top;
            }
            depth = depth.saturating_mul(4);
            rounds += 1;
        }
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

    //a phrase's matching documents as a term-like posting list, so it takes part in WAND like
    //any term; its positions are the phrase starts, so the term frequency counts the phrase
    fn phrase_as_term(&self, words: &[String], xpath: XPathId) -> TermPostings {
        let postings = self.execute_phrase(words, xpath);
        TermPostings {
            doc_freq: postings.len() as u32,
            max_weight: postings
                .items()
                .iter()
                .map(|p| p.weight)
                .max()
                .unwrap_or(0),
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
            Query::Bool(b) => {
                let mut out = HashSet::new();
                for &ctx in ctxs {
                    if *b {
                        out.extend(self.index.bool_true_ids(ctx.xpath).iter());
                    } else {
                        out.extend(self.index.bool_false_ids(ctx.xpath).iter());
                    }
                }
                EvalOutcome::Docs(out)
            }

            Query::Wand(parts) | Query::Or(parts) => self.eval_union(parts, ctxs, mode),
            Query::And(parts) => self.eval_and(parts, ctxs, mode),

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

  
    fn eval_synonym(&self, parts: &[Query], ctxs: &[FieldCtx], mode: EvalMode) -> EvalOutcome {
        if mode == EvalMode::Filter {
            return self.eval_union(parts, ctxs, mode);
        }
        let mut by_doc: HashMap<DocId, ScoredPosting> = HashMap::new();
        for (index, part) in parts.iter().enumerate() {
            let EvalOutcome::Scored(scored) = self.evaluate(part, ctxs, EvalMode::Rank) else {
                continue;
            };
            let weight = synonym_weight(index, part, parts.first());
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

        //contributions that already cover every matching document
        let mut complete: HashMap<DocId, SearchHit> = HashMap::new();

        //same-element relevance across array subfields (bonus on top of field search)
        if include_array_groups {
            for hit in self.score_array_groups(query, restrict) {
                complete
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

        //fields the fast path can rank; the others are evaluated in full right away
        let mut fast: Vec<FieldPostings> = Vec::new();
        for &ctx in ctxs {
            if !ctx.row_keyed {
                if let Some(field) = self.field_postings(query, ctx) {
                    fast.push(field);
                    continue;
                }
            }

            let EvalOutcome::Scored(scored) = self.evaluate(query, &[ctx], EvalMode::Rank) else {
                continue;
            };

            //row-keyed field: resolve row -> doc, keep the best element per doc
            if ctx.row_keyed {
                for hit in self.resolve_group_hits(scored, restrict) {
                    merge_hit(&mut complete, hit);
                }
                continue;
            }

            for p in scored {
                if !restrict.is_none_or(|a| a.contains(&p.doc_id)) {
                    continue;
                }
                merge_hit(&mut complete, SearchHit {
                    doc_id: p.doc_id,
                    matched_terms: p.matched_terms,
                    weight_sum: (p.score / 1000).min(u32::MAX as u64) as u32,
                    distance_factor: p.density,
                    score: ((p.score as f32) / 1000.0) * p.density,
                });
            }
        }

        if fast.is_empty() {
            return top_k_from_hits(complete.into_values(), k);
        }
        let signals = QueryPlanner::signals(query);
        if signals.is_empty() {
            return self.exact_top_k(&fast, complete, k, restrict);
        }
        let mut hits = self.exact_top_k(&fast, complete, k.max(RERANK_POOL), restrict);
        self.apply_phrase_signals(&fast, &signals, &mut hits);
        hits.truncate(k);
        hits
    }

   
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

/// Positions where a phrase starts: each next list has its term one position further on.
fn phrase_starts(position_lists: &[&[u32]]) -> Vec<u32> {
    let Some((first, rest)) = position_lists.split_first() else {
        return Vec::new();
    };
    first
        .iter()
        .copied()
        .filter(|&start| {
            rest.iter()
                .enumerate()
                .all(|(offset, positions)| {
                    positions.binary_search(&(start + (offset as u32) + 1)).is_ok()
                })
        })
        .collect()
}

/// Docs where every list's term occurs at consecutive positions, in order.
///
/// A hit keeps only the positions where the phrase starts. Keeping all positions of the
/// first word made BM25 count every "sodium" in a document as an occurrence of "sodium
/// chloride", so articles about other sodium compounds outranked salt for "NaCl".
/// The weight is the first word's: `Posting::new` would leave it at 0, and both BM25 and
/// WAND's upper bound scale with it, so the phrase would never contribute to a score.
///
/// The shortest list drives the walk, and each other list is searched by galloping forward
/// from where its previous search ended (all lists are sorted by doc id). A full binary
/// search per document from the first word's list was 4-6x slower on common words.
fn phrase_postings(lists: &[PostingList]) -> PostingList {
    if lists.is_empty() || lists.iter().any(|list| list.is_empty()) {
        return PostingList::default();
    }
    let driver = (0..lists.len()).min_by_key(|&index| lists[index].len()).unwrap_or(0);
    let mut cursors = vec![0usize; lists.len()];
    let mut position_lists: Vec<&[u32]> = Vec::with_capacity(lists.len());
    let mut result = Vec::new();
    'docs: for posting in lists[driver].items() {
        let doc = posting.doc_id;
        for (index, list) in lists.iter().enumerate() {
            if index == driver {
                continue;
            }
            let items = list.items();
            //a list that has run out means no later document can hold the phrase
            let Some(found) = gallop(items, cursors[index], doc) else {
                break 'docs;
            };
            cursors[index] = found;
            if items[found].doc_id != doc {
                continue 'docs;
            }
        }
        position_lists.clear();
        for (index, list) in lists.iter().enumerate() {
            let on_doc = if index == driver { posting } else { &list.items()[cursors[index]] };
            position_lists.push(on_doc.positions.as_slice());
        }
        let starts = phrase_starts(&position_lists);
        if !starts.is_empty() {
            let first = if driver == 0 { posting } else { &lists[0].items()[cursors[0]] };
            result.push(Posting::with_weight(doc, starts, first.weight));
        }
    }
    PostingList::from_items(result)
}

/// First index at or after `from` whose doc id is at least `target`, `None` past the end:
/// doubling steps forward, then a binary search inside the last step.
fn gallop(items: &[Posting], from: usize, target: DocId) -> Option<usize> {
    if from >= items.len() {
        return None;
    }
    if items[from].doc_id >= target {
        return Some(from);
    }
    let mut low = from;
    let mut step = 1;
    let mut high = from + step;
    while high < items.len() && items[high].doc_id < target {
        low = high;
        step *= 2;
        high = from + step;
    }
    let high = high.min(items.len());
    let index = low + 1 + items[low + 1..high].partition_point(|p| p.doc_id < target);
    (index < items.len()).then_some(index)
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

fn copy_hit(hit: &SearchHit) -> SearchHit {
    SearchHit {
        doc_id: hit.doc_id,
        matched_terms: hit.matched_terms,
        weight_sum: hit.weight_sum,
        distance_factor: hit.distance_factor,
        score: hit.score,
    }
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
const SYNONYM_WEIGHT: f64 = 0.7;

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
/// only be answered there; on other fields they match nothing (as in the general path),
/// so they are left out.
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
            let typed = alternatives.first();
            let mut out = Vec::with_capacity(alternatives.len());
            for (index, alternative) in alternatives.iter().enumerate() {
                let weight = synonym_weight(index, alternative, typed);
                match alternative {
                    Query::Term(term) => out.push((Alt::Term(term), weight)),
                    Query::Phrase(words) => out.push((Alt::Phrase(words.to_vec()), weight)),
                    Query::Exact(text) if exact_field => {
                        let words: Vec<&str> = text.split_whitespace().collect();
                        if words.len() == 1 {
                            out.push((Alt::Exact(words[0]), weight));
                        } else {
                            out.push((
                                Alt::ExactPhrase(
                                    words
                                        .iter()
                                        .map(|&s| s.to_owned())
                                        .collect()
                                ),
                                weight,
                            ));
                        }
                    }
                    Query::Exact(_) => {}
                    Query::And(parts) => {
                        let terms = plain_terms(parts)?;
                        out.push((
                            Alt::Phrase(
                                terms
                                    .iter()
                                    .map(|&s| s.to_owned())
                                    .collect()
                            ),
                            weight,
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

/// Whether an alternative is written as more than one word.
fn spans_words(query: &Query) -> bool {
    match query {
        Query::Phrase(words) => words.len() > 1,
        Query::Exact(text) | Query::Term(text) => text.split_whitespace().nth(1).is_some(),
        Query::And(parts) | Query::Wand(parts) | Query::Or(parts) => parts.len() > 1,
        _ => false,
    }
}

/// Weight of one synonym alternative. The typed form (the first alternative) counts fully,
/// and so do exact equivalents: digits and Roman numerals, and the spelled-out form of a
/// short form the user typed (NaCl -> sodium chloride, CO2 -> carbon dioxide), which names
/// the one meaning the short form has here.
///
/// The other direction is not equal. A short form found for typed words is ambiguous:
/// "united states" -> US reaches US Yachts, US Melle and every other US title, which
/// outranked the United States article at full weight. So it gets SYNONYM_WEIGHT, like
/// loose equivalents of the same shape (piss / urine, salt / NaCl).
fn synonym_weight(index: usize, alternative: &Query, typed: Option<&Query>) -> f64 {
    if index == 0 {
        return 1.0;
    }
    if let Query::Term(term) = alternative {
        if alternative_weight(index, term) == 1.0 {
            return 1.0;
        }
    }
    match typed {
        Some(typed) if !spans_words(typed) && spans_words(alternative) => 1.0,
        _ => SYNONYM_WEIGHT,
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

/// Expands consecutive sibling terms (in query order). `wrap` is the parent
/// combinator, used to rebuild the user's original words as one alternative.
#[timed(search)]
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
                //the dictionary's phrase for the very words typed only re-finds what the
                //original form already finds, and costs a phrase scan
                let typed: Vec<String> = tokens[start..start + len]
                    .iter()
                    .map(|t| t.to_lowercase())
                    .collect();
                alternatives.retain(|alt| !matches!(alt, Query::Phrase(words) if *words == typed));
            } else {
                push_alternatives(node, &mut alternatives);
                move_typed_form_first(&mut alternatives, &words[index]);
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

/// The executor scores `Synonym` parts[0] as the typed form (full weight, and its document
/// frequency is used for the whole group), so the form the user typed must come first. For
/// a single word that is the matched dictionary variant: `Term` for a case-insensitive entry
/// (`piss`), `Exact` for a case-sensitive one (`CaP`).
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

