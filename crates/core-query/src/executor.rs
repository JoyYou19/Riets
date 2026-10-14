use core_index::{
    analyzer::analyzer::Analyzer,
    fuzzy::{ FuzzyExpansion, FuzzyOptions, FuzzySpec },
    posting::{ Posting, PostingList, ops::intersection },
    search::{ SearchIndex, SearchNumeric, SearchStats },
    types::{ DocId, XPathId },
};
use std::{ borrow::Cow, cmp::Ordering, collections::{ BinaryHeap, HashMap, hash_map::Entry }, u32 };

use ahash::{ HashSet, HashSetExt };

use core_protocol::command_reponse_definitions::{ Fuzziness, MatchMode };
use core_timing::timed;

use crate::{
    ScoredPosting,
    SearchHit,
    TopHit,
    ast::{ PhraseTerm, Query, SynonymAlternative },
    corpus_stats::{ CorpusStats, FieldTotals, FuzzyKey, LeafKey, ScoringStats },
    planner::{ DENSITY_BOOST, RankPlan, Tier },
    prepared::PreparedQuery,
    resolver::{ FieldCtx, FieldQuery, SameElementBinding },
    scorer::{ fuzzy_decay, score_term_into },
    wand::{
        FieldClauses,
        FieldTerms,
        FormRef,
        TermRef,
        WandHit,
        conjunctive_top_k_grouped,
        conjunctive_top_k_multi_field,
        wand_top_k_grouped,
        wand_top_k_multi_field,
    },
};

// A fuzzy query with more words than this matches nothing (one bit per word is tracked).
const MAX_FUZZY_WORDS: usize = 64;

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

// One leaf query read in one field: the key its statistics are filed under, the field it was
// looked up in (the exact field for an exact match) and the postings it found.
struct Leaf<'a> {
    key: LeafKey,
    xpath: XPathId,
    postings: Cow<'a, PostingList>,
}

// The postings of the terms of a flat query in one field, with the document frequency each is
// scored with.
struct FieldLists<'a> {
    xpath: XPathId,
    terms: Vec<(Cow<'a, PostingList>, u32)>,
}

// One form of a query clause read in one field: its posting lists with the document frequency
// each is scored with (see `wand::FormRef`).
struct FormLists<'a> {
    xpath: XPathId,
    lists: Vec<(Cow<'a, PostingList>, u32)>,
    weight: u16,
}

impl FormLists<'_> {
    fn as_ref(&self) -> FormRef<'_> {
        FormRef {
            xpath: self.xpath,
            lists: self.lists
                .iter()
                .map(|(postings, doc_freq)| TermRef {
                    postings,
                    doc_freq: *doc_freq,
                    max_weight: postings.max_weight(),
                })
                .collect(),
            weight: self.weight,
        }
    }
}

// Turns the AST into a PostingList or SearchHit
pub struct QueryExecutor<'a, I> where I: SearchIndex + SearchStats {
    // Which index are we searching?
    index: &'a I,

    // A query is analyzed also the same way as the index, so we could filter out words, stemming,
    // whaatever
    analyzer: &'a Analyzer,

    // Statistics of the whole search. Without them every score uses this index's own, which is
    // right when it holds all the documents.
    corpus: Option<&'a CorpusStats>,

    // Posting lists this search has already read, so they are not read again.
    prepared: Option<&'a PreparedQuery>,
}

impl<'a, I> QueryExecutor<'a, I> where I: SearchIndex + SearchStats + SearchNumeric {
    pub fn new(index: &'a I, analyzer: &'a Analyzer) -> Self {
        Self {
            index,
            analyzer,
            corpus: None,
            prepared: None,
        }
    }

    /// Scores with the statistics of the whole search instead of this index's own.
    pub fn with_corpus(mut self, corpus: &'a CorpusStats) -> Self {
        self.corpus = Some(corpus);
        self
    }

    /// Reads the posting lists `prepared` already holds from it instead of from the index.
    pub fn with_prepared(mut self, prepared: &'a PreparedQuery) -> Self {
        self.prepared = Some(prepared);
        self
    }

