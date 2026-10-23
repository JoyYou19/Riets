use core_index::{
    analyzer::analyzer::{ Analyzer, NAME_KEY, QUALIFIED_NAME_KEY }, fuzzy::{ FuzzyExpansion, FuzzyOptions, FuzzySpec, select_expansions }, posting::{ Posting, PostingList, ops::{intersection, union_many} }, search::{ SearchIndex, SearchNumeric, SearchStats }, types::{ DocId, XPathId, position_span, position_start },
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
    planner::{ DENSITY_BOOST, NAME_BONUS, RankPlan, TIER_STEP, Tier },
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

// The "any word" tier looks at this many candidates before ordering them by words matched.
const ANY_TIER_POOL: usize = 1000;

// The ranked pool (see `rank_cascade`): the share of a query's weight (its words' idf) a page has
// to cover to be in it, and how many candidates it ranks before ordering them by how close
// together their words are.
const MIN_COVERAGE: f32 = 0.5;
const POOL_SIZE: usize = 200;

// Closeness of words found in another order than typed (or not next to each other), see
// `apply_density`.
const OUT_OF_ORDER: f32 = 0.8;

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
    /// The form as WAND reads it, counting for searched field number `field`.
    fn as_ref(&self, field: usize) -> FormRef<'_> {
        FormRef {
            xpath: self.xpath,
            field,
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

    /// The WAND fast path for flat queries: every part a word or a group of words, phrases, exact
    /// matches and typed multi-word spans (best_fields: only queries with synonym groups, the
    /// others take `execute_top_k_retrieval`).
    ///
    /// `cross_fields`: a part matches in any of the fields and adds up its best form in each, as
    /// `cross_field_hits` scores it, one document at a time instead of scoring every match.
    /// Otherwise each field ranks its clauses on its own. `coverage` (cross_fields, a disjunction):
    /// keep only documents covering that share of the query's weight and scale their scores by it
    /// (see `wand::wand_top_k_grouped`); `None` is any part.
    fn execute_grouped_retrieval(
        &self,
        query: &Query,
        ctxs: &[FieldCtx],
        k: usize,
        restrict: Option<&HashSet<DocId>>,
        cross_fields: bool,
        coverage: Option<f32>
    ) -> Option<Vec<WandHit>> {
        let (conjunctive, parts) = grouped_parts(query, !cross_fields)?;
        // the forms of every part, each with the field it counts for
        let lists: Vec<Vec<Vec<(usize, FormLists<'a>)>>> = if cross_fields {
            // one clause per part, its forms in every field (a field's forms next to each other)
            vec![
                parts
                    .iter()
                    .map(|part| {
                        ctxs.iter()
                            .enumerate()
                            .flat_map(|(field, &ctx)| {
                                self.clause_forms(part, ctx)
                                    .into_iter()
                                    .map(move |form| (field, form))
                            })
                            .collect()
                    })
                    .collect()
            ]
        } else {
            ctxs.iter()
                .enumerate()
                .map(|(field, &ctx)| {
                    parts
                        .iter()
                        .map(|part| {
                            self.clause_forms(part, ctx)
                                .into_iter()
                                .map(|form| (field, form))
                                .collect()
                        })
                        .collect()
                })
                .collect()
        };
        let fields: Vec<FieldClauses<'_>> = lists
            .iter()
            .map(|clauses| FieldClauses {
                clauses: clauses
                    .iter()
                    .map(|forms| {
                        forms
                            .iter()
                            .map(|(field, form)| form.as_ref(*field))
                            .collect()
                    })
                    .collect(),
            })
            .collect();
        let stats = self.stats();
        Some(
            if conjunctive {
                conjunctive_top_k_grouped(&stats, &fields, k, restrict)
            } else {
                wand_top_k_grouped(&stats, &fields, k, coverage, restrict)
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
                // a phrase form adds up its words' scores on the documents where they form the
                // phrase, the way the typed words' AND does: the two compete on the same scale
                Query::Phrase(terms) => self.phrase_form(&form.query, terms, ctx, form.weight),
                Query::Exact(_) => self.leaf_form(&form.query, ctx, None, form.weight),
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
            .map(|w| {
                let opts = fuzzy_options(w, fuzziness, spec);
                let candidates = self
                    .fuzzy_candidates(w, xpath, opts)
                    .into_iter()
                    .map(|e| (e.term, e.edits, e.doc_freq));
                let picks = select_expansions(w, candidates, spec.max_expansions);
                let lists: Vec<PostingList> = picks
                    .iter()
                    .map(|p| self.index.lookup(&p.term, xpath))
                    .collect();
                union_many(lists.iter())
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

        // every word has to be in the field: the dictionary says so before any list is read, and
        // the lists are read from the rarest up, stopping as soon as no document has them all
        // (a sentence typed as a query is rarely a title: its rarest words already disagree)
        let doc_freqs: Vec<u32> = words
            .iter()
            .map(|word| self.index.doc_freq(word, xpath))
            .collect();
        if doc_freqs.contains(&0) {
            return PostingList::default();
        }
        let mut order: Vec<usize> = (0..words.len()).collect();
        order.sort_by_key(|&i| doc_freqs[i]);
        let mut lists: Vec<PostingList> = vec![PostingList::default(); words.len()];
        let mut candidates: Vec<DocId> = Vec::new();
        for (step, &i) in order.iter().enumerate() {
            lists[i] = self.index.lookup(words[i], xpath);
            let postings = lists[i].items();
            if step == 0 {
                candidates = postings
                    .iter()
                    .map(|posting| posting.doc_id)
                    .collect();
            } else {
                candidates.retain(|doc| postings.binary_search_by_key(doc, |p| p.doc_id).is_ok());
            }
            if candidates.is_empty() {
                return PostingList::default();
            }
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

            // every live document (inside an AND, a NOT or a filter it means "no restriction")
            Query::MatchAll =>
                EvalOutcome::Docs(match self.index.doc_range() {
                    Some((min, max)) =>
                        (min..=max).filter(|id| !self.index.is_deleted(*id)).collect(),
                    None => HashSet::new(),
                }),
        }
    }

    /// What a leaf query reads in one field.
    /// The key a leaf query's list is filed under and the field it is read from (the exact field
    /// for an exact match); `None` for anything that is not a leaf.
    fn leaf_key(query: &Query, ctx: FieldCtx) -> Option<(LeafKey, XPathId)> {
        Some(match query {
            Query::Term(v) => (LeafKey::Term(v.clone()), ctx.xpath),
            Query::Prefix(v) => (LeafKey::Prefix(v.clone()), ctx.xpath),
            Query::Wildcard(p) => (LeafKey::Wildcard(p.clone()), ctx.xpath),
            Query::Phrase(phrase) => (LeafKey::Phrase(phrase.clone()), ctx.xpath),
            Query::Exact(v) => (LeafKey::Exact(v.clone()), ctx.exact_xpath.unwrap_or(ctx.xpath)),
            _ => {
                return None;
            }
        })
    }

    fn leaf(&self, query: &Query, ctx: FieldCtx) -> Option<Leaf<'a>> {
        let (key, xpath) = Self::leaf_key(query, ctx)?;
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
            // lengths are per field: a cached title length must not score the text, or a long
            // article's text length its title
            doc_len.clear();
            score_term_into(
                &stats,
                &leaf.postings,
                leaf.xpath,
                doc_freq,
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
                // scored like its words, so "world war ii" weighs as much as "world war 2" typed
                Query::Phrase(terms) => self.score_phrase_as_words(terms, ctxs),
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
                //every part scores doc-keyed (row -> doc folded), intersect the doc ids, add scores.
                //Each part is evaluated once: scoring a part reads and scores all its postings
                let mut scored_parts: Vec<Vec<ScoredPosting>> = Vec::with_capacity(parts.len());
                for part in parts {
                    match self.evaluate(part, ctxs, EvalMode::Rank) {
                        EvalOutcome::Scored(scored) if !scored.is_empty() => {
                            scored_parts.push(scored);
                        }
                        _ => {
                            return EvalOutcome::Scored(Vec::new());
                        }
                    }
                }
                // smallest part first: the candidate set only shrinks from there
                scored_parts.sort_by_key(Vec::len);
                let mut candidates: HashSet<DocId> = scored_parts[0]
                    .iter()
                    .map(|p| p.doc_id)
                    .collect();
                for scored in &scored_parts[1..] {
                    let docs: HashSet<DocId> = scored
                        .iter()
                        .map(|p| p.doc_id)
                        .filter(|doc| candidates.contains(doc))
                        .collect();
                    candidates = docs;
                    if candidates.is_empty() {
                        return EvalOutcome::Scored(Vec::new());
                    }
                }

                let mut by_doc: HashMap<DocId, ScoredPosting> = HashMap::with_capacity(
                    candidates.len()
                );
                for scored in scored_parts {
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

            Query::Phrase(terms) => {
                // phrase forms are also scored by their words (score_phrase_as_words)
                for term in terms {
                    self.prepare_leaves(&Query::Term(term.text.clone()), ctx, out);
                }
                let Some((key, xpath)) = Self::leaf_key(query, ctx) else {
                    return;
                };
                if out.list(xpath, &key).is_some() {
                    return;
                }
                out.insert_field(xpath, self.field_totals(xpath));
                let postings = self.fetch_leaf(query, xpath);
                out.insert_leaf(xpath, key, postings);
            }
            _ => {
                let Some((key, xpath)) = Self::leaf_key(query, ctx) else {
                    return;
                };
                // the tiers of a plan share their words: read each list once
                if out.list(xpath, &key).is_some() {
                    return;
                }
                out.insert_field(xpath, self.field_totals(xpath));
                let postings = self.fetch_leaf(query, xpath);
                out.insert_leaf(xpath, key, postings);
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
        match_mode: MatchMode // plan: RankPlan
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
                        self.execute_grouped_retrieval(query, &plain, k, restrict, false, None)
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
                // for "any word" queries both modes score the same (each field's match adds up), so
                // take WAND top-k instead of scoring every document that has one of the words
                if matches!(query, Query::Wand(_) | Query::Term(_)) {
                    return self.rank_retrieve(
                        Some(query),
                        ctxs,
                        k,
                        restrict,
                        MatchMode::BestFields
                    );
                }
                // flat queries (words and synonym groups, all of them or any of them): one
                // document at a time, instead of scoring every match and cutting to k after
                if ctxs.iter().all(|ctx| !ctx.row_keyed) {
                    if
                        let Some(hits) = self.execute_grouped_retrieval(
                            query,
                            ctxs,
                            k,
                            restrict,
                            true,
                            None
                        )
                    {
                        return hits.into_iter().map(wand_hit_to_search_hit).collect();
                    }
                }
                return top_k_from_hits(self.cross_field_hits(query, ctxs, restrict), k);
            }
        }

        top_k_from_hits(by_doc.into_values(), k)
    }

    /// Every document `query` matches across the fields, scored (the CrossFields ranking before
    /// it is cut to k), so the cascade can take several tiers' top hits from one evaluation.
    fn cross_field_hits(
        &self,
        query: &Query,
        ctxs: &[FieldCtx],
        restrict: Option<&HashSet<DocId>>
    ) -> Vec<SearchHit> {
        let EvalOutcome::Scored(scored) = self.evaluate(query, ctxs, EvalMode::Rank) else {
            return Vec::new();
        };
        let mut by_doc: HashMap<DocId, SearchHit> = HashMap::with_capacity(scored.len());
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
        by_doc.into_values().collect()
    }

    /// The documents whose whole value in a name field is one of `keys`: level 0 the value as
    /// written, level 1 the value without its qualifier ("Tom Holland (author)").
    fn name_docs(&self, keys: &[String], ctxs: &[FieldCtx], qualified: bool) -> HashSet<DocId> {
        let marker = if qualified { QUALIFIED_NAME_KEY } else { NAME_KEY };
        let mut docs = HashSet::new();
        for ctx in ctxs.iter().filter(|ctx| !ctx.row_keyed) {
            let Some(xpath) = ctx.exact_xpath else {
                continue;
            };
            for key in keys {
                let list = self.index.lookup(&format!("{marker}{key}"), xpath);
                docs.extend(
                    list
                        .items()
                        .iter()
                        .map(|posting| posting.doc_id)
                );
            }
        }
        docs
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
        // the name, phrase and "all words" tiers score the same query: under CrossFields it is
        // evaluated once (every match, scored) and each tier takes its own top hits from that.
        // Not for flat queries: `rank_retrieve` ranks those one document at a time, faster than
        // scoring every match even once
        let flat_fields = ctxs.iter().all(|ctx| !ctx.row_keyed);
        let shared_query: Option<&Query> = match match_mode {
            MatchMode::CrossFields =>
                plan.tiers.iter().find_map(|tier| {
                    match tier {
                        Tier::All(query) if
                            !matches!(query, Query::Wand(_) | Query::Term(_)) &&
                            !(flat_fields && grouped_parts(query, false).is_some())
                        => {
                            Some(query)
                        }
                        _ => None,
                    }
                }),
            MatchMode::BestFields => None,
        };
        // The ranked pool. For a flat query of several words over plain fields (cross_fields),
        // the "phrase", "all words" and "all words but one" tiers are one ranking instead of three

        let pool_parts: Option<&Vec<Query>> = match match_mode {
            MatchMode::CrossFields if flat_fields => {
                plan.tiers.iter().find_map(|tier| {
                    match tier {
                        Tier::All(Query::And(parts)) if
                            parts.len() >= 2 &&
                            grouped_parts(&Query::Wand(parts.clone()), false).is_some()
                        => {
                            Some(parts)
                        }
                        _ => None,
                    }
                })
            }
            _ => None,
        };
        let mut shared_hits: Option<Vec<SearchHit>> = None;
        let mut ranked = |
            query: &Query,
            fetch: usize,
            allowed: Option<&HashSet<DocId>>
        | -> Vec<SearchHit> {
            match shared_query {
                Some(shared) if shared == query => {
                    let all = shared_hits.get_or_insert_with(||
                        self.cross_field_hits(shared, ctxs, restrict)
                    );
                    match allowed {
                        Some(allowed) =>
                            top_k_from_hits(
                                all
                                    .iter()
                                    .filter(|hit| allowed.contains(&hit.doc_id))
                                    .cloned(),
                                fetch
                            ),
                        None => top_k_from_hits(all.iter().cloned(), fetch),
                    }
                }
                _ => self.rank_retrieve(Some(query), ctxs, fetch, allowed.or(restrict), match_mode),
            }
        };
        for (index, tier) in plan.tiers.iter().enumerate() {
            if out.len() >= k {
                break;
            }
            if plan.stop_after > 0 && out.len() >= plan.stop_after {
                break;
            }
            // each tier owns TIER_STEP consecutive values: the "any word" tier spreads its hits over
            // them by words missing, so every later merge (shards, pages) keeps that order too
            let tier_number = u8::try_from(index * TIER_STEP).unwrap_or(u8::MAX);
            // every hit already taken may come back from this tier too
            let fetch = (k - out.len()).saturating_add(seen.len());

            let hits = match tier {
                // the pool ranks these (see `pool_parts`)
                Tier::Phrase { .. } | Tier::AllButOne(_) if pool_parts.is_some() => {
                    continue;
                }
                Tier::All(_) if pool_parts.is_some() => {
                    let pool_query = Query::Wand(pool_parts.cloned().unwrap_or_default());
                    let mut hits = self
                        .execute_grouped_retrieval(
                            &pool_query,
                            ctxs,
                            fetch.max(POOL_SIZE),
                            restrict,
                            true,
                            Some(MIN_COVERAGE)
                        )
                        .map(|hits| {
                            hits.into_iter().map(wand_hit_to_search_hit).collect::<Vec<_>>()
                        })
                        .unwrap_or_default();

                    // A document whose name is a run of the query's words is what the query is
                    // about: "Vera Wang was born in March" is about Vera Wang. Its score is
                    // raised by NAME_BONUS times the share of the query the run holds, each word
                    // weighed by its idf where it is most common (so "Vera Wang" outweighs
                    // "March"). A name holding every query word counts as holding the whole query.
                    if !plan.words.is_empty() {
                        let stats = self.stats();
                        let weight_of = |word: &str| -> f32 {
                            ctxs.iter()
                                .filter(|ctx| !ctx.row_keyed)
                                .filter_map(|ctx| {
                                    let doc_freq = self.fetch_term_list(word, ctx.xpath).1 as f32;
                                    let docs = stats.doc_count(ctx.xpath) as f32;
                                    (doc_freq > 0.0 && docs > 0.0).then(|| {
                                        ((docs - doc_freq + 0.5) / (doc_freq + 0.5) + 1.0).ln()
                                    })
                                })
                                .reduce(f32::min)
                                .unwrap_or(0.0)
                        };
                        let weights: HashMap<&str, f32> = plan.words
                            .iter()
                            .map(|word| (word.as_str(), weight_of(word)))
                            .collect();
                        let total: f32 = plan.words
                            .iter()
                            .map(|word| weights[word.as_str()])
                            .sum();

                        let mut shares: HashMap<DocId, f32> = HashMap::new();
                        if total > 0.0 {
                            for key in &plan.named_runs {
                                let held: f32 = key
                                    .split(' ')
                                    .filter_map(|word| weights.get(word))
                                    .sum();
                                let share = (held / total).min(1.0);
                                for qualified in [false, true] {
                                    for doc in self.name_docs(
                                        std::slice::from_ref(key),
                                        ctxs,
                                        qualified
                                    ) {
                                        if restrict.is_none_or(|limit| limit.contains(&doc)) {
                                            let best = shares.entry(doc).or_insert(0.0);
                                            *best = best.max(share);
                                        }
                                    }
                                }
                            }
                        }
                        if plan.words.len() >= 2 {
                            for ctx in ctxs
                                .iter()
                                .filter(|ctx| !ctx.row_keyed && ctx.exact_xpath.is_some()) {
                                let lists: Vec<Cow<'a, PostingList>> = plan.words
                                    .iter()
                                    .map(|word| self.fetch_term_list(word, ctx.xpath).0)
                                    .collect();
                                for hit in &hits {
                                    let named = lists.iter().all(|list| {
                                        list.items()
                                            .binary_search_by_key(&hit.doc_id, |p| p.doc_id)
                                            .is_ok()
                                    });
                                    if named {
                                        shares.insert(hit.doc_id, 1.0);
                                    }
                                }
                            }
                        }

                        if !shares.is_empty() {
                            // named documents the pool left out are scored the same way
                            let pooled: HashSet<DocId> = hits
                                .iter()
                                .map(|hit| hit.doc_id)
                                .collect();
                            let missing: HashSet<DocId> = shares
                                .keys()
                                .copied()
                                .filter(|doc| !pooled.contains(doc))
                                .collect();
                            if !missing.is_empty() {
                                if
                                    let Some(extra) = self.execute_grouped_retrieval(
                                        &pool_query,
                                        ctxs,
                                        missing.len(),
                                        Some(&missing),
                                        true,
                                        Some(0.0)
                                    )
                                {
                                    hits.extend(extra.into_iter().map(wand_hit_to_search_hit));
                                }
                            }
                            for hit in &mut hits {
                                if let Some(share) = shares.get(&hit.doc_id) {
                                    hit.score *= 1.0 + NAME_BONUS * share;
                                }
                            }
                            hits.sort_by(|a, b| {
                                b.score
                                    .partial_cmp(&a.score)
                                    .unwrap_or(Ordering::Equal)
                                    .then_with(|| a.doc_id.cmp(&b.doc_id))
                            });
                        }
                    }
                    hits
                }
                Tier::Name { keys, score_with } => {
                    let mut hits = Vec::new();
                    let mut named: HashSet<DocId> = HashSet::new();
                    for (level, qualified) in [
                        (0u8, false),
                        (1u8, true),
                    ] {
                        let mut docs = self.name_docs(keys, ctxs, qualified);
                        docs.retain(|doc| {
                            !named.contains(doc) &&
                                !seen.contains(doc) &&
                                restrict.is_none_or(|limit| limit.contains(doc))
                        });
                        if docs.is_empty() {
                            continue;
                        }
                        named.extend(docs.iter().copied());
                        let mut level_hits = ranked(score_with, docs.len().min(fetch), Some(&docs));
                        // a name found only through its key (the query's words score nothing in
                        // this document) still belongs here, after the scored ones
                        if level_hits.len() < docs.len().min(fetch) {
                            let scored: HashSet<DocId> = level_hits
                                .iter()
                                .map(|hit| hit.doc_id)
                                .collect();
                            let mut rest: Vec<DocId> = docs
                                .iter()
                                .copied()
                                .filter(|doc| !scored.contains(doc))
                                .collect();
                            rest.sort_unstable();
                            level_hits.extend(
                                rest.into_iter().map(|doc_id| SearchHit {
                                    doc_id,
                                    matched_terms: 0,
                                    weight_sum: 0,
                                    distance_factor: 1.0,
                                    score: 0.0,
                                    tier: 0,
                                })
                            );
                        }
                        for hit in &mut level_hits {
                            hit.tier = level;
                        }
                        hits.extend(level_hits);
                    }
                    hits
                }
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
                    ranked(score_with, fetch, Some(&allowed))
                }
                Tier::Fuzzy(Query::Fuzzy(raw, ..)) if self.all_words_known(raw, &plain_xpaths) => {
                    continue;
                }
                Tier::Any(query) => {
                    // a larger pool, so pages with more words survive until they are ordered by words
                    ranked(query, fetch.max(ANY_TIER_POOL), None)
                }
                Tier::AllButOne(Query::Wand(parts)) => {
                    // queries the pool cannot take: an OR of the words' ANDs, each with one word
                    // left out
                    let alternatives: Vec<Query> = (0..parts.len())
                        .map(|left_out| {
                            Query::And(
                                parts
                                    .iter()
                                    .enumerate()
                                    .filter(|(index, _)| *index != left_out)
                                    .map(|(_, part)| part.clone())
                                    .collect()
                            )
                        })
                        .collect();
                    ranked(&Query::Or(alternatives), fetch, None)
                }
                | Tier::Exact(query)
                | Tier::All(query)
                | Tier::AllButOne(query)
                | Tier::Fuzzy(query) => ranked(query, fetch, None),
            };
            let mut hits = hits;
            // within "all words" (and the pool), hits whose words sit close together come first
            if let (Tier::All(_), Some(terms)) = (tier, plan.phrase_terms()) {
                self.apply_density(&mut hits, terms, &plain_xpaths);
                hits.sort_by(|a, b| {
                    b.score
                        .partial_cmp(&a.score)
                        .unwrap_or(Ordering::Equal)
                        .then_with(|| a.doc_id.cmp(&b.doc_id))
                });
            }

            // within "any word", pages holding more of the query's weight come first
            let mut missing_levels: HashMap<DocId, u8> = HashMap::new();
            if let (Tier::Any(_), Some(terms)) = (tier, plan.phrase_terms()) {
                missing_levels = self.order_by_words_matched(&mut hits, terms, &plain_xpaths);
            }

            for mut hit in hits {
                if !seen.insert(hit.doc_id) {
                    continue;
                }
                hit.tier = match (tier, plan.phrase_terms()) {
                    (Tier::Any(_), Some(_)) => {
                        let missing = missing_levels.get(&hit.doc_id).copied().unwrap_or(0);
                        tier_number.saturating_add(missing)
                    }
                    // rank_retrieve leaves tier 0; the name tier sets its level (0 or 1)
                    _ => tier_number.saturating_add(hit.tier),
                };
                out.push(hit);
                if out.len() == k {
                    break;
                }
            }
        }
        out
    }

    /// A phrase scored like the AND of its words, on the documents where the words form the
    /// phrase.
    fn score_phrase_as_words(&self, terms: &[PhraseTerm], ctxs: &[FieldCtx]) -> Vec<ScoredPosting> {
        let mut matched: Vec<DocId> = self.phrase_docs(terms, ctxs).into_iter().collect();
        if matched.is_empty() {
            return Vec::new();
        }
        matched.sort_unstable();

        let stats = self.stats();
        let mut acc: HashMap<DocId, ScoredPosting> = HashMap::new();
        let mut doc_len: HashMap<DocId, f32> = HashMap::new();
        let mut scored: Vec<ScoredPosting> = Vec::new();
        for ctx in ctxs.iter().filter(|ctx| !ctx.row_keyed) {
            doc_len.clear();
            for term in terms {
                let (list, doc_freq) = self.fetch_term_list(&term.text, ctx.xpath);
                let kept = postings_of(&list, &matched);
                scored.clear();
                score_term_into(&stats, &kept, ctx.xpath, doc_freq, &mut doc_len, &mut scored);
                for hit in scored.drain(..) {
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
        }
        acc.into_values().collect()
    }

    /// `score_phrase_as_words` for the WAND path: each word's list in this field, kept to the
    /// documents where the words form the phrase, scored with the word's own document frequency.
    fn phrase_form(
        &self,
        phrase: &Query,
        terms: &[PhraseTerm],
        ctx: FieldCtx,
        weight: u16
    ) -> Option<FormLists<'a>> {
        let leaf = self.leaf(phrase, ctx)?;
        let docs: Vec<DocId> = leaf.postings
            .items()
            .iter()
            .map(|p| p.doc_id)
            .collect();
        let lists = terms
            .iter()
            .map(|term| {
                let (list, doc_freq) = self.fetch_term_list(&term.text, ctx.xpath);
                (Cow::Owned(postings_of(&list, &docs)), doc_freq)
            })
            .collect();
        Some(FormLists {
            xpath: ctx.xpath,
            lists,
            weight,
        })
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
    /// Multiplies each hit's score by 1 + DENSITY_BOOST × how close together its words are, in
    /// its best field: the span of the words as typed against the span they take in the field
    /// (1 when they form the phrase), times the share of the query's words the field holds, so
    /// a page with two of four words side by side gets less than one with all four. A field
    /// needs at least two of the words to count; hits without one keep their score.
    fn apply_density(&self, hits: &mut [SearchHit], terms: &[PhraseTerm], xpaths: &[XPathId]) {
        if terms.len() < 2 || hits.is_empty() {
            return;
        }

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

        let mut present: Vec<(u32, &[u32])> = Vec::with_capacity(terms.len());
        for hit in hits.iter_mut() {
            let mut best: Option<f32> = None;
            for lists in &fields {
                // the words this field of the document holds, with where they were typed
                present.clear();
                for (term, list) in terms.iter().zip(lists) {
                    let items = list.items();
                    if
                        let Ok(index) = items.binary_search_by_key(
                            &hit.doc_id,
                            |posting| posting.doc_id
                        )
                    {
                        present.push((term.offset, items[index].positions.as_slice()));
                    }
                }
                if present.len() < 2 {
                    continue;
                }
                let positions: Vec<&[u32]> = present
                    .iter()
                    .map(|&(_, positions)| positions)
                    .collect();
                let Some(span) = min_cover_span(&positions) else {
                    continue;
                };
                // the span of these words as typed, stopword gaps included ("king of the hill" -> 4)
                let first = present
                    .iter()
                    .map(|&(offset, _)| offset)
                    .min()
                    .unwrap_or(0);
                let last = present
                    .iter()
                    .map(|&(offset, _)| offset)
                    .max()
                    .unwrap_or(0);
                let ideal = (last - first).saturating_add(1);
                let mut closeness = ((ideal as f32) / (span.max(1) as f32)).min(1.0);
                // full credit only for the words in the order they were typed: "red balloon"
                // before "balloon red"
                let offsets: Vec<u32> = present
                    .iter()
                    .map(|&(offset, _)| offset)
                    .collect();
                if !phrase_matches(&positions, &offsets) {
                    closeness *= OUT_OF_ORDER;
                }
                let density = (closeness * (present.len() as f32)) / (terms.len() as f32);
                best = Some(best.map_or(density, |known| known.max(density)));
            }

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
    /// Orders hits by how much of the query's weight they hold (each word weighed by its idf in
    /// the plain field where it is most common, as the coverage pool weighs it), then by score:
    /// a page missing only common words ("the year", "was") comes before one missing the rare
    /// word the query is about. Returns each hit's level within the tier, 0 (holds everything)
    /// to TIER_STEP - 1 (holds nothing), so merges across shards and pages keep the order.
    fn order_by_words_matched(
        &self,
        hits: &mut [SearchHit],
        terms: &[PhraseTerm],
        xpaths: &[XPathId]
    ) -> HashMap<DocId, u8> {
        let stats = self.stats();
        // each word's list in each field, read once for all candidates, and its weight
        let words: Vec<(Vec<Cow<'a, PostingList>>, f32)> = terms
            .iter()
            .map(|term| {
                let mut lists = Vec::with_capacity(xpaths.len());
                let mut weight: Option<f32> = None;
                for &xpath in xpaths {
                    let (list, doc_freq) = self.fetch_term_list(&term.text, xpath);
                    let docs = stats.doc_count(xpath) as f32;
                    if doc_freq > 0 && docs > 0.0 {
                        let df = (doc_freq as f32).min(docs);
                        let idf = ((docs - df + 0.5) / (df + 0.5) + 1.0).ln();
                        weight = Some(weight.map_or(idf, |known| known.min(idf)));
                    }
                    lists.push(list);
                }
                (lists, weight.unwrap_or(0.0))
            })
            .collect();
        let total: f32 = words.iter().map(|(_, weight)| weight).sum();

        let mut levels: HashMap<DocId, u8> = HashMap::with_capacity(hits.len());
        for hit in hits.iter_mut() {
            let mut held = 0.0f32;
            let mut count = 0usize;
            for (lists, weight) in &words {
                let present = lists.iter().any(|list| {
                    list.items()
                        .binary_search_by_key(&hit.doc_id, |posting| posting.doc_id)
                        .is_ok()
                });
                if present {
                    held += weight;
                    count += 1;
                }
            }
            hit.matched_terms = count;
            let missing = if total > 0.0 { (1.0 - held / total).clamp(0.0, 1.0) } else { 0.0 };
            let level = (missing * (TIER_STEP - 1) as f32).round() as u8;
            levels.insert(hit.doc_id, level);
        }

        hits.sort_by(|a, b| {
            levels[&a.doc_id]
                .cmp(&levels[&b.doc_id])
                .then_with(|| b.score.partial_cmp(&a.score).unwrap_or(Ordering::Equal))
                .then_with(|| a.doc_id.cmp(&b.doc_id))
        });
        levels
    }

    fn all_words_known(&self, raw: &str, xpaths: &[XPathId]) -> bool {
        self.fuzzy_words(raw)
            .iter()
            .all(|word| { xpaths.iter().any(|&xpath| self.fetch_term_list(word, xpath).1 > 0) })
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
                // lengths are per field (see score_leaf)
                doc_len.clear();
                //guess words based on max edit count, from the words of every shard
                let candidates = self
                    .fuzzy_candidates(word, ctx.xpath, opts)
                    .into_iter()
                    .map(|e| (e.term, e.edits, e.doc_freq));
                let expansions = select_expansions(word, candidates, spec.max_expansions);

                //one word counts onces per firled so "Spiderman" and "Spidermen" count as one match not two
                let mut best_in_field: HashMap<DocId, ScoredPosting> = HashMap::new();
                //iterate all possible hits
                for expansion in expansions {
                    //                      the long function needs wand kip
                    let postings = self.index.lookup(&expansion.term, ctx.xpath);

                    scored_buf.clear();
                    let true_df = expansion.doc_freq;
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
/// The postings of `list` for the given documents (sorted), found by binary search: the phrase
/// matches are few, the word's list can be long.
fn postings_of(list: &PostingList, docs: &[DocId]) -> PostingList {
    let items = list.items();
    PostingList::from_sorted(
        docs
            .iter()
            .filter_map(|doc| {
                items
                    .binary_search_by_key(doc, |posting| posting.doc_id)
                    .ok()
                    .map(|index| items[index].clone())
            })
            .collect()
    )
}

#[timed(search)]
fn phrase_matches(position_lists: &[&[u32]], offsets: &[u32]) -> bool {
    let Some((first, rest)) = position_lists.split_first() else {
        return false;
    };

    'starts: for &encoded in *first {
        let mut next = position_start(encoded) + position_span(encoded);
        for (i, positions) in rest.iter().enumerate() {
            // query words dropped between these two (stopwords) left the same gap in the document
            let gap = offsets[i + 1].saturating_sub(offsets[i]).saturating_sub(1);
            let want = next + gap;
            let at = positions.partition_point(|&p| position_start(p) < want);
            match positions.get(at) {
                Some(&p) if position_start(p) == want => {
                    next = want + position_span(p);
                }
                _ => {
                    continue 'starts;
                }
            }
        }
        return true;
    }
    false
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
        .flat_map(|(word, positions)| {
            positions.iter().map(move |&position| (position_start(position), word))
        })
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

/// The parts of a flat query, and whether all of them must match: every part a word or a group
/// of words, phrases, exact matches and words that must all be present. `None` for any other
/// shape, and with `needs_group` for a query without synonym groups (`additive_terms` and the
/// conjunctive path take those in best_fields).
fn grouped_parts(query: &Query, needs_group: bool) -> Option<(bool, Vec<&Query>)> {
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
    (fits && (has_group || !needs_group)).then_some((conjunctive, parts))
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