    fn stats(&self) -> ScoringStats<'_, I> {
        ScoringStats {
            index: self.index,
            corpus: self.corpus,
        }
    }

    //row-keyed postings are per element/row fold a hit onto its document
    fn fold_to_doc(&self, ctx: FieldCtx, id: DocId) -> Option<DocId> {
        if ctx.row_keyed { self.index.doc_of_row(id) } else { Some(id) }
    }

    /// The words `word` can be changed into, with the number of documents each is in: from every
    /// shard when the search has statistics, from this index otherwise.
    fn fuzzy_candidates(
        &self,
        word: &str,
        xpath: XPathId,
        opts: FuzzyOptions
    ) -> Vec<FuzzyExpansion> {
        self.corpus
            .and_then(|corpus| corpus.fuzzy(xpath, &FuzzyKey::new(word, opts)))
            .unwrap_or_else(|| self.index.fuzzy_expansions(word, xpath, opts))
    }

    fn field_totals(&self, xpath: XPathId) -> FieldTotals {
        FieldTotals {
            doc_count: self.index.doc_count(xpath),
            total_doc_len: self.index.total_doc_len(xpath),
        }
    }

    fn leaf_doc_freq(&self, xpath: XPathId, key: &LeafKey, local: usize) -> u32 {
        self.corpus.and_then(|corpus| corpus.doc_freq(xpath, key)).unwrap_or(local as u32)
    }

    fn fetch_field_lists(&self, terms: &[&str], xpaths: &[XPathId]) -> Vec<FieldLists<'a>> {
        xpaths
            .iter()
            .map(|&xpath| FieldLists {
                xpath,
                terms: terms
                    .iter()
                    .map(|term| self.fetch_term_list(term, xpath))
                    .collect(),
            })
            .collect()
    }

    #[timed(search)]
    fn fetch_term_list(&self, term: &str, xpath: XPathId) -> (Cow<'a, PostingList>, u32) {
        let key = LeafKey::Term(term.to_string());
        let (postings, local_doc_freq) = match self.prepared.and_then(|p| p.list(xpath, &key)) {
            Some(list) => (Cow::Borrowed(list), list.len() as u32),
            None => {
                let found = self.index.lookup_term(term, xpath);
                (Cow::Owned(found.postings), found.doc_freq)
            }
        };
        let doc_freq = self.leaf_doc_freq(xpath, &key, local_doc_freq as usize);
        (postings, doc_freq)
    }
    //WAND/conjunctive fast path for flat term lists (skips the general traversal)
    fn execute_top_k_retrieval(
        &self,
        query: &Query,
        xpaths: &[XPathId],
        k: usize,
        restrict: Option<&HashSet<DocId>>
    ) -> Option<Vec<WandHit>> {
        //disjunctive flat-term cases (Term / Wand / Or) -> additive WAND
        if let Some(terms) = additive_terms(query) {
            let lists = self.fetch_field_lists(&terms, xpaths);
            return Some(wand_top_k_multi_field(&self.stats(), &field_terms(&lists), k, restrict));
        }

        //conjunctive flat-term case (And) -> conjunctive top-k
        if let Query::And(parts) = query {
            if parts.iter().all(|p| matches!(p, Query::Term(_))) {
                let terms: Vec<&str> = parts
                    .iter()
                    .filter_map(|p| {
                        match p {
                            Query::Term(t) => Some(t.as_str()),
                            _ => None,
                        }
                    })
                    .collect();
                let lists = self.fetch_field_lists(&terms, xpaths);
                return Some(
                    conjunctive_top_k_multi_field(&self.stats(), &field_terms(&lists), k, restrict)
                );
            }
        }

        None
    }

    /// The WAND fast path for flat queries with synonym groups: every part a word or a group of
    /// words, phrases, exact matches and typed multi-word spans.
    fn execute_grouped_retrieval(
        &self,
        query: &Query,
        ctxs: &[FieldCtx],
        k: usize,
        restrict: Option<&HashSet<DocId>>
    ) -> Option<Vec<WandHit>> {
        let (conjunctive, parts) = grouped_parts(query)?;
        let lists: Vec<Vec<Vec<FormLists<'a>>>> = ctxs
            .iter()
            .map(|&ctx| {
                parts
                    .iter()
                    .map(|part| self.clause_forms(part, ctx))
                    .collect()
            })
            .collect();
        let fields: Vec<FieldClauses<'_>> = lists
            .iter()
            .map(|clauses| FieldClauses {
                clauses: clauses
                    .iter()
                    .map(|forms| forms.iter().map(FormLists::as_ref).collect())
                    .collect(),
            })
            .collect();
        let stats = self.stats();
        Some(
            if conjunctive {
                conjunctive_top_k_grouped(&stats, &fields, k, restrict)
            } else {
                wand_top_k_grouped(&stats, &fields, k, restrict)
            }
        )
    }

    /// The forms of one part of a flat query in one field, read and weighted the way
    /// `eval_synonym` scores them.
    fn clause_forms(&self, part: &Query, ctx: FieldCtx) -> Vec<FormLists<'a>> {
        let Query::Synonym(forms) = part else {
            return self.leaf_form(part, ctx, None, 1000).into_iter().collect();
        };
        let mut out = Vec::with_capacity(forms.len());
        // the typed word's document frequency, the lowest the other words are scored with
        let mut floor = None;
        for (index, form) in forms.iter().enumerate() {
            let built = match &form.query {
                Query::Term(_) => self.leaf_form(&form.query, ctx, floor, form.weight),
                Query::Phrase(_) | Query::Exact(_) => {
                    self.leaf_form(&form.query, ctx, None, form.weight)
                }
                Query::And(words) => {
                    let lists: Option<Vec<(Cow<'a, PostingList>, u32)>> = words
                        .iter()
                        .map(|word| {
                            let leaf = self.leaf(word, ctx)?;
                            let doc_freq = self.leaf_doc_freq(
                                leaf.xpath,
                                &leaf.key,
                                leaf.postings.len()
                            );
                            Some((leaf.postings, doc_freq))
                        })
                        .collect();
                    lists.map(|lists| FormLists {
                        xpath: ctx.xpath,
                        lists,
                        weight: form.weight,
                    })
                }
                _ => None,
            };
            if index == 0 && matches!(form.query, Query::Term(_)) {
                floor = built
                    .as_ref()
                    .map(|form| form.lists[0].1)
                    .filter(|&doc_freq| doc_freq > 0);
            }
            out.extend(built);
        }
        out
    }

    fn leaf_form(
        &self,
        query: &Query,
        ctx: FieldCtx,
        min_doc_freq: Option<u32>,
        weight: u16
    ) -> Option<FormLists<'a>> {
        let leaf = self.leaf(query, ctx)?;
        let own = self.leaf_doc_freq(leaf.xpath, &leaf.key, leaf.postings.len());
        let doc_freq = min_doc_freq.map_or(own, |floor| own.max(floor));
        Some(FormLists {
            xpath: leaf.xpath,
            lists: vec![(leaf.postings, doc_freq)],
            weight,
        })
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
    fn execute_phrase(&self, terms: &[PhraseTerm], xpath: XPathId) -> PostingList {
        use core_index::posting::Posting;

        if terms.is_empty() {
            return PostingList::default();
        }
        //dont really understand fully the Cow
        let lists: Vec<Cow<'a, PostingList>> = terms
            .iter()
            .map(|term| self.fetch_term_list(&term.text, xpath).0)
            .collect();
        if lists.iter().any(|list| list.is_empty()) {
            return PostingList::default();
        }

        let offsets: Vec<u32> = terms
            .iter()
            .map(|term| term.offset)
            .collect();
        // walk the rarest word's documents: the cost is that of the rarest word, not the first
        let rarest = (0..lists.len()).min_by_key(|&i| lists[i].len()).unwrap_or(0);

        let mut result = Vec::new();

        'docs: for driver in lists[rarest].items() {
            let doc_id = driver.doc_id;
            // in the words' own order, so phrase_matches lines them up with their offsets
            let mut position_lists: Vec<&[u32]> = Vec::with_capacity(lists.len());
            let mut first = driver;

            for (i, list) in lists.iter().enumerate() {
                let posting = if i == rarest {
                    driver
                } else {
                    match list.items().binary_search_by_key(&doc_id, |p| p.doc_id) {
                        Ok(index) => &list.items()[index],
                        Err(_) => {
                            continue 'docs;
                        }
                    }
                };
                if i == 0 {
                    first = posting;
                }
                position_lists.push(posting.positions.as_slice());
            }

            if phrase_matches(&position_lists, &offsets) {
                // the first word's posting, as before, so phrase scoring is unchanged
                result.push(Posting::with_weight(doc_id, first.positions.clone(), first.weight));
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
        // the exact field keeps every word, stopwords too: the words are adjacent
        let offsets: Vec<u32> = (0..words.len() as u32).collect();
        let rarest = (0..lists.len()).min_by_key(|&i| lists[i].len()).unwrap_or(0);
        let mut result = Vec::new();
        //a bit smarter loop than before 
        'docs: for driver in lists[rarest].items() {
            let doc_id = driver.doc_id;
            // in the words' own order, so phrase_matches lines them up with their offsets
            let mut position_lists: Vec<&[u32]> = Vec::with_capacity(lists.len());
            let mut first = driver;

            for (i, list) in lists.iter().enumerate() {
                let posting = if i == rarest {
                    driver
                } else {
                    match list.items().binary_search_by_key(&doc_id, |p| p.doc_id) {
                        Ok(index) => &list.items()[index],
                        Err(_) => {
                            continue 'docs;
                        }
                    }
                };
                if i == 0 {
                    first = posting;
                }
                position_lists.push(posting.positions.as_slice());
            }

            if phrase_matches(&position_lists, &offsets) {
                // the first word's posting, as before, so exact scoring is unchanged
                result.push(Posting::with_weight(doc_id, first.positions.clone(), first.weight));
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

    #[timed(search)]
    pub fn evaluate(&self, query: &Query, ctxs: &[FieldCtx], mode: EvalMode) -> EvalOutcome {
        match query {
            | Query::Term(_)
            | Query::Prefix(_)
            | Query::Wildcard(_)
            | Query::Phrase(_)
            | Query::Exact(_) => self.eval_leaf(query, ctxs, mode),
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
                        EvalOutcome::Scored(self.execute_scored_fuzzy(v, ctxs, *fz, *spec))
                    }
                }
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
            Query::Synonym(forms) => self.eval_synonym(forms, ctxs, mode),

            Query::SameElement(_) => EvalOutcome::Docs(HashSet::new()),

            Query::MatchAll => EvalOutcome::Docs(HashSet::new()),
        }
    }

    /// What a leaf query reads in one field.
    fn leaf(&self, query: &Query, ctx: FieldCtx) -> Option<Leaf<'a>> {
        let (key, xpath) = match query {
            Query::Term(v) => (LeafKey::Term(v.clone()), ctx.xpath),
            Query::Prefix(v) => (LeafKey::Prefix(v.clone()), ctx.xpath),
            Query::Wildcard(p) => (LeafKey::Wildcard(p.clone()), ctx.xpath),
            Query::Phrase(phrase) => (LeafKey::Phrase(phrase.clone()), ctx.xpath),
            Query::Exact(v) => (LeafKey::Exact(v.clone()), ctx.exact_xpath.unwrap_or(ctx.xpath)),
            _ => {
                return None;
            }
        };
        let postings = match self.prepared.and_then(|p| p.list(xpath, &key)) {
            Some(list) => Cow::Borrowed(list),
            None => Cow::Owned(self.fetch_leaf(query, xpath)),
        };
        Some(Leaf {
            key,
            xpath,
            postings,
        })
    }

    fn fetch_leaf(&self, query: &Query, xpath: XPathId) -> PostingList {
        match query {
            Query::Term(v) => self.execute_term(v, xpath).unwrap_or_default(),
            Query::Prefix(v) => self.execute_prefix(v, xpath).unwrap_or_default(),
            Query::Wildcard(p) => self.execute_wildcard(p, xpath),
            Query::Phrase(phrase) => self.execute_phrase(phrase, xpath),
            Query::Exact(v) => self.execute_exact(v, xpath),
            _ => PostingList::default(),
        }
    }

    fn eval_leaf(&self, query: &Query, ctxs: &[FieldCtx], mode: EvalMode) -> EvalOutcome {
        match mode {
            EvalMode::Filter => {
                let mut out = HashSet::new();
                for &ctx in ctxs {
                    if let Some(leaf) = self.leaf(query, ctx) {
                        out.extend(self.posting_ids(&leaf.postings));
                    }
                }
                EvalOutcome::Docs(out)
            }
            EvalMode::Rank => EvalOutcome::Scored(self.score_leaf(query, ctxs, &[])),
        }
    }

    /// Scores a leaf in every field of `ctxs` and adds the fields up. `min_doc_freqs[i]`, when
    /// set, is the lowest document frequency the leaf is scored with in `ctxs[i]`.
    fn score_leaf(
        &self,
        query: &Query,
        ctxs: &[FieldCtx],
        min_doc_freqs: &[Option<u32>]
    ) -> Vec<ScoredPosting> {
        let stats = self.stats();
        let mut acc: HashMap<DocId, ScoredPosting> = HashMap::new();
        //one map and one buffer for all the fields: the fields of a query land on the
        //same documents, and the per-field Vec only ever got drained into acc
        let mut doc_len: HashMap<DocId, f32> = HashMap::new();
        let mut scored: Vec<ScoredPosting> = Vec::new();

        for (index, &ctx) in ctxs.iter().enumerate() {
            let Some(leaf) = self.leaf(query, ctx) else {
                continue;
            };
            let own = self.leaf_doc_freq(leaf.xpath, &leaf.key, leaf.postings.len());
            let doc_freq = match min_doc_freqs.get(index).copied().flatten() {
                Some(floor) => own.max(floor),
                None => own,
            };

            scored.clear();
            score_term_into(
                &stats,
                &leaf.postings,
                leaf.xpath,
                doc_freq as f32,
                &mut doc_len,
                &mut scored
            );

            for mut hit in scored.drain(..) {
                //match array row to document
                let Some(doc) = self.fold_to_doc(ctx, hit.doc_id) else {
                    continue;
                };
                hit.doc_id = doc;

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
        }
        acc.into_values().collect()
    }

    /// A synonym group counts as one part of the query: a document scores by the best weighted
    /// form it contains.

    /// The typed form scores exactly as it would without the dictionary, so no document the
    /// typed words find loses score. Another single-word form is scored with the larger of its
    /// own document frequency and the typed word's (in each field, from all shards when the
    /// search has their statistics), so it never gets a larger idf than either: a rare form
    /// (`viii` for 8) cannot outrank the typed word, and a common one (`water` for H2O) cannot
    /// borrow the rare typed word's idf.
    fn eval_synonym(
        &self,
        forms: &[SynonymAlternative],
        ctxs: &[FieldCtx],
        mode: EvalMode
    ) -> EvalOutcome {
        if mode == EvalMode::Filter {
            let mut out = HashSet::new();
            for form in forms {
                if let EvalOutcome::Docs(docs) = self.evaluate(&form.query, ctxs, EvalMode::Filter) {
                    out.extend(docs);
                }
            }
            return EvalOutcome::Docs(out);
        }

        let typed_doc_freqs: Vec<Option<u32>> = match forms.first().map(|form| &form.query) {
            Some(typed @ Query::Term(_)) =>
                ctxs
                    .iter()
                    .map(|&ctx| {
                        let leaf = self.leaf(typed, ctx)?;
                        let doc_freq = self.leaf_doc_freq(
                            leaf.xpath,
                            &leaf.key,
                            leaf.postings.len()
                        );
                        (doc_freq > 0).then_some(doc_freq)
                    })
                    .collect(),
            _ => Vec::new(),
        };

        let mut best: HashMap<DocId, ScoredPosting> = HashMap::new();
        for form in forms {
            let scored = match &form.query {
                Query::Term(_) => self.score_leaf(&form.query, ctxs, &typed_doc_freqs),
                other =>
                    match self.evaluate(other, ctxs, EvalMode::Rank) {
                        EvalOutcome::Scored(scored) => scored,
                        EvalOutcome::Docs(_) => {
                            continue;
                        }
                    }
            };
            for mut hit in scored {
                hit.score = hit.score.saturating_mul(u64::from(form.weight)) / 1000;
                match best.entry(hit.doc_id) {
                    Entry::Vacant(e) => {
                        e.insert(hit);
                    }
                    Entry::Occupied(mut e) => {
                        if hit.score > e.get().score {
                            e.insert(hit);
                        }
                    }
                }
            }
        }
        EvalOutcome::Scored(best.into_values().collect())
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

    //AND logic: filter = row-level intersection, rank = doc-level intersection + additive scores
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
                //every part scores doc-keyed (row -> doc folded), intersect the doc ids, add scores
                let mut candidates: Option<HashSet<DocId>> = None;
                for part in parts {
                    let docs: HashSet<DocId> = match self.evaluate(part, ctxs, EvalMode::Rank) {
                        EvalOutcome::Scored(scored) => {
                            scored
                                .into_iter()
                                .map(|p| p.doc_id)
                                .collect()
                        }
                        EvalOutcome::Docs(_) => HashSet::new(),
                    };
                    candidates = Some(match candidates {
                        Some(cur) => cur.intersection(&docs).copied().collect(),
                        None => docs,
                    });
                    if candidates.as_ref().is_some_and(|s| s.is_empty()) {
                        return EvalOutcome::Scored(Vec::new());
                    }
                }
                let candidates = candidates.unwrap_or_default();

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
                                    cur.matched_terms = cur.matched_terms.saturating_add(
                                        hit.matched_terms
                                    );
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

    /// Reads what every given query needs from this index, once per field, into one
    /// `PreparedQuery`: posting lists, field totals and this index's share of the statistics.
    pub fn prepare<'q>(
        &self,
        queries: impl IntoIterator<Item = &'q Query>,
        ctxs: &[FieldCtx]
    ) -> PreparedQuery {
        let mut prepared = PreparedQuery::default();
        for query in queries {
            for &ctx in ctxs {
                self.prepare_leaves(query, ctx, &mut prepared);
            }
        }
        prepared
    }

    fn prepare_leaves(&self, query: &Query, ctx: FieldCtx, out: &mut PreparedQuery) {
        match query {
            Query::Wand(parts) | Query::Or(parts) | Query::And(parts) => {
                for part in parts {
                    self.prepare_leaves(part, ctx, out);
                }
            }
            Query::Synonym(forms) => {
                for form in forms {
                    self.prepare_leaves(&form.query, ctx, out);
                }
            }
            Query::Fuzzy(raw, fuzziness, spec) => {
                let words = self.fuzzy_words(raw);
                if words.len() >= MAX_FUZZY_WORDS {
                    return;
                }
                out.insert_field(ctx.xpath, self.field_totals(ctx.xpath));
                for word in &words {
                    let opts = fuzzy_options(word, *fuzziness, *spec);
                    out.insert_fuzzy(
                        ctx.xpath,
                        FuzzyKey::new(word, opts),
                        self.index.fuzzy_expansions(word, ctx.xpath, opts)
                    );
                }
            }
            _ => {
                if let Some(leaf) = self.leaf(query, ctx) {
                    out.insert_field(leaf.xpath, self.field_totals(leaf.xpath));
                    out.insert_leaf(leaf.xpath, leaf.key, leaf.postings.into_owned());
                }
            }
        }
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

    //now rank has multiple methods basically it works the same
    pub fn rank_retrieve(
        &self,
        query: Option<&Query>,
        ctxs: &[FieldCtx],
        k: usize,
        restrict: Option<&HashSet<DocId>>,
        match_mode: MatchMode
        // plan: RankPlan
    ) -> Vec<SearchHit> {
        if k == 0 || restrict.is_some_and(|d| d.is_empty()) || self.index.doc_range().is_none() {
            return Vec::new();
        }

        //no query
        let Some(query) = query else {
            //filter-only search
            return match restrict {
                Some(allowed) =>
                    top_k_from_hits(
                        allowed
                            .iter()
                            .copied()
                            .map(|doc_id| SearchHit {
                                doc_id,
                                matched_terms: 0,
                                weight_sum: 0,
                                distance_factor: 1.0,
                                score: 0.0,
                                tier: 0,
                            }),
                        k
                    ),
                None => Vec::new(),
            };
        };

        //explicit match_all
        if let Query::MatchAll = query {
            return match restrict {
                Some(allowed) =>
                    top_k_from_hits(
                        allowed
                            .iter()
                            .copied()
                            .map(|doc_id| SearchHit {
                                doc_id,
                                matched_terms: 0,
                                weight_sum: 0,
                                distance_factor: 1.0,
                                score: 0.0,
                                tier: 0,
                            }),
                        k
                    ),
                None =>
                    match self.index.doc_range() {
                        Some((min, max)) =>
                            top_k_from_hits(
                                (min..=max)
                                    .filter(|id| !self.index.is_deleted(*id))
                                    .map(|doc_id| SearchHit {
                                        doc_id,
                                        matched_terms: 0,
                                        weight_sum: 0,
                                        distance_factor: 1.0,
                                        score: 0.0,
                                        tier: 0,
                                    }),
                                k
                            ),
                        None => Vec::new(),
                    }
            };
        }

        let mut by_doc: HashMap<DocId, SearchHit> = HashMap::new();

        match match_mode {
            MatchMode::BestFields => {
                //flat term queries rank all the plain fields together: cutting every field to
                //its own top k before adding the fields up drops contributions, and then a
                //document's score depends on k
                let plain_xpaths: Vec<XPathId> = ctxs
                    .iter()
                    .filter(|ctx| !ctx.row_keyed)
                    .map(|ctx| ctx.xpath)
                    .collect();
                let flat_hits = if plain_xpaths.is_empty() {
                    None
                } else {
                    self.execute_top_k_retrieval(query, &plain_xpaths, k, restrict).or_else(|| {
                        let plain: Vec<FieldCtx> = ctxs
                            .iter()
                            .copied()
                            .filter(|ctx| !ctx.row_keyed)
                            .collect();
                        self.execute_grouped_retrieval(query, &plain, k, restrict)
                    })
                };
                let ranked_flat = flat_hits.is_some();
                for h in flat_hits.into_iter().flatten() {
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

                for &ctx in ctxs {
                    if ranked_flat && !ctx.row_keyed {
                        continue;
                    }
                    self.rank_single_field(query, ctx, k, restrict, &mut by_doc);
                }
            }
            MatchMode::CrossFields => {
                if let EvalOutcome::Scored(scored) = self.evaluate(query, ctxs, EvalMode::Rank) {
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
                            tier: 0,
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
            }
        }

        top_k_from_hits(by_doc.into_values(), k)
    }

    /// Runs the plan's tiers from most to least accurate until `k` hits are found. Each document
    /// appears once, at the best tier that found it; within a tier, hits order by score.
    #[timed(search)]
    pub fn rank_cascade(
        &self,
        plan: &RankPlan,
        ctxs: &[FieldCtx],
        k: usize,
        restrict: Option<&HashSet<DocId>>,
        match_mode: MatchMode
    ) -> Vec<SearchHit> {
        let mut out: Vec<SearchHit> = Vec::with_capacity(k);
        let mut seen: HashSet<DocId> = HashSet::new();
        let plain_xpaths: Vec<XPathId> = ctxs
            .iter()
            .filter(|ctx| !ctx.row_keyed)
            .map(|ctx| ctx.xpath)
            .collect();
        for (index, tier) in plan.tiers.iter().enumerate() {
            if out.len() >= k {
                break;
            }
            if plan.stop_after > 0 && out.len() >= plan.stop_after {
                break;
            }
            let tier_number = u8::try_from(index).unwrap_or(u8::MAX);
            // every hit already taken may come back from this tier too
            let fetch = (k - out.len()).saturating_add(seen.len());

            let hits = match tier {
                Tier::Phrase { terms, score_with } => {
                    let matched: std::collections::HashSet<
                        u64,
                        ahash::RandomState
                    > = self.phrase_docs(terms, ctxs);
                    let allowed: HashSet<DocId> = match restrict {
                        Some(limit) => matched.intersection(limit).copied().collect(),
                        None => matched,
                    };
                    if allowed.is_empty() {
                        continue;
                    }
                    self.rank_retrieve(Some(score_with), ctxs, fetch, Some(&allowed), match_mode)
                }
                | Tier::Exact(query)
                | Tier::All(query)
                | Tier::AllButOne(query)
                | Tier::Any(query)
                | Tier::Fuzzy(query) => {
                    self.rank_retrieve(Some(query), ctxs, fetch, restrict, match_mode)
                }
            };
            //This is the density part idk if we need it at all
            let mut hits = hits;
            // within "all words", hits whose words sit close together come first
            if let (Tier::All(_), Some(terms)) = (tier, plan.phrase_terms()) {
                self.apply_density(&mut hits, terms, &plain_xpaths);
                hits.sort_by(|a, b| {
                    b.score
                        .partial_cmp(&a.score)
                        .unwrap_or(Ordering::Equal)
                        .then_with(|| a.doc_id.cmp(&b.doc_id))
                });
            }

            for mut hit in hits {
                if !seen.insert(hit.doc_id) {
                    continue;
                }
                hit.tier = tier_number;
                out.push(hit);
                if out.len() == k {
                    break;
                }
            }
        }
        out
    }

    /// Documents containing the phrase in at least one plain field.
    fn phrase_docs(&self, terms: &[PhraseTerm], ctxs: &[FieldCtx]) -> HashSet<DocId> {
        let mut docs = HashSet::new();
        for ctx in ctxs.iter().filter(|ctx| !ctx.row_keyed) {
            docs.extend(
                self
                    .execute_phrase(terms, ctx.xpath)
                    .items()
                    .iter()
                    .map(|p| p.doc_id)
            );
        }
        docs
    }
    //density we all needed like betona koktelis
    /// Multiplies each hit's score by 1 + DENSITY_BOOST × how close together its words are, in
    /// its best field. Hits with no field holding every word keep their score.
    fn apply_density(&self, hits: &mut [SearchHit], terms: &[PhraseTerm], xpaths: &[XPathId]) {
        if terms.len() < 2 || hits.is_empty() {
            return;
        }
        // the span of the words as typed, stopword gaps included ("king of the hill" -> 4)
        let ideal = terms
            .last()
            .map_or(0, |term| term.offset)
            .saturating_add(1);

        // every word's list in every field, read once for all candidates
        let fields: Vec<Vec<Cow<'a, PostingList>>> = xpaths
            .iter()
            .map(|&xpath| {
                terms
                    .iter()
                    .map(|term| self.fetch_term_list(&term.text, xpath).0)
                    .collect()
            })
            .collect();

        for hit in hits.iter_mut() {
            let best = fields
                .iter()
                .filter_map(|lists| doc_span(lists, hit.doc_id))
                .map(|span| ((ideal as f32) / (span.max(1) as f32)).min(1.0))
                .fold(None, |best: Option<f32>, density| {
                    Some(best.map_or(density, |known| known.max(density)))
                });

            if let Some(density) = best {
                hit.score *= 1.0 + DENSITY_BOOST * density;
                hit.distance_factor = density;
            }
        }
    }
    pub fn rank(
        &self,
        query: Option<&Query>,
        ctxs: &[FieldCtx],
        k: usize,
        restrict: Option<&HashSet<DocId>>,
        match_mode: MatchMode,
        plan: &RankPlan
    ) -> Vec<SearchHit> {
        if plan.tiers.is_empty() {
            return self.rank_retrieve(query, ctxs, k, restrict, match_mode);
        }
        self.rank_cascade(plan, ctxs, k, restrict, match_mode)
    }
    //per-field ranking: one ctx at a time (best_fields arm + row-keyed cross_fields arm)
    fn rank_single_field(
        &self,
        query: &Query,
        ctx: FieldCtx,
        k: usize,
        restrict: Option<&HashSet<DocId>>,
        by_doc: &mut HashMap<DocId, SearchHit>
    ) {
        if !ctx.row_keyed {
            if let Some(hits) = self.execute_top_k_retrieval(query, &[ctx.xpath], k, restrict) {
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
                return;
            }
        }

        let EvalOutcome::Scored(scored) = self.evaluate(query, &[ctx], EvalMode::Rank) else {
            return;
        };

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
                tier: 0,
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

    #[timed(search)]
    fn execute_scored_fuzzy(
        &self,
        raw: &str,
        ctxs: &[FieldCtx],
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
        if words.len() >= MAX_FUZZY_WORDS {
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

            //word can land in ANY field, a doc passes when every word hit somewhere
            for &ctx in ctxs {
                //guess words based on max edit count, from the words of every shard
                let mut expansions = self.fuzzy_candidates(word, ctx.xpath, opts);
                //best guesses for the words
                rank_and_cap(&mut expansions, spec.max_expansions);

                //one word counts onces per firled so "Spiderman" and "Spidermen" count as one match not two
                let mut best_in_field: HashMap<DocId, ScoredPosting> = HashMap::new();
                //iterate all possible hits
                for expansion in expansions {
                    //                      the long function needs wand kip
                    let postings = self.index.lookup(&expansion.term, ctx.xpath);

                    scored_buf.clear();
                    let true_df = expansion.doc_freq as f32;
                    //BM25 scoring
                    score_term_into(
                        &self.stats(),
                        &postings,
                        ctx.xpath,
                        true_df,
                        &mut doc_len,
                        &mut scored_buf
                    );

                    let decay = fuzzy_decay(expansion.edits);

                    for mut scored in scored_buf.drain(..) {
                        //apply the edit decay to the score
                        scored.score = ((scored.score as f32) * decay) as u64;

                        //row-keyed postings are per element: fold onto the document
                        let Some(doc) = self.fold_to_doc(ctx, scored.doc_id) else {
                            continue;
                        };
                        scored.doc_id = doc;

                        match best_in_field.entry(doc) {
                            Entry::Occupied(mut slot) => {
                                if scored.score > slot.get().score {
                                    slot.insert(scored);
                                }
                            }
                            Entry::Vacant(slot) => {
                                slot.insert(scored);
                            }
                        }
                    }
                }

                // add this word's best match in this field to each document's total
                for (doc, scored) in best_in_field {
                    match acc.entry(doc) {
                        Entry::Occupied(mut slot) => {
                            let (hit, mask) = slot.get_mut();
                            if scored.score > hit.score {
                                hit.positions = scored.positions.clone();
                                hit.density = hit.density.max(scored.density);
                            }
                            hit.score = hit.score.saturating_add(scored.score);
                            // a word found in a second field is still one matched word
                            if (*mask & bit) == 0 {
                                hit.matched_terms = hit.matched_terms.saturating_add(1);
                            }
                            *mask |= bit;
                        }
                        Entry::Vacant(slot) => {
                            slot.insert((scored, bit));
                        }
                    }
                }
            }
        }
        let all_words = (1u64 << words.len()) - 1;

        acc.into_values()
            .filter(|(_, mask)| *mask == all_words)
            .map(|(hit, _)| hit)
            .collect()
    }
}
#[timed(search)]
fn phrase_matches(position_lists: &[&[u32]], offsets: &[u32]) -> bool {
    if position_lists.is_empty() {
        return false;
    }
    let anchor = (0..position_lists.len()).min_by_key(|&i| position_lists[i].len()).unwrap_or(0);

    position_lists[anchor].iter().any(|&position| {
        let Some(start) = position.checked_sub(offsets[anchor]) else {
            return false;
        };
        position_lists
            .iter()
            .zip(offsets)
            .enumerate()
            .all(|(i, (positions, &offset))| {
                i == anchor ||
                    start
                        .checked_add(offset)
                        .is_some_and(|expected| positions.binary_search(&expected).is_ok())
            })
    })
}

/// Smallest span of the document's positions that holds every list's word, or `None` when
/// the document lacks one of them.
/// test part with the density
fn doc_span(lists: &[Cow<'_, PostingList>], doc: DocId) -> Option<u32> {
    let mut positions: Vec<&[u32]> = Vec::with_capacity(lists.len());
    for list in lists {
        let items = list.items();
        let index = items.binary_search_by_key(&doc, |posting| posting.doc_id).ok()?;
        positions.push(items[index].positions.as_slice());
    }
    min_cover_span(&positions)
}
/// Smallest span (last - first + 1) holding at least one position from every list. Each list
/// must be sorted; `None` when a list is empty.
fn min_cover_span(lists: &[&[u32]]) -> Option<u32> {
    if lists.is_empty() || lists.iter().any(|list| list.is_empty()) {
        return None;
    }
    let mut events: Vec<(u32, usize)> = lists
        .iter()
        .enumerate()
        .flat_map(|(word, positions)| positions.iter().map(move |&position| (position, word)))
        .collect();
    events.sort_unstable();

    let mut counts = vec![0u32; lists.len()];
    let mut covered = 0usize;
    let mut best: Option<u32> = None;
    let mut left = 0usize;

    for right in 0..events.len() {
        let word = events[right].1;
        if counts[word] == 0 {
            covered += 1;
        }
        counts[word] += 1;

        // shrink from the left while the window still holds every word
        while covered == lists.len() {
            let span = events[right].0 - events[left].0 + 1;
            best = Some(best.map_or(span, |known| known.min(span)));
            let leaving = events[left].1;
            counts[leaving] -= 1;
            if counts[leaving] == 0 {
                covered -= 1;
            }
            left += 1;
        }
    }
    best
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
        a.tier
            .cmp(&b.tier)
            .then_with(|| b.score.partial_cmp(&a.score).unwrap_or(Ordering::Equal))
            .then_with(|| a.doc_id.cmp(&b.doc_id))
    });

    hits
}

//derives how much a word can be fuzzed basically
pub fn fuzzy_options(word: &str, fuzziness: Fuzziness, spec: FuzzySpec) -> FuzzyOptions {
    // a number is never a typo: 2021 must not also match 2020 or 2022
    let max_edits = if !word.is_empty() && word.bytes().all(|b| b.is_ascii_digit()) {
        0
    } else {
        fuzziness.resolve(word)
    };
    FuzzyOptions {
        max_edits,
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
        tier: 0,
    }
}

fn field_terms<'l>(lists: &'l [FieldLists<'_>]) -> Vec<FieldTerms<'l>> {
    lists
        .iter()
        .map(|field| FieldTerms {
            xpath: field.xpath,
            terms: field.terms
                .iter()
                .map(|(postings, doc_freq)| TermRef {
                    postings,
                    doc_freq: *doc_freq,
                    max_weight: postings.max_weight(),
                })
                .collect(),
        })
        .collect()
}

/// The parts of a flat query that has synonym groups, and whether all of them must match: every
/// part a word or a group of words, phrases, exact matches and words that must all be present.
/// `None` for any other shape, and for a query without synonym groups (`additive_terms` and the
/// conjunctive path take those).
fn grouped_parts(query: &Query) -> Option<(bool, Vec<&Query>)> {
    let (conjunctive, parts): (bool, Vec<&Query>) = match query {
        Query::Synonym(_) => (false, vec![query]),
        Query::Wand(parts) | Query::Or(parts) => (false, parts.iter().collect()),
        Query::And(parts) => (true, parts.iter().collect()),
        _ => {
            return None;
        }
    };
    let form_fits = |form: &Query| {
        match form {
            Query::Term(_) | Query::Phrase(_) | Query::Exact(_) => true,
            Query::And(words) => words.iter().all(|word| matches!(word, Query::Term(_))),
            _ => false,
        }
    };
    let fits = parts.iter().all(|part| {
        match part {
            Query::Term(_) => true,
            Query::Synonym(forms) => forms.iter().all(|form| form_fits(&form.query)),
            _ => false,
        }
    });
    let has_group = parts.iter().any(|part| matches!(part, Query::Synonym(_)));
    (fits && has_group).then_some((conjunctive, parts))
}

fn additive_terms(query: &Query) -> Option<Vec<&str>> {
    match query {
        Query::Term(term) => Some(vec![term.as_str()]),
        Query::Wand(parts) if parts.iter().all(|part| matches!(part, Query::Term(_))) =>
            Some(
                parts
                    .iter()
                    .filter_map(|part| {
                        match part {
                            Query::Term(term) => Some(term.as_str()),
                            _ => None,
                        }
                    })
                    .collect()
            ),
        _ => None,
    }
}
